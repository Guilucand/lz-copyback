//! xz decoding in place, over the patched liblzma in `external/lzma-sys`.

use crate::copybacks::{copyback_hook, Copybacks};
use crate::in_place::{protected, ring_too_small, InPlaceDecoder, Pieces, RingMap};
use crate::Copyback;
use lzcb_lzma_sys as ffi;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::io::{self, BufRead, BufReader, ErrorKind, Read};
use std::ops::Range;
use std::ptr::NonNull;
use std::{mem, ptr};

const INPUT_BUFFER_SIZE: usize = 1 << 17;

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

/// How far past the bytes it is asked for liblzma may write into the dictionary: what
/// `dict_repeat()` copies beyond its limit, and the zero a reset writes in front of the
/// restart.
const DICT_SLACK: usize = 64;

/// How much larger than the bytes to retain the dictionary is at least: room for the gaps
/// resets leave between blocks and for decoding ahead.
const DICT_MARGIN: usize = 1 << 20;

/// An xz decoder over a [`Read`] that decodes in place: liblzma's own dictionary is the ring
/// the output is read from (see [`InPlaceDecoder`]), with the patched liblzma skipping its
/// copy out of it. The dictionary is at least as large as the bytes to retain, which resets
/// leave in place.
///
/// A match is recorded, whole, during the `decode_to` call that decodes its first byte. The
/// output of a Block whose filter chain is not a lone LZMA2 (BCJ, delta) is not in the
/// dictionary, so it is decoded, copied, into a ring of the decoder's own; such Blocks record
/// no matches either, as those filters rewrite the LZ output. Such chains are only written on
/// request (`xz --x86`, `--delta`, `--filters=`).
pub struct XzInPlaceDecoder<R> {
    reader: BufReader<R>,
    stream: Stream,
    copybacks: Copybacks,
    /// The in-place state liblzma reads and updates, at a stable address.
    state: Box<ffi::lzma_lz_in_place>,
    /// The dictionaries liblzma handed over, kept while they hold readable bytes.
    retired: Box<Vec<NonNull<u8>>>,
    map: RingMap,
    /// The ring of the filtered Blocks, allocated with the first one, written at
    /// `spill_write`; the ones it replaced, kept while they hold readable bytes.
    spill: Option<(NonNull<u8>, usize)>,
    spill_write: usize,
    old_spills: Vec<(NonNull<u8>, usize)>,
    retain: usize,
    finished: bool,
    /// Consecutive decoding calls that made no progress.
    stalls: u32,
}

// SAFETY: the pointers point into buffers the decoder owns (the dictionaries through
// liblzma), and liblzma's thread-local state is only set for the length of a call.
unsafe impl<R: Send> Send for XzInPlaceDecoder<R> {}

/// liblzma hands over a dictionary it no longer uses.
unsafe extern "C" fn retire(ctx: *mut c_void, buf: *mut u8, _size: usize) {
    let retired = unsafe { &mut *(ctx as *mut Vec<NonNull<u8>>) };
    retired.extend(NonNull::new(buf));
}

impl<R: Read> XzInPlaceDecoder<R> {
    /// Decodes one .xz stream, keeping the last `retain` bytes readable. `hard_min_len` is the hard threshold: the decoder itself leaves out the matches
    /// shorter than it, or than the copybacks' `min_len` if shorter, but for those continuing
    /// the last one reported (see [`InPlaceDecoder::set_copybacks_min_len`]).
    pub fn new(reader: R, retain: usize, hard_min_len: u32) -> Self {
        Self::with_decoder(reader, retain, hard_min_len, |stream| unsafe {
            ffi::lzma_stream_decoder(stream, u64::MAX, 0)
        })
    }

    /// Decodes .xz streams one after the other until the end of the input, and also `.lzma`
    /// and `.lz` input, keeping the last `retain` bytes readable; as [`Self::new`] for
    /// `hard_min_len`.
    pub fn new_multi_decoder(reader: R, retain: usize, hard_min_len: u32) -> Self {
        Self::with_decoder(reader, retain, hard_min_len, |stream| unsafe {
            ffi::lzma_auto_decoder(stream, u64::MAX, ffi::LZMA_CONCATENATED)
        })
    }

    fn with_decoder(
        reader: R,
        retain: usize,
        hard_min_len: u32,
        init: impl FnOnce(*mut ffi::lzma_stream) -> ffi::lzma_ret,
    ) -> Self {
        let mut stream = Stream(unsafe { mem::zeroed() });
        let ret = init(&mut stream.0);
        assert_eq!(ret, ffi::LZMA_OK, "liblzma could not create a decoder (lzma_ret {ret})");
        let mut retired = Box::new(Vec::new());
        let state = Box::new(ffi::lzma_lz_in_place {
            min_size: retain + DICT_MARGIN,
            min_size_plus_dict: false,
            dict_size: 0,
            buf: ptr::null_mut(),
            size: 0,
            next: 0,
            filtered: false,
            retire: Some(retire),
            retire_ctx: &mut *retired as *mut Vec<NonNull<u8>> as *mut c_void,
        });
        XzInPlaceDecoder {
            reader: BufReader::with_capacity(INPUT_BUFFER_SIZE, reader),
            stream,
            copybacks: Copybacks::new(hard_min_len),
            state,
            retired,
            map: RingMap::default(),
            spill: None,
            spill_write: 0,
            old_spills: Vec::new(),
            retain,
            finished: false,
            stalls: 0,
        }
    }
}

impl<R> XzInPlaceDecoder<R> {
    /// The underlying reader.
    pub fn get_ref(&self) -> &R {
        self.reader.get_ref()
    }

    /// The bytes kept readable behind the target of a `decode_to`: the window too, with
    /// [`InPlaceDecoder::retain_window`].
    fn retained(&self) -> usize {
        self.retain + if self.state.min_size_plus_dict { self.state.dict_size } else { 0 }
    }

    /// The ring of the filtered Blocks, as large as a dictionary would be: replaced by a larger
    /// one when the window to retain grows.
    fn spill(&mut self) -> (NonNull<u8>, usize) {
        let needed = self.retained() + DICT_MARGIN;
        match self.spill {
            Some(spill) if spill.1 >= needed => spill,
            old => {
                self.old_spills.extend(old);
                let buffer: Box<[u8]> = vec![0; needed].into_boxed_slice();
                let spill = (NonNull::new(Box::into_raw(buffer) as *mut u8).unwrap(), needed);
                self.spill = Some(spill);
                self.spill_write = 0;
                spill
            }
        }
    }

    /// Frees the dictionaries liblzma handed over, and the spill rings replaced, that hold no
    /// readable byte anymore.
    fn free_retired(&mut self) {
        let map = &self.map;
        self.retired.retain(|&buf| {
            let used = map.uses(buf);
            if !used {
                unsafe { ffi::lzma_lz_in_place_free(buf.as_ptr()) };
            }
            used
        });
        self.old_spills.retain(|&(spill, len)| {
            let used = map.uses(spill);
            if !used {
                free_spill(spill, len);
            }
            used
        });
    }
}

impl<R: Read> XzInPlaceDecoder<R> {
    /// One `lzma_code` call, decoding at most up to `target`: into the dictionary at the
    /// position liblzma reported, or for a filtered Block, into the spill ring.
    fn step(&mut self, target: u64, protect: u64) -> io::Result<()> {
        let want = usize::try_from(target - self.map.end()).unwrap_or(usize::MAX);
        let dictionary = NonNull::new(self.state.buf).filter(|_| !self.state.filtered);
        let (base, index, avail, slack) = if let Some(buf) = dictionary {
            let index = self.state.next;
            let writable = self.map.writable(buf, index, protect).saturating_sub(DICT_SLACK);
            let avail = (self.state.size - index).min(want).min(writable);
            (buf, index, avail, DICT_SLACK)
        } else {
            // A filtered Block, or no dictionary yet: then some decoders read their headers
            // only with room for output, which liblzma decodes into the dictionary once it
            // exists, and nothing here (the position does not match), unless the first Block
            // is filtered.
            let (spill, len) = self.spill();
            if self.spill_write == len {
                self.spill_write = 0;
            }
            let index = self.spill_write;
            let mut avail = (len - index).min(want).min(self.map.writable(spill, index, protect));
            if self.state.buf.is_null() {
                avail = avail.min(4096);
            }
            (spill, index, avail, 0)
        };
        if avail == 0 {
            return Err(ring_too_small("xz"));
        }
        self.map.overwrite(base, index, avail + slack);

        let input = self.reader.fill_buf()?;
        let eof = input.is_empty();
        let stream = &mut self.stream.0;
        let (before_in, before_out) = (stream.total_in, stream.total_out);
        stream.next_in = input.as_ptr();
        stream.avail_in = input.len();
        stream.next_out = unsafe { base.as_ptr().add(index) };
        stream.avail_out = avail;

        let track = self.copybacks.tracking;
        if track {
            let (min_len, last_end, last_offset) =
                (self.copybacks.hard_min_len(), self.copybacks.last_end, self.copybacks.last_offset);
            let ctx = self.copybacks.hook_ctx();
            unsafe {
                ffi::lzma_lz_set_match_hook(Some(copyback_hook), ctx, before_out, min_len, last_end, last_offset)
            };
        }
        unsafe { ffi::lzma_lz_set_in_place(&mut *self.state) };
        let ret = unsafe { ffi::lzma_code(stream, if eof { ffi::LZMA_FINISH } else { ffi::LZMA_RUN }) };
        unsafe { ffi::lzma_lz_set_in_place(ptr::null_mut()) };
        if track {
            unsafe { ffi::lzma_lz_set_match_hook(None, ptr::null_mut(), 0, 0, u64::MAX, 0) };
        }
        let produced = (stream.total_out - before_out) as usize;
        let consumed = (stream.total_in - before_in) as usize;
        stream.next_in = ptr::null();
        stream.avail_in = 0;
        stream.next_out = ptr::null_mut();
        stream.avail_out = 0;
        self.reader.consume(consumed);

        self.map.push(base, index, produced);
        if self.spill.is_some_and(|(spill, _)| spill == base) {
            self.spill_write += produced;
        }
        if check(ret)? {
            self.finished = true;
            return Ok(());
        }
        // A call can make no progress when the position moved (liblzma decodes only at the
        // one it reports), never twice in a row.
        if produced == 0 && consumed == 0 {
            self.stalls += 1;
            if self.stalls >= 2 {
                return Err(if eof {
                    io::Error::new(ErrorKind::UnexpectedEof, "premature eof")
                } else {
                    io::Error::new(ErrorKind::InvalidData, "corrupt xz stream")
                });
            }
        } else {
            self.stalls = 0;
        }
        Ok(())
    }
}

impl<R: Read> InPlaceDecoder for XzInPlaceDecoder<R> {
    fn decode_to(&mut self, target: u64) -> io::Result<u64> {
        let protect = protected(target, self.retained());
        while !self.finished && self.map.end() < target {
            self.step(target, protect)?;
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
        self.stream.0.total_in
    }

    fn window_size(&self) -> u64 {
        self.state.dict_size as u64
    }

    fn retain_window(&mut self) {
        assert_eq!(self.map.end(), 0, "xz: the window is retained from the first byte only");
        // liblzma allocates each dictionary the declared size larger than `min_size`.
        self.state.min_size_plus_dict = true;
    }
}

fn free_spill(spill: NonNull<u8>, len: usize) {
    drop(unsafe { Box::from_raw(ptr::slice_from_raw_parts_mut(spill.as_ptr(), len)) });
}

impl<R> Drop for XzInPlaceDecoder<R> {
    fn drop(&mut self) {
        for &buf in self.retired.iter() {
            unsafe { ffi::lzma_lz_in_place_free(buf.as_ptr()) };
        }
        for (spill, len) in self.spill.into_iter().chain(self.old_spills.drain(..)) {
            free_spill(spill, len);
        }
    }
}
