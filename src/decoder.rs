use crate::backend::{LzBackend, SLACK};
use crate::config::{Direction, TrackerConfig};
use crate::copy::LzCopy;
use crate::error::Error;
use crate::recorder::CopyRecorder;
use crate::sort::by_source;
use crate::stats::TrackerStats;

/// One emitted half-window of decompressed output.
#[derive(Debug)]
pub struct Window<'a> {
    /// 0-based counter.
    pub index: u64,
    /// Absolute stream offset of `data[0]`.
    pub start_abs: u64,
    /// The emitted bytes: exactly `W` of them except for the last window.
    pub data: &'a [u8],
    /// Already decoded bytes that follow `data` (the second half plus any overshoot).
    /// Copy destinations always fall inside `data ++ lookahead`. Empty on the last window.
    pub lookahead: &'a [u8],
    /// The copies of this window, sorted by the end that selected it: under
    /// [`Direction::Forward`] the ones whose *source* lies entirely inside `data`, sorted
    /// by `(src, dst)`; under [`Direction::Backward`] the ones whose *destination* lies
    /// entirely inside `data`, sorted by `(dst, src)` and with a source that may lie
    /// arbitrarily far back in the stream. Under [`Direction::Both`] it is the
    /// source-keyed list, as under `Forward`.
    pub copies: &'a [LzCopy],
    /// Under [`Direction::Both`], the copies whose *destination* lies entirely inside
    /// `data`, sorted by `(dst, src)`. Empty under the other directions -- where the
    /// destination-keyed list, if any, is `copies`. Use [`Window::src_copies`] and
    /// [`Window::dst_copies`] instead of picking a field by hand.
    pub back_copies: &'a [LzCopy],
    /// The direction the decoder was configured with.
    pub direction: Direction,
    pub is_last: bool,
}

impl<'a> Window<'a> {
    /// The copies whose source lies in `data`; empty under [`Direction::Backward`].
    pub fn src_copies(&self) -> &'a [LzCopy] {
        match self.direction {
            Direction::Forward | Direction::Both => self.copies,
            Direction::Backward => &[],
        }
    }

    /// The copies whose destination lies in `data`; empty under [`Direction::Forward`].
    pub fn dst_copies(&self) -> &'a [LzCopy] {
        match self.direction {
            Direction::Forward => &[],
            Direction::Backward => self.copies,
            Direction::Both => self.back_copies,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Nothing emitted yet.
    Fresh,
    /// A window was returned by the previous call and must be retired first.
    Running,
    Done,
}

/// Decompresses a stream through an [`LzBackend`] and emits it in half-windows of `W`
/// bytes, each with the LZ copies selected by the configured [`Direction`]: the ones
/// sourced inside it, or the ones landing inside it. See the crate docs.
pub struct TrackingDecoder<B: LzBackend> {
    backend: B,
    /// Two buffers of `guard + W + SLACK` bytes that swap roles: the half being filled and
    /// the first part waiting to be emitted. `buf[..guard]` holds the `guard` stream bytes
    /// preceding `buf[guard]` (deflate reads its 32 KiB history there), so only the guard
    /// and the overshoot (at most `SLACK` bytes) are ever copied between them.
    bufs: [Vec<u8>; 2],
    guard: usize,
    /// The half being filled; `1 - cur` holds the first part.
    cur: usize,
    /// Valid bytes after the guard in `bufs[cur]` (may exceed `W` by the overshoot).
    cur_filled: usize,
    /// Valid bytes of the first part (`W` except at the end of the stream).
    first_len: usize,
    w: usize,
    /// Absolute stream offset of the first part's first byte.
    base_abs: u64,
    eof: bool,
    phase: Phase,
    recorder: CopyRecorder,
    emit_buf: Vec<LzCopy>,
    emit_back: Vec<LzCopy>,
    window_index: u64,
    direction: Direction,
}

impl<B: LzBackend> TrackingDecoder<B> {
    pub fn new(backend: B, config: TrackerConfig) -> Self {
        let max_distance = usize::try_from(backend.max_distance()).unwrap_or(usize::MAX);
        let min_window = config.min_window.max(1);
        let max_window = config.max_window.max(min_window);
        let w = max_distance.clamp(min_window, max_window);
        let guard = backend.history_required();
        Self {
            direction: config.direction,
            recorder: CopyRecorder::new(&config, w as u64),
            backend,
            bufs: [vec![0u8; guard + w + SLACK], vec![0u8; guard + w + SLACK]],
            guard,
            cur: 0,
            cur_filled: 0,
            first_len: 0,
            w,
            base_abs: 0,
            eof: false,
            phase: Phase::Fresh,
            emit_buf: Vec::new(),
            emit_back: Vec::new(),
            window_index: 0,
        }
    }

    /// The half-window `W`.
    pub fn window_size(&self) -> usize {
        self.w
    }

    /// Which end of a copy selects the window it is reported in.
    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// The stream's maximum copy distance, as the backend reported it.
    pub fn max_distance(&self) -> u64 {
        self.backend.max_distance()
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn into_backend(self) -> B {
        self.backend
    }

    pub fn tracking(&self) -> bool {
        self.recorder.is_enabled()
    }

    /// Turn copy tracking off or on. Copies already known stay valid and are emitted;
    /// events that arrive while tracking is off are ignored.
    pub fn set_tracking(&mut self, enabled: bool) {
        self.recorder.set_enabled(enabled);
    }

    pub fn stats(&self) -> TrackerStats {
        self.recorder.stats
    }

    /// Emit the next window, or `None` once the last window has been returned.
    pub fn next_window(&mut self) -> Result<Option<Window<'_>>, Error> {
        let fresh = match self.phase {
            Phase::Done => return Ok(None),
            Phase::Running => {
                self.retire_first_part();
                false
            }
            Phase::Fresh => true,
        };

        if fresh {
            // No first part yet: fill it, then promote it and fill the second.
            self.fill()?;
            self.promote();
        }
        self.fill()?;

        let (data, lookahead) = self.parts();
        let len = data.len();
        let is_last = self.eof && lookahead.is_empty();
        let limit = self.base_abs + len as u64;

        self.recorder.flush_for_emission(limit, is_last);
        self.emit_buf.clear();
        self.emit_back.clear();
        match self.direction {
            Direction::Forward => {
                self.recorder.take_emittable(limit, &mut self.emit_buf);
                by_source(&mut self.emit_buf);
            }
            Direction::Backward => {
                self.recorder.take_emittable_back(limit, &mut self.emit_buf);
                debug_assert_sorted_by_dst(&self.emit_buf);
            }
            Direction::Both => {
                self.recorder.take_emittable(limit, &mut self.emit_buf);
                by_source(&mut self.emit_buf);
                self.recorder.take_emittable_back(limit, &mut self.emit_back);
                debug_assert_sorted_by_dst(&self.emit_back);
            }
        }

        let stats = &mut self.recorder.stats;
        stats.windows.inc();
        for c in &self.emit_buf {
            stats.emitted_copies.inc();
            stats.emitted_bytes.add(c.len());
            stats.longest_copy.max_with(c.len());
        }
        for c in &self.emit_back {
            stats.emitted_back_copies.inc();
            stats.emitted_back_bytes.add(c.len());
        }

        let index = self.window_index;
        self.window_index += 1;
        self.phase = if is_last { Phase::Done } else { Phase::Running };

        let (data, lookahead) = self.parts();
        Ok(Some(Window {
            index,
            start_abs: self.base_abs,
            data,
            lookahead,
            copies: &self.emit_buf,
            back_copies: &self.emit_back,
            direction: self.direction,
            is_last,
        }))
    }

    /// The first part (to emit) and everything decoded after it.
    fn parts(&self) -> (&[u8], &[u8]) {
        let g = self.guard;
        (&self.bufs[1 - self.cur][g..g + self.first_len], &self.bufs[self.cur][g..g + self.cur_filled])
    }

    /// Fill the current half up to `W` (overshoot lands in its slack).
    fn fill(&mut self) -> Result<(), Error> {
        let (g, w) = (self.guard, self.w);
        while !self.eof && self.cur_filled < w {
            let n = self.backend.decode(&mut self.bufs[self.cur], g + self.cur_filled, g + w, &mut self.recorder)?;
            if n == 0 {
                self.eof = true;
            } else {
                debug_assert!(self.cur_filled + n <= w + SLACK);
                self.cur_filled += n;
                self.recorder.stats.output_bytes.add(n as u64);
            }
        }
        Ok(())
    }

    /// The current half becomes the first part; the other buffer receives the history
    /// guard and the overshoot and becomes the half to fill.
    fn promote(&mut self) {
        let (g, w) = (self.guard, self.w);
        let len = self.cur_filled.min(w);
        let overshoot = self.cur_filled - len;
        let (a, b) = self.bufs.split_at_mut(1);
        let (src, dst) = if self.cur == 0 { (&a[0], &mut b[0]) } else { (&b[0], &mut a[0]) };
        // The last `guard` stream bytes before the overshoot are src[len..g + len].
        dst[..g].copy_from_slice(&src[len..g + len]);
        dst[g..g + overshoot].copy_from_slice(&src[g + len..g + len + overshoot]);
        self.first_len = len;
        self.cur = 1 - self.cur;
        self.cur_filled = overshoot;
    }

    /// Drop the emitted first part and make the second part the new first part.
    fn retire_first_part(&mut self) {
        debug_assert_eq!(self.first_len, self.w);
        self.base_abs += self.first_len as u64;
        self.promote();
        self.recorder.emitted_end = self.base_abs;
    }
}

/// The destination-keyed list needs no sorting: a backend reports its matches in
/// destination order, destinations never overlap, and a copy cut at the window boundary
/// leaves its tail at exactly that boundary, so the list is built in order. This checks it
/// in debug builds, where a backend that breaks the contract shows up here rather than in
/// a consumer.
#[inline]
fn debug_assert_sorted_by_dst(copies: &[LzCopy]) {
    debug_assert!(
        copies.is_sorted_by_key(|c| (c.dst(), c.src())),
        "the destination-keyed list must arrive in destination order",
    );
}
