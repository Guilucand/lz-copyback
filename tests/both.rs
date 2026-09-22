//! `Direction::Both` reports every copy twice: once at its source, once at its
//! destination.
//!
//! The forward list is what `Forward` reports, the backward list what `Backward` reports
//! -- except that the two are split at different window boundaries, so the pieces differ
//! and only the covered byte correspondences can be compared.

mod common;

use common::{fasta, plant, random_bytes, scripted, tar_bytes, tar_end, xz_encode, ScriptedBackend};
use lz_copyback::backends::XzBackend;
use lz_copyback::tar::{TarConfig, TarTracker};
use lz_copyback::LzBackend;
use lz_copyback::{Direction, LzCopy, TrackerConfig, TrackingDecoder};

const MIN_LEN: u64 = 62;

fn config(window: usize, direction: Direction) -> TrackerConfig {
    TrackerConfig::default()
        .with_window(window)
        .with_min_len(MIN_LEN)
        .with_direction(direction)
}

/// Drain a `Both` decoder, checking both lists' invariants against the reference, and
/// return them separately in emission order.
fn drain_both<B: LzBackend>(
    dec: &mut TrackingDecoder<B>,
    reference: &[u8],
) -> (Vec<LzCopy>, Vec<LzCopy>) {
    let (mut fwd, mut back) = (Vec::new(), Vec::new());
    let mut next_start = 0u64;
    let window = dec.window_size();
    loop {
        let Some(w) = dec.next_window().expect("decode error") else { break };
        assert_eq!(w.start_abs, next_start, "windows are contiguous");
        let end = w.start_abs + w.data.len() as u64;
        let decoded_end = end + w.lookahead.len() as u64;
        assert_eq!(
            reference[w.start_abs as usize..end as usize],
            *w.data,
            "window bytes equal the reference"
        );
        if !w.is_last {
            assert_eq!(w.data.len(), window, "non-last windows are full");
        }

        assert_eq!(w.copies.as_ptr(), w.src_copies().as_ptr(), "src_copies is the forward list");
        for pair in w.copies.windows(2) {
            assert!((pair[0].src(), pair[0].dst()) <= (pair[1].src(), pair[1].dst()), "sorted by src");
        }
        for c in w.copies {
            assert!(c.src() >= w.start_abs && c.src_end() <= end, "source inside window: {c:?}");
            assert!(c.dst_end() <= decoded_end, "destination decoded: {c:?}");
            check_copy(c, reference);
            fwd.push(*c);
        }

        assert_eq!(w.back_copies.as_ptr(), w.dst_copies().as_ptr(), "dst_copies is the back list");
        for pair in w.back_copies.windows(2) {
            assert!((pair[0].dst(), pair[0].src()) <= (pair[1].dst(), pair[1].src()), "sorted by dst");
            assert!(pair[0].dst_end() <= pair[1].dst(), "destinations are disjoint");
        }
        for c in w.back_copies {
            assert!(c.dst() >= w.start_abs && c.dst_end() <= end, "destination inside window: {c:?}");
            check_copy(c, reference);
            back.push(*c);
        }

        next_start = end;
        if w.is_last {
            break;
        }
    }
    assert_eq!(next_start as usize, reference.len(), "all bytes emitted");
    (fwd, back)
}

fn check_copy(c: &LzCopy, reference: &[u8]) {
    assert!(c.dst() > c.src() && c.distance() >= c.len(), "non-overlapping copy: {c:?}");
    assert!(c.len() >= MIN_LEN, "minimum length: {c:?}");
    assert_eq!(
        reference[c.src() as usize..c.src_end() as usize],
        reference[c.dst() as usize..c.dst_end() as usize],
        "copy bytes match: {c:?}"
    );
}

/// The set of `(src + i, dst + i)` byte correspondences a list covers. Splitting a copy
/// at a window boundary does not change it, so two differently split lists compare.
fn covered(copies: &[LzCopy]) -> std::collections::BTreeSet<(u64, u64)> {
    let mut out = std::collections::BTreeSet::new();
    for c in copies {
        for i in 0..c.len() {
            out.insert((c.src() + i, c.dst() + i));
        }
    }
    out
}

fn drain_single<B: LzBackend>(dec: &mut TrackingDecoder<B>) -> Vec<LzCopy> {
    let mut out = Vec::new();
    while let Some(w) = dec.next_window().expect("decode error") {
        out.extend_from_slice(w.copies);
        if w.is_last {
            break;
        }
    }
    out
}

/// A stream with planted repeats plus whatever the compressor finds by itself.
fn sample() -> Vec<u8> {
    let mut data = fasta(81, 60, 8 << 10, 0.6, 4);
    plant(&mut data, 500, 300_000, 20 << 10);
    plant(&mut data, 40_000, 200_000, 8 << 10);
    data
}

fn xz_decoder(bytes: &[u8], window: usize, direction: Direction) -> TrackingDecoder<XzBackend> {
    let backend = XzBackend::new(std::io::Cursor::new(bytes.to_vec())).unwrap();
    TrackingDecoder::new(backend, config(window, direction))
}

#[test]
fn both_lists_cover_what_each_single_direction_covers() {
    let data = sample();
    let bytes = xz_encode(&data, 1 << 22);
    let window = 1 << 16;

    let (fwd, back) = drain_both(&mut xz_decoder(&bytes, window, Direction::Both), &data);
    let only_fwd = drain_single(&mut xz_decoder(&bytes, window, Direction::Forward));
    let only_back = drain_single(&mut xz_decoder(&bytes, window, Direction::Backward));

    assert!(!fwd.is_empty() && !back.is_empty());
    assert_eq!(covered(&fwd), covered(&only_fwd), "forward list unchanged by Both");
    assert_eq!(covered(&back), covered(&only_back), "backward list unchanged by Both");
    // The backward list is the larger one: it also holds what forward drops as late.
    assert!(covered(&fwd).is_subset(&covered(&back)));
}

#[test]
fn a_copy_farther_than_the_window_is_in_the_backward_list_only() {
    let window = 1 << 16;
    let (src, dst, len) = (1000u64, 5 * window as u64, 4096u64);
    let events = vec![(dst, dst - src, len)];
    let data = scripted(6 * window, &events);

    let backend = ScriptedBackend::new(data.clone(), events).max_distance(window as u64);
    let mut dec = TrackingDecoder::new(backend, config(window, Direction::Both));
    let (fwd, back) = drain_both(&mut dec, &data);

    assert!(fwd.is_empty(), "the source window was long gone: {fwd:?}");
    assert!(
        back.iter().any(|c| c.src() == src && c.dst() == dst && c.len() == len),
        "the distant copy was not reported backward: {back:?}"
    );
    assert_eq!(dec.stats().dropped_late.get(), 1, "counted as lost for the forward list");
    assert_eq!(dec.stats().emitted_back_copies.get(), 1);
    assert_eq!(dec.stats().emitted_back_bytes.get(), len);
}

#[test]
fn the_pending_backward_list_stays_bounded() {
    let data = sample();
    let bytes = xz_encode(&data, 1 << 22);
    let window = 1 << 16;
    let mut dec = xz_decoder(&bytes, window, Direction::Both);
    drain_both(&mut dec, &data);
    let peak = dec.stats().pending_back_peak.get();
    assert!(peak > 0, "nothing was ever pending");
    // A pending copy's destination lies in the decoded-but-unemitted region, and copies
    // have disjoint destinations of at least `min_len` bytes.
    let bound = (2 * window as u64 + 1024) / MIN_LEN + 1;
    assert!(peak <= bound, "pending backward peak {peak} exceeds {bound}");
}

#[test]
fn tar_clips_the_backward_list_to_member_content() {
    let body = fasta(82, 6, 20 << 10, 0.0, 1);
    let filler = random_bytes(83, 400 << 10);
    let mut archive = tar_bytes(&[("a.fa", &body), ("filler.bin", &filler), ("b.fa", &body)]);
    archive.extend_from_slice(&tar_end());
    let bytes = xz_encode(&archive, 1 << 22);

    let backend = XzBackend::new(std::io::Cursor::new(bytes)).unwrap();
    let max_distance = backend.max_distance();
    let mut dec = TrackingDecoder::new(backend, config(1 << 16, Direction::Both));
    let mut tar = TarTracker::new(TarConfig {
        min_len: MIN_LEN,
        retain_back: max_distance + 2 * dec.window_size() as u64,
        ..TarConfig::default()
    });
    // Content ranges of the three members, in archive coordinates.
    let mut content: Vec<(u64, u64)> = Vec::new();
    let mut back_bytes = 0u64;
    loop {
        let Some(w) = dec.next_window().expect("decode error") else { break };
        let tw = tar.process(&w).expect("tar");
        for m in tw.new_members {
            content.push((m.content_start, m.content_end));
        }
        for c in tw.back_copies {
            assert!(
                content.iter().any(|&(s, e)| c.dst() >= s && c.dst_end() <= e),
                "backward destination outside member content: {c:?}"
            );
            back_bytes += c.len();
        }
        if w.is_last {
            break;
        }
    }
    assert!(
        back_bytes as f64 >= 0.8 * body.len() as f64,
        "only {back_bytes} of {} bytes clipped into members",
        body.len()
    );
}
