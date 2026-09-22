//! The lz4 backend must reproduce the reference decoder's output byte for byte and every
//! reported copy must be a genuine forward repeat, in linked and independent block modes
//! and across concatenated frames.

mod common;

use common::{covers, drain, drain_result, fasta, lz4_encode, lz4_reference, plant};
use lz_copyback::backends::Lz4Backend;
use lz_copyback::{LzBackend, TrackerConfig, TrackingDecoder};

fn decoder(bytes: Vec<u8>, config: TrackerConfig) -> TrackingDecoder<Lz4Backend> {
    TrackingDecoder::new(Lz4Backend::new(std::io::Cursor::new(bytes)), config)
}

#[test]
fn linked_and_independent_blocks_roundtrip_with_valid_copies() {
    let data = fasta(41, 600, 4096, 0.7, 4);
    for linked in [true, false] {
        let bytes = lz4_encode(&data, linked);
        assert_eq!(lz4_reference(&bytes), data);
        let backend = Lz4Backend::new(std::io::Cursor::new(bytes));
        assert_eq!(backend.max_distance(), 65535);
        let mut dec = TrackingDecoder::new(backend, TrackerConfig::default());
        drain(&mut dec, &data, 62);
        let stats = dec.stats();
        assert!(
            stats.emitted_bytes.get() as f64 >= 0.3 * data.len() as f64,
            "linked {linked}: copies cover {} of {} bytes",
            stats.emitted_bytes.get(),
            data.len()
        );
    }
}

#[test]
fn planted_repeat_is_reported_whole() {
    let mut data = fasta(42, 100, 4096, 0.0, 1);
    let (src, dst, len) = (5000usize, 50_000usize, 10_000usize);
    plant(&mut data, src, dst, len);
    let bytes = lz4_encode(&data, true);
    let mut dec = decoder(bytes, TrackerConfig::default());
    let snaps = drain(&mut dec, &data, 62);
    let copies: Vec<_> = snaps.iter().flat_map(|s| s.copies.iter().copied()).collect();
    assert!(covers(&copies, src as u64, dst as u64, len as u64, 0.9), "copies: {copies:?}");
    assert!(copies.iter().any(|c| c.len() >= 9000), "{copies:?}");
}

#[test]
fn concatenated_frames_decode_as_one_output() {
    let a = fasta(43, 50, 4096, 0.5, 4);
    let b = fasta(44, 50, 4096, 0.5, 4);
    // Both frames use linked blocks: upstream lz4_flex has a debug assertion on its
    // buffer capacity that trips when concatenated frames switch block mode.
    let mut bytes = lz4_encode(&a, true);
    bytes.extend_from_slice(&lz4_encode(&b, true));
    let reference = [a.as_slice(), b.as_slice()].concat();
    // The `lz4` C-binding reader stops after the first frame; lz4_flex (and this backend)
    // continue with the next one, like ggcat's other multi-member readers.
    assert_eq!(lz4_reference(&bytes), a);
    let mut dec = decoder(bytes, TrackerConfig::default().with_window(128 << 10));
    let snaps = drain(&mut dec, &reference, 62);
    assert!(snaps.iter().any(|s| !s.copies.is_empty()));
    let boundary = a.len() as u64;
    for c in snaps.iter().flat_map(|s| s.copies.iter()) {
        assert!(c.dst_end() <= boundary || c.src() >= boundary, "copy across frames: {c:?}");
    }
}

#[test]
fn small_windows_exercise_many_decode_calls() {
    let data = fasta(45, 400, 4096, 0.7, 4);
    let bytes = lz4_encode(&data, true);
    let mut dec = decoder(bytes, TrackerConfig::default().with_window(64 << 10));
    let snaps = drain(&mut dec, &data, 62);
    assert!(snaps.len() > 10);
    assert!(dec.stats().emitted_copies.get() > 0);
}

#[test]
fn tracking_off_gives_identical_bytes_and_no_events() {
    let data = fasta(46, 100, 4096, 0.7, 4);
    let bytes = lz4_encode(&data, true);
    let mut dec = decoder(bytes, TrackerConfig { tracking: false, ..TrackerConfig::default() });
    let snaps = drain(&mut dec, &data, 62);
    assert!(snaps.iter().all(|s| s.copies.is_empty()));
    assert_eq!(dec.stats().raw_events.get(), 0);
}

#[test]
fn truncated_input_is_an_error() {
    let data = fasta(47, 100, 4096, 0.7, 4);
    let bytes = lz4_encode(&data, true);
    let truncated = bytes[..bytes.len() / 2].to_vec();
    assert!(drain_result(&mut decoder(truncated, TrackerConfig::default())).is_err());
}
