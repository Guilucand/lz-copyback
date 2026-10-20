//! Arbitrary bytes, decoded by lz-copyback's zstd decoder and by the stock one; see `common::raw`.
#![no_main]

mod common;

libfuzzer_sys::fuzz_target!(|bytes: &[u8]| common::raw(common::Codec::Zstd, bytes));
