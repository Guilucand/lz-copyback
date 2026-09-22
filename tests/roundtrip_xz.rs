//! The xz backend must reproduce liblzma's output byte for byte and every copy it
//! reports must be a genuine forward repeat, for small and large dictionaries,
//! concatenated streams, capped windows and unsupported filter chains.

mod common;

use common::{covers, drain, fasta, plant, xz_encode, xz_encode_bcj, xz_reference};
use lz_copyback::backends::XzBackend;
use lz_copyback::{LzBackend, TrackerConfig, TrackingDecoder};

fn decoder(bytes: Vec<u8>, config: TrackerConfig) -> TrackingDecoder<XzBackend> {
    TrackingDecoder::new(XzBackend::new(std::io::Cursor::new(bytes)).unwrap(), config)
}

#[test]
fn dictionary_sizes_roundtrip_with_valid_copies_and_hot_path_coverage() {
    for (dict, record_len) in [(4 << 10, 512), (64 << 10, 8 << 10), (1 << 20, 64 << 10), (8 << 20, 256 << 10)] {
        let data = fasta(dict as u64, 96, record_len, 0.7, 4);
        let bytes = xz_encode(&data, dict);
        assert_eq!(xz_reference(&bytes), data);
        let backend = XzBackend::new(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!(backend.max_distance(), dict as u64, "dict {dict}");
        let mut dec = TrackingDecoder::new(backend, TrackerConfig::default().with_min_len(62));
        drain(&mut dec, &data, 62);
        let stats = dec.stats();
        assert!(
            stats.emitted_bytes.get() as f64 >= 0.3 * data.len() as f64,
            "dict {dict}: copies cover {} of {} bytes",
            stats.emitted_bytes.get(),
            data.len()
        );
        assert_eq!(stats.output_bytes.get() as usize, data.len());
    }
}

#[test]
fn planted_long_repeat_is_reported_as_one_merged_copy() {
    let mut data = fasta(7, 40, 16 << 10, 0.0, 1);
    let (src, dst, len) = (1000usize, 300_000usize, 10 << 10);
    plant(&mut data, src, dst, len);
    let bytes = xz_encode(&data, 1 << 20);
    let mut dec = decoder(bytes, TrackerConfig::default());
    let snaps = drain(&mut dec, &data, 62);
    let copies: Vec<_> = snaps.iter().flat_map(|s| s.copies.iter().copied()).collect();
    assert!(covers(&copies, src as u64, dst as u64, len as u64, 0.9), "copies: {copies:?}");
    let longest = copies.iter().map(|c| c.len()).max().unwrap();
    assert!(longest >= 9 << 10, "chained matches were not merged: longest {longest}");
}

#[test]
fn capped_window_still_decodes_and_counts_late_copies() {
    // Repeats up to ~6 MiB apart with an 8 MiB dictionary, tracked through a 1 MiB window.
    let data = fasta(11, 48, 256 << 10, 0.7, 24);
    let bytes = xz_encode(&data, 8 << 20);
    let mut dec = decoder(bytes, TrackerConfig::default().with_window(1 << 20));
    assert_eq!(dec.window_size(), 1 << 20);
    drain(&mut dec, &data, 62);
    let stats = dec.stats();
    assert!(stats.dropped_late.get() > 0, "{stats:?}");
    assert!(stats.emitted_copies.get() > 0, "{stats:?}");
}

#[test]
fn concatenated_streams_decode_as_one_output() {
    let a = fasta(1, 16, 4096, 0.5, 4);
    let b = fasta(2, 16, 4096, 0.5, 4);
    let mut bytes = xz_encode(&a, 64 << 10);
    bytes.extend_from_slice(&xz_encode(&b, 64 << 10));
    let reference = [a.as_slice(), b.as_slice()].concat();
    assert_eq!(xz_reference(&bytes), reference);
    let mut dec = decoder(bytes, TrackerConfig::default().with_window(64 << 10));
    let snaps = drain(&mut dec, &reference, 62);
    assert!(snaps.iter().any(|s| !s.copies.is_empty()));
}

#[test]
fn bcj_filter_chain_disables_tracking_but_decodes() {
    let data = fasta(3, 16, 4096, 0.7, 4);
    let bytes = xz_encode_bcj(&data);
    let backend = XzBackend::new(std::io::Cursor::new(bytes)).unwrap();
    assert_eq!(backend.max_distance(), 0);
    assert!(!backend.tracking_supported());
    let mut dec = TrackingDecoder::new(backend, TrackerConfig::default().with_window(64 << 10));
    let snaps = drain(&mut dec, &data, 62);
    assert!(snaps.iter().all(|s| s.copies.is_empty()));
    assert_eq!(dec.stats().raw_events.get(), 0);
}

#[test]
fn tracking_off_gives_identical_bytes_and_no_events() {
    let data = fasta(4, 32, 8192, 0.7, 4);
    let bytes = xz_encode(&data, 1 << 20);
    let config = TrackerConfig { tracking: false, ..TrackerConfig::default() };
    let mut dec = decoder(bytes, config);
    let snaps = drain(&mut dec, &data, 62);
    assert!(snaps.iter().all(|s| s.copies.is_empty()));
    assert_eq!(dec.stats().raw_events.get(), 0);
}

#[test]
fn truncated_and_corrupted_input_are_errors() {
    let data = fasta(5, 32, 8192, 0.7, 4);
    let bytes = xz_encode(&data, 1 << 20);

    let truncated = bytes[..bytes.len() / 2].to_vec();
    let mut dec = decoder(truncated, TrackerConfig::default());
    let mut result = Ok(());
    while let Some(w) = match dec.next_window() {
        Ok(w) => w,
        Err(e) => {
            result = Err(e);
            None
        }
    } {
        if w.is_last {
            break;
        }
    }
    assert!(result.is_err(), "truncated stream must fail");

    let mut garbage = bytes.clone();
    garbage.extend_from_slice(b"not xz data at all");
    let mut dec = decoder(garbage, TrackerConfig::default());
    let mut failed = false;
    loop {
        match dec.next_window() {
            Ok(Some(w)) if !w.is_last => {}
            Ok(_) => break,
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    assert!(failed, "trailing garbage must fail");

    let empty = XzBackend::new(std::io::Cursor::new(Vec::new())).unwrap();
    let mut dec = TrackingDecoder::new(empty, TrackerConfig::default());
    assert!(dec.next_window().is_err(), "empty input is not an xz stream");
}
