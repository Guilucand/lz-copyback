//! gzip/deflate backend over `streaming-libdeflate-rs`.
//!
//! The decoder writes straight into the tracking window: [`WindowOutput`] implements
//! `DeflateOutput` over the caller's buffer for the duration of one `decode` call. Deflate
//! reads its 32 KiB history through the same pointers, which the window layout provides,
//! and reports every match through the `on_lz_copy` hook added to the trait.

use crate::backend::{BackendError, LzBackend};
use crate::recorder::CopyRecorder;
use crc32fast::Hasher;
use std::io::{self, ErrorKind, Read};
use std::sync::{Arc, Mutex};
use streaming_libdeflate_rs::decompress_deflate::OutStreamResult;
use streaming_libdeflate_rs::decompress_gzip::LibdeflateGzipDecompressor;
use streaming_libdeflate_rs::streams::deflate_chunked_buffer_input::DeflateChunkedBufferInput;
use streaming_libdeflate_rs::{
    DeflateInput, DeflateOutput, LibdeflateDecodeTables, LibdeflateDecompressResult, LibdeflateError,
    libdeflate_alloc_decode_tables,
};

const INPUT_BUFFER: usize = 1 << 20;

/// Output state over the tracking window. Re-anchored to the caller's buffer on every
/// `decode` call; nothing is pending between calls. Wrapped in [`Out`] with the tracking
/// flag as a const so the untracked copy loop has no per-match branch at all.
struct WindowOutput {
    buf_start: *mut u8,
    /// First byte of this call's output (never consumed before the call ends).
    pending_start: usize,
    /// Write index.
    cur: usize,
    /// `cur + n <= limit` is required before writing `n` bytes; `OVERWRITE_MAX` bytes of
    /// garbage may land beyond it.
    limit: usize,
    /// Absolute stream offset of `buf[0]` (modulo 2^64: it may be "negative" while the
    /// guard region precedes the stream start).
    abs_buf_base: u64,
    member_written: usize,
    crc: Hasher,
    /// Null while tracking is off.
    rec: *mut CopyRecorder,
}

impl WindowOutput {
    fn detached() -> Self {
        Self {
            buf_start: std::ptr::null_mut(),
            pending_start: 0,
            cur: 0,
            limit: 0,
            abs_buf_base: 0,
            member_written: 0,
            crc: Hasher::new(),
            rec: std::ptr::null_mut(),
        }
    }
}

impl WindowOutput {
    fn pending_len(&self) -> usize {
        self.cur - self.pending_start
    }

    fn consume(&mut self, consumed: usize) {
        debug_assert!(consumed <= self.pending_len());
        let bytes = unsafe { std::slice::from_raw_parts(self.buf_start.add(self.pending_start), consumed) };
        self.crc.update(bytes);
        self.member_written += consumed;
        self.pending_start += consumed;
    }
}

/// `DeflateOutput` view of the window state; `TRACK` selects the hooked copy loop.
struct Out<'a, const TRACK: bool>(&'a mut WindowOutput);

impl<'a, const TRACK: bool> DeflateOutput for Out<'a, TRACK> {
    const TRACK_LZ: bool = TRACK;

    #[inline(always)]
    fn has_writable_length(&mut self, length: usize) -> bool {
        self.0.cur + length <= self.0.limit
    }

    #[inline(always)]
    fn get_output_ptr(&mut self) -> *mut u8 {
        unsafe { self.0.buf_start.add(self.0.cur) }
    }

    #[inline(always)]
    unsafe fn set_output_ptr(&mut self, ptr: *mut u8) {
        self.0.cur = unsafe { ptr.offset_from(self.0.buf_start) } as usize;
    }

    fn pending_output(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.0.buf_start.add(self.0.pending_start), self.0.pending_len()) }
    }

    fn consume_output(&mut self, consumed_offset: usize) {
        self.0.consume(consumed_offset);
    }

    fn increase_buffer_size(&mut self, _additional: usize) {
        unreachable!("the window output never grows");
    }

    fn finish_member(&mut self) -> Result<OutStreamResult, ()> {
        let st = &mut *self.0;
        if st.cur != st.pending_start {
            return Err(());
        }
        let result = OutStreamResult {
            written: st.member_written,
            crc32: st.crc.clone().finalize(),
        };
        st.crc = Hasher::new();
        st.member_written = 0;
        Ok(result)
    }

    #[inline(always)]
    fn on_lz_copy(&mut self, dst: *const u8, distance: usize, len: usize) {
        if TRACK && len != 0 {
            let st = &mut *self.0;
            let off = unsafe { dst.offset_from(st.buf_start) } as u64;
            // SAFETY: non-null only during a tracked `decode` call on this thread.
            let rec = unsafe { &mut *st.rec };
            rec.record(st.abs_buf_base.wrapping_add(off), distance as u64, len as u64);
        }
    }
}

/// One `decode` call's gzip loop, monomorphised for the tracked and untracked outputs.
fn run_gzip<O: DeflateOutput>(
    gzip: &mut LibdeflateGzipDecompressor,
    tables: &mut LibdeflateDecodeTables,
    input: &mut DeflateChunkedBufferInput<'static>,
    out: &mut O,
    finished: &mut bool,
) -> Result<(), BackendError> {
    loop {
        match gzip.decompress(tables, input, out) {
            Err(e) => return Err(map_error(e)),
            Ok(LibdeflateDecompressResult::MoreData) => return Ok(()),
            Ok(LibdeflateDecompressResult::EndOfStream) => {
                if !out.pending_output().is_empty() {
                    return Ok(());
                }
                input.ensure_overread_length();
                if !input.has_valid_bytes_slow() {
                    *finished = true;
                    return Ok(());
                }
                // Another concatenated member follows.
                *gzip = LibdeflateGzipDecompressor::new();
            }
        }
    }
}

pub struct DeflateBackend {
    input: DeflateChunkedBufferInput<'static>,
    read_error: Arc<Mutex<Option<io::Error>>>,
    tables: LibdeflateDecodeTables,
    gzip: LibdeflateGzipDecompressor,
    out: WindowOutput,
    total_out: u64,
    finished: bool,
}

// SAFETY: the input closure owns a `Read + Send` reader and an `Arc<Mutex<_>>`; the raw
// pointers in `WindowOutput` are only set during a `decode` call on the calling thread.
unsafe impl Send for DeflateBackend {}

impl DeflateBackend {
    pub fn new(mut reader: impl Read + Send + 'static) -> Self {
        let read_error = Arc::new(Mutex::new(None));
        let error_slot = read_error.clone();
        let input = DeflateChunkedBufferInput::new(
            move |buf: &mut [u8]| loop {
                match reader.read(buf) {
                    Ok(n) => return n,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(e) => {
                        *error_slot.lock().unwrap() = Some(e);
                        return 0;
                    }
                }
            },
            INPUT_BUFFER,
        );
        Self {
            input,
            read_error,
            tables: libdeflate_alloc_decode_tables(),
            gzip: LibdeflateGzipDecompressor::new(),
            out: WindowOutput::detached(),
            total_out: 0,
            finished: false,
        }
    }
}

fn map_error(e: LibdeflateError) -> BackendError {
    Box::new(io::Error::new(ErrorKind::InvalidData, format!("corrupt gzip stream ({e:?})")))
}

impl LzBackend for DeflateBackend {
    fn max_distance(&self) -> u64 {
        <Out<'static, false> as DeflateOutput>::MAX_LOOK_BACK as u64
    }

    fn history_required(&self) -> usize {
        <Out<'static, false> as DeflateOutput>::MAX_LOOK_BACK
    }

    fn decode(
        &mut self,
        buf: &mut [u8],
        pos: usize,
        _target: usize,
        rec: &mut CopyRecorder,
    ) -> Result<usize, BackendError> {
        if self.finished {
            return Ok(0);
        }
        // Anchor the output to this call's buffer. Deflate fills as far as its writable
        // check allows, which is inside the slack past the target.
        self.out.buf_start = buf.as_mut_ptr();
        self.out.pending_start = pos;
        self.out.cur = pos;
        self.out.limit = buf.len() - <Out<'static, false> as DeflateOutput>::OVERWRITE_MAX;
        self.out.abs_buf_base = self.total_out.wrapping_sub(pos as u64);
        let result = if rec.is_enabled() {
            self.out.rec = rec as *mut CopyRecorder;
            run_gzip(&mut self.gzip, &mut self.tables, &mut self.input, &mut Out::<true>(&mut self.out), &mut self.finished)
        } else {
            run_gzip(&mut self.gzip, &mut self.tables, &mut self.input, &mut Out::<false>(&mut self.out), &mut self.finished)
        };
        self.out.rec = std::ptr::null_mut();

        let produced = self.out.pending_len();
        self.out.consume(produced);
        self.total_out += produced as u64;
        self.out.buf_start = std::ptr::null_mut();

        if let Some(e) = self.read_error.lock().unwrap().take() {
            return Err(Box::new(e));
        }
        result?;
        if produced == 0 && !self.finished {
            return Err(Box::new(io::Error::new(ErrorKind::UnexpectedEof, "truncated gzip stream")));
        }
        Ok(produced)
    }
}
