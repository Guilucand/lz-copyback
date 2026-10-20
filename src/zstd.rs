//! zstd decoding in place, over the patched libzstd in `external/zstd-sys` and its
//! buffer-less API.

use crate::copybacks::{copyback_hook, Copybacks};
use crate::in_place::{protected, ring_too_small, InPlaceDecoder, Pieces, RingMap};
use crate::Copyback;
use lzcb_zstd_sys as ffi;
use std::collections::VecDeque;
use std::ffi::{c_void, CStr};
use std::io::{self, BufRead, BufReader, ErrorKind, Read};
use std::ops::Range;
use std::ptr::{self, NonNull};

/// The zstd decompression context, freed on drop.
struct Context(NonNull<ffi::ZSTD_DCtx>);

// SAFETY: the context is owned here and used from one thread at a time.
unsafe impl Send for Context {}
unsafe impl Sync for Context {}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe { ffi::ZSTD_freeDCtx(self.0.as_ptr()) };
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

/// The largest window accepted, as zstd's own decoder does unless told otherwise
/// (`ZSTD_WINDOWLOG_LIMIT_DEFAULT`).
const IN_PLACE_MAX_WINDOW: u64 = 1 << 27;

/// How much past a block zstd may write, beyond its decoded bytes (`WILDCOPY_OVERLENGTH`, with
/// a margin).
const BLOCK_SLACK: usize = 64;

/// zstd from any reader, decoded in place with zstd's buffer-less API: each block is decoded
/// straight into a ring buffer, see [`InPlaceDecoder`]. Frames are decoded one after the
/// other until the end of the input, as zstd's own reader does, and an input that ends inside
/// a frame is an error. A match is recorded when its block is decoded, at most one block
/// (128 KiB) ahead of the bytes asked for.
pub struct ZstdInPlaceDecoder<R> {
    reader: BufReader<R>,
    context: Context,
    copybacks: Copybacks,
    /// The ring, `ring_len` bytes, written at `write`; replaced by a larger one when a frame
    /// needs it, the old one kept in `retired` while it holds readable bytes.
    ring: NonNull<u8>,
    ring_len: usize,
    write: usize,
    retired: Vec<(NonNull<u8>, usize)>,
    map: RingMap,
    /// The input of the next decoding step, and the frame header read so far.
    input: Vec<u8>,
    header: Vec<u8>,
    /// The largest block of the frame being decoded, 0 between frames.
    block_max: usize,
    in_frame: bool,
    /// Whether the last frame was decoded to its end: the input can end only then.
    finished_frame: bool,
    finished: bool,
    retain: usize,
    total_in: u64,
}

// SAFETY: the pointers point into buffers the decoder owns.
unsafe impl<R: Send> Send for ZstdInPlaceDecoder<R> {}

/// A zeroed buffer of `len` bytes, owned through a raw pointer.
fn alloc_ring(len: usize) -> NonNull<u8> {
    let buffer: Box<[u8]> = vec![0; len].into_boxed_slice();
    NonNull::new(Box::into_raw(buffer) as *mut u8).unwrap()
}

fn free_ring(ring: NonNull<u8>, len: usize) {
    drop(unsafe { Box::from_raw(ptr::slice_from_raw_parts_mut(ring.as_ptr(), len)) });
}

impl<R: Read> ZstdInPlaceDecoder<R> {
    /// Decodes `reader`, keeping the last `retain` bytes readable (see [`InPlaceDecoder`]).
    pub fn new(reader: R, retain: usize) -> io::Result<Self> {
        let context = NonNull::new(unsafe { ffi::ZSTD_createDCtx() })
            .map(Context)
            .ok_or_else(|| io::Error::new(ErrorKind::OutOfMemory, "zstd: cannot create a context"))?;
        Ok(Self {
            reader: BufReader::with_capacity(unsafe { ffi::ZSTD_DStreamInSize() }, reader),
            context,
            copybacks: Copybacks::new(),
            ring: NonNull::dangling(),
            ring_len: 0,
            write: 0,
            retired: Vec::new(),
            map: RingMap::default(),
            input: Vec::new(),
            header: Vec::new(),
            block_max: 0,
            in_frame: false,
            finished_frame: false,
            finished: false,
            retain,
            total_in: 0,
        })
    }

    /// The underlying reader.
    pub fn get_ref(&self) -> &R {
        self.reader.get_ref()
    }

    /// Reads exactly `len` input bytes into `self.input`; false if the input ends first.
    fn read_input(&mut self, len: usize) -> io::Result<bool> {
        self.input.clear();
        while self.input.len() < len {
            let available = self.reader.fill_buf()?;
            if available.is_empty() {
                return Ok(false);
            }
            let count = available.len().min(len - self.input.len());
            self.input.extend_from_slice(&available[..count]);
            self.reader.consume(count);
        }
        self.total_in += len as u64;
        Ok(true)
    }

    /// Makes the ring large enough for the frame whose header is `self.header`: its window
    /// and its largest block, plus the bytes to retain.
    fn prepare_frame(&mut self) -> io::Result<()> {
        let mut frame = ffi::ZSTD_FrameHeader::default();
        check(unsafe {
            ffi::ZSTD_getFrameHeader(&mut frame, self.header.as_ptr() as *const c_void, self.header.len())
        })?;
        if frame.frameType == ffi::ZSTD_skippableFrame {
            self.block_max = 0;
            return Ok(());
        }
        if frame.windowSize > IN_PLACE_MAX_WINDOW {
            return Err(io::Error::other("Frame requires too much memory for decoding"));
        }
        self.block_max = frame.blockSizeMax as usize;
        let minimum = unsafe { ffi::ZSTD_decodingBufferSize_min(frame.windowSize, frame.frameContentSize) };
        check(minimum)?;
        // The retained bytes, a block decoded past them, and a block of room to decode the
        // next one, besides what is wasted at the end of the ring when it wraps.
        let needed = minimum.max(self.retain + 3 * (self.block_max + BLOCK_SLACK));
        if needed > self.ring_len {
            if self.ring_len > 0 {
                self.retired.push((self.ring, self.ring_len));
            }
            self.ring = alloc_ring(needed);
            self.ring_len = needed;
            self.write = 0;
        }
        Ok(())
    }

    /// Frees the retired rings that hold no readable byte anymore.
    fn free_retired(&mut self) {
        let map = &self.map;
        self.retired.retain(|&(ring, len)| {
            let used = map.uses(ring);
            if !used {
                free_ring(ring, len);
            }
            used
        });
    }

    /// Runs one decoding step: a frame header, a block (decoded into the ring) or a
    /// checksum. False at the end of the input.
    fn step(&mut self, protect: u64) -> io::Result<bool> {
        let dctx = self.context.0.as_ptr();
        if !self.in_frame {
            if self.reader.fill_buf()?.is_empty() {
                if !self.finished_frame {
                    return Err(io::Error::new(ErrorKind::UnexpectedEof, "incomplete frame"));
                }
                return Ok(false);
            }
            check(unsafe { ffi::ZSTD_decompressBegin(dctx) })?;
            self.in_frame = true;
            self.finished_frame = false;
            self.header.clear();
            self.block_max = 0;
        }

        let size = unsafe { ffi::ZSTD_nextSrcSizeToDecompress(dctx) };
        if size == 0 {
            self.in_frame = false;
            self.finished_frame = true;
            return Ok(true);
        }
        let kind = unsafe { ffi::ZSTD_nextInputType(dctx) };
        if !self.read_input(size)? {
            return Err(io::Error::new(ErrorKind::UnexpectedEof, "incomplete frame"));
        }
        let is_header = kind == ffi::ZSTDnit_frameHeader;
        if is_header {
            self.header.extend_from_slice(&self.input);
        }

        // A block needs room for the largest one; the other steps write nothing.
        let is_block = kind == ffi::ZSTDnit_block || kind == ffi::ZSTDnit_lastBlock;
        if is_block {
            let room = self.block_max + BLOCK_SLACK;
            if self.write + room > self.ring_len {
                self.write = 0;
            }
            if self.map.writable(self.ring, self.write, protect) < room {
                return Err(ring_too_small("zstd"));
            }
            self.map.overwrite(self.ring, self.write, room);
        }

        let track = self.copybacks.tracking;
        if track {
            let min_len = self.copybacks.min_len;
            let ctx = self.copybacks.hook_ctx();
            unsafe { ffi::ZSTD_DCtx_setLzMatchHook(dctx, Some(copyback_hook), ctx, min_len) };
        }
        let (dst, capacity) = if self.ring_len > 0 {
            (unsafe { self.ring.as_ptr().add(self.write) }, self.ring_len - self.write)
        } else {
            (ptr::null_mut(), 0)
        };
        let decoded = unsafe {
            ffi::ZSTD_decompressContinue(
                dctx,
                dst as *mut c_void,
                capacity,
                self.input.as_ptr() as *const c_void,
                self.input.len(),
            )
        };
        if track {
            unsafe { ffi::ZSTD_DCtx_setLzMatchHook(dctx, None, ptr::null_mut(), 0) };
        }
        let decoded = check(decoded)?;
        self.map.push(self.ring, self.write, decoded);
        self.write += decoded;

        // The header is complete once zstd moves on: the ring can be sized for the frame.
        if is_header && unsafe { ffi::ZSTD_nextInputType(dctx) } != kind {
            self.prepare_frame()?;
        }
        Ok(true)
    }
}

impl<R: Read> InPlaceDecoder for ZstdInPlaceDecoder<R> {
    fn decode_to(&mut self, target: u64) -> io::Result<u64> {
        let protect = protected(target, self.retain);
        while !self.finished && self.map.end() < target {
            if !self.step(protect)? {
                self.finished = true;
            }
        }
        self.free_retired();
        Ok(self.map.end())
    }

    fn decoded_len(&self) -> u64 {
        self.map.end()
    }

    fn is_finished(&self) -> bool {
        self.finished
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
}

impl<R> Drop for ZstdInPlaceDecoder<R> {
    fn drop(&mut self) {
        if self.ring_len > 0 {
            free_ring(self.ring, self.ring_len);
        }
        for &(ring, len) in &self.retired {
            free_ring(ring, len);
        }
    }
}
