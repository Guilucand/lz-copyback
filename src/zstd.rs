//! zstd decoding over the patched libzstd in `external/zstd-sys`, with the interface of
//! `zstd::stream::read::Decoder`.

use crate::copybacks::{copyback_hook, copyback_methods, Copybacks};
use lzcb_zstd_sys as ffi;
use std::ffi::{c_void, CStr};
use std::io::{self, BufRead, BufReader, ErrorKind, Read};
use std::ptr::{self, NonNull};

/// A zstd decoder over a [`BufRead`] that also records the LZ matches it decodes.
///
/// zstd decodes a whole block (at most 128 KiB) into a buffer of its own before handing it
/// out, so a match is recorded during the `read` call that returns the first byte of its
/// block, possibly before the call that returns its own bytes. Frames are decoded one after
/// the other until the end of the input, as upstream does unless [`Self::single_frame`].
pub struct ZstdDecoder<R> {
    reader: R,
    context: Context,
    copybacks: Copybacks,
    state: State,
    single_frame: bool,
    finished_frame: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Reading,
    PastEof,
    Finished,
}

/// The zstd decompression context, freed on drop.
struct Context(NonNull<ffi::ZSTD_DCtx>);

// SAFETY: the context is owned here and used from one thread at a time.
unsafe impl Send for Context {}
unsafe impl Sync for Context {}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe { ffi::ZSTD_freeDStream(self.0.as_ptr()) };
    }
}

impl<R: Read> ZstdDecoder<BufReader<R>> {
    /// Decodes `reader`, buffered with the input size zstd recommends.
    pub fn new(reader: R) -> io::Result<Self> {
        let capacity = unsafe { ffi::ZSTD_DStreamInSize() };
        Self::with_buffer(BufReader::with_capacity(capacity, reader))
    }
}

impl<R: BufRead> ZstdDecoder<R> {
    /// Decodes an already buffered reader.
    pub fn with_buffer(reader: R) -> io::Result<Self> {
        let context = NonNull::new(unsafe { ffi::ZSTD_createDStream() })
            .map(Context)
            .ok_or_else(|| io::Error::new(ErrorKind::OutOfMemory, "zstd: cannot create a context"))?;
        check(unsafe { ffi::ZSTD_initDStream(context.0.as_ptr()) })?;
        Ok(ZstdDecoder {
            reader,
            context,
            copybacks: Copybacks::new(),
            state: State::Reading,
            single_frame: false,
            finished_frame: false,
        })
    }

    /// Stops after the first frame instead of decoding the frames that follow it.
    #[must_use]
    pub fn single_frame(mut self) -> Self {
        self.single_frame = true;
        self
    }

    /// Accepts frames whose window is up to `2^log_distance` bytes; zstd refuses windows
    /// larger than 128 MiB (`--long=27`) unless told otherwise.
    pub fn window_log_max(&mut self, log_distance: u32) -> io::Result<()> {
        check(unsafe {
            ffi::ZSTD_DCtx_setParameter(
                self.context.0.as_ptr(),
                ffi::ZSTD_dParameter::ZSTD_d_windowLogMax,
                log_distance as i32,
            )
        })
        .map(drop)
    }

    /// The output buffer size zstd recommends: one whole block.
    pub fn recommended_output_size() -> usize {
        unsafe { ffi::ZSTD_DStreamOutSize() }
    }

    /// The underlying reader.
    pub fn get_ref(&self) -> &R {
        &self.reader
    }

    /// The underlying reader. Reading from it directly would corrupt the decoding.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.reader
    }

    /// The underlying reader. Calling this is not required after reading the stream.
    pub fn finish(self) -> R {
        self.reader
    }

    copyback_methods!();

    /// One `ZSTD_decompressStream` call, with the match hook installed while tracking; the
    /// short matches are filtered out in C. The offsets count every byte the context
    /// decoded since it was created (the patched library never resets that count), so they
    /// run on across frames.
    fn run(&mut self, input: &mut ffi::ZSTD_inBuffer, output: &mut ffi::ZSTD_outBuffer) -> io::Result<usize> {
        let dctx = self.context.0.as_ptr();
        let track = self.copybacks.tracking;
        if track {
            let min_len = self.copybacks.min_len;
            let ctx = self.copybacks.hook_ctx();
            unsafe { ffi::ZSTD_DCtx_setLzMatchHook(dctx, Some(copyback_hook), ctx, min_len) };
        }
        let hint = unsafe { ffi::ZSTD_decompressStream(dctx, output, input) };
        if track {
            unsafe { ffi::ZSTD_DCtx_setLzMatchHook(dctx, None, ptr::null_mut(), 0) };
        }
        check(hint)
    }
}

/// The same state machine as zstd's own reader: drain what the context holds, feed it input
/// until it produces something, reset it between frames, and fail on a truncated frame.
impl<R: BufRead> Read for ZstdDecoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut first = true;
        loop {
            match self.state {
                State::Reading => {
                    let (consumed, written) = {
                        // The first call takes no input: it flushes what the context holds.
                        let input: &[u8] = if first { b"" } else { self.reader.fill_buf()? };
                        if !first && input.is_empty() {
                            self.state = State::PastEof;
                            continue;
                        }
                        first = false;

                        if self.finished_frame && !input.is_empty() {
                            check(unsafe {
                                ffi::ZSTD_DCtx_reset(
                                    self.context.0.as_ptr(),
                                    ffi::ZSTD_ResetDirective::ZSTD_reset_session_only,
                                )
                            })?;
                            self.finished_frame = false;
                        }

                        let mut src = ffi::ZSTD_inBuffer {
                            src: input.as_ptr() as *const c_void,
                            size: input.len(),
                            pos: 0,
                        };
                        let mut dst = ffi::ZSTD_outBuffer {
                            dst: buf.as_mut_ptr() as *mut c_void,
                            size: buf.len(),
                            pos: 0,
                        };
                        let hint = self.run(&mut src, &mut dst)?;
                        if hint == 0 {
                            self.finished_frame = true;
                            if self.single_frame {
                                self.state = State::Finished;
                            }
                        }
                        (src.pos, dst.pos)
                    };
                    self.reader.consume(consumed);
                    if written > 0 {
                        return Ok(written);
                    }
                }
                State::PastEof => {
                    if !self.finished_frame {
                        return Err(io::Error::new(ErrorKind::UnexpectedEof, "incomplete frame"));
                    }
                    self.state = State::Finished;
                    return Ok(0);
                }
                State::Finished => return Ok(0),
            }
        }
    }
}

/// `code` if it is not a zstd error code.
fn check(code: usize) -> io::Result<usize> {
    if unsafe { ffi::ZSTD_isError(code) } == 0 {
        return Ok(code);
    }
    let name = unsafe { CStr::from_ptr(ffi::ZSTD_getErrorName(code)) };
    Err(io::Error::other(name.to_string_lossy().into_owned()))
}
