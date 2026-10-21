//! Decoding in place: the decoders leave their output in a ring buffer of their own, where it
//! is read without being copied out.

use crate::Copyback;
use std::collections::VecDeque;
use std::io;
use std::ops::Range;
use std::ptr::NonNull;

/// A decoder that decodes into a ring buffer of its own, and hands its output out of it
/// rather than copying it into the caller's buffer. The ring keeps the last bytes decoded: a
/// decoder created with `retain` keeps, after [`Self::decode_to`]`(target)`, every byte from
/// `target - retain` on readable with [`Self::bytes`], until the next `decode_to`; with
/// [`Self::retain_window`], from `target - retain - window_size()` on.
///
/// The decoders of the crate: [`crate::xz::XzInPlaceDecoder`],
/// [`crate::zstd::ZstdInPlaceDecoder`], [`crate::lz4::Lz4InPlaceDecoder`] and
/// [`crate::gzip::GzipInPlaceDecoder`]. Their copybacks do not depend on how far each
/// `decode_to` goes.
pub trait InPlaceDecoder {
    /// Decodes until at least `target` bytes were decoded in all, or the stream ends, and
    /// returns how many were: `target` or a block more at most, fewer only at the end of the
    /// stream.
    fn decode_to(&mut self, target: u64) -> io::Result<u64>;

    /// Bytes decoded so far.
    fn decoded_len(&self) -> u64;

    /// Whether the stream has ended: no byte will be decoded anymore.
    fn is_finished(&self) -> bool;

    /// The decoded bytes of `range`, in order, in the pieces the ring holds them in: more
    /// than one where they wrap around its end.
    ///
    /// # Panics
    ///
    /// If `range` is not readable: see [`Self::decode_to`].
    fn bytes(&self, range: Range<u64>) -> Pieces<'_>;

    /// [`Self::bytes`] and [`Self::peek_copybacks`] at once.
    fn bytes_and_copybacks(&mut self, range: Range<u64>)
    -> (Pieces<'_>, &mut VecDeque<Copyback>);

    /// Every copyback recorded so far, in stream order. The decoder only pushes to the back
    /// of the queue, or extends its last entry with a match that continues it, at the same
    /// offset, from where it ends: a long repeat cut into matches comes as one entry, as long
    /// as the last one stays in the queue (the rest of one the caller removed comes as an entry
    /// of its own, however short). Removing entries is up to the caller.
    fn peek_copybacks(&mut self) -> &mut VecDeque<Copyback>;

    /// Turns the recording of LZ matches on or off; it is on when the decoder is created.
    fn set_copybacks_tracking(&mut self, enabled: bool);

    /// Copybacks shorter than `min_len` bytes, their continuations merged, are not recorded; 1
    /// when the decoder is created. A copyback enters the queue once it is `min_len` long,
    /// until then a match that does not continue it drops it. The decoder leaves out the
    /// matches shorter than the hard threshold it was created with, `hard_min_len`, or
    /// `min_len` if shorter, but for those that continue the last one reported.
    fn set_copybacks_min_len(&mut self, min_len: u32);

    /// Compressed bytes taken from the input so far.
    fn total_in(&self) -> u64;

    /// How far back a match of the stream being decoded can reach at most, as its format or
    /// header says: the dictionary size of the xz Block (or `.lzma` stream), the window of the
    /// zstd frame, 32 KiB for gzip, 64 KiB for lz4. The xz and zstd ones are 0 until the first
    /// header that declares one is decoded, and change with each Block or frame. A caller that
    /// decodes this far past the bytes it reads knows, along with them, every match that copies
    /// them.
    fn window_size(&self) -> u64;

    /// Keeps the window readable too: after [`Self::decode_to`]`(target)`, every byte from
    /// `target - retain - window_size()` on, the window of the Block or frame being decoded when
    /// `decode_to` starts. To be called before the first byte is decoded.
    ///
    /// # Panics
    ///
    /// If a byte was decoded already.
    fn retain_window(&mut self);
}

/// The pieces of a range of decoded bytes, in order: see [`InPlaceDecoder::bytes`].
pub struct Pieces<'a> {
    runs: std::collections::vec_deque::Iter<'a, Run>,
    range: Range<u64>,
}

impl<'a> Iterator for Pieces<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        while self.range.start < self.range.end {
            let run = self.runs.next()?;
            let run_end = run.start + run.len as u64;
            if run_end <= self.range.start {
                continue;
            }
            let skip = (self.range.start - run.start) as usize;
            let len = (run_end.min(self.range.end) - self.range.start) as usize;
            self.range.start += len as u64;
            // SAFETY: the run is readable, so its buffer is alive and the bytes there are the
            // decoded ones; the pieces borrow the decoder, which only writes when mutated.
            return Some(unsafe { std::slice::from_raw_parts(run.base.as_ptr().add(run.index + skip), len) });
        }
        None
    }
}

/// Bytes decoded one after the other into a buffer: from stream offset `start`, `len` bytes at
/// `index` of the buffer at `base`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Run {
    pub(crate) start: u64,
    pub(crate) base: NonNull<u8>,
    pub(crate) index: usize,
    pub(crate) len: usize,
}

/// Where the decoded bytes that are still readable are: runs of them, oldest first, in the
/// buffers of a decoder. What is overwritten is forgotten together with everything older, so
/// the readable bytes are always the last ones decoded.
#[derive(Default)]
pub(crate) struct RingMap {
    runs: VecDeque<Run>,
    /// Offset of the byte after the last one decoded.
    end: u64,
}

// SAFETY: the runs point into buffers owned by the decoder that owns the map, which moves
// along with them.
unsafe impl Send for RingMap {}
unsafe impl Sync for RingMap {}

impl RingMap {
    pub(crate) fn end(&self) -> u64 {
        self.end
    }

    /// Offset of the first readable byte.
    pub(crate) fn start(&self) -> u64 {
        self.runs.front().map_or(self.end, |run| run.start)
    }

    /// Records the next `len` decoded bytes, at `index` of the buffer at `base`.
    pub(crate) fn push(&mut self, base: NonNull<u8>, index: usize, len: usize) {
        if len == 0 {
            return;
        }
        if let Some(last) = self.runs.back_mut() {
            if last.base == base && last.index + last.len == index {
                last.len += len;
                self.end += len as u64;
                return;
            }
        }
        self.runs.push_back(Run { start: self.end, base, index, len });
        self.end += len as u64;
    }

    /// How many bytes can be written at `index` of the buffer at `base` (and on) without
    /// overwriting a readable byte from offset `protect` on: `usize::MAX` if any number.
    pub(crate) fn writable(&self, base: NonNull<u8>, index: usize, protect: u64) -> usize {
        let mut writable = usize::MAX;
        for run in self.runs.iter().rev() {
            let run_end = run.start + run.len as u64;
            if run_end <= protect {
                break;
            }
            if run.base != base {
                continue;
            }
            let skip = protect.saturating_sub(run.start) as usize;
            let (low, high) = (run.index + skip, run.index + run.len);
            if high <= index {
                continue;
            }
            writable = writable.min(low.saturating_sub(index));
        }
        writable
    }

    /// Forgets the bytes that writing `len` bytes at `index` of the buffer at `base`
    /// overwrites, and every byte decoded before them.
    pub(crate) fn overwrite(&mut self, base: NonNull<u8>, index: usize, len: usize) {
        let end = index + len;
        let Some(position) = self
            .runs
            .iter()
            .rposition(|run| run.base == base && run.index < end && index < run.index + run.len)
        else {
            return;
        };
        self.runs.drain(..position);
        let run = &mut self.runs[0];
        // What the run has after the overwritten bytes stays readable.
        let kept = (run.index + run.len).saturating_sub(end.max(run.index));
        if kept == 0 {
            self.runs.pop_front();
        } else {
            let dropped = run.len - kept;
            run.start += dropped as u64;
            run.index += dropped;
            run.len = kept;
        }
    }

    /// Whether any readable byte is in the buffer at `base`.
    pub(crate) fn uses(&self, base: NonNull<u8>) -> bool {
        self.runs.iter().any(|run| run.base == base)
    }

    /// The bytes of `range`, which must be readable.
    pub(crate) fn pieces(&self, range: Range<u64>) -> Pieces<'_> {
        assert!(
            self.start() <= range.start && range.start <= range.end && range.end <= self.end,
            "bytes {range:?} are not readable, only {:?} are",
            self.start()..self.end
        );
        let first = self.runs.partition_point(|run| run.start + run.len as u64 <= range.start);
        Pieces { runs: self.runs.range(first..), range }
    }
}

/// The protected offset of a `decode_to(target)`: the bytes from there on must stay readable.
pub(crate) fn protected(target: u64, retain: usize) -> u64 {
    target.saturating_sub(retain as u64)
}

/// The error of a ring that cannot keep the bytes it must while decoding on.
pub(crate) fn ring_too_small(codec: &str) -> io::Error {
    io::Error::other(format!("{codec}: the ring buffer cannot keep the bytes to retain"))
}
