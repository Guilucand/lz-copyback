//! What the fuzz targets share: the decoders by codec, fuzzer bytes read as parameters, a reader
//! that checks the decoders' guarantees as it reads, the stock decoders, and data with repeats.
#![allow(dead_code)]

use lz_copyback::gzip::GzipInPlaceDecoder;
use lz_copyback::lz4::Lz4InPlaceDecoder;
use lz_copyback::xz::XzInPlaceDecoder;
use lz_copyback::zstd::ZstdInPlaceDecoder;
use lz_copyback::{Copyback, InPlaceDecoder};
use std::io::{self, Read};

/// The most output decoded from one input: compression bombs decode to far more.
pub const LIMIT: u64 = 16 << 20;

/// The most `decode_to` steps per input, so that tiny chunks do not time out.
pub const MAX_STEPS: u64 = 100_000;

/// How many parameter bytes the raw targets read before the compressed input.
pub const PARAMS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Xz,
    Zstd,
    Lz4,
    Gzip,
}

pub fn decoder(codec: Codec, encoded: &[u8], retain: usize) -> Option<Box<dyn InPlaceDecoder + '_>> {
    Some(match codec {
        Codec::Xz => Box::new(XzInPlaceDecoder::new_multi_decoder(encoded, retain)),
        Codec::Zstd => Box::new(ZstdInPlaceDecoder::new(encoded, retain).ok()?),
        Codec::Lz4 => Box::new(Lz4InPlaceDecoder::new(encoded, retain).ok()?),
        Codec::Gzip => Box::new(GzipInPlaceDecoder::new(encoded, retain)),
    })
}

/// Fuzzer bytes read as parameters, zeros once they run out.
pub struct Params<'a>(pub &'a [u8]);

impl Params<'_> {
    pub fn u8(&mut self) -> u8 {
        match self.0.split_first() {
            Some((&byte, rest)) => {
                self.0 = rest;
                byte
            }
            None => 0,
        }
    }

    pub fn u32(&mut self) -> u32 {
        u32::from_le_bytes([self.u8(), self.u8(), self.u8(), self.u8()])
    }

    pub fn bool(&mut self) -> bool {
        self.u8() & 1 == 1
    }

    /// A number below `bound`.
    pub fn below(&mut self, bound: u32) -> u32 {
        self.u32() % bound
    }

    /// A size whose base-2 logarithm is uniform up to `max_log`: tiny and large sizes alike.
    pub fn size(&mut self, max_log: u32) -> usize {
        let base = 1usize << self.below(max_log + 1);
        base + self.below(base as u32) as usize
    }
}

/// How a decoder is read: chunks of `chunk` bytes, `ahead` bytes decoded past each, `extra`
/// more bytes retained, the shortest match recorded, and whether the tracking starts on and
/// switches over at some point.
#[derive(Clone, Copy, Debug)]
pub struct Reading {
    pub chunk: usize,
    pub ahead: usize,
    pub extra: usize,
    pub min_len: u32,
    pub tracking: bool,
    pub switch_at: Option<u64>,
}

impl Reading {
    pub fn new(params: &mut Params) -> Self {
        let chunk = params.size(20);
        let ahead = if params.bool() { 0 } else { params.size(21) };
        let extra = if params.bool() { 0 } else { params.size(16) };
        let min_len = match params.below(4) {
            0 => 1,
            1 => 4,
            2 => 62,
            _ => 1 + params.below(300),
        };
        let tracking = params.below(8) != 0;
        let switch_at = params.bool().then(|| params.u32() as u64 % (4 << 20));
        Self { chunk, ahead, extra, min_len, tracking, switch_at }
    }

    pub fn retain(&self) -> usize {
        self.chunk + self.ahead + self.extra
    }

    /// The most output read with these chunks.
    pub fn limit(&self) -> u64 {
        LIMIT.min(self.chunk as u64 * MAX_STEPS)
    }
}

/// What a decoder produced: the output, the copybacks, and the error it ended with, if any.
pub struct Outcome {
    pub out: Vec<u8>,
    pub copybacks: Vec<Copyback>,
    pub error: Option<io::Error>,
}

/// How many bytes are compared again after every step, at the oldest retained offset and just
/// before the newest chunk.
const RECHECK: u64 = 256;

/// Reads `decoder` as a consumer of chunks does, up to `limit` bytes, and checks as it goes:
/// the decoded length only grows and is what `decode_to` returns, which stops short of the
/// target only at the end of the stream; the bytes decoded do not change while they are
/// retained (the oldest and the newest of them are read again after every step); every piece
/// is the next bytes; copybacks come no later than their bytes, in stream order, each a
/// correct raw match of at least the shortest length (see [`check_copybacks`] for `sourceless`).
pub fn read_checked(decoder: &mut dyn InPlaceDecoder, reading: &Reading, limit: u64, sourceless: bool) -> Outcome {
    let retain = reading.retain() as u64;
    let mut tracking = reading.tracking;
    decoder.set_copybacks_min_len(reading.min_len);
    decoder.set_copybacks_tracking(tracking);

    let mut out: Vec<u8> = Vec::new();
    let mut copybacks = Vec::new();
    let mut error = None;
    let mut decoded_before = 0;
    let mut switched = false;
    loop {
        let position = out.len() as u64;
        if position >= limit {
            break;
        }
        if let Some(at) = reading.switch_at.filter(|&at| !switched && position >= at) {
            let _ = at;
            tracking = !tracking;
            decoder.set_copybacks_tracking(tracking);
            switched = true;
        }
        let target = position + (reading.chunk + reading.ahead) as u64;
        let result = if error.is_none() { decoder.decode_to(target) } else { Ok(decoder.decoded_len()) };
        let decoded = decoder.decoded_len();
        assert!(decoded >= decoded_before, "the decoded length went back from {decoded_before} to {decoded}");
        decoded_before = decoded;
        match result {
            Ok(returned) if error.is_none() => {
                assert_eq!(returned, decoded, "decode_to returned {returned}, decoded_len {decoded}");
                if returned < target {
                    assert!(decoder.is_finished(), "stopped at {returned} before {target} unfinished");
                }
            }
            Ok(_) => {}
            Err(failure) => error = Some(failure),
        }

        // What was handed out and is still retained has not changed.
        let oldest = target.saturating_sub(retain).min(position);
        for (from, to) in [
            (oldest, (oldest + RECHECK).min(position)),
            (position.saturating_sub(RECHECK).max(oldest), position),
        ] {
            if from < to {
                let again: Vec<u8> = decoder.bytes(from..to).flatten().copied().collect();
                assert!(again == out[from as usize..to as usize], "the bytes at {from}..{to} changed while retained");
            }
        }

        let end = decoded.min(position + reading.chunk as u64).min(limit);
        let (pieces, queue) = decoder.bytes_and_copybacks(position..end);
        for piece in pieces {
            assert!(!piece.is_empty(), "an empty piece");
            out.extend_from_slice(piece);
        }
        assert_eq!(out.len() as u64, end, "the pieces of {position}..{end} are not that long");
        for copyback in queue.drain(..) {
            assert!(copyback.destination >= position, "{copyback:?} pushed after {position} bytes were handed out");
            copybacks.push(copyback);
        }
        if end == position {
            break;
        }
    }
    check_copybacks(&out, &copybacks, reading.min_len, sourceless);
    Outcome { out, copybacks, error }
}

/// The invariants of every list of raw matches: in stream order, destinations never
/// overlapping, each at least `min_len` bytes and a correct repeat of earlier output (byte by
/// byte, so an overlapping, periodic match is checked as it is decoded), as far as `out` goes.
/// With `sourceless`, a match may reach before the start of the output: streaming-libdeflate-rs
/// takes a corrupt deflate distance reaching before the stream's start, a known and accepted
/// behaviour (it copies zeros, within its buffers), which the gzip trailer's checksum rejects.
pub fn check_copybacks(out: &[u8], copybacks: &[Copyback], min_len: u32, sourceless: bool) {
    let mut next_free = 0;
    for copyback in copybacks {
        let (dst, off, len) = (copyback.destination, copyback.offset as u64, copyback.len as u64);
        assert!(off > 0 && (sourceless || off <= dst), "{copyback:?} has no source in the output");
        assert!(len >= min_len.max(1) as u64, "{copyback:?} is shorter than {min_len}");
        assert!(dst >= next_free, "{copyback:?} overlaps the previous destination");
        next_free = dst + len;
        if off > dst {
            continue;
        }
        for i in dst..(dst + len).min(out.len() as u64) {
            assert_eq!(out[i as usize], out[(i - off) as usize], "{copyback:?} differs at {i}");
        }
    }
}

/// The stock decoder of `codec`, with the same reading of concatenated input: its output up
/// to `limit`, and the error it ended with.
pub fn stock(codec: Codec, encoded: &[u8], limit: u64) -> (Vec<u8>, Option<io::Error>) {
    fn drain(mut reader: impl Read, limit: u64) -> (Vec<u8>, Option<io::Error>) {
        let mut out = Vec::new();
        let mut buffer = vec![0; 1 << 16];
        while (out.len() as u64) < limit {
            let room = (limit - out.len() as u64).min(buffer.len() as u64) as usize;
            match reader.read(&mut buffer[..room]) {
                Ok(0) => return (out, None),
                Ok(count) => out.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return (out, Some(error)),
            }
        }
        (out, None)
    }
    match codec {
        Codec::Xz => {
            let stream = liblzma::stream::Stream::new_auto_decoder(u64::MAX, liblzma::stream::CONCATENATED).unwrap();
            drain(liblzma::read::XzDecoder::new_stream(encoded, stream), limit)
        }
        Codec::Zstd => match zstd::stream::read::Decoder::new(encoded) {
            Ok(decoder) => drain(decoder, limit),
            Err(error) => (Vec::new(), Some(error)),
        },
        Codec::Lz4 => match lz4::Decoder::new(encoded) {
            Ok(decoder) => drain(StockLz4(Some(decoder)), limit),
            Err(error) => (Vec::new(), Some(error)),
        },
        Codec::Gzip => drain(StockGzip::new(encoded), limit),
    }
}

/// lz4's decoder, with a truncated frame an error, which it tells only when finished.
struct StockLz4<'a>(Option<lz4::Decoder<&'a [u8]>>);

impl Read for StockLz4<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let Some(decoder) = self.0.as_mut() else { return Ok(0) };
        let count = decoder.read(buffer)?;
        if count == 0 && self.0.take().unwrap().finish().1.is_err() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "lz4: truncated frame"));
        }
        Ok(count)
    }
}

/// streaming-libdeflate-rs's push decompressor, as a `Read`, members one after the other. An
/// error is returned once the bytes decoded before it are read, up to 64 KiB of them.
struct StockGzip<'a> {
    input: &'a [u8],
    decompressor: streaming_libdeflate_rs::LibdeflateGzipPushDecompressor,
    pending: usize,
    error: Option<io::Error>,
}

impl<'a> StockGzip<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            decompressor: streaming_libdeflate_rs::LibdeflateGzipPushDecompressor::new(1 << 16),
            pending: 0,
            error: None,
        }
    }
}

impl Read for StockGzip<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        use streaming_libdeflate_rs::LibdeflateDecompressResult as Result;
        while self.pending == self.decompressor.pending_output().len() {
            if let Some(error) = self.error.take() {
                return Err(error);
            }
            self.decompressor.consume_output(self.pending);
            self.pending = 0;
            let result = match self.decompressor.decompress() {
                Ok(result) => result,
                Err(error) => {
                    self.error = Some(io::Error::other(format!("{error:?}")));
                    continue;
                }
            };
            match result {
                Result::EndOfStream => return Ok(0),
                Result::NeedInput => {
                    let space = self.decompressor.input_space_mut();
                    let count = space.len().min(self.input.len()).min(16 << 10);
                    if count == 0 {
                        self.decompressor.finish_input();
                    } else {
                        space[..count].copy_from_slice(&self.input[..count]);
                        self.input = &self.input[count..];
                        self.decompressor.commit_input(count);
                    }
                }
                Result::MoreData => {}
            }
        }
        let pending = &self.decompressor.pending_output()[self.pending..];
        let count = pending.len().min(buffer.len());
        buffer[..count].copy_from_slice(&pending[..count]);
        self.pending += count;
        Ok(count)
    }
}

/// A raw target: the first `PARAMS` bytes choose how the decoder is read, the rest is the
/// compressed input, decoded by ours and by the stock decoder. Both must agree on every byte
/// both decoded, and ours must not fail short of where the stock one goes on (failing past it
/// is fine: ours decodes ahead of the limit, the stock one stops there, and zstd's streaming
/// decoder checks a frame's content size only when the frame fits its buffer). Where the stock
/// decoder fails and ours does not, ours must have decoded at least as much; that is a bug
/// but for xz, whose dictionary, at least the bytes to retain, also takes corrupt matches
/// reaching farther back than the stream's own dictionary size. gzip is also checked against
/// flate2 on zlib-rs, as its stock decoder is the same libdeflate as ours: on every byte both
/// decoded, so up to where zlib rejects a corrupt stream (libdeflate takes a distance reaching
/// before a member's start, a known and accepted behaviour, and fails later on its checksum).
pub fn raw(codec: Codec, bytes: &[u8]) {
    let (head, encoded) = bytes.split_at(bytes.len().min(PARAMS));
    let reading = Reading::new(&mut Params(head));
    let limit = reading.limit();
    let Some(mut decoder) = decoder(codec, encoded, reading.retain()) else { return };
    let ours = read_checked(&mut *decoder, &reading, limit, codec == Codec::Gzip);
    drop(decoder);
    let (theirs, their_error) = stock(codec, encoded, limit);

    let common = ours.out.len().min(theirs.len());
    assert!(ours.out[..common] == theirs[..common], "{codec:?}: the outputs differ within {common} bytes");
    if codec == Codec::Gzip {
        let mut other = Vec::new();
        let _ = flate2::read::MultiGzDecoder::new(encoded).take(limit).read_to_end(&mut other);
        let common = ours.out.len().min(other.len());
        assert!(ours.out[..common] == other[..common], "gzip: the output differs from flate2's within {common} bytes");
    }
    match (&ours.error, &their_error) {
        (None, None) => assert_eq!(ours.out.len(), theirs.len(), "{codec:?}: both decoded, not as much"),
        (Some(error), None) => assert!(
            ours.out.len() >= theirs.len(),
            "{codec:?}: failed ({error}) after {} bytes, where the stock decoder decoded {}",
            ours.out.len(),
            theirs.len()
        ),
        (None, Some(error)) => {
            assert!(ours.out.len() >= theirs.len(), "{codec:?}: decoded less without failing");
            assert!(codec == Codec::Xz, "{codec:?}: decoded {} bytes where the stock decoder failed ({error})", ours.out.len());
        }
        (Some(_), Some(_)) => {}
    }
}

/// Data with repeats at all distances, grown from `seed` up to a size `params` chooses: copies
/// of earlier stretches (sometimes overlapping themselves, sometimes with a byte changed) and
/// fresh bytes, DNA-like or of any value.
pub fn data(params: &mut Params, seed: &[u8], max_len: usize) -> Vec<u8> {
    let len = params.size(21).min(max_len);
    let mut state = ((params.u32() as u64) << 32 | params.u32() as u64) | 1;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let dna = params.bool();
    let mut data = Vec::with_capacity(len + 1);
    data.extend_from_slice(&seed[..seed.len().min(len)]);
    while data.len() < len {
        if data.is_empty() || next() % 4 == 0 {
            for _ in 0..1 + next() % 256 {
                let random = next();
                data.push(if dna { b"ACGT\n"[(random % 5) as usize] } else { random as u8 });
            }
        } else {
            let distance = 1 + match next() % 3 {
                0 => next() % 64,
                1 => next() % 65_536,
                _ => next(),
            } as usize % data.len();
            let count = 1 + match next() % 3 {
                0 => next() % 16,
                1 => next() % 1024,
                _ => next() % 65_536,
            } as usize;
            let mutate = next() % 4 == 0;
            for index in 0..count {
                let mut byte = data[data.len() - distance];
                if mutate && index == count / 2 {
                    byte ^= 1;
                }
                data.push(byte);
            }
        }
    }
    data.truncate(len);
    data
}

/// A round trip: `encoded` must decode, read as `reading` says, to exactly `expected`.
pub fn round_trip(codec: Codec, encoded: &[u8], expected: &[u8], reading: &Reading) {
    let mut decoder = decoder(codec, encoded, reading.retain()).expect("a decoder");
    let outcome = read_checked(&mut *decoder, reading, u64::MAX, false);
    if let Some(error) = &outcome.error {
        panic!("{codec:?}: failed ({error}) after {} of {} bytes", outcome.out.len(), expected.len());
    }
    assert!(outcome.out == expected, "{codec:?}: {} bytes decoded, not the {} encoded", outcome.out.len(), expected.len());
}
