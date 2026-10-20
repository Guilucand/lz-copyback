//! Data with repeats, encoded by liblz4 in one frame (block sizes, linked or independent
//! blocks, checksums, content size, levels), sometimes followed by another frame that must be
//! ignored, then decoded to exactly the first frame's data, read as the fuzzer chooses.
#![no_main]

mod common;

use common::{Codec, Params, Reading, MAX_STEPS};
use lz4::liblz4::BlockChecksum;
use lz4::{BlockMode, BlockSize, ContentChecksum};
use std::io::Write;

fn encode(params: &mut Params, data: &[u8]) -> Vec<u8> {
    let mut builder = lz4::EncoderBuilder::new();
    builder
        .block_size([BlockSize::Max64KB, BlockSize::Max256KB, BlockSize::Max1MB, BlockSize::Max4MB][params.below(4) as usize].clone())
        .block_mode(if params.bool() { BlockMode::Linked } else { BlockMode::Independent })
        .checksum(if params.bool() { ContentChecksum::ChecksumEnabled } else { ContentChecksum::NoChecksum })
        .block_checksum(if params.bool() { BlockChecksum::BlockChecksumEnabled } else { BlockChecksum::NoBlockChecksum })
        .level(params.below(13));
    if params.bool() {
        builder.content_size(data.len() as u64);
    }
    let mut encoder = builder.build(Vec::new()).unwrap();
    encoder.write_all(data).unwrap();
    let (encoded, result) = encoder.finish();
    result.unwrap();
    encoded
}

libfuzzer_sys::fuzz_target!(|bytes: &[u8]| {
    let (head, seed) = bytes.split_at(bytes.len().min(64));
    let mut params = Params(head);
    let reading = Reading::new(&mut params);
    let max_len = (reading.chunk as u64 * MAX_STEPS).min(8 << 20) as usize;
    let data = common::data(&mut params, seed, max_len);
    let mut encoded = encode(&mut params, &data);
    if params.bool() {
        let other = common::data(&mut params, seed, 1 << 16);
        encoded.extend(encode(&mut params, &other));
    }
    common::round_trip(Codec::Lz4, &encoded, &data, &reading);
});
