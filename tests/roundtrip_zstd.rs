//! The zstd backend must reproduce the reference decoder's output byte for byte and every
//! reported copy must be a genuine forward repeat, across window sizes and multiple frames.

mod common;

use common::{covers, drain, drain_result, fasta, plant, zstd_encode, zstd_reference};
use lz_copyback::backends::ZstdBackend;
use lz_copyback::{LzBackend, TrackerConfig, TrackingDecoder};

fn decoder(bytes: Vec<u8>, config: TrackerConfig) -> TrackingDecoder<ZstdBackend> {
    TrackingDecoder::new(ZstdBackend::new(std::io::Cursor::new(bytes)).unwrap(), config)
}

#[test]
fn window_sizes_roundtrip_with_valid_copies_and_hot_path_coverage() {
    for (log, record_len) in [(10u32, 128usize), (16, 4 << 10), (20, 64 << 10), (23, 256 << 10)] {
        let data = fasta(log as u64, 96, record_len, 0.7, 4);
        let bytes = zstd_encode(&data, log);
        assert_eq!(zstd_reference(&bytes), data);
        let backend = ZstdBackend::new(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!(backend.max_distance(), 1u64 << log, "window log {log}");
        let mut dec = TrackingDecoder::new(backend, TrackerConfig::default());
        drain(&mut dec, &data, 62);
        let stats = dec.stats();
        assert!(
            stats.emitted_bytes.get() as f64 >= 0.3 * data.len() as f64,
            "log {log}: copies cover {} of {} bytes",
            stats.emitted_bytes.get(),
            data.len()
        );
    }
}

#[test]
fn planted_long_repeat_is_reported_as_one_merged_copy() {
    let mut data = fasta(31, 40, 16 << 10, 0.0, 1);
    let (src, dst, len) = (1000usize, 300_000usize, 10 << 10);
    plant(&mut data, src, dst, len);
    let bytes = zstd_encode(&data, 20);
    let mut dec = decoder(bytes, TrackerConfig::default());
    let snaps = drain(&mut dec, &data, 62);
    let copies: Vec<_> = snaps.iter().flat_map(|s| s.copies.iter().copied()).collect();
    assert!(covers(&copies, src as u64, dst as u64, len as u64, 0.9), "copies: {copies:?}");
    assert!(copies.iter().any(|c| c.len() >= 9 << 10), "long match not reported whole: {copies:?}");
}

#[test]
fn multiple_frames_decode_as_one_output() {
    let a = fasta(32, 50, 4096, 0.5, 4);
    let b = fasta(33, 50, 4096, 0.5, 4);
    let mut bytes = zstd_encode(&a, 16);
    bytes.extend_from_slice(&zstd_encode(&b, 18));
    let reference = [a.as_slice(), b.as_slice()].concat();
    assert_eq!(zstd_reference(&bytes), reference);
    let mut dec = decoder(bytes, TrackerConfig::default().with_window(64 << 10));
    let snaps = drain(&mut dec, &reference, 62);
    assert!(snaps.iter().any(|s| !s.copies.is_empty()));
    let boundary = a.len() as u64;
    for c in snaps.iter().flat_map(|s| s.copies.iter()) {
        assert!(c.dst_end() <= boundary || c.src() >= boundary, "copy across frames: {c:?}");
    }
}

#[test]
fn skippable_frame_before_data_is_handled() {
    let data = fasta(34, 50, 4096, 0.5, 4);
    let payload = b"metadata";
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0x184D2A50u32.to_le_bytes());
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(payload);
    bytes.extend_from_slice(&zstd_encode(&data, 16));
    assert_eq!(zstd_reference(&bytes), data);
    let backend = ZstdBackend::new(std::io::Cursor::new(bytes)).unwrap();
    assert_eq!(backend.max_distance(), 1 << 16);
    let mut dec = TrackingDecoder::new(backend, TrackerConfig::default());
    drain(&mut dec, &data, 62);
}

#[test]
fn tracking_off_gives_identical_bytes_and_no_events() {
    let data = fasta(35, 100, 4096, 0.7, 4);
    let bytes = zstd_encode(&data, 20);
    let mut dec = decoder(bytes, TrackerConfig { tracking: false, ..TrackerConfig::default() });
    let snaps = drain(&mut dec, &data, 62);
    assert!(snaps.iter().all(|s| s.copies.is_empty()));
    assert_eq!(dec.stats().raw_events.get(), 0);
}

#[test]
fn truncated_and_corrupted_input_are_errors() {
    let data = fasta(36, 100, 4096, 0.7, 4);
    let bytes = zstd_encode(&data, 20);
    let truncated = bytes[..bytes.len() / 2].to_vec();
    assert!(drain_result(&mut decoder(truncated, TrackerConfig::default())).is_err());
    let mut garbage = bytes.clone();
    garbage.extend_from_slice(b"definitely not zstd");
    assert!(drain_result(&mut decoder(garbage, TrackerConfig::default())).is_err());
}
