//! The rings: the decoders read what the stock decoders do, a chunk at a time with a window
//! of look-ahead, with the retained bytes readable after every step and copybacks that do not
//! depend on the step, while the rings wrap around, across frames, members, blocks, filters
//! and dictionary sizes.

mod common;

use common::{Codec, check_copybacks, decoder, fasta, gzip_encode, lz4_encode, random_bytes, xz_encode, xz_encode_bcj, zstd_encode};
use lz_copyback::lz4::Lz4InPlaceDecoder;
use lz_copyback::xz::XzInPlaceDecoder;
use lz_copyback::zstd::ZstdInPlaceDecoder;
use lz_copyback::{Copyback, InPlaceDecoder, gzip::GzipInPlaceDecoder};
use std::io::{Read, Write};

const MIN_LEN: u32 = 16;

/// What the stock decoder of `codec` decodes `encoded` to.
fn stock(codec: Codec, encoded: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    match codec {
        Codec::Xz => drop(liblzma::read::XzDecoder::new_multi_decoder(encoded).read_to_end(&mut out).unwrap()),
        Codec::Zstd => out = zstd::stream::decode_all(encoded).unwrap(),
        Codec::Lz4 => drop(lz4::Decoder::new(encoded).unwrap().read_to_end(&mut out).unwrap()),
        Codec::Gzip => drop(flate2::read::MultiGzDecoder::new(encoded).read_to_end(&mut out).unwrap()),
    }
    out
}

/// Reads `decoder` as a consumer does: before each `chunk`, decoding `ahead` bytes past it.
/// Every chunk must be the expected bytes, and so must the oldest retained bytes after every
/// `decode_to`. Returns the copybacks.
fn read_in_place(decoder: &mut dyn InPlaceDecoder, expected: &[u8], chunk: usize, ahead: usize) -> Vec<Copyback> {
    let retain = (chunk + ahead) as u64;
    let total = expected.len() as u64;
    let mut copybacks = Vec::new();
    let mut position = 0;
    loop {
        let target = position + retain;
        let decoded = decoder.decode_to(target).expect("decode error");
        if decoded < target {
            assert!(decoder.is_finished(), "stopped at {decoded} before {target}");
        }
        assert_eq!(decoded.min(total), decoded, "decoded past the end");
        assert_eq!(decoder.decoded_len(), decoded);

        let oldest = target - retain;
        let check_to = (oldest + 64).min(decoded);
        let bytes: Vec<u8> = decoder.bytes(oldest..check_to).flatten().copied().collect();
        assert!(bytes == expected[oldest as usize..check_to as usize], "retained bytes at {oldest} differ");

        let end = (position + chunk as u64).min(decoded);
        if position == end {
            assert!(decoder.is_finished());
            break;
        }
        let (pieces, queue) = decoder.bytes_and_copybacks(position..end);
        let bytes: Vec<u8> = pieces.flatten().copied().collect();
        assert!(bytes == expected[position as usize..end as usize], "chunk at {position} differs");
        copybacks.extend(queue.drain(..));
        position = end;
    }
    assert_eq!(position, total);
    copybacks.extend(decoder.peek_copybacks().drain(..));
    copybacks
}

/// The decoder of `codec` reads what the stock one does, for several chunk and look-ahead
/// sizes, with copybacks that are correct raw matches and the same for all of them.
fn check(codec: Codec, encoded: &[u8]) {
    check_with(codec, encoded, &[(64 << 10, 1 << 20), (4096, 64 << 10), (1, 1000)]);
}

fn check_with(codec: Codec, encoded: &[u8], sizes: &[(usize, usize)]) {
    let expected = stock(codec, encoded);
    let mut reference: Option<Vec<Copyback>> = None;
    for &(chunk, ahead) in sizes {
        let mut decoder = decoder(codec, encoded, chunk + ahead);
        decoder.set_copybacks_min_len(MIN_LEN);
        let found = read_in_place(&mut *decoder, &expected, chunk, ahead);
        match &reference {
            None => {
                check_copybacks(&expected, &found);
                reference = Some(found);
            }
            Some(reference) => assert!(found == *reference, "chunk {chunk}, ahead {ahead}: the copybacks differ"),
        }
    }
}

fn data(seed: u64, size: usize) -> Vec<u8> {
    let mut data = fasta(seed, size / 1500 + 1, 1500, 0.4, 50);
    data.truncate(size);
    data
}

fn xz_with(data: &[u8], preset: u32, dict_size: Option<u32>) -> Vec<u8> {
    use liblzma::stream::{Check, Filters, LzmaOptions, Stream};
    let mut options = LzmaOptions::new_preset(preset).unwrap();
    if let Some(dict_size) = dict_size {
        options.dict_size(dict_size);
    }
    let mut filters = Filters::new();
    filters.lzma2(&options);
    let stream = Stream::new_stream_encoder(&filters, Check::Crc64).unwrap();
    let mut encoder = liblzma::write::XzEncoder::new_stream(Vec::new(), stream);
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// Blocks of `block_size` bytes, each starting with a reset of the dictionary.
fn xz_blocks(data: &[u8], block_size: u64) -> Vec<u8> {
    let stream = liblzma::stream::MtStreamBuilder::new()
        .preset(6)
        .threads(2)
        .block_size(block_size)
        .encoder()
        .unwrap();
    let mut encoder = liblzma::write::XzEncoder::new_stream(Vec::new(), stream);
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn check_xz(encoded: &[u8]) {
    check(Codec::Xz, encoded);
}

#[test]
fn xz_one_block() {
    check_xz(&xz_encode(&data(1, 3 << 20), 8 << 20));
}

#[test]
fn xz_dictionary_wraps() {
    // A dictionary much smaller than the data: the ring wraps many times.
    check_xz(&xz_with(&data(2, 6 << 20), 6, Some(64 << 10)));
}

#[test]
fn xz_blocks_reset_the_dictionary_in_place() {
    check_xz(&xz_blocks(&data(3, 5 << 20), 100_000));
    // Tiny blocks: many resets per lap of the dictionary.
    check_xz(&xz_blocks(&data(4, 1 << 20), 5000));
}

#[test]
fn xz_concatenated_streams_grow_the_dictionary() {
    let (a, b, c) = (data(5, 1 << 20), data(6, 2 << 20), data(7, 1 << 20));
    let mut encoded = xz_with(&a, 0, Some(64 << 10));
    encoded.extend(xz_with(&b, 6, Some(32 << 20)));
    encoded.extend(xz_with(&c, 1, None));
    check_xz(&encoded);
}

#[test]
fn xz_filtered_blocks_are_decoded_apart() {
    check_xz(&xz_encode_bcj(&data(8, 2 << 20)));
    // Alternating filter chains: liblzma replaces its dictionary at each switch.
    let mut encoded = xz_encode(&data(9, 1 << 20), 8 << 20);
    encoded.extend(xz_encode_bcj(&data(10, 1 << 20)));
    encoded.extend(xz_encode(&data(11, 1 << 20), 8 << 20));
    check_xz(&encoded);
}

#[test]
fn xz_incompressible_chunks() {
    let mut mixed = random_bytes(12, 1 << 20);
    mixed.extend(data(13, 1 << 20));
    mixed.extend(random_bytes(14, 1 << 20));
    check_xz(&xz_with(&mixed, 6, Some(1 << 20)));
}

#[test]
fn lzma_alone() {
    use liblzma::stream::{LzmaOptions, Stream};
    let stream = Stream::new_lzma_encoder(&LzmaOptions::new_preset(6).unwrap()).unwrap();
    let mut encoder = liblzma::write::XzEncoder::new_stream(Vec::new(), stream);
    encoder.write_all(&data(15, 2 << 20)).unwrap();
    check_xz(&encoder.finish().unwrap());
}

#[test]
fn xz_truncated_is_an_error() {
    let mut encoded = xz_encode(&data(16, 1 << 20), 8 << 20);
    encoded.truncate(encoded.len() / 2);
    let mut decoder = XzInPlaceDecoder::new_multi_decoder(encoded.as_slice(), 1 << 16);
    assert!(decoder.decode_to(u64::MAX).is_err());
}

fn check_zstd(encoded: &[u8]) {
    check(Codec::Zstd, encoded);
}

#[test]
fn zstd_frames() {
    check_zstd(&zstd_encode(&data(20, 3 << 20), 20));
    // A larger window, then a frame whose size is known, then a skippable frame.
    let mut encoded = zstd_encode(&data(21, 1 << 20), 17);
    encoded.extend(zstd_encode(&data(22, 2 << 20), 23));
    encoded.extend(zstd::stream::encode_all(data(23, 300_000).as_slice(), 19).unwrap());
    encoded.extend(0x184d2a50u32.to_le_bytes());
    encoded.extend(5u32.to_le_bytes());
    encoded.extend(b"skip!");
    encoded.extend(zstd_encode(&data(24, 1 << 20), 20));
    check_zstd(&encoded);
}

#[test]
fn zstd_incompressible() {
    check_zstd(&zstd_encode(&random_bytes(25, 2 << 20), 20));
}

#[test]
fn zstd_truncated_is_an_error() {
    let mut encoded = zstd_encode(&data(26, 1 << 20), 20);
    encoded.truncate(encoded.len() / 2);
    let mut decoder = ZstdInPlaceDecoder::new(encoded.as_slice(), 1 << 16).unwrap();
    assert!(decoder.decode_to(u64::MAX).is_err());
}

fn check_lz4(encoded: &[u8]) {
    check(Codec::Lz4, encoded);
}

#[test]
fn lz4_blocks() {
    for linked in [false, true] {
        for block_size in [lz4::BlockSize::Max64KB, lz4::BlockSize::Max4MB] {
            check_lz4(&lz4_encode(&data(30, 9 << 20), linked, block_size));
        }
    }
    check_lz4(&lz4_encode(&random_bytes(31, 1 << 20), true, lz4::BlockSize::Max256KB));
}

#[test]
fn lz4_ring_wraps() {
    // The ring has room for three blocks of 4 MiB besides the bytes to retain.
    for linked in [false, true] {
        let encoded = lz4_encode(&data(33, 40 << 20), linked, lz4::BlockSize::Max4MB);
        check_with(Codec::Lz4, &encoded, &[(64 << 10, 1 << 20), (4096, 64 << 10)]);
    }
}

#[test]
fn lz4_truncated_is_an_error() {
    let mut encoded = lz4_encode(&data(32, 1 << 20), true, lz4::BlockSize::Max64KB);
    encoded.truncate(encoded.len() - 100);
    let mut decoder = Lz4InPlaceDecoder::new(encoded.as_slice(), 1 << 16).unwrap();
    assert!(decoder.decode_to(u64::MAX).is_err());
}

fn check_gzip(encoded: &[u8]) {
    check(Codec::Gzip, encoded);
}

#[test]
fn gzip_members() {
    check_gzip(&gzip_encode(&data(40, 3 << 20), 6));
    let mut encoded = gzip_encode(&data(41, 1 << 20), 1);
    encoded.extend(gzip_encode(&data(42, 500_000), 9));
    // Stored blocks.
    encoded.extend(gzip_encode(&random_bytes(43, 300_000), 0));
    encoded.extend(gzip_encode(b"", 6));
    check_gzip(&encoded);
}

#[test]
fn gzip_damage_is_an_error() {
    let mut encoded = gzip_encode(&data(44, 1 << 20), 6);
    let crc = encoded.len() - 8;
    encoded[crc] ^= 0xff;
    let mut decoder = GzipInPlaceDecoder::new(encoded.as_slice(), 1 << 16);
    assert!(decoder.decode_to(u64::MAX).is_err());
    encoded[crc] ^= 0xff;
    encoded.truncate(encoded.len() / 2);
    let mut decoder = GzipInPlaceDecoder::new(encoded.as_slice(), 1 << 16);
    assert!(decoder.decode_to(u64::MAX).is_err());
}
