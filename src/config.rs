/// What to do with a merged copy that is longer than its distance, i.e. a periodic
/// repetition whose destination overlaps its own source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelfOverlap {
    /// Keep the first `distance` bytes, which form a valid non-overlapping copy of the
    /// bytes immediately preceding the destination, and drop the periodic remainder.
    Clip,
    /// Discard the whole copy.
    Drop,
}

/// Which end of a copy decides the window it is reported in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Report a copy with the window holding its *source*: "these bytes will reappear
    /// later". The destination may still be undecoded, so copies are held back until the
    /// source window is emitted and copies farther than the window are lost.
    Forward,
    /// Report a copy with the window holding its *destination*: "these bytes repeat an
    /// earlier range". The source is already decoded when the copy is known, so nothing is
    /// held back and no copy is lost, whatever its distance; the consumer needs the older
    /// bytes (or only their offsets) to make use of it.
    Backward,
    /// Report a copy twice: once in the window holding its source
    /// ([`crate::Window::copies`]) and once in the window holding its destination
    /// ([`crate::Window::back_copies`]).
    ///
    /// The two lists are split at different window boundaries -- sources for the first,
    /// destinations for the second -- so the two reports of one copy are generally *not*
    /// the same range, and a consumer must match them by interval, not by equality. A copy
    /// farther than the window is still missing from the forward list (counted by
    /// [`crate::TrackerStats::dropped_late`]) while the backward list has it.
    Both,
}

/// Default minimum copy length: `2k` for `k = 31`.
pub const DEFAULT_MIN_LEN: u64 = 1;
/// Default lower bound of the half-window, so deflate streams are not cut into 32 KiB pieces.
pub const DEFAULT_MIN_WINDOW: usize = 1 << 20;
/// Default upper bound of the half-window; larger dictionaries lose copies farther than this.
pub const DEFAULT_MAX_WINDOW: usize = 64 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackerConfig {
    /// Copies shorter than this are not reported (`w` in the design notes).
    pub min_len: u64,
    pub self_overlap: SelfOverlap,
    /// Whether copies are reported at their source or at their destination.
    pub direction: Direction,
    /// The half-window `W` is `max_distance` of the stream clamped into `[min_window, max_window]`.
    pub min_window: usize,
    pub max_window: usize,
    /// Initial state of the tracking switch (see `TrackingDecoder::set_tracking`).
    pub tracking: bool,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            min_len: DEFAULT_MIN_LEN,
            self_overlap: SelfOverlap::Clip,
            direction: Direction::Forward,
            min_window: DEFAULT_MIN_WINDOW,
            max_window: DEFAULT_MAX_WINDOW,
            tracking: true,
        }
    }
}

impl TrackerConfig {
    /// A configuration with a fixed half-window, handy for tests and tools.
    pub fn with_window(mut self, window: usize) -> Self {
        self.min_window = window;
        self.max_window = window;
        self
    }

    pub fn with_min_len(mut self, min_len: u64) -> Self {
        self.min_len = min_len;
        self
    }

    pub fn with_direction(mut self, direction: Direction) -> Self {
        self.direction = direction;
        self
    }
}
