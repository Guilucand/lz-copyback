//! lz4 frame decoding over the patched liblz4 in `external/lz4-sys`, with the interface of
//! `lz4::Decoder`.

use crate::copybacks::{copyback_hook, copyback_methods, Copybacks};
use lzcb_lz4_sys as ffi;
use std::ffi::CStr;
use std::io::{self, ErrorKind, Read};
use std::ptr;

const INPUT_BUFFER_SIZE: usize = 32 * 1024;

/// An lz4 frame decoder over a [`Read`] that also records the LZ matches it decodes.
///
/// lz4 decodes a whole block (64 KiB to 4 MiB, as the frame says) at once, into a buffer of
/// its own when the caller's is smaller, so a match is recorded during the `read` call that
/// returns the first byte of its block, possibly before the call that returns its own
/// bytes. Like upstream, the decoder stops after the first frame.
pub struct Lz4Decoder<R> {
    reader: R,
    context: Context,
    copybacks: Copybacks,
    buf: Box<[u8]>,
    pos: usize,
    len: usize,
    /// liblz4's hint of how many input bytes it expects next; 0 once the frame has ended.
    next: usize,
}

/// The lz4 frame decompression context, freed on drop.
struct Context(ffi::LZ4FDecompressionContext);

impl Drop for Context {
    fn drop(&mut self) {
        unsafe { ffi::LZ4F_freeDecompressionContext(self.0) };
    }
}

// The context is only reached through `&mut self`.
unsafe impl Sync for Context {}

impl<R: Read> Lz4Decoder<R> {
    /// Decodes the lz4 frame read from `reader`.
    pub fn new(reader: R) -> io::Result<Self> {
        let mut context = ffi::LZ4FDecompressionContext(ptr::null_mut());
        check(unsafe { ffi::LZ4F_createDecompressionContext(&mut context, ffi::LZ4F_VERSION) })?;
        Ok(Lz4Decoder {
            reader,
            context: Context(context),
            copybacks: Copybacks::new(),
            buf: vec![0; INPUT_BUFFER_SIZE].into_boxed_slice(),
            pos: INPUT_BUFFER_SIZE,
            len: INPUT_BUFFER_SIZE,
            // The smallest lz4 frame.
            next: 11,
        })
    }

    /// The underlying reader.
    pub fn reader(&self) -> &R {
        &self.reader
    }

    /// The underlying reader, and an error if the frame was not read to its end: a truncated
    /// frame only shows here, since `read` then just stops returning bytes.
    pub fn finish(self) -> (R, io::Result<()>) {
        let result = match self.next {
            0 => Ok(()),
            _ => Err(io::Error::new(
                ErrorKind::Interrupted,
                "Finish runned before read end of compressed stream",
            )),
        };
        (self.reader, result)
    }

    copyback_methods!();
}

/// The same loop as upstream's decoder.
impl<R: Read> Read for Lz4Decoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.next == 0 || buf.is_empty() {
            return Ok(0);
        }
        let mut dst_offset = 0;
        while dst_offset == 0 {
            if self.pos >= self.len {
                let need = self.buf.len().min(self.next);
                self.len = self.reader.read(&mut self.buf[..need])?;
                // Even with nothing read, the context may still have bytes to hand out.
                self.pos = 0;
                self.next -= self.len;
            }
            while dst_offset < buf.len() && (self.pos < self.len || self.len == 0) {
                let mut src_size = self.len - self.pos;
                let mut dst_size = buf.len() - dst_offset;
                // The hook reports the matches of this call only, the short ones filtered
                // out in C, at offsets counting every byte the context decoded since it was
                // created.
                let track = self.copybacks.tracking;
                if track {
                    let min_len = self.copybacks.min_len;
                    let ctx = self.copybacks.hook_ctx();
                    unsafe { ffi::LZ4F_setLzMatchHook(self.context.0, Some(copyback_hook), ctx, min_len) };
                }
                let code = unsafe {
                    ffi::LZ4F_decompress(
                        self.context.0,
                        buf[dst_offset..].as_mut_ptr(),
                        &mut dst_size,
                        self.buf[self.pos..].as_ptr(),
                        &mut src_size,
                        ptr::null(),
                    )
                };
                if track {
                    unsafe { ffi::LZ4F_setLzMatchHook(self.context.0, None, ptr::null_mut(), 0) };
                }
                let hint = check(code)?;
                self.pos += src_size;
                dst_offset += dst_size;

                // Keep asking until the context stops producing, even with no input left.
                if dst_size == 0 && src_size == 0 {
                    return Ok(dst_offset);
                }
                if hint == 0 {
                    self.next = 0;
                    return Ok(dst_offset);
                } else if self.next < hint {
                    self.next = hint;
                }
            }
        }
        Ok(dst_offset)
    }
}

/// `code` if it is not an lz4 frame error code.
fn check(code: ffi::LZ4FErrorCode) -> io::Result<usize> {
    if unsafe { ffi::LZ4F_isError(code) } == 0 {
        return Ok(code);
    }
    let name = unsafe { CStr::from_ptr(ffi::LZ4F_getErrorName(code)) };
    Err(io::Error::other(format!("LZ4 error: {}", name.to_string_lossy())))
}
