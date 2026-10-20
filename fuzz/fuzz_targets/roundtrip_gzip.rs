//! Data with repeats, encoded by flate2 in up to four concatenated gzip members (every level,
//! stored blocks included, empty members), then decoded to exactly that data, read as the
//! fuzzer chooses.
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
    let members = 1 + params.below(4);
    for _ in 0..members {
        let data = if params.below(8) == 0 { Vec::new() } else { common::data(&mut params, seed, max_len / members as usize) };
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(params.below(10)));
        encoder.write_all(&data).unwrap();
        encoded.extend(encoder.finish().unwrap());
        expected.extend(data);
    }
    common::round_trip(Codec::Gzip, &encoded, &expected, &reading);
});
