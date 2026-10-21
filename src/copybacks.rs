//! The copyback state every decoder carries, and the hook the C decoders call.

use crate::Copyback;
use std::collections::VecDeque;
use std::ffi::c_void;

#[derive(Debug)]
pub(crate) struct Copybacks {
    pub(crate) queue: VecDeque<Copyback>,
    /// The hard threshold, which the decoder is created with: a match shorter than this is
    /// filtered out by the decoder itself, the C code for xz, zstd and lz4, without a call into
    /// Rust, unless it continues the last match reported (the tail of a repeat cut into
    /// matches). Capped at the soft threshold, `min_len`, so that a shorter `min_len` still
    /// gets its short matches.
    hard: u32,
    /// The soft threshold: a copyback, its continuations merged, shorter than this is dropped
    /// once the next match shows it cannot grow anymore.
    pub(crate) min_len: u32,
    pub(crate) tracking: bool,
    /// The newest copyback while it is shorter than `min_len`: kept out of the queue, where a
    /// caller could see it, until it reaches `min_len` or a match that does not continue it
    /// drops it.
    pending: Option<Copyback>,
    /// Whether the last match reported went to the queue: its continuation does too, even if
    /// the caller took it out of the queue in between.
    published: bool,
    /// Where the destination of the last match reported ends, and its offset: the hook reports
    /// a match continuing it whatever its length. `u64::MAX` before the first one.
    pub(crate) last_end: u64,
    pub(crate) last_offset: u32,
}

impl Copybacks {
    /// With the hard threshold `hard`, see [`Self::hard_min_len`].
    pub(crate) fn new(hard: u32) -> Self {
        Self {
            queue: VecDeque::new(),
            hard,
            min_len: 1,
            tracking: true,
            pending: None,
            published: false,
            last_end: u64::MAX,
            last_offset: 0,
        }
    }

    /// The threshold the decoders filter matches with before reporting them: the hard one,
    /// capped at the soft one.
    pub(crate) fn hard_min_len(&self) -> u32 {
        self.hard.min(self.min_len).max(1)
    }

    /// Whether a match at `destination`, `offset` back, continues the last one reported, so
    /// that it is reported whatever its length.
    #[inline(always)]
    pub(crate) fn continues(&self, destination: u64, offset: u32) -> bool {
        destination == self.last_end && offset == self.last_offset
    }

    /// Takes a match the decoder reported: at least the hard threshold long, or continuing the
    /// last one. A continuation extends the copyback it continues: the pending one, or the
    /// last of the queue; if the caller took that one out already, the continuation comes as a
    /// copyback of its own, however short. Any other match starts a new copyback, and the
    /// pending one, which cannot grow anymore, is dropped. A copyback enters the queue once it
    /// is `min_len` long.
    #[inline(always)]
    pub(crate) fn push(&mut self, destination: u64, offset: u32, len: u32) {
        let continues = self.continues(destination, offset);
        self.last_end = destination + len as u64;
        self.last_offset = offset;
        // A match with no source in the output (a zero offset, or a source before the first
        // output byte) gets through a decoder only on corrupt input.
        if offset == 0 || destination < offset as u64 {
            self.pending = None;
            self.published = false;
            return;
        }
        let copyback = Copyback { destination, offset, len };
        if continues {
            if let Some(pending) = &mut self.pending {
                if let Some(grown) = pending.len.checked_add(len) {
                    pending.len = grown;
                    if grown >= self.min_len {
                        self.queue.extend(self.pending.take());
                        self.published = true;
                    }
                    return;
                }
            } else if self.published {
                // A repeat cut into matches, LZMA's of 273 bytes at most, comes as one.
                if let Some(last) = self.queue.back_mut() {
                    if last.offset == offset && last.destination + last.len as u64 == destination {
                        if let Some(grown) = last.len.checked_add(len) {
                            last.len = grown;
                            return;
                        }
                    }
                }
                self.queue.push_back(copyback);
                return;
            }
        }
        if len >= self.min_len {
            self.pending = None;
            self.queue.push_back(copyback);
            self.published = true;
        } else {
            self.pending = Some(copyback);
            self.published = false;
        }
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
    copybacks.push(destination, offset, len);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(copybacks: &Copybacks) -> Vec<(u64, u32, u32)> {
        copybacks.queue.iter().map(|c| (c.destination, c.offset, c.len)).collect()
    }

    /// A copyback shorter than the threshold stays out of the queue: a disjoint match drops it,
    /// continuations grow it into the queue, and the queue's last one keeps growing there; taken
    /// out of the queue, its continuations come on their own.
    #[test]
    fn copybacks_enter_the_queue_once_their_merged_length_is_enough() {
        let mut copybacks = Copybacks::new(128);
        copybacks.min_len = 100;
        copybacks.push(1000, 500, 60);
        assert!(queue(&copybacks).is_empty(), "60 bytes, pending");
        // Not a continuation: the pending one cannot grow anymore, and goes.
        copybacks.push(1070, 500, 60);
        assert!(queue(&copybacks).is_empty());
        // Its continuation, a tail however short, brings it to 100 and into the queue.
        copybacks.push(1130, 500, 40);
        assert_eq!(queue(&copybacks), [(1070, 500, 100)]);
        copybacks.push(1170, 500, 5);
        assert_eq!(queue(&copybacks), [(1070, 500, 105)]);
        // A new one long enough goes in at once; the continuation check follows the last match.
        copybacks.push(2000, 7, 300);
        assert_eq!(queue(&copybacks), [(1070, 500, 105), (2000, 7, 300)]);
        assert!(copybacks.continues(2300, 7) && !copybacks.continues(2300, 8));
        // A pending one dropped by the end of the matches never shows.
        copybacks.push(3000, 9, 50);
        assert_eq!(copybacks.queue.len(), 2);
        // The hard threshold, capped at the soft one.
        assert_eq!(copybacks.hard_min_len(), 100);
        copybacks.min_len = 1000;
        assert_eq!(copybacks.hard_min_len(), 128);

        copybacks.min_len = 100;
        copybacks.push(4000, 11, 273);
        copybacks.queue.clear();
        copybacks.push(4273, 11, 10);
        assert_eq!(queue(&copybacks), [(4273, 11, 10)], "the tail of a copyback taken out");
    }
}
