//! gzip decoding in place, over `streaming-libdeflate-rs`: deflate decodes straight into a
//! ring buffer, and the matches come from its `on_lz_copy` hook.

use crate::copybacks::Copybacks;
use crate::in_place::{protected, ring_too_small, InPlaceDecoder, Pieces, RingMap};
use crate::Copyback;
use std::collections::VecDeque;
use std::io::{self, ErrorKind, Read};
use std::ops::Range;
use std::ptr::NonNull;
use streaming_libdeflate_rs::decompress_deflate::{OutStreamResult, MAX_WRITE};
use streaming_libdeflate_rs::decompress_gzip::LibdeflateGzipDecompressor;
use streaming_libdeflate_rs::streams::deflate_chunked_buffer_input::DeflateChunkedBufferInput;
use streaming_libdeflate_rs::streams::deflate_chunked_buffer_output::DeflateChunkedBufferOutput;
use streaming_libdeflate_rs::{
    libdeflate_alloc_decode_tables, DeflateInput, DeflateOutput, LibdeflateDecodeTables, LibdeflateDecompressResult,
};

/// How far back deflate's matches reach: what is copied in front of the ring at every wrap.
const HISTORY: usize = 32 * 1024;

/// How much the deflate decoder writes past its output pointer at once, at most: it asks for
/// `MAX_WRITE` bytes of room and may write a few more than the bytes it decodes.
const WRITE_AHEAD: usize = MAX_WRITE + <DeflateChunkedBufferOutput<'static> as DeflateOutput>::OVERWRITE_MAX;

/// How much of the ring is cleared for writing at once.
const CLEAR_STEP: usize = 64 * 1024;

/// The input buffer of the in-place decoder, and the most it reads at once: what it has read
/// ahead of what it decoded, counted in its `total_in`, stays small.
const IN_PLACE_INPUT_SIZE: usize = 64 * 1024;
const IN_PLACE_READ_SIZE: usize = 16 * 1024;

/// The output of a [`GzipInPlaceDecoder`]: a ring that deflate decodes straight into, with the
/// history its matches reach copied in front of the ring at every wrap.
struct GzipRing {
    /// `HISTORY` bytes, the ring up to `ring_end`, and room for the decoder to write past
    /// the end of the ring. Allocated as a raw pointer: it is only accessed through pointers.
    base: NonNull<u8>,
    len: usize,
    ring_end: usize,
    /// Where the decoder writes next.
    ptr: *mut u8,
    /// The bytes decoded but not recorded in `map` yet start here, at stream offset
    /// `run_start`.
    run_ptr: *mut u8,
    run_start: u64,
    /// The decoder can write up to here without [`Self::make_room`].
    fast_end: *mut u8,
    /// Decoding stops at this stream offset.
    limit: u64,
    /// No readable byte from this stream offset on may be overwritten.
    protect: u64,
    /// The ring is cleared for writing up to this index.
    cleared: usize,
    /// Whether decoding stopped because the ring could not be cleared without overwriting
    /// protected bytes.
    blocked: bool,
    map: RingMap,
    /// The checksum and the size of the gzip member being decoded.
    crc: crc32fast::Hasher,
    written: usize,
    copybacks: Copybacks,
    /// The threshold, at least 1 so that entries with literals only, which report a zero
    /// length, never pass.
    min_len: u32,
}

// SAFETY: the pointers point into the buffer the ring owns.
unsafe impl Send for GzipRing {}
unsafe impl Sync for GzipRing {}

impl GzipRing {
    fn new(retain: usize) -> Self {
        let ring_end = HISTORY + retain + CLEAR_STEP + 4 * WRITE_AHEAD + 2 * HISTORY;
        let len = ring_end + WRITE_AHEAD;
        let buffer: Box<[u8]> = vec![0; len].into_boxed_slice();
        let base = NonNull::new(Box::into_raw(buffer) as *mut u8).unwrap();
        let start = unsafe { base.as_ptr().add(HISTORY) };
        Self {
            base,
            len,
            ring_end,
            ptr: start,
            run_ptr: start,
            run_start: 0,
            fast_end: start,
            limit: 0,
            protect: 0,
            cleared: HISTORY,
            blocked: false,
            map: RingMap::default(),
            crc: crc32fast::Hasher::new(),
            written: 0,
            copybacks: Copybacks::new(),
            min_len: 1,
        }
    }

    fn index(&self, ptr: *const u8) -> usize {
        ptr as usize - self.base.as_ptr() as usize
    }

    fn at(&self, index: usize) -> *mut u8 {
        unsafe { self.base.as_ptr().add(index) }
    }

    /// Bytes decoded so far.
    fn decoded(&self) -> u64 {
        self.run_start + (self.ptr as usize - self.run_ptr as usize) as u64
    }

    /// Records the bytes decoded since the last time: in the map, the checksum and the size.
    fn record(&mut self) {
        let len = self.ptr as usize - self.run_ptr as usize;
        if len == 0 {
            return;
        }
        self.crc.update(unsafe { std::slice::from_raw_parts(self.run_ptr, len) });
        self.written += len;
        self.map.push(self.base, self.index(self.run_ptr), len);
        self.run_start += len as u64;
        self.run_ptr = self.ptr;
    }

    fn set_bounds(&mut self, limit: u64, protect: u64) {
        self.limit = limit;
        self.protect = protect;
        self.update_fast_end();
    }

    /// Before the end of the ring, of the cleared part and of the limit, no check is needed.
    fn update_fast_end(&mut self) {
        let to_limit = self.limit.saturating_sub(self.decoded()).min(self.len as u64) as usize;
        let end = (self.index(self.ptr) + to_limit)
            .min(self.ring_end)
            .min(self.cleared.saturating_sub(WRITE_AHEAD))
            .max(self.index(self.ptr));
        self.fast_end = self.at(end);
    }

    #[inline(always)]
    fn has_writable_length(&mut self, length: usize) -> bool {
        if (self.ptr as usize + length) <= self.fast_end as usize {
            return true;
        }
        self.make_room(length)
    }

    /// Stops at the limit, wraps at the end of the ring, and clears the ring ahead of the
    /// decoder, if that does not overwrite protected bytes.
    #[cold]
    #[inline(never)]
    fn make_room(&mut self, length: usize) -> bool {
        if self.decoded() >= self.limit {
            return false;
        }
        if self.index(self.ptr) + length > self.ring_end {
            self.wrap();
        }
        let needed = (self.index(self.ptr) + length + WRITE_AHEAD).min(self.ring_end);
        while self.cleared < needed {
            let step = CLEAR_STEP.min(self.ring_end - self.cleared);
            if self.map.writable(self.base, self.cleared, self.protect) < step {
                self.blocked = true;
                return false;
            }
            self.map.overwrite(self.base, self.cleared, step);
            self.cleared += step;
        }
        self.update_fast_end();
        true
    }

    /// Goes back to the start of the ring, with the history in front of it.
    fn wrap(&mut self) {
        self.record();
        let index = self.index(self.ptr);
        unsafe { std::ptr::copy(self.at(index - HISTORY), self.base.as_ptr(), HISTORY) };
        self.ptr = self.at(HISTORY);
        self.run_ptr = self.ptr;
        self.cleared = HISTORY;
    }

    fn finish_member(&mut self) -> OutStreamResult {
        self.record();
        let result = OutStreamResult {
            written: self.written,
            crc32: std::mem::replace(&mut self.crc, crc32fast::Hasher::new()).finalize(),
        };
        self.written = 0;
        result
    }
}

impl Drop for GzipRing {
    fn drop(&mut self) {
        drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(self.base.as_ptr(), self.len)) });
    }
}

/// The ring as the decoder's output, recording the matches (`Tracked`) or not, in which case
/// the decoding loop is the stock one.
struct Tracked<'a>(&'a mut GzipRing);
struct Untracked<'a>(&'a mut GzipRing);

macro_rules! ring_output {
    ($output:ident, $track:expr) => {
        impl DeflateOutput for $output<'_> {
            const TRACK_LZ: bool = $track;

            #[inline(always)]
            fn has_writable_length(&mut self, length: usize) -> bool {
                self.0.has_writable_length(length)
            }

            #[inline(always)]
            fn get_output_ptr(&mut self) -> *mut u8 {
                self.0.ptr
            }

            #[inline(always)]
            unsafe fn set_output_ptr(&mut self, ptr: *mut u8) {
                self.0.ptr = ptr;
            }

            // The bytes are read out of the ring, never handed over.
            fn pending_output(&self) -> &[u8] {
                &[]
            }

            fn consume_output(&mut self, _consumed_offset: usize) {}

            fn increase_buffer_size(&mut self, _additional: usize) {}

            fn finish_member(&mut self) -> Result<OutStreamResult, ()> {
                Ok(self.0.finish_member())
            }

            #[inline(always)]
            fn on_lz_copy(&mut self, dst: *const u8, distance: usize, len: usize) {
                let ring = &mut *self.0;
                if len as u32 >= ring.min_len {
                    let destination = ring.run_start + (dst as usize - ring.run_ptr as usize) as u64;
                    ring.copybacks.push(destination, distance as u32, len as u32);
                }
            }
        }
    };
}

ring_output!(Tracked, true);
ring_output!(Untracked, false);

/// Gzip from any reader, decoded in place: see [`InPlaceDecoder`]. Concatenated members are
/// decoded one after the other, as a concatenation of gzip files has them. A match is
/// recorded as it is decoded, before its bytes are readable.
pub struct GzipInPlaceDecoder<R> {
    source: R,
    input: DeflateChunkedBufferInput<'static>,
    tables: LibdeflateDecodeTables,
    gzip: LibdeflateGzipDecompressor,
    ring: GzipRing,
    retain: usize,
    total_in: u64,
    finished: bool,
}

impl<R: Read> GzipInPlaceDecoder<R> {
    /// Decodes `source`, keeping the last `retain` bytes readable (see [`InPlaceDecoder`]).
    pub fn new(source: R, retain: usize) -> Self {
        Self {
            source,
            input: DeflateChunkedBufferInput::new_push(IN_PLACE_INPUT_SIZE),
            tables: libdeflate_alloc_decode_tables(),
            gzip: LibdeflateGzipDecompressor::new(),
            ring: GzipRing::new(retain),
            retain,
            total_in: 0,
            finished: false,
        }
    }

    /// The underlying reader.
    pub fn get_ref(&self) -> &R {
        &self.source
    }

    /// Decodes until the ring is full up to its limit (`MoreData`), the input is used up
    /// (`NeedInput`) or the stream ends, going on into the members that follow.
    fn step(&mut self) -> io::Result<LibdeflateDecompressResult> {
        loop {
            let ring = &mut self.ring;
            let result = if ring.copybacks.tracking {
                ring.min_len = ring.copybacks.min_len.max(1);
                self.gzip.decompress(&mut self.tables, &mut self.input, &mut Tracked(ring))
            } else {
                self.gzip.decompress(&mut self.tables, &mut self.input, &mut Untracked(ring))
            };
            match result.map_err(|error| io::Error::new(ErrorKind::InvalidData, format!("gzip: {error:?}")))? {
                LibdeflateDecompressResult::EndOfStream => {
                    // Another member follows if there is any input left.
                    if !self.input.ensure_available(1) {
                        if !self.input.is_eof() {
                            return Ok(LibdeflateDecompressResult::NeedInput);
                        }
                        return Ok(LibdeflateDecompressResult::EndOfStream);
                    }
                    self.gzip = LibdeflateGzipDecompressor::new();
                }
                result => return Ok(result),
            }
        }
    }

    /// Reads more compressed bytes, or marks the end of the input.
    fn read_input(&mut self) -> io::Result<()> {
        let space = self.input.input_space_mut();
        if space.is_empty() {
            return Err(io::Error::other("gzip: the input buffer is full"));
        }
        let len = space.len().min(IN_PLACE_READ_SIZE);
        let count = loop {
            match self.source.read(&mut space[..len]) {
                Ok(count) => break count,
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        };
        if count == 0 {
            self.input.finish_input();
        } else {
            self.input.commit_input(count);
            self.total_in += count as u64;
        }
        Ok(())
    }
}

impl<R: Read> InPlaceDecoder for GzipInPlaceDecoder<R> {
    fn decode_to(&mut self, target: u64) -> io::Result<u64> {
        self.ring.set_bounds(target, protected(target, self.retain));
        while !self.finished && self.ring.decoded() < target {
            self.ring.blocked = false;
            match self.step()? {
                LibdeflateDecompressResult::MoreData => {
                    if self.ring.blocked {
                        return Err(ring_too_small("gzip"));
                    }
                }
                LibdeflateDecompressResult::NeedInput => self.read_input()?,
                LibdeflateDecompressResult::EndOfStream => self.finished = true,
            }
        }
        self.ring.record();
        Ok(self.ring.map.end())
    }

    fn decoded_len(&self) -> u64 {
        self.ring.map.end()
    }

    fn is_finished(&self) -> bool {
        self.finished
    }

    fn bytes(&self, range: Range<u64>) -> Pieces<'_> {
        self.ring.map.pieces(range)
    }

    fn bytes_and_copybacks(&mut self, range: Range<u64>) -> (Pieces<'_>, &mut VecDeque<Copyback>) {
        (self.ring.map.pieces(range), &mut self.ring.copybacks.queue)
    }

    fn peek_copybacks(&mut self) -> &mut VecDeque<Copyback> {
        &mut self.ring.copybacks.queue
    }

    fn set_copybacks_tracking(&mut self, enabled: bool) {
        self.ring.copybacks.tracking = enabled;
    }

    fn set_copybacks_min_len(&mut self, min_len: u32) {
        self.ring.copybacks.min_len = min_len;
    }

    fn total_in(&self) -> u64 {
        self.total_in
    }
}
