//! zstd backend over the patched zstd-sys (see `external/zstd-sys/PATCH.md`).
//!
//! zstd decodes into its own window buffer and copies out, so no history is needed before
//! the write position. The match hook lives in the decompression context, so it is
//! per-instance: it is installed around every `ZSTD_decompressStream` call and removed
//! afterwards.

use crate::backend::{BackendError, LzBackend};
use crate::recorder::CopyRecorder;
use std::ffi::{CStr, c_uint, c_ulonglong, c_void};
use std::io::{self, ErrorKind, Read};
use std::ptr;
use zstd_sys as ffi;

type MatchHook = Option<unsafe extern "C" fn(*mut c_void, c_ulonglong, c_uint, c_uint)>;

/// `ZSTD_frameHeader` from `ZSTD_STATIC_LINKING_ONLY`, which the pregenerated zstd-sys
/// bindings leave out.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct ZstdFrameHeader {
    frame_content_size: c_ulonglong,
    window_size: c_ulonglong,
    block_size_max: c_uint,
    frame_type: c_uint,
    header_size: c_uint,
    dict_id: c_uint,
    checksum_flag: c_uint,
    _reserved1: c_uint,
    _reserved2: c_uint,
}

/// `ZSTD_frameType_e::ZSTD_skippableFrame`.
const ZSTD_SKIPPABLE_FRAME: c_uint = 1;

unsafe extern "C" {
    /// Added by the ggcat patch to `zstd_decompress.c`.
    #[link_name = "lzcb_ZSTD_DCtx_setLzMatchHook"]
    fn ZSTD_DCtx_setLzMatchHook(dctx: *mut ffi::ZSTD_DCtx, hook: MatchHook, ctx: *mut c_void);
    #[link_name = "lzcb_ZSTD_DCtx_getLzTotalOut"]
    fn ZSTD_DCtx_getLzTotalOut(dctx: *const ffi::ZSTD_DCtx) -> c_ulonglong;
    /// Static-linking-only zstd API, always present in the static library.
    #[link_name = "lzcb_ZSTD_getFrameHeader"]
    fn ZSTD_getFrameHeader(zfh: *mut ZstdFrameHeader, src: *const c_void, src_size: usize) -> usize;
}

unsafe extern "C" fn zstd_hook(ctx: *mut c_void, dst_abs: c_ulonglong, distance: c_uint, len: c_uint) {
    // SAFETY: `ctx` is the `&mut CopyRecorder` of the `ZSTD_decompressStream` call running
    // on this thread; the hook is cleared before that borrow ends.
    let rec = unsafe { &mut *(ctx as *mut CopyRecorder) };
    rec.record(dst_abs, distance as u64, len as u64);
}

const INPUT_BUFFER: usize = 1 << 20;
/// Largest frame header (zstd's `ZSTD_FRAMEHEADERSIZE_MAX`).
const FRAME_HEADER_MAX: usize = 18;

pub struct ZstdBackend {
    reader: Box<dyn Read + Send>,
    inbuf: Vec<u8>,
    in_pos: usize,
    in_len: usize,
    input_eof: bool,
    zds: *mut ffi::ZSTD_DStream,
    total_out: u64,
    finished: bool,
    /// The last `ZSTD_decompressStream` call returned 0: a frame was completely decoded
    /// and flushed, so input ending here is a clean end of stream.
    at_frame_boundary: bool,
    max_distance: u64,
}

// SAFETY: the context is owned by this struct and used from one thread at a time.
unsafe impl Send for ZstdBackend {}

impl ZstdBackend {
    pub fn new(reader: impl Read + Send + 'static) -> io::Result<Self> {
        let zds = unsafe { ffi::ZSTD_createDStream() };
        if zds.is_null() {
            return Err(io::Error::new(ErrorKind::OutOfMemory, "ZSTD_createDStream failed"));
        }
        let ret = unsafe { ffi::ZSTD_initDStream(zds) };
        if unsafe { ffi::ZSTD_isError(ret) } != 0 {
            unsafe { ffi::ZSTD_freeDStream(zds) };
            return Err(io::Error::new(ErrorKind::Other, "ZSTD_initDStream failed"));
        }
        let mut backend = Self {
            reader: Box::new(reader),
            inbuf: vec![0u8; INPUT_BUFFER],
            in_pos: 0,
            in_len: 0,
            input_eof: false,
            zds,
            total_out: 0,
            finished: false,
            at_frame_boundary: true,
            max_distance: 0,
        };
        backend.probe_window()?;
        Ok(backend)
    }

    fn refill(&mut self) -> io::Result<bool> {
        if self.input_eof {
            return Ok(false);
        }
        if self.in_pos > 0 {
            self.inbuf.copy_within(self.in_pos..self.in_len, 0);
            self.in_len -= self.in_pos;
            self.in_pos = 0;
        }
        if self.in_len == self.inbuf.len() {
            self.inbuf.resize(self.inbuf.len() * 2, 0);
        }
        loop {
            match self.reader.read(&mut self.inbuf[self.in_len..]) {
                Ok(0) => {
                    self.input_eof = true;
                    return Ok(false);
                }
                Ok(n) => {
                    self.in_len += n;
                    return Ok(true);
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Read the first (non-skippable) frame header to learn its window size. Bytes stay
    /// buffered for decoding; skippable frames are skipped in the buffer only for probing
    /// purposes, the decoder sees and skips them itself.
    fn probe_window(&mut self) -> io::Result<()> {
        let mut probe_pos = self.in_pos;
        loop {
            // Make sure the candidate header is buffered (best effort: short inputs fall
            // through to the decoder's own error reporting).
            while self.in_len - probe_pos < FRAME_HEADER_MAX && !self.input_eof {
                let before = self.in_pos;
                if !self.refill()? {
                    break;
                }
                probe_pos -= before - self.in_pos;
            }
            let avail = self.in_len - probe_pos;
            if avail == 0 {
                return Ok(());
            }
            let mut zfh = ZstdFrameHeader::default();
            let ret = unsafe {
                ZSTD_getFrameHeader(&mut zfh, self.inbuf.as_ptr().add(probe_pos) as *const c_void, avail)
            };
            if unsafe { ffi::ZSTD_isError(ret) } != 0 || ret != 0 {
                return Ok(());
            }
            if zfh.frame_type == ZSTD_SKIPPABLE_FRAME {
                // Magic + 4-byte little-endian payload size, then the payload.
                if avail < 8 {
                    return Ok(());
                }
                let b = &self.inbuf[probe_pos + 4..probe_pos + 8];
                let payload = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
                let skip = 8 + payload;
                while self.in_len - probe_pos < skip {
                    let before = self.in_pos;
                    if !self.refill()? {
                        return Ok(());
                    }
                    probe_pos -= before - self.in_pos;
                }
                probe_pos += skip;
                continue;
            }
            self.max_distance = zfh.window_size;
            return Ok(());
        }
    }
}

impl Drop for ZstdBackend {
    fn drop(&mut self) {
        unsafe { ffi::ZSTD_freeDStream(self.zds) };
    }
}

fn zstd_error(code: usize) -> BackendError {
    let name = unsafe { CStr::from_ptr(ffi::ZSTD_getErrorName(code)) };
    Box::new(io::Error::new(ErrorKind::InvalidData, format!("zstd: {}", name.to_string_lossy())))
}

impl LzBackend for ZstdBackend {
    fn max_distance(&self) -> u64 {
        self.max_distance
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
        loop {
            if self.in_pos == self.in_len && !self.refill()? {
                // Input is exhausted. Between frames that is the end of the stream;
                // inside a frame it is truncation.
                if self.at_frame_boundary {
                    self.finished = true;
                    return Ok(0);
                }
                return Err(Box::new(io::Error::new(ErrorKind::UnexpectedEof, "truncated zstd stream")));
            }
            let mut input = ffi::ZSTD_inBuffer {
                src: unsafe { self.inbuf.as_ptr().add(self.in_pos) } as *const c_void,
                size: self.in_len - self.in_pos,
                pos: 0,
            };
            let mut output = ffi::ZSTD_outBuffer {
                dst: unsafe { buf.as_mut_ptr().add(pos) } as *mut c_void,
                size: target - pos,
                pos: 0,
            };

            let track = rec.is_enabled();
            if track {
                unsafe { ZSTD_DCtx_setLzMatchHook(self.zds, Some(zstd_hook), rec as *mut CopyRecorder as *mut c_void) };
            }
            let ret = unsafe { ffi::ZSTD_decompressStream(self.zds, &mut output, &mut input) };
            if track {
                unsafe { ZSTD_DCtx_setLzMatchHook(self.zds, None, ptr::null_mut()) };
            }

            self.in_pos += input.pos;
            let produced = output.pos;
            self.total_out += produced as u64;
            if unsafe { ffi::ZSTD_isError(ret) } != 0 {
                return Err(zstd_error(ret));
            }
            self.at_frame_boundary = ret == 0;
            // At a frame boundary everything decoded has been flushed, so the patched
            // counter must agree with the bytes we received; legacy (v0.x) frames bypass
            // the counted path and would silently go untracked.
            if self.at_frame_boundary && unsafe { ZSTD_DCtx_getLzTotalOut(self.zds) } != self.total_out {
                return Err(Box::new(io::Error::new(
                    ErrorKind::Unsupported,
                    "zstd output not tracked (legacy frame format)",
                )));
            }
            if produced > 0 {
                return Ok(produced);
            }
            // Nothing produced: the decoder consumed headers, a checksum, a skippable
            // frame or needs more input; loop to feed it or to detect the end.
        }
    }
}
