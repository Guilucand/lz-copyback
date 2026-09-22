use crate::config::{Direction, SelfOverlap, TrackerConfig};
use crate::copy::LzCopy;
use crate::stats::{Stat, TrackerStats};

/// Receives raw match events from a backend and turns them into filtered [`LzCopy`]
/// candidates.
///
/// Events arrive in destination order. Contiguous events with the same distance are one
/// *run*: a compressor encodes a long repeat as several maximum-length matches, and
/// liblzma splits a single match at its dictionary limit. A run is only judged (self
/// overlap, minimum length, lateness) when it closes, so the filters see the merged copy.
///
/// Which end of a copy decides the window it belongs to is the [`Direction`]: the source
/// under `Forward`, the destination under `Backward`, both under `Both`. Everything else
/// is identical.
pub struct CopyRecorder {
    enabled: bool,
    /// Keep a source-keyed list (`Forward`, `Both`).
    fwd: bool,
    /// Keep a destination-keyed list (`Backward`, `Both`).
    bwd: bool,
    run_dst: u64,
    run_dist: u64,
    run_len: u64,
    min_len: u64,
    clip: bool,
    window: u64,
    /// Absolute start of the not-yet-emitted region; a copy whose source starts before it
    /// can no longer be reported.
    pub(crate) emitted_end: u64,
    /// Source-keyed, empty unless `fwd`.
    pub(crate) pending: Vec<LzCopy>,
    /// Destination-keyed, empty unless `bwd`.
    pub(crate) pending_back: Vec<LzCopy>,
    pub(crate) stats: TrackerStats,
}

impl CopyRecorder {
    pub(crate) fn new(config: &TrackerConfig, window: u64) -> Self {
        Self {
            enabled: config.tracking,
            fwd: matches!(config.direction, Direction::Forward | Direction::Both),
            bwd: matches!(config.direction, Direction::Backward | Direction::Both),
            run_dst: 0,
            run_dist: 0,
            run_len: 0,
            min_len: config.min_len.max(1),
            clip: config.self_overlap == SelfOverlap::Clip,
            window,
            emitted_end: 0,
            pending: Vec::new(),
            pending_back: Vec::new(),
            stats: {
                let mut stats = TrackerStats::default();
                stats.window_size.set(window);
                stats
            },
        }
    }

    /// Whether events are currently recorded. Backends may skip their hook entirely
    /// while this is false.
    #[inline(always)]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Report one executed LZ match: `len` bytes copied to absolute offset `dst` from
    /// `dst - distance`. `len == 0` is ignored.
    #[inline(always)]
    pub fn record(&mut self, dst: u64, distance: u64, len: u64) {
        if !self.enabled || len == 0 {
            return;
        }
        self.stats.raw_events.inc();
        if distance == self.run_dist && dst == self.run_dst + self.run_len {
            self.run_len += len;
            return;
        }
        self.flush_run();
        self.run_dst = dst;
        self.run_dist = distance;
        self.run_len = len;
    }

    #[inline(never)]
    pub(crate) fn flush_run(&mut self) {
        if self.run_len == 0 {
            return;
        }
        let (dst, dist, mut len) = (self.run_dst, self.run_dist, self.run_len);
        self.run_len = 0;
        self.stats.merged_runs.inc();

        if len > dist {
            if self.clip {
                len = dist;
                self.stats.clipped_self_overlap.inc();
            } else {
                self.stats.dropped_self_overlap.inc();
                return;
            }
        }
        if len < self.min_len {
            self.stats.dropped_short.inc();
            return;
        }
        if dist > self.window {
            self.stats.over_window.inc();
        }
        // Distances never exceed the decoded length, but guard against a broken backend.
        let Some(src) = dst.checked_sub(dist) else {
            self.stats.dropped_late.inc();
            return;
        };
        // A distance or a length beyond `u32` cannot come from a real LZ match; a broken
        // backend reporting one loses the copy instead of corrupting it.
        let Some(copy) = LzCopy::try_new(src, dst, len) else {
            self.stats.dropped_oversized.inc();
            return;
        };
        if self.fwd {
            // The forward list can only hold a copy whose source is still unemitted.
            if src < self.emitted_end {
                self.stats.dropped_late.inc();
            } else {
                self.pending.push(copy);
            }
        }
        // The destination is being decoded right now, so a source in already emitted
        // bytes is fine: the backward list never loses a copy, whatever its distance.
        if self.bwd {
            self.pending_back.push(copy);
            self.stats.pending_back_peak.max_with(self.pending_back.len() as u64);
        }
    }

    /// Close the open run if the end that decides its window (the source under `Forward`,
    /// the destination under `Backward`) starts before `limit`, so that it is judged now
    /// and emitted with the window ending at `limit`; otherwise leave it open so that it
    /// can keep growing across the slide. `force` closes it unconditionally.
    ///
    /// Under `Both` the source is the earlier of the two ends, so it is the one that
    /// decides: a long run is then closed at every window boundary instead of growing
    /// across the slide, and the backward list sees the same pieces the forward one does.
    pub(crate) fn flush_for_emission(&mut self, limit: u64, force: bool) {
        let key = if self.fwd { self.run_dst.wrapping_sub(self.run_dist) } else { self.run_dst };
        if self.run_len != 0 && (force || key < limit) {
            self.flush_run();
        }
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        if !enabled {
            self.flush_run();
        }
        self.run_len = 0;
        self.enabled = enabled;
    }

    /// Move every pending copy of the source-keyed list whose source starts before
    /// `limit` into `out`, splitting copies that straddle `limit` so that each emitted
    /// copy lies entirely before it. The remainder of a split copy (and every copy
    /// starting at or after `limit`) stays pending. Pieces shorter than `min_len` are
    /// dropped and counted.
    pub(crate) fn take_emittable(&mut self, limit: u64, out: &mut Vec<LzCopy>) {
        drain_pending(
            &mut self.pending,
            limit,
            false,
            self.min_len,
            &mut self.stats.dropped_split_short,
            out,
        );
    }

    /// The same, for the destination-keyed list and keyed on the destination.
    pub(crate) fn take_emittable_back(&mut self, limit: u64, out: &mut Vec<LzCopy>) {
        drain_pending(
            &mut self.pending_back,
            limit,
            true,
            self.min_len,
            &mut self.stats.dropped_split_short_back,
            out,
        );
    }
}

/// Shared body of the two `take_emittable`s: `by_dst` picks the end that decides.
fn drain_pending(
    pending: &mut Vec<LzCopy>,
    limit: u64,
    by_dst: bool,
    min_len: u64,
    short: &mut Stat,
    out: &mut Vec<LzCopy>,
) {
    let mut keep = 0;
    for i in 0..pending.len() {
        let c = pending[i];
        let (start, end) = if by_dst { (c.dst(), c.dst_end()) } else { (c.src(), c.src_end()) };
        if end <= limit {
            out.push(c);
        } else if start < limit {
            let head = limit - start;
            if head >= min_len {
                out.push(c.head(head));
            } else {
                short.inc();
            }
            let tail = c.len() - head;
            if tail >= min_len {
                pending[keep] = c.tail(head);
                keep += 1;
            } else {
                short.inc();
            }
        } else {
            pending[keep] = c;
            keep += 1;
        }
    }
    pending.truncate(keep);
}
