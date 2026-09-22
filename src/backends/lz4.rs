//! lz4 frame backend over the patched `lz4_flex` (see `external/lz4-flex/PATCH.md`).
//!
//! lz4_flex decodes each block into its own buffer and serves it through `Read`, so the
//! window needs no history; the frame decoder converts block positions into absolute
//! stream offsets before calling the hook.

use crate::backend::{BackendError, LzBackend};
use crate::recorder::CopyRecorder;
use lz4_flex::block::MatchHook;
use lz4_flex::frame::FrameDecoder;
use std::io::{ErrorKind, Read};

/// Forwards matches to the recorder of the running `decode` call (null otherwise).
pub struct RecorderHook {
    rec: *mut CopyRecorder,
}

impl MatchHook for RecorderHook {
    #[inline(always)]
    fn on_match(&mut self, dst_pos: usize, offset: usize, match_length: usize) {
        if !self.rec.is_null() {
            // SAFETY: set from the `&mut CopyRecorder` of the current `decode` call and
            // cleared before it returns.
            let rec = unsafe { &mut *self.rec };
            rec.record(dst_pos as u64, offset as u64, match_length as u64);
        }
    }
}

/// Largest lz4 back-reference distance (16-bit offsets).
pub const LZ4_MAX_DISTANCE: u64 = 65535;

pub struct Lz4Backend {
    dec: FrameDecoder<Box<dyn Read + Send>, RecorderHook>,
    finished: bool,
}

// SAFETY: the raw recorder pointer is only non-null during a `decode` call on the calling
// thread; everything else is `Send`.
unsafe impl Send for Lz4Backend {}

impl Lz4Backend {
    pub fn new(reader: impl Read + Send + 'static) -> Self {
        let reader: Box<dyn Read + Send> = Box::new(reader);
        Self {
            dec: FrameDecoder::with_hook(reader, RecorderHook { rec: std::ptr::null_mut() }),
            finished: false,
        }
    }
}

impl LzBackend for Lz4Backend {
    fn max_distance(&self) -> u64 {
        LZ4_MAX_DISTANCE
    }

    fn decode(
        &mut self,
        buf: &mut [u8],
        pos: usize,
        target: usize,
        rec: &mut CopyRecorder,
    ) -> Result<usize, BackendError> {
        if self.finished {
            return Ok(0);
        }
        self.dec.hook_mut().rec = if rec.is_enabled() { rec as *mut CopyRecorder } else { std::ptr::null_mut() };
        // lz4_flex reports a zero-length read at the end of every frame; only a second
        // zero means the input is exhausted (concatenated frames continue).
        let mut retried = false;
        let result = loop {
            match self.dec.read(&mut buf[pos..target]) {
                Ok(0) if !retried => retried = true,
                Ok(n) => break Ok(n),
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => break Err(e),
            }
        };
        self.dec.hook_mut().rec = std::ptr::null_mut();
        let n = result.map_err(|e| -> BackendError { Box::new(e) })?;
        if n == 0 {
            self.finished = true;
        }
        Ok(n)
    }
}
