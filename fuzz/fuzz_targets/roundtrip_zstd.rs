//! Data with repeats, encoded by libzstd in up to three frames (levels, windows, checksums,
//! content sizes, long distance matching) with skippable frames between them, then decoded to
//! exactly that data, read as the fuzzer chooses.
#![no_main]

mod common;

use common::{Codec, Params, Reading, MAX_STEPS};
use std::io::Write;

libfuzzer_sys::fuzz_target!(|bytes: &[u8]| {
    let (head, seed) = bytes.split_at(bytes.len().min(64));
    let mut params = Params(head);
    let reading = Reading::new(&mut params);
    let max_len = (reading.chunk as u64 * MAX_STEPS).min(4 << 20) as usize;
    let (mut encoded, mut expected) = (Vec::new(), Vec::new());
    let frames = 1 + params.below(3);
    for _ in 0..frames {
        if params.below(4) == 0 {
            let payload = params.below(64) as usize;
            encoded.extend((0x184d_2a50u32 + params.below(16)).to_le_bytes());
            encoded.extend((payload as u32).to_le_bytes());
            encoded.extend(std::iter::repeat_n(0xa5u8, payload));
        }
        let data = common::data(&mut params, seed, max_len / frames as usize);
        let level = params.below(14) as i32 - 3;
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), level).unwrap();
        encoder.window_log(10 + params.below(14)).unwrap();
        encoder.include_checksum(params.bool()).unwrap();
        if params.bool() {
            encoder.set_pledged_src_size(Some(data.len() as u64)).unwrap();
        }
        if params.below(4) == 0 {
            encoder.long_distance_matching(true).unwrap();
        }
        encoder.write_all(&data).unwrap();
        encoded.extend(encoder.finish().unwrap());
        expected.extend(data);
    }
    common::round_trip(Codec::Zstd, &encoded, &expected, &reading);
});
