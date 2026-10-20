//! Decoding throughput on the stream layouts that stress the in-place decoders, next to the
//! stock decoder of the format: tiny xz blocks and streams (a dictionary reset, and a run of
//! the ring, each), filtered xz blocks (decoded apart, with a copy), tiny zstd frames and
//! windows, thousands of gzip members, stored gzip blocks, lz4 block sizes and modes, and
//! chunks from 64 bytes to 1 MiB. One run per configuration, single-threaded, from memory.
//!
//! ```text
//! cargo bench --bench stress -- [--mib N]
//! ```

use lz_copyback::InPlaceDecoder;
use std::hint::black_box;
use std::io::{Read, Write};
use std::time::Instant;

#[derive(Clone, Copy, Debug)]
enum Codec {
    Xz,
    Zstd,
    Lz4,
    Gzip,
}

/// A batch of near-identical genomes, as in the decode bench: long repeats a genome apart.
fn genomes(size: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        state >> 33
    };
    let base: Vec<u8> = (0..1 << 20).map(|_| b"ACGT"[(next() % 4) as usize]).collect();
    let mut out = Vec::with_capacity(size + (2 << 20));
    let mut genome = 0;
    while out.len() < size {
        out.extend_from_slice(format!(">genome_{genome}\n").as_bytes());
        let sequence: Vec<u8> = base
            .iter()
            .map(|&base| if next() % 1000 == 0 { b"ACGT"[(next() % 4) as usize] } else { base })
            .collect();
        for line in sequence.chunks(80) {
            out.extend_from_slice(line);
            out.push(b'\n');
        }
        genome += 1;
    }
    out.truncate(size);
    out
}

fn xz(data: &[u8], block_size: Option<u64>, bcj: bool) -> Vec<u8> {
    use liblzma::stream::{Check, Filters, LzmaOptions, MtStreamBuilder, Stream};
    let stream = match (block_size, bcj) {
        (Some(size), _) => MtStreamBuilder::new().threads(1).preset(6).block_size(size).encoder().unwrap(),
        (None, true) => {
            let mut filters = Filters::new();
            filters.x86().lzma2(&LzmaOptions::new_preset(6).unwrap());
            Stream::new_stream_encoder(&filters, Check::Crc64).unwrap()
        }
        (None, false) => Stream::new_easy_encoder(6, Check::Crc64).unwrap(),
    };
    let mut encoder = liblzma::write::XzEncoder::new_stream(Vec::new(), stream);
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn zstd(data: &[u8], level: i32, window_log: u32) -> Vec<u8> {
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), level).unwrap();
    encoder.window_log(window_log).unwrap();
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn lz4(data: &[u8], block_size: lz4::BlockSize, linked: bool) -> Vec<u8> {
    let mut encoder = lz4::EncoderBuilder::new()
        .level(1)
        .block_size(block_size)
        .block_mode(if linked { lz4::BlockMode::Linked } else { lz4::BlockMode::Independent })
        .build(Vec::new())
        .unwrap();
    encoder.write_all(data).unwrap();
    let (out, result) = encoder.finish();
    result.unwrap();
    out
}

fn gzip(data: &[u8], level: u32) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(level));
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// `data` cut in `pieces`, each encoded on its own, back to back.
fn concatenated(data: &[u8], pieces: usize, encode: impl Fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
    data.chunks(data.len().div_ceil(pieces)).flat_map(|piece| encode(piece)).collect()
}

/// MB/s of the stock decoder of `codec`, reading 64 KiB at a time.
fn stock(codec: Codec, encoded: &[u8], len: usize) -> f64 {
    let start = Instant::now();
    let mut buffer = vec![0u8; 1 << 16];
    let mut total = 0;
    let mut read = |mut reader: Box<dyn Read + '_>| loop {
        let count = reader.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        total += count;
    };
    match codec {
        Codec::Xz => read(Box::new(liblzma::read::XzDecoder::new_multi_decoder(encoded))),
        Codec::Zstd => read(Box::new(zstd::stream::read::Decoder::new(encoded).unwrap())),
        Codec::Lz4 => read(Box::new(lz4::Decoder::new(encoded).unwrap())),
        Codec::Gzip => read(Box::new(flate2::read::MultiGzDecoder::new(encoded))),
    }
    assert_eq!(total, len, "{codec:?} stock: wrong size");
    len as f64 / start.elapsed().as_secs_f64() / 1e6
}

/// MB/s of the in-place decoder of `codec`, tracking at min 62, read `chunk` bytes at a time
/// with `window` bytes decoded past each chunk.
fn in_place(codec: Codec, encoded: &[u8], len: usize, chunk: usize, window: usize) -> f64 {
    let retain = chunk + window;
    let start = Instant::now();
    let mut decoder: Box<dyn InPlaceDecoder> = match codec {
        Codec::Xz => Box::new(lz_copyback::xz::XzInPlaceDecoder::new_multi_decoder(encoded, retain)),
        Codec::Zstd => Box::new(lz_copyback::zstd::ZstdInPlaceDecoder::new(encoded, retain).unwrap()),
        Codec::Lz4 => Box::new(lz_copyback::lz4::Lz4InPlaceDecoder::new(encoded, retain).unwrap()),
        Codec::Gzip => Box::new(lz_copyback::gzip::GzipInPlaceDecoder::new(encoded, retain)),
    };
    decoder.set_copybacks_min_len(62);
    let mut position = 0u64;
    loop {
        let decoded = decoder.decode_to(position + retain as u64).unwrap();
        let end = decoded.min(position + chunk as u64);
        if end == position {
            break;
        }
        let (pieces, queue) = decoder.bytes_and_copybacks(position..end);
        for piece in pieces {
            black_box(piece);
        }
        queue.clear();
        position = end;
    }
    assert_eq!(position, len as u64, "{codec:?} in place: wrong size");
    len as f64 / start.elapsed().as_secs_f64() / 1e6
}

fn main() {
    let mut mib = 32;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mib" => mib = args.next().unwrap().parse().unwrap(),
            "--bench" => {}
            other => panic!("unknown argument {other}"),
        }
    }
    let data = genomes(mib << 20);
    let small = &data[..8 << 20];

    type Encode<'a> = Box<dyn Fn() -> Vec<u8> + 'a>;
    let cases: Vec<(&str, Codec, &[u8], Encode)> = vec![
        ("xz -6, one block", Codec::Xz, &data, Box::new(|| xz(&data, None, false))),
        ("xz -6, 1 MiB blocks", Codec::Xz, &data, Box::new(|| xz(&data, Some(1 << 20), false))),
        ("xz -6, 64 KiB blocks", Codec::Xz, &data, Box::new(|| xz(&data, Some(64 << 10), false))),
        ("xz -6, 4 KiB blocks", Codec::Xz, &data, Box::new(|| xz(&data, Some(4 << 10), false))),
        ("xz -6, 4 KiB blocks, 8 MiB", Codec::Xz, small, Box::new(|| xz(small, Some(4 << 10), false))),
        ("xz -6, 1000 streams", Codec::Xz, &data, Box::new(|| concatenated(&data, 1000, |piece| xz(piece, None, false)))),
        ("xz -6, BCJ filter", Codec::Xz, &data, Box::new(|| xz(&data, None, true))),
        ("zstd -3", Codec::Zstd, &data, Box::new(|| zstd(&data, 3, 23))),
        ("zstd -3, 1 KiB window", Codec::Zstd, &data, Box::new(|| zstd(&data, 3, 10))),
        ("zstd -3, 1000 frames", Codec::Zstd, &data, Box::new(|| concatenated(&data, 1000, |piece| zstd(piece, 3, 20)))),
        ("lz4, 64 KiB linked blocks", Codec::Lz4, &data, Box::new(|| lz4(&data, lz4::BlockSize::Max64KB, true))),
        ("lz4, 4 MiB blocks", Codec::Lz4, &data, Box::new(|| lz4(&data, lz4::BlockSize::Max4MB, false))),
        ("gzip -6", Codec::Gzip, &data, Box::new(|| gzip(&data, 6))),
        ("gzip -6, 1000 members", Codec::Gzip, &data, Box::new(|| concatenated(&data, 1000, |piece| gzip(piece, 6)))),
        ("gzip -0, stored", Codec::Gzip, &data, Box::new(|| gzip(&data, 0))),
    ];

    println!("{mib} MiB of genomes; chunks of 64 KiB, 1 MiB window unless said; one run each");
    println!("| layout | compressed bytes | stock MB/s | in place MB/s | in place / stock |");
    println!("|---|---:|---:|---:|---:|");
    let mut encoded_xz = None;
    let mut encoded_gzip = None;
    for (name, codec, input, encode) in &cases {
        let encoded = encode();
        let stock = stock(*codec, &encoded, input.len());
        let ours = in_place(*codec, &encoded, input.len(), 64 << 10, 1 << 20);
        println!("| {name} | {} | {stock:.0} | {ours:.0} | {:.2} |", encoded.len(), ours / stock);
        if *name == "xz -6, one block" {
            encoded_xz = Some(encoded);
        } else if *name == "gzip -6" {
            encoded_gzip = Some(encoded);
        }
    }

    println!("\n| chunk | xz -6 in place MB/s | gzip -6 in place MB/s |");
    println!("|---:|---:|---:|");
    let (xz, gzip) = (encoded_xz.unwrap(), encoded_gzip.unwrap());
    for chunk in [64, 4 << 10, 64 << 10, 1 << 20] {
        let ours_xz = in_place(Codec::Xz, &xz, data.len(), chunk, 1 << 20);
        let ours_gzip = in_place(Codec::Gzip, &gzip, data.len(), chunk, 1 << 20);
        println!("| {chunk} | {ours_xz:.0} | {ours_gzip:.0} |");
    }
}
