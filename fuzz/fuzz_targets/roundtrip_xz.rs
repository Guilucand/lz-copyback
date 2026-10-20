//! Data with repeats, encoded by liblzma in up to three concatenated .xz streams (presets,
//! checks, dictionary sizes, blocks of any size, a BCJ filter in front of LZMA2) or as one
//! .lzma file, then decoded to exactly that data, read as the fuzzer chooses.
#![no_main]

mod common;

use common::{Codec, Params, Reading, MAX_STEPS};
use liblzma::stream::{Check, Filters, LzmaOptions, MtStreamBuilder, Stream};
use std::io::Write;

fn encode(stream: Stream, data: &[u8]) -> Vec<u8> {
    let mut encoder = liblzma::write::XzEncoder::new_stream(Vec::new(), stream);
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

libfuzzer_sys::fuzz_target!(|bytes: &[u8]| {
    let (head, seed) = bytes.split_at(bytes.len().min(64));
    let mut params = Params(head);
    let reading = Reading::new(&mut params);
    let max_len = (reading.chunk as u64 * MAX_STEPS).min(4 << 20) as usize;
    let (mut encoded, mut expected) = (Vec::new(), Vec::new());
    let streams = 1 + params.below(3);
    for index in 0..streams {
        let data = common::data(&mut params, seed, max_len / streams as usize);
        let check = [Check::None, Check::Crc32, Check::Crc64, Check::Sha256][params.below(4) as usize];
        let preset = params.below(4);
        let stream = match params.below(5) {
            0 => Stream::new_easy_encoder(preset, check).unwrap(),
            1 | 2 => {
                let mut options = LzmaOptions::new_preset(preset).unwrap();
                options.dict_size((4096 + params.below(8 << 20)).max(4096));
                let mut filters = Filters::new();
                if params.below(4) == 0 {
                    filters.x86();
                }
                filters.lzma2(&options);
                Stream::new_stream_encoder(&filters, check).unwrap()
            }
            3 => MtStreamBuilder::new()
                .threads(1)
                .preset(preset)
                .check(check)
                .block_size(params.size(20).max(512) as u64)
                .encoder()
                .unwrap(),
            // A .lzma file cannot be followed by anything.
            _ if index == 0 && streams == 1 => Stream::new_lzma_encoder(&LzmaOptions::new_preset(preset).unwrap()).unwrap(),
            _ => Stream::new_easy_encoder(preset, check).unwrap(),
        };
        encoded.extend(encode(stream, &data));
        expected.extend(data);
    }
    common::round_trip(Codec::Xz, &encoded, &expected, &reading);
});
