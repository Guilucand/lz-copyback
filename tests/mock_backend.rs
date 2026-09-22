//! The window/recorder semantics, checked with a backend that replays scripted events:
//! merging, self-overlap handling, thresholds, window-boundary splits, retention,
//! lateness, ordering, end-of-stream shapes and the tracking switch.

mod common;

use common::{ScriptedBackend, drain, scripted};
use lz_copyback::{LzCopy, SelfOverlap, TrackerConfig, TrackingDecoder};

const W: usize = 1024;

fn cfg(min_len: u64) -> TrackerConfig {
    TrackerConfig::default().with_window(W).with_min_len(min_len)
}

/// Runs the scenario with and without a 32 KiB history guard (as deflate needs) and
/// requires identical windows, copies and stats.
fn run(len: usize, events: &[(u64, u64, u64)], config: TrackerConfig) -> (Vec<common::Snap>, lz_copyback::TrackerStats) {
    let data = scripted(len, events);
    let mut dec = TrackingDecoder::new(ScriptedBackend::new(data.clone(), events.to_vec()), config.clone());
    let snaps = drain(&mut dec, &data, config.min_len);
    let stats = dec.stats();
    let mut dec2 = TrackingDecoder::new(ScriptedBackend::new(data.clone(), events.to_vec()).history(32768), config.clone());
    let snaps2 = drain(&mut dec2, &data, config.min_len);
    assert_eq!(snaps, snaps2, "results differ with a history guard");
    assert_eq!(stats, dec2.stats());
    (snaps, stats)
}

fn all_copies(snaps: &[common::Snap]) -> Vec<LzCopy> {
    snaps.iter().flat_map(|s| s.copies.iter().copied()).collect()
}

#[test]
fn merges_contiguous_events_with_equal_distance() {
    let events = [(100, 50, 10), (110, 50, 10), (120, 50, 10)];
    let (snaps, stats) = run(300, &events, cfg(20));
    assert_eq!(all_copies(&snaps), vec![LzCopy::new(50, 100, 30)]);
    assert_eq!(stats.raw_events.get(), 3);
    assert_eq!(stats.merged_runs.get(), 1);
}

#[test]
fn does_not_merge_when_distance_differs_or_destination_gaps() {
    let events = [(100, 50, 10), (110, 60, 10), (130, 60, 10)];
    let (snaps, stats) = run(300, &events, cfg(5));
    assert_eq!(
        all_copies(&snaps),
        vec![
            LzCopy::new(50, 100, 10),
            LzCopy::new(50, 110, 10),
            LzCopy::new(70, 130, 10),
        ]
    );
    assert_eq!(stats.merged_runs.get(), 3);
}

#[test]
fn ignores_len_zero_events_without_breaking_a_run() {
    let events = [(100, 50, 10), (110, 50, 0), (110, 50, 10)];
    let (snaps, stats) = run(300, &events, cfg(5));
    assert_eq!(all_copies(&snaps), vec![LzCopy::new(50, 100, 20)]);
    assert_eq!(stats.raw_events.get(), 2);
}

#[test]
fn drop_mode_discards_self_overlapping_run_and_counts_it() {
    let mut config = cfg(5);
    config.self_overlap = SelfOverlap::Drop;
    let (snaps, stats) = run(300, &[(100, 10, 40)], config);
    assert!(all_copies(&snaps).is_empty());
    assert_eq!(stats.dropped_self_overlap.get(), 1);
    assert_eq!(stats.clipped_self_overlap.get(), 0);
}

#[test]
fn clip_mode_clips_self_overlapping_run_to_distance() {
    let (snaps, stats) = run(300, &[(100, 10, 40)], cfg(5));
    assert_eq!(all_copies(&snaps), vec![LzCopy::new(90, 100, 10)]);
    assert_eq!(stats.clipped_self_overlap.get(), 1);
}

#[test]
fn drops_runs_shorter_than_threshold() {
    let events = [(200, 100, 61), (400, 100, 62)];
    let (snaps, stats) = run(600, &events, cfg(62));
    assert_eq!(all_copies(&snaps), vec![LzCopy::new(300, 400, 62)]);
    assert_eq!(stats.dropped_short.get(), 1);
}

#[test]
fn splits_copy_whose_source_straddles_window_end() {
    let (snaps, _) = run(2500, &[(1500, 500, 100)], cfg(10));
    assert_eq!(snaps.len(), 3);
    assert_eq!(snaps[0].copies, vec![LzCopy::new(1000, 1500, 24)]);
    assert_eq!(snaps[1].copies, vec![LzCopy::new(1024, 1524, 76)]);
    assert!(snaps[2].copies.is_empty());
}

#[test]
fn drops_split_pieces_shorter_than_threshold_and_counts_them() {
    let (snaps, stats) = run(2500, &[(1500, 500, 100)], cfg(30));
    assert!(snaps[0].copies.is_empty());
    assert_eq!(snaps[1].copies, vec![LzCopy::new(1024, 1524, 76)]);
    assert_eq!(stats.dropped_split_short.get(), 1);

    let (snaps, stats) = run(2500, &[(1500, 500, 40)], cfg(30));
    assert!(all_copies(&snaps).is_empty());
    assert_eq!(stats.dropped_split_short.get(), 2);
}

#[test]
fn retains_copies_with_source_in_second_half_until_next_window() {
    let (snaps, _) = run(2500, &[(1900, 400, 50)], cfg(10));
    assert!(snaps[0].copies.is_empty());
    assert_eq!(snaps[1].copies, vec![LzCopy::new(1500, 1900, 50)]);
}

#[test]
fn drops_late_copy_whose_source_precedes_emitted_region() {
    let data = scripted(4000, &[(3000, 2000, 50)]);
    let backend = ScriptedBackend::new(data.clone(), vec![(3000, 2000, 50)]).max_distance(4096);
    let mut dec = TrackingDecoder::new(backend, cfg(10));
    assert_eq!(dec.window_size(), W);
    let snaps = drain(&mut dec, &data, 10);
    assert!(all_copies(&snaps).is_empty());
    let stats = dec.stats();
    assert_eq!(stats.dropped_late.get(), 1);
    assert_eq!(stats.over_window.get(), 1);
}

#[test]
fn keeps_over_window_copy_whose_source_is_still_buffered() {
    let data = scripted(4000, &[(2500, 1200, 50)]);
    let backend = ScriptedBackend::new(data.clone(), vec![(2500, 1200, 50)]).max_distance(4096);
    let mut dec = TrackingDecoder::new(backend, cfg(10));
    let snaps = drain(&mut dec, &data, 10);
    assert_eq!(snaps[1].copies, vec![LzCopy::new(1300, 2500, 50)]);
    assert_eq!(dec.stats().over_window.get(), 1);
    assert_eq!(dec.stats().dropped_late.get(), 0);
}

#[test]
fn emits_copies_sorted_by_src_then_dst() {
    let events = [(300, 100, 20), (400, 300, 20)];
    let (snaps, _) = run(600, &events, cfg(10));
    assert_eq!(
        snaps[0].copies,
        vec![LzCopy::new(100, 400, 20), LzCopy::new(200, 300, 20)]
    );
}

#[test]
fn eof_with_more_than_w_remaining_emits_normal_then_short_last_window() {
    let (snaps, _) = run(1500, &[], cfg(10));
    assert_eq!(snaps.len(), 2);
    assert_eq!((snaps[0].data.len(), snaps[0].is_last, snaps[0].lookahead_len), (1024, false, 476));
    assert_eq!((snaps[1].data.len(), snaps[1].is_last, snaps[1].lookahead_len), (476, true, 0));
}

#[test]
fn eof_with_at_most_w_remaining_emits_single_last_window() {
    for len in [1000, 1024] {
        let (snaps, _) = run(len, &[], cfg(10));
        assert_eq!(snaps.len(), 1, "len {len}");
        assert!(snaps[0].is_last);
        assert_eq!(snaps[0].data.len(), len);
    }
}

#[test]
fn stream_length_multiple_of_w_has_no_empty_trailing_window() {
    let (snaps, stats) = run(2048, &[], cfg(10));
    assert_eq!(snaps.len(), 2);
    assert_eq!(snaps[1].data.len(), 1024);
    assert!(snaps[1].is_last);
    assert_eq!(stats.windows.get(), 2);
}

#[test]
fn empty_stream_emits_one_empty_last_window() {
    let (snaps, stats) = run(0, &[], cfg(10));
    assert_eq!(snaps.len(), 1);
    assert!(snaps[0].is_last && snaps[0].data.is_empty());
    assert_eq!(stats.output_bytes.get(), 0);
}

#[test]
fn accumulates_short_backend_reads_into_full_windows() {
    let events = [(1500, 500, 100), (2600, 300, 80)];
    let data = scripted(3000, &events);
    let backend = ScriptedBackend::new(data.clone(), events.to_vec()).chunk(7);
    let mut dec = TrackingDecoder::new(backend, cfg(10));
    let snaps = drain(&mut dec, &data, 10);
    assert_eq!(snaps.len(), 3);
    assert_eq!(all_copies(&snaps).len(), 3); // the first copy is split at 1024
    assert_eq!(dec.stats().output_bytes.get(), 3000);
}

#[test]
fn keeps_overshoot_bytes_written_past_target() {
    let data = scripted(3000, &[]);
    // The overshoot seeds the next half, so the lookahead is exactly W (with or without
    // a history guard).
    for history in [0, 32768] {
        let backend = ScriptedBackend::new(data.clone(), vec![]).overshoot(300).history(history);
        let mut dec = TrackingDecoder::new(backend, cfg(10));
        let snaps = drain(&mut dec, &data, 10);
        assert_eq!(snaps[0].lookahead_len, 1024);
        assert_eq!(snaps.len(), 3);
    }
}

#[test]
fn open_run_with_source_after_window_end_survives_slide_as_one_copy() {
    let events = [(1900, 300, 100), (2000, 300, 100)];
    let (snaps, stats) = run(3000, &events, cfg(10));
    assert!(snaps[0].copies.is_empty());
    assert_eq!(snaps[1].copies, vec![LzCopy::new(1600, 1900, 200)]);
    assert_eq!(stats.merged_runs.get(), 1);
}

#[test]
fn set_tracking_false_ignores_events_and_true_resumes() {
    let events = [(1500, 400, 50), (2600, 400, 50), (3500, 400, 50)];
    let data = scripted(4000, &events);
    let mut dec = TrackingDecoder::new(ScriptedBackend::new(data.clone(), events.to_vec()), cfg(10));

    let w0 = dec.next_window().unwrap().unwrap();
    assert!(w0.copies.is_empty());
    dec.set_tracking(false);
    let w1 = dec.next_window().unwrap().unwrap();
    assert_eq!(w1.copies, &[LzCopy::new(1100, 1500, 50)]);
    dec.set_tracking(true);
    let w2 = dec.next_window().unwrap().unwrap();
    assert!(w2.copies.is_empty());
    let w3 = dec.next_window().unwrap().unwrap();
    assert_eq!(w3.copies, &[LzCopy::new(3100, 3500, 50)]);
    assert!(w3.is_last);
    assert!(dec.next_window().unwrap().is_none());
    assert_eq!(dec.stats().raw_events.get(), 2);
}

#[test]
fn stats_counters_are_consistent() {
    let events = [(1500, 500, 100), (2600, 300, 80), (2900, 5, 30)];
    let (snaps, stats) = run(3000, &events, cfg(10));
    let copies = all_copies(&snaps);
    assert_eq!(stats.emitted_copies.get(), copies.len() as u64);
    assert_eq!(stats.emitted_bytes.get(), copies.iter().map(|c| c.len()).sum::<u64>());
    assert_eq!(stats.longest_copy.get(), copies.iter().map(|c| c.len()).max().unwrap());
    assert_eq!(stats.output_bytes.get(), 3000);
    assert_eq!(stats.windows.get(), snaps.len() as u64);
    assert_eq!(stats.window_size.get(), W as u64);
    assert_eq!(stats.raw_events.get(), 3);
    assert_eq!(stats.merged_runs.get(), 3);
    assert_eq!(stats.clipped_self_overlap.get(), 1);
}

#[test]
fn window_size_is_clamped_between_min_and_max() {
    let config = TrackerConfig { min_window: 1024, max_window: 4096, ..TrackerConfig::default() };
    let dec = TrackingDecoder::new(ScriptedBackend::new(vec![], vec![]).max_distance(100), config.clone());
    assert_eq!(dec.window_size(), 1024);
    let dec = TrackingDecoder::new(ScriptedBackend::new(vec![], vec![]).max_distance(1 << 40), config.clone());
    assert_eq!(dec.window_size(), 4096);
    let dec = TrackingDecoder::new(ScriptedBackend::new(vec![], vec![]).max_distance(2000), config);
    assert_eq!(dec.window_size(), 2000);
}

#[test]
fn history_guard_is_reserved_before_the_stream() {
    let data = scripted(3000, &[(1500, 500, 100)]);
    let backend = ScriptedBackend::new(data.clone(), vec![(1500, 500, 100)]).history(32768);
    let mut dec = TrackingDecoder::new(backend, cfg(10));
    let snaps = drain(&mut dec, &data, 10);
    assert_eq!(all_copies(&snaps).len(), 2);
}
