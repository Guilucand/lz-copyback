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
