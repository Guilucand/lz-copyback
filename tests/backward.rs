//! `Direction::Backward` reports every copy with the window holding its *destination*.
//!
//! The source is already decoded by then, so nothing is held back: a copy is reported
//! whatever its distance, which is what the forward direction cannot do.

mod common;

use common::{fasta, plant, random_bytes, scripted, tar_bytes, tar_end, xz_encode, ScriptedBackend};
use lz_copyback::tar::{TarConfig, TarTracker};
use lz_copyback::{Direction, LzCopy, TrackerConfig, TrackingDecoder};
use lz_copyback::backends::XzBackend;
use lz_copyback::LzBackend;

/// Drain a backward decoder, checking the invariants of every window against the
/// reference output, and return the copies in emission order.
fn drain_backward<B: LzBackend>(
    dec: &mut TrackingDecoder<B>,
    reference: &[u8],
    min_len: u64,
) -> Vec<LzCopy> {
    let mut copies = Vec::new();
    let mut next_start = 0u64;
    let window = dec.window_size();
    loop {
        let Some(w) = dec.next_window().expect("decode error") else { break };
        assert_eq!(w.start_abs, next_start, "windows are contiguous");
        let end = w.start_abs + w.data.len() as u64;
        assert_eq!(
            reference[w.start_abs as usize..end as usize],
            *w.data,
            "window bytes equal the reference"
        );
        if !w.is_last {
            assert_eq!(w.data.len(), window, "non-last windows are full");
        }
        for pair in w.copies.windows(2) {
            assert!((pair[0].dst(), pair[0].src()) <= (pair[1].dst(), pair[1].src()), "copies sorted by dst");
        }
        for c in w.copies {
            assert!(c.dst() >= w.start_abs && c.dst_end() <= end, "destination inside window: {c:?}");
            assert!(c.dst() > c.src() && c.distance() >= c.len(), "non-overlapping copy: {c:?}");
            assert!(c.len() >= min_len, "minimum length: {c:?}");
            assert_eq!(
                reference[c.src() as usize..c.src_end() as usize],
                reference[c.dst() as usize..c.dst_end() as usize],
                "copy bytes match: {c:?}"
            );
            copies.push(*c);
        }
        next_start = end;
        if w.is_last {
            break;
        }
    }
    assert_eq!(next_start as usize, reference.len(), "all bytes emitted");
    copies
}

/// `min_len` is set explicitly so that the checks below do not depend on the default.
fn config(window: usize) -> TrackerConfig {
    TrackerConfig::default()
        .with_window(window)
        .with_min_len(62)
        .with_direction(Direction::Backward)
}

#[test]
fn a_copy_farther_than_the_window_is_kept_where_forward_loses_it() {
    // One copy whose source is four windows behind its destination.
    let window = 1 << 16;
    let (src, dst, len) = (1000u64, 5 * window as u64, 4096u64);
    let events = vec![(dst, dst - src, len)];
    let data = scripted(6 * window, &events);

    let backend = ScriptedBackend::new(data.clone(), events.clone()).max_distance(window as u64);
    let mut dec = TrackingDecoder::new(backend, config(window));
    let copies = drain_backward(&mut dec, &data, 62);
    assert!(
        copies.iter().any(|c| c.src() == src && c.dst() == dst && c.len() == len),
        "the distant copy was not reported: {copies:?}"
    );

    let backend = ScriptedBackend::new(data.clone(), events).max_distance(window as u64);
    let mut dec =
        TrackingDecoder::new(backend, TrackerConfig::default().with_window(window).with_min_len(62));
    let mut forward = Vec::new();
    while let Some(w) = dec.next_window().expect("decode error") {
        forward.extend_from_slice(w.copies);
        if w.is_last {
            break;
        }
    }
    assert!(forward.is_empty(), "forward should have lost it: {forward:?}");
    assert_eq!(dec.stats().dropped_late.get(), 1);
}

#[test]
fn every_copy_of_a_real_stream_is_reported_at_its_destination() {
    let mut data = fasta(71, 60, 8 << 10, 0.6, 4);
    let (src, dst, len) = (500usize, 300_000usize, 20 << 10);
    plant(&mut data, src, dst, len);
    let bytes = xz_encode(&data, 1 << 22);

    let backend = XzBackend::new(std::io::Cursor::new(bytes)).unwrap();
    let mut dec = TrackingDecoder::new(backend, config(1 << 16));
    let copies = drain_backward(&mut dec, &data, 62);
    assert!(!copies.is_empty());
    // Every window's copies land in it, so the destinations come out in stream order.
    for pair in copies.windows(2) {
        assert!(pair[0].dst() <= pair[1].dst(), "destinations in stream order");
    }
    let planted = copies
        .iter()
        .filter(|c| c.distance() == (dst - src) as u64)
        .map(|c| c.len())
        .sum::<u64>();
    assert!(planted as f64 >= 0.9 * len as f64, "planted repeat covered {planted} of {len}");
}

#[test]
fn tar_members_are_named_on_both_ends() {
    // Two members whose content repeats a third one far behind: under `Backward` the
    // source member must still be in the table when the copy is reported.
    let body = fasta(72, 6, 20 << 10, 0.0, 1);
    let filler = random_bytes(73, 400 << 10);
    let mut archive = tar_bytes(&[
        ("a.fa", &body),
        ("filler.bin", &filler),
        ("b.fa", &body),
    ]);
    archive.extend_from_slice(&tar_end());
    let bytes = xz_encode(&archive, 1 << 22);

    let backend = XzBackend::new(std::io::Cursor::new(bytes)).unwrap();
    let max_distance = backend.max_distance();
    let mut dec = TrackingDecoder::new(backend, config(1 << 16));
    let mut tar = TarTracker::new(TarConfig {
        min_len: 62,
        retain_back: max_distance + 2 * dec.window_size() as u64,
        ..TarConfig::default()
    });
    let mut cross = 0u64;
    loop {
        let Some(w) = dec.next_window().expect("decode error") else { break };
        let tw = tar.process(&w).expect("tar");
        for c in tw.copies {
            if c.src_member != c.dst_member {
                assert_eq!((c.src_member, c.dst_member), (0, 2), "a.fa comes back as b.fa");
                cross += c.len();
            }
        }
        if w.is_last {
            break;
        }
    }
    assert!(
        cross as f64 >= 0.8 * body.len() as f64,
        "only {cross} of {} bytes attributed across members",
        body.len()
    );
}
