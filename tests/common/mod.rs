//! Helpers shared by the integration tests: one interface over the four decoders, a
//! reader that checks when copybacks arrive, the invariants every copyback list must
//! satisfy, and test data with encoders.
#![allow(dead_code)]

use lz_copyback::Copyback;
use rand::{Rng, SeedableRng, rngs::StdRng};
use std::collections::VecDeque;
use std::io::{Read, Write};

/// The three copyback methods every fork adds, plus `Read`.
pub trait CopybackReader: Read {
    fn tracking(&mut self, enabled: bool);
    fn min_len(&mut self, min_len: u32);
    fn copybacks(&mut self) -> &mut VecDeque<Copyback>;
}

macro_rules! copyback_reader {
    ($(impl[$($generics:tt)*] for $decoder:ty;)*) => {$(
        impl<$($generics)*> CopybackReader for $decoder {
            fn tracking(&mut self, enabled: bool) {
                self.set_copybacks_tracking(enabled);
            }
            fn min_len(&mut self, min_len: u32) {
                self.set_copybacks_min_len(min_len);
            }
            fn copybacks(&mut self) -> &mut VecDeque<Copyback> {
                self.peek_copybacks()
            }
        }
    )*};
}

copyback_reader! {
    impl[R: Read] for lz_copyback::xz::XzDecoder<R>;
    impl[R: std::io::BufRead] for lz_copyback::zstd::ZstdDecoder<R>;
    impl[R: Read] for lz_copyback::lz4::Lz4Decoder<R>;
    impl[R: Read] for lz_copyback::gzip::GzipDecoder<R>;
}

/// What a decoder produced: its output and every copyback, in the order they were pushed.
pub struct Drained {
    pub out: Vec<u8>,
    pub copybacks: Vec<Copyback>,
}

/// Read `decoder` to the end in calls of `read_size` bytes, taking the copybacks out of its
/// queue after every call. A copyback must arrive no later than the call that returns its
/// first destination byte: pushing one for bytes already handed out fails the test.
pub fn drain(decoder: &mut impl CopybackReader, read_size: usize) -> Drained {
    let mut out = Vec::new();
    let mut copybacks = Vec::new();
    let mut buffer = vec![0; read_size];
    loop {
        let delivered = out.len() as u64;
        let count = decoder.read(&mut buffer).expect("decode error");
        for copyback in decoder.copybacks().drain(..) {
            assert!(
                copyback.destination >= delivered,
                "{copyback:?} pushed after {delivered} bytes were handed out"
            );
            copybacks.push(copyback);
        }
        if count == 0 {
            return Drained { out, copybacks };
        }
        out.extend_from_slice(&buffer[..count]);
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
