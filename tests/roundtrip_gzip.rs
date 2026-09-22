//! The deflate backend must reproduce flate2's output byte for byte while writing
//! straight into the window, and every reported copy must be a genuine forward repeat,
//! across compression levels, stored blocks and concatenated members.

mod common;

use common::{covers, drain, drain_result, fasta, gzip_encode, gzip_reference, plant};
use lz_copyback::backends::DeflateBackend;
use lz_copyback::{LzBackend, TrackerConfig, TrackingDecoder};

fn decoder(bytes: Vec<u8>, config: TrackerConfig) -> TrackingDecoder<DeflateBackend> {
    TrackingDecoder::new(DeflateBackend::new(std::io::Cursor::new(bytes)), config)
}

#[test]
fn levels_roundtrip_with_valid_copies_and_hot_path_coverage() {
    let data = fasta(21, 600, 4096, 0.7, 4);
    // miniz_oxide (flate2's default backend) barely finds long matches in 2-bit data at
    // level 1 and splits them into 258-byte pieces at level 6; level 9 does lazy matching.
    for (level, min_cover) in [(1, 0.1), (6, 0.25), (9, 0.3)] {
        let bytes = gzip_encode(&data, level);
        assert_eq!(gzip_reference(&bytes), data);
        let backend = DeflateBackend::new(std::io::Cursor::new(bytes));
        assert_eq!(backend.max_distance(), 32768);
        let mut dec = TrackingDecoder::new(backend, TrackerConfig::default());
        assert_eq!(dec.window_size(), 1 << 20, "gzip windows are at least min_window");
        drain(&mut dec, &data, 62);
        let stats = dec.stats();
        assert!(
            stats.emitted_bytes.get() as f64 >= min_cover * data.len() as f64,
            "level {level}: copies cover {} of {} bytes",
            stats.emitted_bytes.get(),
            data.len()
        );
    }
}

#[test]
fn stored_blocks_roundtrip_without_copies() {
    let data = fasta(22, 100, 4096, 0.7, 4);
    let bytes = gzip_encode(&data, 0);
    let mut dec = decoder(bytes, TrackerConfig::default());
    let snaps = drain(&mut dec, &data, 62);
    assert!(snaps.iter().all(|s| s.copies.is_empty()));
    assert_eq!(dec.stats().raw_events.get(), 0);
}

#[test]
fn planted_repeat_within_32k_is_reported_and_merged() {
    let mut data = fasta(23, 200, 4096, 0.0, 1);
    let (src, dst, len) = (5000usize, 30_000usize, 4000usize);
    plant(&mut data, src, dst, len);
    let bytes = gzip_encode(&data, 9);
    let mut dec = decoder(bytes, TrackerConfig::default());
    let snaps = drain(&mut dec, &data, 62);
    let copies: Vec<_> = snaps.iter().flat_map(|s| s.copies.iter().copied()).collect();
    assert!(covers(&copies, src as u64, dst as u64, len as u64, 0.9), "copies: {copies:?}");
    assert!(copies.iter().any(|c| c.len() >= 3500), "258-byte matches were not merged: {copies:?}");
}

#[test]
fn concatenated_members_decode_as_one_output() {
    let a = fasta(24, 50, 4096, 0.5, 4);
    let b = fasta(25, 50, 4096, 0.5, 4);
    let mut bytes = gzip_encode(&a, 6);
    bytes.extend_from_slice(&gzip_encode(&b, 9));
    let reference = [a.as_slice(), b.as_slice()].concat();
    assert_eq!(gzip_reference(&bytes), reference);
    let mut dec = decoder(bytes, TrackerConfig::default().with_window(64 << 10));
    let snaps = drain(&mut dec, &reference, 62);
    assert!(snaps.iter().any(|s| !s.copies.is_empty()));
    // No copy crosses the member boundary: deflate history restarts per member.
    let boundary = a.len() as u64;
    for c in snaps.iter().flat_map(|s| s.copies.iter()) {
        assert!(c.dst_end() <= boundary || c.src() >= boundary, "{c:?}");
    }
}

#[test]
fn small_windows_exercise_many_decode_calls() {
    let data = fasta(26, 400, 4096, 0.7, 4);
    let bytes = gzip_encode(&data, 6);
    // 64 KiB half-windows: every decode call spans the history slide.
    let mut dec = decoder(bytes, TrackerConfig::default().with_window(64 << 10));
    let snaps = drain(&mut dec, &data, 62);
    assert!(snaps.len() > 10);
    assert!(dec.stats().emitted_copies.get() > 0);
}

#[test]
fn tracking_off_gives_identical_bytes_and_no_events() {
    let data = fasta(27, 100, 4096, 0.7, 4);
    let bytes = gzip_encode(&data, 6);
    let mut dec = decoder(bytes, TrackerConfig { tracking: false, ..TrackerConfig::default() });
    let snaps = drain(&mut dec, &data, 62);
    assert!(snaps.iter().all(|s| s.copies.is_empty()));
    assert_eq!(dec.stats().raw_events.get(), 0);
}

#[test]
fn truncated_and_corrupted_input_are_errors() {
    let data = fasta(28, 100, 4096, 0.7, 4);
    let bytes = gzip_encode(&data, 6);

    let truncated = bytes[..bytes.len() / 2].to_vec();
    assert!(drain_result(&mut decoder(truncated, TrackerConfig::default())).is_err());

    let mut bad_crc = bytes.clone();
    let n = bad_crc.len();
    bad_crc[n - 8] ^= 0xff;
    assert!(drain_result(&mut decoder(bad_crc, TrackerConfig::default())).is_err());

    let mut garbage = bytes.clone();
    garbage.extend_from_slice(b"definitely not gzip");
    assert!(drain_result(&mut decoder(garbage, TrackerConfig::default())).is_err());
}
