//! Helpers shared by the integration tests: the decoders by codec, a reader that checks when
//! copybacks arrive, the invariants every copyback list must satisfy, and test data with
//! encoders.
#![allow(dead_code)]

use lz_copyback::gzip::GzipInPlaceDecoder;
use lz_copyback::lz4::Lz4InPlaceDecoder;
use lz_copyback::xz::XzInPlaceDecoder;
use lz_copyback::zstd::ZstdInPlaceDecoder;
use lz_copyback::{Copyback, InPlaceDecoder};
use rand::{Rng, SeedableRng, rngs::StdRng};
use std::io::Write;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Xz,
    Zstd,
    Lz4,
    Gzip,
}

/// The hard threshold of the tests' decoders: below the larger soft thresholds they set, so
/// that the decoders' own filter and the queue's both run.
pub const HARD_MIN_LEN: u32 = 32;

/// The decoder of `codec` over `encoded`, keeping `retain` bytes readable, with the hard
/// threshold [`HARD_MIN_LEN`].
pub fn decoder(codec: Codec, encoded: &[u8], retain: usize) -> Box<dyn InPlaceDecoder + '_> {
    decoder_hard(codec, encoded, retain, HARD_MIN_LEN)
}

/// As [`decoder`], with the hard threshold `hard_min_len`.
pub fn decoder_hard(codec: Codec, encoded: &[u8], retain: usize, hard_min_len: u32) -> Box<dyn InPlaceDecoder + '_> {
    match codec {
        Codec::Xz => Box::new(XzInPlaceDecoder::new_multi_decoder(encoded, retain, hard_min_len)),
        Codec::Zstd => Box::new(ZstdInPlaceDecoder::new(encoded, retain, hard_min_len).unwrap()),
        Codec::Lz4 => Box::new(Lz4InPlaceDecoder::new(encoded, retain, hard_min_len).unwrap()),
        Codec::Gzip => Box::new(GzipInPlaceDecoder::new(encoded, retain, hard_min_len)),
    }
}

/// What a decoder produced: its output and every copyback, in the order they were pushed.
pub struct Drained {
    pub out: Vec<u8>,
    pub copybacks: Vec<Copyback>,
}

/// Reads `decoder` to the end as a consumer of chunks does: `chunk` bytes at a time, decoding
/// `ahead` bytes past each first (the decoder must retain `chunk + ahead`), and taking the
/// copybacks queued after each step. A copyback must arrive no later than the step that
/// decodes its first destination byte: one pushed for bytes already handed out fails the test.
pub fn try_drain(decoder: &mut dyn InPlaceDecoder, chunk: usize, ahead: usize) -> std::io::Result<Drained> {
    try_drain_min(decoder, chunk, ahead, 1)
}

/// As [`try_drain`], the decoder's threshold set to `min_len`: a copyback must arrive no later
/// than the step that decodes its first `min_len` bytes.
pub fn try_drain_min(
    decoder: &mut dyn InPlaceDecoder,
    chunk: usize,
    ahead: usize,
    min_len: u32,
) -> std::io::Result<Drained> {
    decoder.set_copybacks_min_len(min_len);
    let mut out = Vec::new();
    let mut copybacks = Vec::new();
    loop {
        let position = out.len() as u64;
        let decoded = decoder.decode_to(position + (chunk + ahead) as u64)?;
        let end = decoded.min(position + chunk as u64);
        let (pieces, queue) = decoder.bytes_and_copybacks(position..end);
        for piece in pieces {
            out.extend_from_slice(piece);
        }
        for copyback in queue.drain(..) {
            assert!(
                copyback.destination + min_len as u64 > position,
                "{copyback:?} pushed after {position} bytes were handed out"
            );
            copybacks.push(copyback);
        }
        if end == position {
            return Ok(Drained { out, copybacks });
        }
    }
}

pub fn drain(decoder: &mut dyn InPlaceDecoder, chunk: usize, ahead: usize) -> Drained {
    try_drain(decoder, chunk, ahead).expect("decode error")
}

/// As [`drain`], the decoder's threshold set to `min_len`.
pub fn drain_min(decoder: &mut dyn InPlaceDecoder, chunk: usize, ahead: usize, min_len: u32) -> Drained {
    try_drain_min(decoder, chunk, ahead, min_len).expect("decode error")
}

/// The whole of `encoded`, decoded by the decoder of `codec`, read in 64 KiB chunks with
/// 1 MiB of look-ahead, as ggcat reads it.
pub fn full(codec: Codec, encoded: &[u8]) -> Drained {
    let (chunk, ahead) = (64 << 10, 1 << 20);
    drain(&mut *decoder(codec, encoded, chunk + ahead), chunk, ahead)
}

/// The whole output of `decoder`, from its first byte (which must still be readable) to the
/// end, read a chunk at a time, leaving its copyback queue alone.
pub fn read_out(decoder: &mut dyn InPlaceDecoder, chunk: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut position = 0;
    loop {
        let decoded = decoder.decode_to(position + chunk as u64).expect("decode error");
        let end = decoded.min(position + chunk as u64);
        if end == position {
            return out;
        }
        for piece in decoder.bytes(position..end) {
            out.extend_from_slice(piece);
        }
        position = end;
    }
}

/// The invariants of every list of raw matches: in stream order, destinations never
/// overlapping, each a correct repeat of earlier output (byte by byte, so an overlapping,
/// periodic match is checked as it is decoded).
pub fn check_copybacks(out: &[u8], copybacks: &[Copyback]) {
    let mut next_free = 0;
    for copyback in copybacks {
        let (dst, off, len) = (copyback.destination, copyback.offset as u64, copyback.len as u64);
        assert!(off > 0 && off <= dst, "{copyback:?} has no source in the output");
        assert!(len > 0, "{copyback:?} is empty");
        assert!(dst >= next_free, "{copyback:?} overlaps the previous destination");
        assert!(dst + len <= out.len() as u64, "{copyback:?} ends past the output");
        for i in dst..dst + len {
            assert_eq!(out[i as usize], out[(i - off) as usize], "{copyback:?} differs at {i}");
        }
        next_free = dst + len;
    }
}

/// `copybacks` with each one continuing the one before, at the same offset, merged into it,
/// as a decoder merges them while the one before is still in its queue: lists taken out of the
/// queue at different times compare equal merged.
pub fn merged(copybacks: &[Copyback]) -> Vec<Copyback> {
    let mut out: Vec<Copyback> = Vec::with_capacity(copybacks.len());
    for &copyback in copybacks {
        if let Some(last) = out.last_mut() {
            if last.offset == copyback.offset && last.destination + last.len as u64 == copyback.destination {
                last.len += copyback.len;
                continue;
            }
        }
        out.push(copyback);
    }
    out
}

/// Whether `copyback` continues `before`: at the same offset, from where it ends.
pub fn continues(before: &Copyback, copyback: &Copyback) -> bool {
    before.offset == copyback.offset && before.destination + before.len as u64 == copyback.destination
}

/// Does some copyback repeat at least `min_cover` of the planted `len` bytes at `dst`, at
/// their distance?
pub fn covers(copybacks: &[Copyback], src: u64, dst: u64, len: u64, min_cover: f64) -> bool {
    let covered: u64 = copybacks
        .iter()
        .filter(|c| c.offset as u64 == dst - src)
        .map(|c| {
            let lo = c.destination.max(dst);
            let hi = (c.destination + c.len as u64).min(dst + len);
            hi.saturating_sub(lo)
        })
        .sum();
    covered as f64 >= min_cover * len as f64
}

/// Pseudo-random bytes from a fixed LCG.
pub fn random_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

/// Make `data[dst..dst + len]` a copy of `data[src..src + len]` (`dst >= src + len`).
pub fn plant(data: &mut [u8], src: usize, dst: usize, len: usize) {
    assert!(dst >= src + len);
    let (a, b) = data.split_at_mut(dst);
    b[..len].copy_from_slice(&a[src..src + len]);
}

/// Random ACGT records of `record_len` bases wrapped at 80 columns. With probability
/// `repeat_prob` a record is a verbatim copy of one of the previous `back` records, so the
/// data has long repeats at distances bounded by `back * record_len`.
pub fn fasta(seed: u64, records: usize, record_len: usize, repeat_prob: f64, back: usize) -> Vec<u8> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut out = Vec::new();
    let mut starts: Vec<(usize, usize)> = Vec::new();
    for i in 0..records {
        let header = format!(">r{i}\n");
        let body_start = out.len() + header.len();
        if !starts.is_empty() && rng.gen_bool(repeat_prob) {
            let lo = starts.len().saturating_sub(back);
            let (s, e) = starts[rng.gen_range(lo..starts.len())];
            let copy = out[s..e].to_vec();
            out.extend_from_slice(header.as_bytes());
            out.extend_from_slice(&copy);
        } else {
            out.extend_from_slice(header.as_bytes());
            for col in 0..record_len {
                out.push(b"ACGT"[rng.gen_range(0..4)]);
                if col % 80 == 79 || col + 1 == record_len {
                    out.push(b'\n');
                }
            }
        }
        starts.push((body_start, out.len()));
    }
    out
}

/// FASTA-like data with long repeats, plus one planted repeat of `len` bytes at distance
/// `distance`; returns the data and the planted `(src, dst, len)`.
pub fn sample(seed: u64, size: usize, distance: usize, len: usize) -> (Vec<u8>, (u64, u64, u64)) {
    let mut data = fasta(seed, size / 2000 + 1, 2000, 0.5, 8);
    data.truncate(size);
    let dst = size - len - 1000;
    let src = dst - distance;
    // Random bytes, so that the repeat can only be found at the planted distance.
    data[src..src + len].copy_from_slice(&random_bytes(seed, len));
    plant(&mut data, src, dst, len);
    (data, (src as u64, dst as u64, len as u64))
}

pub fn xz_encode(data: &[u8], dict_size: u32) -> Vec<u8> {
    use liblzma::stream::{Check, Filters, LzmaOptions, Stream};
    let mut opts = LzmaOptions::new_preset(6).unwrap();
    opts.dict_size(dict_size);
    let mut filters = Filters::new();
    filters.lzma2(&opts);
    let stream = Stream::new_stream_encoder(&filters, Check::Crc64).unwrap();
    let mut enc = liblzma::write::XzEncoder::new_stream(Vec::new(), stream);
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

/// An xz stream with an x86 BCJ filter in front of LZMA2.
pub fn xz_encode_bcj(data: &[u8]) -> Vec<u8> {
    use liblzma::stream::{Check, Filters, LzmaOptions, Stream};
    let opts = LzmaOptions::new_preset(6).unwrap();
    let mut filters = Filters::new();
    filters.x86().lzma2(&opts);
    let stream = Stream::new_stream_encoder(&filters, Check::Crc64).unwrap();
    let mut enc = liblzma::write::XzEncoder::new_stream(Vec::new(), stream);
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

pub fn gzip_encode(data: &[u8], level: u32) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(level));
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

pub fn zstd_encode(data: &[u8], window_log: u32) -> Vec<u8> {
    let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
    enc.window_log(window_log).unwrap();
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

pub fn lz4_encode(data: &[u8], linked: bool, block_size: lz4::BlockSize) -> Vec<u8> {
    let mode = if linked { lz4::BlockMode::Linked } else { lz4::BlockMode::Independent };
    let mut enc = lz4::EncoderBuilder::new()
        .level(4)
        .block_mode(mode)
        .block_size(block_size)
        .build(Vec::new())
        .unwrap();
    enc.write_all(data).unwrap();
    let (out, result) = enc.finish();
    result.unwrap();
    out
}
