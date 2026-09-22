//! xz backend over the patched liblzma (see `external/lzma-sys/PATCH.md`).
//!
//! liblzma decodes into its own dictionary and copies the bytes out, so no history is
//! needed before the write position. Matches are reported through a thread-local hook
//! that is installed around every `lzma_code` call and removed afterwards, so other
//! liblzma users on the same thread never see it.

use crate::backend::{BackendError, LzBackend};
use crate::recorder::CopyRecorder;
use liblzma_sys as ffi;
use std::ffi::c_void;
use std::io::{self, ErrorKind, Read};
use std::ptr;

type MatchHook = Option<unsafe extern "C" fn(*mut c_void, u64, u32, u32)>;

unsafe extern "C" {
    /// Added by the ggcat patch to `lz_decoder.c`.
    #[link_name = "lzcb_lzma_lz_set_match_hook"]
    fn lzma_lz_set_match_hook(hook: MatchHook, ctx: *mut c_void, out_abs_base: u64);
}

unsafe extern "C" fn xz_hook(ctx: *mut c_void, dst_abs: u64, distance: u32, len: u32) {
    // SAFETY: `ctx` is the `&mut CopyRecorder` passed to the `lzma_code` call that is
    // executing on this thread; the hook is cleared before that borrow ends.
    let rec = unsafe { &mut *(ctx as *mut CopyRecorder) };
    rec.record(dst_abs, distance as u64, len as u64);
}

/// Dictionary size assumed when the first block header cannot be parsed.
const FALLBACK_DICT_SIZE: u64 = 64 << 20;
const INPUT_BUFFER: usize = 1 << 20;

pub struct XzBackend {
    reader: Box<dyn Read + Send>,
    inbuf: Vec<u8>,
    in_pos: usize,
    in_len: usize,
    input_eof: bool,
    strm: Box<ffi::lzma_stream>,
    total_out: u64,
    finished: bool,
    max_distance: u64,
    /// False when the filter chain is not a lone LZMA2 filter (BCJ, delta): the LZ
    /// dictionary then holds filtered bytes, not output bytes, and copies would not be
    /// byte-for-byte repeats.
    tracking_supported: bool,
}

// SAFETY: the raw pointers inside `lzma_stream` only ever point into buffers owned by
// this struct and liblzma's private state, which is used from one thread at a time.
unsafe impl Send for XzBackend {}

impl XzBackend {
    pub fn new(reader: impl Read + Send + 'static) -> io::Result<Self> {
        let mut strm: Box<ffi::lzma_stream> = Box::new(unsafe { std::mem::zeroed() });
        let ret = unsafe { ffi::lzma_stream_decoder(&mut *strm, u64::MAX, ffi::LZMA_CONCATENATED) };
        if ret != ffi::LZMA_OK {
            return Err(io::Error::new(ErrorKind::Other, format!("lzma_stream_decoder failed: {ret}")));
        }
        let mut backend = Self {
            reader: Box::new(reader),
            inbuf: vec![0u8; INPUT_BUFFER],
            in_pos: 0,
            in_len: 0,
            input_eof: false,
            strm,
            total_out: 0,
            finished: false,
            max_distance: FALLBACK_DICT_SIZE,
            tracking_supported: true,
        };
        backend.probe_dictionary()?;
        Ok(backend)
    }

    /// Whether copies can be tracked for this stream.
    pub fn tracking_supported(&self) -> bool {
        self.tracking_supported
    }

    /// Read more input into `inbuf` (compacting first). Returns false at input EOF.
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

    /// Make at least `n` bytes available without consuming them; false if the input ends first.
    fn peek_at_least(&mut self, n: usize) -> io::Result<bool> {
        while self.in_len - self.in_pos < n {
            if !self.refill()? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Parse the stream header and the first block header to learn the LZMA2 dictionary
    /// size and whether the filter chain is trackable. Bytes stay buffered for decoding.
    fn probe_dictionary(&mut self) -> io::Result<()> {
        const STREAM_HEADER: usize = ffi::LZMA_STREAM_HEADER_SIZE as usize;
        if !self.peek_at_least(STREAM_HEADER + 1)? {
            // Too short to be a valid stream; let the decoder report the error.
            return Ok(());
        }
        let mut flags: ffi::lzma_stream_flags = unsafe { std::mem::zeroed() };
        let ret = unsafe { ffi::lzma_stream_header_decode(&mut flags, self.inbuf.as_ptr()) };
        if ret != ffi::LZMA_OK {
            return Ok(());
        }
        let indicator = self.inbuf[STREAM_HEADER];
        if indicator == 0 {
            // Index indicator: an empty stream has no blocks.
            self.max_distance = 0;
            return Ok(());
        }
        let header_size = (indicator as usize + 1) * 4;
        if header_size > ffi::LZMA_BLOCK_HEADER_SIZE_MAX as usize
            || !self.peek_at_least(STREAM_HEADER + header_size)?
        {
            return Ok(());
        }

        let mut filters: [ffi::lzma_filter; ffi::LZMA_FILTERS_MAX as usize + 1] =
            unsafe { std::mem::zeroed() };
        let mut block: ffi::lzma_block = unsafe { std::mem::zeroed() };
        block.version = 1;
        block.header_size = header_size as u32;
        block.check = flags.check;
        block.filters = filters.as_mut_ptr();
        let ret = unsafe {
            ffi::lzma_block_header_decode(&mut block, ptr::null(), self.inbuf.as_ptr().add(STREAM_HEADER))
        };
        if ret != ffi::LZMA_OK {
            return Ok(());
        }

        let mut count = 0;
        let mut dict_size = None;
        for f in &filters {
            if f.id == ffi::LZMA_VLI_UNKNOWN {
                break;
            }
            count += 1;
            if f.id == ffi::LZMA_FILTER_LZMA2 && !f.options.is_null() {
                let opts = unsafe { &*(f.options as *const ffi::lzma_options_lzma) };
                dict_size = Some(opts.dict_size as u64);
            }
        }
        match (count, dict_size) {
            (1, Some(d)) => self.max_distance = d,
            _ => {
                self.tracking_supported = false;
                self.max_distance = 0;
            }
        }
        unsafe { ffi::lzma_filters_free(filters.as_mut_ptr(), ptr::null()) };
        Ok(())
    }
}

impl Drop for XzBackend {
    fn drop(&mut self) {
        unsafe { ffi::lzma_end(&mut *self.strm) };
    }
}

fn lzma_error(ret: ffi::lzma_ret) -> BackendError {
    let (kind, what) = match ret {
        ffi::LZMA_BUF_ERROR => (ErrorKind::UnexpectedEof, "truncated xz stream"),
        ffi::lzma_ret_LZMA_DATA_ERROR => (ErrorKind::InvalidData, "corrupt xz stream"),
        ffi::lzma_ret_LZMA_FORMAT_ERROR => (ErrorKind::InvalidData, "not an xz stream"),
        ffi::lzma_ret_LZMA_MEM_ERROR | ffi::lzma_ret_LZMA_MEMLIMIT_ERROR => {
            (ErrorKind::OutOfMemory, "xz memory limit")
        }
        ffi::lzma_ret_LZMA_OPTIONS_ERROR => (ErrorKind::Unsupported, "unsupported xz options"),
        _ => (ErrorKind::Other, "xz decoding failed"),
    };
    Box::new(io::Error::new(kind, format!("{what} (lzma_ret {ret})")))
}

impl LzBackend for XzBackend {
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
            if self.in_pos == self.in_len {
                self.refill()?;
            }
            let avail_in = self.in_len - self.in_pos;
            let avail_out = target - pos;
            self.strm.next_in = unsafe { self.inbuf.as_ptr().add(self.in_pos) };
            self.strm.avail_in = avail_in;
            self.strm.next_out = unsafe { buf.as_mut_ptr().add(pos) };
            self.strm.avail_out = avail_out;
            let action = if self.input_eof { ffi::LZMA_FINISH } else { ffi::LZMA_RUN };

            let track = self.tracking_supported && rec.is_enabled();
            if track {
                unsafe {
                    lzma_lz_set_match_hook(Some(xz_hook), rec as *mut CopyRecorder as *mut c_void, self.total_out)
                };
            }
            let ret = unsafe { ffi::lzma_code(&mut *self.strm, action) };
            if track {
                unsafe { lzma_lz_set_match_hook(None, ptr::null_mut(), 0) };
            }

            let consumed = avail_in - self.strm.avail_in;
            let produced = avail_out - self.strm.avail_out;
            self.in_pos += consumed;
            self.total_out += produced as u64;

            match ret {
                ffi::LZMA_OK => {
                    if produced > 0 {
                        return Ok(produced);
                    }
                    if self.input_eof && consumed == 0 {
                        return Err(Box::new(io::Error::new(ErrorKind::UnexpectedEof, "truncated xz stream")));
                    }
                }
                ffi::LZMA_STREAM_END => {
                    self.finished = true;
                    return Ok(produced);
                }
                other => return Err(lzma_error(other)),
            }
        }
    }
}
