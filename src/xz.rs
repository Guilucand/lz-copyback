//! xz decoding over the patched liblzma in `external/lzma-sys`, with the interface of
//! `liblzma::read::XzDecoder`.

use crate::copybacks::{copyback_hook, copyback_methods, Copybacks};
use lzcb_lzma_sys as ffi;
use std::io::{self, BufRead, BufReader, ErrorKind, Read};
use std::{mem, ptr};

const INPUT_BUFFER_SIZE: usize = 1 << 17;

/// An xz decoder over a [`Read`] that also records the LZ matches it decodes.
///
/// A match is recorded, whole, during the `read` call that returns its first byte, even when
/// it continues past the end of the caller's buffer. Blocks whose filter chain is not a lone
/// LZMA2 (BCJ, delta) record nothing: those filters rewrite the LZ output, so its matches do
/// not repeat output bytes. Such chains are only written on request (`xz --x86`, `--delta`,
/// `--filters=`).
pub struct XzDecoder<R> {
    reader: BufReader<R>,
    stream: Stream,
    copybacks: Copybacks,
}

/// The liblzma decoder state, ended on drop.
struct Stream(ffi::lzma_stream);

// SAFETY: the stream only points into liblzma's private state, and into the caller's buffers
// for the length of one `lzma_code` call.
unsafe impl Send for Stream {}
unsafe impl Sync for Stream {}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe { ffi::lzma_end(&mut self.0) };
    }
}

impl<R: Read> XzDecoder<R> {
    /// Decodes one .xz stream.
    pub fn new(reader: R) -> Self {
        Self::with_decoder(reader, |stream| unsafe { ffi::lzma_stream_decoder(stream, u64::MAX, 0) })
    }

    /// Decodes .xz streams one after the other until the end of the input, and also `.lzma`
    /// and `.lz` input (liblzma's auto decoder).
    pub fn new_multi_decoder(reader: R) -> Self {
        Self::with_decoder(reader, |stream| unsafe {
            ffi::lzma_auto_decoder(stream, u64::MAX, ffi::LZMA_CONCATENATED)
        })
    }

    fn with_decoder(reader: R, init: impl FnOnce(*mut ffi::lzma_stream) -> ffi::lzma_ret) -> Self {
        // LZMA_STREAM_INIT is all zeros.
        let mut stream = Stream(unsafe { mem::zeroed() });
        let ret = init(&mut stream.0);
        // Only a failed allocation gets here, which upstream reports with a panic as well.
        assert_eq!(ret, ffi::LZMA_OK, "liblzma could not create a decoder (lzma_ret {ret})");
        XzDecoder {
            reader: BufReader::with_capacity(INPUT_BUFFER_SIZE, reader),
            stream,
            copybacks: Copybacks::new(),
        }
    }
}

impl<R> XzDecoder<R> {
    /// The underlying reader.
    pub fn get_ref(&self) -> &R {
        self.reader.get_ref()
    }

    /// The underlying reader. Reading from it directly would corrupt the decoding.
    pub fn get_mut(&mut self) -> &mut R {
        self.reader.get_mut()
    }

    /// The underlying reader. Input already buffered by the decoder is lost.
    pub fn into_inner(self) -> R {
        self.reader.into_inner()
    }

    /// Compressed bytes consumed so far.
    pub fn total_in(&self) -> u64 {
        self.stream.0.total_in
    }

    /// Bytes decoded so far: the absolute offset of the next output byte.
    pub fn total_out(&self) -> u64 {
        self.stream.0.total_out
    }

    copyback_methods!();
}

impl<R: Read> Read for XzDecoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let input = self.reader.fill_buf()?;
            let eof = input.is_empty();
            let stream = &mut self.stream.0;
            let (before_in, before_out) = (stream.total_in, stream.total_out);
            stream.next_in = input.as_ptr();
            stream.avail_in = input.len();
            stream.next_out = buf.as_mut_ptr();
            stream.avail_out = buf.len();

            // The hook reports the matches of this call only, the short ones filtered out
            // in C. total_out never resets, not even between concatenated streams, so it is
            // the absolute offset of the call's first output byte.
            let track = self.copybacks.tracking;
            if track {
                let min_len = self.copybacks.min_len;
                let ctx = self.copybacks.hook_ctx();
                unsafe { ffi::lzma_lz_set_match_hook(Some(copyback_hook), ctx, before_out, min_len) };
            }
            let ret = unsafe { ffi::lzma_code(stream, if eof { ffi::LZMA_FINISH } else { ffi::LZMA_RUN }) };
            if track {
                unsafe { ffi::lzma_lz_set_match_hook(None, ptr::null_mut(), 0, 0) };
            }
            let read = (stream.total_out - before_out) as usize;
            let consumed = (stream.total_in - before_in) as usize;
            stream.next_in = ptr::null();
            stream.avail_in = 0;
            stream.next_out = ptr::null_mut();
            stream.avail_out = 0;
            self.reader.consume(consumed);

            // As liblzma's own reader does.
            let stream_end = check(ret)?;
            if read > 0 || eof || stream_end {
                if read == 0 && !stream_end {
                    return Err(io::Error::new(ErrorKind::UnexpectedEof, "premature eof"));
                }
                return Ok(read);
            }
            if consumed == 0 {
                return Err(io::Error::new(ErrorKind::InvalidData, "corrupt xz stream"));
            }
        }
    }
}

/// Whether `ret` ends the stream, or the error it reports, with the kinds liblzma's own
/// reader uses.
fn check(ret: ffi::lzma_ret) -> io::Result<bool> {
    let (kind, what) = match ret {
        ffi::LZMA_OK | ffi::LZMA_GET_CHECK | ffi::LZMA_BUF_ERROR => return Ok(false),
        ffi::LZMA_STREAM_END => return Ok(true),
        ffi::LZMA_DATA_ERROR => (ErrorKind::InvalidData, "xz: corrupt data"),
        ffi::LZMA_FORMAT_ERROR => (ErrorKind::InvalidData, "xz: unknown file format"),
        ffi::LZMA_OPTIONS_ERROR => (ErrorKind::InvalidInput, "xz: unsupported options"),
        ffi::LZMA_NO_CHECK => (ErrorKind::InvalidInput, "xz: no integrity check"),
        ffi::LZMA_UNSUPPORTED_CHECK => (ErrorKind::Other, "xz: unsupported integrity check"),
        ffi::LZMA_MEM_ERROR => (ErrorKind::Other, "xz: out of memory"),
        ffi::LZMA_MEMLIMIT_ERROR => (ErrorKind::Other, "xz: memory limit reached"),
        _ => (ErrorKind::Other, "xz: decoder error"),
    };
    Err(io::Error::new(kind, format!("{what} (lzma_ret {ret})")))
}
