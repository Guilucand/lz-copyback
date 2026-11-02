//! The copyback state every decoder carries, and the hook the C decoders call.

use crate::Copyback;
use std::collections::VecDeque;
use std::ffi::c_void;

#[derive(Debug)]
pub(crate) struct Copybacks {
    /// The copybacks recorded, each with `min_len <= len <= offset`: its source ends before its
    /// destination starts, and it is long enough to be worth handling.
    pub(crate) queue: VecDeque<Copyback>,
    /// The hard threshold, which the decoder is created with: a match shorter than this is
    /// filtered out by the decoder itself, the C code for xz, zstd and lz4, without a call into
    /// Rust, unless it continues the last match reported (the tail of a repeat cut into
    /// matches). Capped at the soft threshold, `min_len`, so that a shorter `min_len` still
    /// gets its short matches.
    hard_min_len: u32,
    /// The soft threshold: no copyback shorter than this enters the queue. A match whose offset
    /// is shorter makes no copyback at all, as none of its pieces can reach it.
    pub(crate) min_len: u32,
    pub(crate) tracking: bool,
    /// The newest piece while it is shorter than `min_len`: kept out of the queue, where a
    /// caller could see it, until continuations make it `min_len` long, or a match that does
    /// not continue it drops it.
    pending: Option<Copyback>,
    /// Where the destination of the last match reported ends, and its offset: the hook reports
    /// a match continuing it whatever its length. `u64::MAX` before the first one.
    pub(crate) last_end: u64,
    pub(crate) last_offset: u32,
}

impl Copybacks {
    /// With the hard threshold `hard_min_len`, see [`Self::report_min_len`].
    pub(crate) fn new(hard_min_len: u32) -> Self {
        Self {
            queue: VecDeque::new(),
            hard_min_len,
            min_len: 1,
            tracking: true,
            pending: None,
            last_end: u64::MAX,
            last_offset: 0,
        }
    }

    /// The threshold the decoders filter matches with before reporting them: the hard one,
    /// capped at the soft one.
    pub(crate) fn report_min_len(&self) -> u32 {
        self.hard_min_len.min(self.min_len).max(1)
    }

    /// Whether a match at `destination`, `offset` back, continues the last one reported, so
    /// that it is reported whatever its length.
    #[inline(always)]
    pub(crate) fn continues(&self, destination: u64, offset: u32) -> bool {
        destination == self.last_end && offset == self.last_offset
    }

    /// Takes a match the decoder reported: at least the hard threshold long, or continuing the
    /// last one. It is cut into pieces of `offset` bytes, each copying the one before, so that
    /// no copyback's source runs into its destination (a periodic repeat), the last piece
    /// shorter. A continuation first fills the copyback it continues up to `offset` bytes: the
    /// pending one, or the last of the queue, if the caller has not taken it out; then it goes
    /// on in new pieces. A piece enters the queue once it is `min_len` long; until then it is
    /// pending, and a match that does not continue it drops it. A match whose offset is shorter
    /// than `min_len` is dropped whole, with its continuations: homopolymer runs and short
    /// tandem repeats.
    #[inline(always)]
    pub(crate) fn record_match(&mut self, mut destination: u64, offset: u32, mut len: u32) {
        let continues = self.continues(destination, offset);
        self.last_end = destination + len as u64;
        self.last_offset = offset;
        // A match with no source in the output (a zero offset, or a source before the first
        // output byte) gets through a decoder only on corrupt input.
        if offset == 0 || destination < offset as u64 || offset < self.min_len {
            self.pending = None;
            return;
        }
        if continues {
            let continued = match &mut self.pending {
                Some(pending) => Some(pending),
                None => (self.queue.back_mut()).filter(|last| {
                    last.offset == offset && last.destination + last.len as u64 == destination
                }),
            };
            // Otherwise the caller took it out: the rest starts a new piece.
            if let Some(continued) = continued {
                debug_assert!(
                    continued.offset == offset
                        && continued.destination + continued.len as u64 == destination
                );
                let take = len.min(offset - continued.len);
                continued.len += take;
                destination += take as u64;
                len -= take;
            }
            if (self.pending).is_some_and(|pending| pending.len >= self.min_len) {
                self.queue.extend(self.pending.take());
            }
        } else {
            // It cannot grow anymore.
            self.pending = None;
        }
        while len > 0 {
            let take = len.min(offset);
            let piece = Copyback { destination, offset, len: take };
            if take >= self.min_len {
                self.queue.push_back(piece);
            } else {
                self.pending = Some(piece);
            }
            destination += take as u64;
            len -= take;
        }
        debug_assert!((self.queue.back())
            .is_none_or(|last| self.min_len <= last.len && last.len <= last.offset));
    }

    /// The `ctx` of [`copyback_hook`] for a decoding call made while `self` stays put.
    pub(crate) fn hook_ctx(&mut self) -> *mut c_void {
        self as *mut Copybacks as *mut c_void
    }
}

/// The match hook of the patched C decoders. `ctx` is the `Copybacks` of the decoder whose
/// decoding call is running: the hook is installed with it right before that call and
/// removed right after, on the same thread, since the decoders move around freely.
pub(crate) unsafe extern "C" fn copyback_hook(
    ctx: *mut c_void,
    destination: u64,
    offset: u32,
    len: u32,
) {
    let copybacks = unsafe { &mut *(ctx as *mut Copybacks) };
    copybacks.record_match(destination, offset, len);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(copybacks: &Copybacks) -> Vec<(u64, u32, u32)> {
        copybacks.queue.iter().map(|c| (c.destination, c.offset, c.len)).collect()
    }

    /// A copyback shorter than the threshold stays out of the queue: a disjoint match drops it,
    /// continuations grow it into the queue, and the queue's last one keeps growing there.
    #[test]
    fn copybacks_enter_the_queue_once_their_merged_length_is_enough() {
        let mut copybacks = Copybacks::new(128);
        copybacks.min_len = 100;
        copybacks.record_match(1000, 500, 60);
        assert!(queue(&copybacks).is_empty(), "60 bytes, pending");
        // Not a continuation: the pending one cannot grow anymore, and goes.
        copybacks.record_match(1070, 500, 60);
        assert!(queue(&copybacks).is_empty());
        // Its continuation, a tail however short, brings it to 100 and into the queue.
        copybacks.record_match(1130, 500, 40);
        assert_eq!(queue(&copybacks), [(1070, 500, 100)]);
        copybacks.record_match(1170, 500, 5);
        assert_eq!(queue(&copybacks), [(1070, 500, 105)]);
        // A new one long enough goes in at once; the continuation check follows the last match.
        copybacks.record_match(2000, 700, 300);
        assert_eq!(queue(&copybacks), [(1070, 500, 105), (2000, 700, 300)]);
        assert!(copybacks.continues(2300, 700) && !copybacks.continues(2300, 701));
        // A pending one dropped by the end of the matches never shows.
        copybacks.record_match(3000, 900, 50);
        assert_eq!(copybacks.queue.len(), 2);
        // The hard threshold, capped at the soft one.
        assert_eq!(copybacks.report_min_len(), 100);
        copybacks.min_len = 1000;
        assert_eq!(copybacks.report_min_len(), 128);
    }

    /// A match that overlaps its own output comes in pieces of its offset, each copying the one
    /// before, the last one only once it is `min_len` long; continuations fill the last piece up
    /// to the offset, then start the next. A match whose offset is shorter than `min_len` makes
    /// nothing, nor do its continuations.
    #[test]
    fn overlapping_matches_come_in_pieces_of_their_offset() {
        let mut copybacks = Copybacks::new(128);
        copybacks.min_len = 100;
        copybacks.record_match(10_000, 150, 400);
        assert_eq!(
            queue(&copybacks),
            [(10_000, 150, 150), (10_150, 150, 150), (10_300, 150, 100)]
        );
        copybacks.queue.clear();
        // The last piece, 40 bytes, is pending.
        copybacks.record_match(20_000, 150, 340);
        assert_eq!(queue(&copybacks), [(20_000, 150, 150), (20_150, 150, 150)]);
        // Filled to 150 bytes, then 90 more, pending.
        copybacks.record_match(20_340, 150, 200);
        assert_eq!(queue(&copybacks)[2..], [(20_300, 150, 150)]);
        copybacks.record_match(20_540, 150, 10);
        assert_eq!(queue(&copybacks)[3..], [(20_450, 150, 100)]);
        // The queue's last grows to 150 bytes, and the 10 left are pending.
        copybacks.record_match(20_550, 150, 60);
        assert_eq!(queue(&copybacks)[3..], [(20_450, 150, 150)]);
        assert!(copybacks.pending.is_some_and(|pending| pending.len == 10));

        copybacks.queue.clear();
        copybacks.record_match(30_000, 99, 5000);
        copybacks.record_match(35_000, 99, 300);
        assert!(queue(&copybacks).is_empty() && copybacks.pending.is_none());
    }

    /// Once the caller took a copyback out of the queue, it cannot grow: its continuation starts
    /// a new piece, held until it is `min_len` long, dropped if the matches end before.
    #[test]
    fn a_continuation_of_a_copyback_taken_out_starts_a_new_piece() {
        let mut copybacks = Copybacks::new(128);
        copybacks.min_len = 100;
        copybacks.record_match(4000, 1100, 273);
        assert_eq!(queue(&copybacks), [(4000, 1100, 273)]);
        copybacks.queue.clear();
        copybacks.record_match(4273, 1100, 10);
        assert!(queue(&copybacks).is_empty(), "the tail of a copyback taken out");
        copybacks.record_match(4283, 1100, 95);
        assert_eq!(queue(&copybacks), [(4273, 1100, 105)]);
        copybacks.queue.clear();
        copybacks.record_match(4378, 1100, 50);
        copybacks.record_match(9000, 1100, 200);
        assert_eq!(queue(&copybacks), [(9000, 1100, 200)]);
    }
}
