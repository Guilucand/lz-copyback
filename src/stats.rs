use std::fmt;

/// One counter.
///
/// With the `stats` feature (on by default) it is a `u64`. Without it the field is not
/// there at all: [`Stat`] is a zero-sized type, every update compiles to nothing, and
/// reading one gives `0`. A consumer that only wants the copies pays nothing for the
/// bookkeeping it never reads, and the code that keeps the counters needs no `cfg` of its
/// own.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Stat {
    #[cfg(feature = "stats")]
    value: u64,
}

#[cfg(feature = "stats")]
impl Stat {
    /// The value, or `0` when the counters are compiled out.
    #[inline(always)]
    pub fn get(&self) -> u64 {
        self.value
    }

    #[inline(always)]
    pub fn add(&mut self, n: u64) {
        self.value += n;
    }

    #[inline(always)]
    pub fn set(&mut self, n: u64) {
        self.value = n;
    }

    /// Keep the larger of the two.
    #[inline(always)]
    pub fn max_with(&mut self, n: u64) {
        if n > self.value {
            self.value = n;
        }
    }
}

#[cfg(not(feature = "stats"))]
impl Stat {
    #[inline(always)]
    pub fn get(&self) -> u64 {
        0
    }

    #[inline(always)]
    pub fn add(&mut self, _n: u64) {}

    #[inline(always)]
    pub fn set(&mut self, _n: u64) {}

    #[inline(always)]
    pub fn max_with(&mut self, _n: u64) {}
}

impl Stat {
    #[inline(always)]
    pub fn inc(&mut self) {
        self.add(1);
    }

    /// Whether the counters are compiled in at all.
    pub const ENABLED: bool = cfg!(feature = "stats");
}

impl fmt::Debug for Stat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if Self::ENABLED {
            write!(f, "{}", self.get())
        } else {
            f.write_str("off")
        }
    }
}

impl fmt::Display for Stat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.get(), f)
    }
}

impl From<Stat> for u64 {
    #[inline(always)]
    fn from(s: Stat) -> u64 {
        s.get()
    }
}

/// Counters of a [`crate::TrackingDecoder`]. Everything is cumulative over the stream, and
/// every field is a [`Stat`], so the whole struct is zero-sized without the `stats`
/// feature.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrackerStats {
    /// Match events reported by the backend (counted only while tracking is on).
    pub raw_events: Stat,
    /// Runs of contiguous, equal-distance events that were merged into one candidate copy.
    pub merged_runs: Stat,
    /// Candidates shorter than `min_len` after merging.
    pub dropped_short: Stat,
    /// Candidates longer than their distance, dropped under `SelfOverlap::Drop`.
    pub dropped_self_overlap: Stat,
    /// Candidates longer than their distance, clipped under `SelfOverlap::Clip`.
    pub clipped_self_overlap: Stat,
    /// Candidates whose source was already emitted when they became known. Only possible
    /// when the stream's dictionary is larger than the half-window. Under
    /// [`crate::Direction::Both`] these are missing from the forward list only; the
    /// backward list still has them.
    pub dropped_late: Stat,
    /// Candidates whose distance or length did not fit in the `u32` fields of an
    /// [`crate::LzCopy`], which no LZ match of a supported format can exceed.
    pub dropped_oversized: Stat,
    /// Candidates with a distance larger than the half-window (kept when still buffered).
    pub over_window: Stat,
    /// Window-boundary split pieces shorter than `min_len`.
    pub dropped_split_short: Stat,
    /// The same, for the destination-keyed list under [`crate::Direction::Both`].
    pub dropped_split_short_back: Stat,
    /// Longest the destination-keyed pending list ever got, in entries.
    pub pending_back_peak: Stat,
    pub emitted_copies: Stat,
    pub emitted_bytes: Stat,
    /// Copies and bytes of the destination-keyed list under [`crate::Direction::Both`].
    pub emitted_back_copies: Stat,
    pub emitted_back_bytes: Stat,
    pub longest_copy: Stat,
    pub output_bytes: Stat,
    pub windows: Stat,
    /// The half-window `W`. [`crate::TrackingDecoder::window_size`] reports it whether or
    /// not the counters are compiled in.
    pub window_size: Stat,
}
