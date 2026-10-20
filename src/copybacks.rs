//! The copyback state every decoder carries, and the hook the C decoders call.

use crate::Copyback;
use std::collections::VecDeque;
use std::ffi::c_void;

#[derive(Debug)]
pub(crate) struct Copybacks {
    pub(crate) queue: VecDeque<Copyback>,
    pub(crate) min_len: u32,
    pub(crate) tracking: bool,
}

impl Copybacks {
    pub(crate) fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            min_len: 1,
            tracking: true,
        }
    }

    #[inline(always)]
    pub(crate) fn push(&mut self, destination: u64, offset: u32, len: u32) {
        // A match with no source in the output (a zero offset, or a source before the first
        // output byte) gets through a decoder only on corrupt input.
        if len >= self.min_len && offset != 0 && destination >= offset as u64 {
            self.queue.push_back(Copyback { destination, offset, len });
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

/// The three copyback methods every decoder has, over its `Copybacks` (the `copybacks` field
/// unless a path to it is given).
macro_rules! copyback_methods {
    () => {
        copyback_methods!(copybacks);
    };
    ($($field:ident).+) => {
        /// Turns the recording of LZ matches on or off; it is on when the decoder is
        /// created. While it is off, the matches the decoder decodes are ignored.
        pub fn set_copybacks_tracking(&mut self, enabled: bool) {
            self.$($field).+.tracking = enabled;
        }

        /// Matches shorter than `min_len` bytes are not recorded; 1 when the decoder is
        /// created, so every match is.
        pub fn set_copybacks_min_len(&mut self, min_len: u32) {
            self.$($field).+.min_len = min_len;
        }

        /// Every LZ match recorded so far, in stream order. The decoder only pushes to the
        /// back of the queue; removing entries is up to the caller.
        pub fn peek_copybacks(&mut self) -> &mut std::collections::VecDeque<crate::Copyback> {
            &mut self.$($field).+.queue
        }
    };
}

pub(crate) use copyback_methods;
