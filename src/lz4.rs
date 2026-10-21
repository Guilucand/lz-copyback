//! lz4 frame decoding in place, over the patched liblz4 in `external/lz4-sys`.

use crate::copybacks::{copyback_hook, Copybacks};
use crate::in_place::{protected, ring_too_small, InPlaceDecoder, Pieces, RingMap};
use crate::Copyback;
use lzcb_lz4_sys as ffi;
use std::collections::VecDeque;
use std::ffi::CStr;
use std::io::{self, ErrorKind, Read};
use std::ops::Range;
use std::ptr::{self, NonNull};

/// The lz4 frame decompression context, freed on drop.
struct Context(ffi::LZ4FDecompressionContext);

impl Drop for Context {
    fn drop(&mut self) {
        unsafe { ffi::LZ4F_freeDecompressionContext(self.0) };
    }
}

// The context is only reached through `&mut self`.
unsafe impl Sync for Context {}

/// `code` if it is not an lz4 frame error code.
fn check(code: ffi::LZ4FErrorCode) -> io::Result<usize> {
    if unsafe { ffi::LZ4F_isError(code) } == 0 {
        return Ok(code);
    }
    let name = unsafe { CStr::from_ptr(ffi::LZ4F_getErrorName(code)) };
    Err(io::Error::other(format!("LZ4 error: {}", name.to_string_lossy())))
}

/// The largest lz4 block (4 MiB, `LZ4F_max4MB`).
const MAX_BLOCK: usize = 4 << 20;

/// Room past a block for the decoder's writes beyond its decoded bytes.
const BLOCK_SLACK: usize = 64;

/// The smallest frame header: the first input asked for.
const HEADER_MIN: usize = 7;

/// How far back an lz4 match reaches at most: its offsets are 16 bits.
const WINDOW: usize = 64 << 10;

/// A ring keeping `retain` bytes readable: those, a block decoded past them, a block of room
/// for the next one, and what is wasted at the end of the ring when it wraps; liblz4 keeps
/// 64 KiB of history before the block it decodes.
fn alloc_ring(retain: usize) -> (NonNull<u8>, usize) {
    let len = retain + 3 * (MAX_BLOCK + BLOCK_SLACK) + WINDOW;
    let buffer: Box<[u8]> = vec![0; len].into_boxed_slice();
    (NonNull::new(Box::into_raw(buffer) as *mut u8).unwrap(), len)
}

fn free_ring(ring: NonNull<u8>, len: usize) {
    drop(unsafe { Box::from_raw(ptr::slice_from_raw_parts_mut(ring.as_ptr(), len)) });
}

/// An lz4 frame from any reader, decoded in place: each block is decoded straight into a
/// ring buffer (see [`InPlaceDecoder`]), the input being fed as liblz4 asks for it, a block
/// at a time. Like upstream's decoder, it stops after the first frame; an input that ends
/// inside it is an error. A match is recorded when its block (64 KiB to 4 MiB, as the frame
/// says) is decoded, possibly a block ahead of the bytes asked for.
pub struct Lz4InPlaceDecoder<R> {
    reader: R,
    context: Context,
    copybacks: Copybacks,
    ring: NonNull<u8>,
    ring_len: usize,
    write: usize,
    map: RingMap,
    /// The input read and how much of it liblz4 took.
    input: Vec<u8>,
    input_pos: usize,
    /// liblz4's hint of how many input bytes it expects next; 0 once the frame has ended.
    next: usize,
    retain: usize,
    total_in: u64,
}

// SAFETY: the pointers point into the ring the decoder owns, and the context is only reached
// through `&mut self`.
unsafe impl<R: Send> Send for Lz4InPlaceDecoder<R> {}

impl<R: Read> Lz4InPlaceDecoder<R> {
    /// Decodes the lz4 frame read from `reader`, keeping the last `retain` bytes readable
    /// (see [`InPlaceDecoder`]). `hard_min_len` is the hard threshold: the decoder itself leaves out the matches
    /// shorter than it, or than the copybacks' `min_len` if shorter, but for those continuing
    /// the last one reported (see [`InPlaceDecoder::set_copybacks_min_len`]).
    pub fn new(reader: R, retain: usize, hard_min_len: u32) -> io::Result<Self> {
        let mut context = ffi::LZ4FDecompressionContext(ptr::null_mut());
        check(unsafe { ffi::LZ4F_createDecompressionContext(&mut context, ffi::LZ4F_VERSION) })?;
        let (ring, ring_len) = alloc_ring(retain);
        Ok(Self {
            reader,
            context: Context(context),
            copybacks: Copybacks::new(hard_min_len),
            ring,
            ring_len,
            write: 0,
            map: RingMap::default(),
            input: Vec::new(),
            input_pos: 0,
            next: HEADER_MIN,
            retain,
            total_in: 0,
        })
    }

    /// The underlying reader.
    pub fn reader(&self) -> &R {
        &self.reader
    }

    /// Reads the input liblz4 asks for next, or as much of it as there is.
    fn read_input(&mut self) -> io::Result<()> {
        self.input.resize(self.next, 0);
        let mut filled = 0;
        while filled < self.next {
            match self.reader.read(&mut self.input[filled..]) {
                Ok(0) => break,
                Ok(count) => filled += count,
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        if filled == 0 {
            return Err(io::Error::new(ErrorKind::UnexpectedEof, "lz4: the input ends inside a frame"));
        }
        self.input.truncate(filled);
        self.input_pos = 0;
        self.total_in += filled as u64;
        Ok(())
    }

    /// Feeds liblz4 the next input, decoding at most a block, straight into the ring.
    fn step(&mut self, protect: u64) -> io::Result<()> {
        if self.input_pos == self.input.len() {
            self.read_input()?;
        }
        // liblz4 decodes a block in place only with room for the largest one.
        let room = MAX_BLOCK + BLOCK_SLACK;
        if self.write + room > self.ring_len {
            self.write = 0;
        }
        if self.map.writable(self.ring, self.write, protect) < room {
            return Err(ring_too_small("lz4"));
        }
        self.map.overwrite(self.ring, self.write, room);

        let mut src_size = self.input.len() - self.input_pos;
        let mut dst_size = self.ring_len - self.write;
        let track = self.copybacks.tracking;
        if track {
            let (min_len, last_end, last_offset) =
                (self.copybacks.hard_min_len(), self.copybacks.last_end, self.copybacks.last_offset);
            let ctx = self.copybacks.hook_ctx();
            unsafe {
                ffi::LZ4F_setLzMatchHook(self.context.0, Some(copyback_hook), ctx, min_len, last_end, last_offset)
            };
        }
        let code = unsafe {
            ffi::LZ4F_decompress(
                self.context.0,
                self.ring.as_ptr().add(self.write),
                &mut dst_size,
                self.input[self.input_pos..].as_ptr(),
                &mut src_size,
                ptr::null(),
            )
        };
        if track {
            unsafe { ffi::LZ4F_setLzMatchHook(self.context.0, None, ptr::null_mut(), 0, u64::MAX, 0) };
        }
        self.next = check(code)?;
        self.input_pos += src_size;
        self.map.push(self.ring, self.write, dst_size);
        self.write += dst_size;
        Ok(())
    }
}

impl<R: Read> InPlaceDecoder for Lz4InPlaceDecoder<R> {
    fn decode_to(&mut self, target: u64) -> io::Result<u64> {
        let protect = protected(target, self.retain);
        while self.next != 0 && self.map.end() < target {
            self.step(protect)?;
        }
        Ok(self.map.end())
    }

    fn decoded_len(&self) -> u64 {
        self.map.end()
    }

    fn is_finished(&self) -> bool {
        self.next == 0
    }

    fn bytes(&self, range: Range<u64>) -> Pieces<'_> {
        self.map.pieces(range)
    }

    fn bytes_and_copybacks(&mut self, range: Range<u64>) -> (Pieces<'_>, &mut VecDeque<Copyback>) {
        (self.map.pieces(range), &mut self.copybacks.queue)
    }

    fn peek_copybacks(&mut self) -> &mut VecDeque<Copyback> {
        &mut self.copybacks.queue
    }

    fn set_copybacks_tracking(&mut self, enabled: bool) {
        self.copybacks.tracking = enabled;
    }

    fn set_copybacks_min_len(&mut self, min_len: u32) {
        self.copybacks.min_len = min_len;
    }

    fn total_in(&self) -> u64 {
        self.total_in
    }

    fn window_size(&self) -> u64 {
        WINDOW as u64
    }

    fn retain_window(&mut self) {
        assert_eq!(self.map.end(), 0, "lz4: the window is retained from the first byte only");
        self.retain += WINDOW;
        free_ring(self.ring, self.ring_len);
        (self.ring, self.ring_len) = alloc_ring(self.retain);
    }
}

impl<R> Drop for Lz4InPlaceDecoder<R> {
    fn drop(&mut self) {
        free_ring(self.ring, self.ring_len);
    }
}
