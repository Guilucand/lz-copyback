//! The copyback contract, checked on each of the four decoders: the output is unchanged,
//! every copyback is a correct raw match pushed no later than its first byte, the list does
//! not depend on how the output is read, and the threshold and the tracking switch behave.

mod common;

use common::{CopybackReader, Drained, check_copybacks, covers, drain, sample};
use lz_copyback::Copyback;
use lz_copyback::{gzip::GzipDecoder, lz4::Lz4Decoder, xz::XzDecoder, zstd::ZstdDecoder};

/// The longest match each format can encode in one raw match, when it has a limit.
const XZ_MAX_MATCH: u32 = 273;
const DEFLATE_MAX_MATCH: u32 = 258;

fn full(decoder: &mut impl CopybackReader) -> Drained {
    drain(decoder, 1 << 16)
}

/// A decoder over one test input, and the facts the generic tests check it against.
struct Case {
    data: Vec<u8>,
    planted: (u64, u64, u64),
    encoded: Vec<u8>,
    max_match: Option<u32>,
}

macro_rules! codec_tests {
    ($codec:ident, $case:expr, $decoder:expr) => {
        mod $codec {
            use super::*;
            use std::io::Read;

            fn case() -> Case {
                $case
            }

            #[allow(clippy::redundant_closure_call)]
            fn decoder(encoded: &[u8]) -> impl CopybackReader + '_ {
                ($decoder)(encoded)
            }

            #[test]
            fn output_is_unchanged_and_copybacks_are_correct_raw_matches() {
                let case = case();
                let drained = full(&mut decoder(&case.encoded));
                assert_eq!(drained.out, case.data);
                assert!(!drained.copybacks.is_empty());
                check_copybacks(&drained.out, &drained.copybacks);
                let (src, dst, len) = case.planted;
                assert!(covers(&drained.copybacks, src, dst, len, 0.9), "planted repeat not found");
                if let Some(max) = case.max_match {
                    let longest = drained.copybacks.iter().map(|c| c.len).max().unwrap();
                    assert!(longest <= max, "a raw match of {longest} bytes: merged or misreported");
                }
            }

            #[test]
            fn copybacks_do_not_depend_on_the_read_size() {
                let case = case();
                let reference = full(&mut decoder(&case.encoded)).copybacks;
                for read_size in [7, 4099, 1 << 20] {
                    let drained = drain(&mut decoder(&case.encoded), read_size);
                    assert_eq!(drained.out, case.data, "read size {read_size}");
                    assert!(drained.copybacks == reference, "read size {read_size} changed the copybacks");
                }
            }

            #[test]
            fn min_len_filters_the_raw_matches() {
                let case = case();
                let all = full(&mut decoder(&case.encoded)).copybacks;
                for min_len in [4, 40, 300] {
                    let mut dec = decoder(&case.encoded);
                    dec.min_len(min_len);
                    let filtered: Vec<Copyback> =
                        all.iter().copied().filter(|c| c.len >= min_len).collect();
                    assert!(full(&mut dec).copybacks == filtered, "min_len {min_len}");
                }
            }

            #[test]
            fn tracking_off_records_nothing_and_leaves_the_output_alone() {
                let case = case();
                let mut dec = decoder(&case.encoded);
                dec.tracking(false);
                let drained = full(&mut dec);
                assert_eq!(drained.out, case.data);
                assert!(drained.copybacks.is_empty());
            }

            #[test]
            fn turning_tracking_off_keeps_what_was_recorded_and_stops_there() {
                let case = case();
                let all = full(&mut decoder(&case.encoded)).copybacks;
                let mut dec = decoder(&case.encoded);
                let mut buffer = vec![0; 1 << 16];
                let mut out = Vec::new();
                let count = dec.read(&mut buffer).unwrap();
                out.extend_from_slice(&buffer[..count]);
                let before: Vec<Copyback> = dec.copybacks().iter().copied().collect();
                dec.tracking(false);
                loop {
                    let count = dec.read(&mut buffer).unwrap();
                    if count == 0 {
                        break;
                    }
                    out.extend_from_slice(&buffer[..count]);
                }
                assert_eq!(out, case.data);
                let after: Vec<Copyback> = dec.copybacks().iter().copied().collect();
                assert!(after == before, "the queue changed while tracking was off");
                assert!(!before.is_empty() && all.starts_with(&before));
            }
        }
    };
}

codec_tests!(
    xz,
    {
        let (data, planted) = sample(1, 600_000, 300_000, 20_000);
        let encoded = common::xz_encode(&data, 1 << 20);
        Case { data, planted, encoded, max_match: Some(XZ_MAX_MATCH) }
    },
    XzDecoder::new_multi_decoder
);

codec_tests!(
    zstd,
    {
        let (data, planted) = sample(2, 600_000, 300_000, 20_000);
        let encoded = common::zstd_encode(&data, 20);
        Case { data, planted, encoded, max_match: None }
    },
    |encoded| ZstdDecoder::new(encoded).unwrap()
);

codec_tests!(
    lz4,
    {
        let (data, planted) = sample(3, 600_000, 40_000, 8_000);
        let encoded = common::lz4_encode(&data, true, ::lz4::BlockSize::Max256KB);
        Case { data, planted, encoded, max_match: None }
    },
    |encoded| Lz4Decoder::new(encoded).unwrap()
);

codec_tests!(
    gzip,
    {
        let (data, planted) = sample(4, 600_000, 20_000, 5_000);
        let encoded = common::gzip_encode(&data, 6);
        Case { data, planted, encoded, max_match: Some(DEFLATE_MAX_MATCH) }
    },
    |encoded| GzipDecoder::with_buffer_size(encoded, 1 << 16)
);

/// Two inputs encoded separately and concatenated: the destinations of the second count on
/// from the end of the first, so its copybacks still check against the joint output.
fn check_concatenated(first: &[u8], second: &[u8], drained: &Drained) {
    assert_eq!(drained.out, [first, second].concat());
    check_copybacks(&drained.out, &drained.copybacks);
    assert!(drained.copybacks.iter().any(|c| c.destination > first.len() as u64));
}

#[test]
fn xz_offsets_run_on_across_concatenated_streams() {
    let (a, _) = sample(5, 200_000, 50_000, 5_000);
    let (b, _) = sample(6, 200_000, 50_000, 5_000);
    let encoded = [common::xz_encode(&a, 1 << 20), common::xz_encode(&b, 1 << 20)].concat();
    let drained = full(&mut XzDecoder::new_multi_decoder(&encoded[..]));
    check_concatenated(&a, &b, &drained);
}

#[test]
fn zstd_offsets_run_on_across_concatenated_frames() {
    let (a, _) = sample(7, 200_000, 50_000, 5_000);
    let (b, _) = sample(8, 200_000, 50_000, 5_000);
    let encoded = [common::zstd_encode(&a, 20), common::zstd_encode(&b, 20)].concat();
    let drained = full(&mut ZstdDecoder::new(&encoded[..]).unwrap());
    check_concatenated(&a, &b, &drained);
}

#[test]
fn gzip_offsets_run_on_across_concatenated_members() {
    let (a, _) = sample(9, 200_000, 20_000, 5_000);
    let (b, _) = sample(10, 200_000, 20_000, 5_000);
    let encoded = [common::gzip_encode(&a, 6), common::gzip_encode(&b, 6)].concat();
    let drained = full(&mut GzipDecoder::with_buffer_size(&encoded[..], 1 << 16));
    check_concatenated(&a, &b, &drained);
}

#[test]
fn lz4_stops_after_the_first_frame_like_upstream() {
    let (a, _) = sample(11, 200_000, 20_000, 5_000);
    let (b, _) = sample(12, 200_000, 20_000, 5_000);
    let encoded = [
        common::lz4_encode(&a, true, ::lz4::BlockSize::Max64KB),
        common::lz4_encode(&b, true, ::lz4::BlockSize::Max64KB),
    ]
    .concat();
    let drained = full(&mut Lz4Decoder::new(&encoded[..]).unwrap());
    assert_eq!(drained.out, a);
    check_copybacks(&drained.out, &drained.copybacks);
}

#[test]
fn xz_lzma_alone_streams_report_too() {
    use liblzma::stream::{LzmaOptions, Stream};
    use std::io::Write;
    let (data, planted) = sample(13, 300_000, 100_000, 10_000);
    let stream = Stream::new_lzma_encoder(&LzmaOptions::new_preset(6).unwrap()).unwrap();
    let mut encoder = liblzma::write::XzEncoder::new_stream(Vec::new(), stream);
    encoder.write_all(&data).unwrap();
    let encoded = encoder.finish().unwrap();
    let drained = full(&mut XzDecoder::new_multi_decoder(&encoded[..]));
    assert_eq!(drained.out, data);
    check_copybacks(&drained.out, &drained.copybacks);
    let (src, dst, len) = planted;
    assert!(covers(&drained.copybacks, src, dst, len, 0.9));
}

#[test]
fn xz_blocks_behind_a_bcj_filter_report_nothing() {
    let (plain, _) = sample(14, 200_000, 50_000, 5_000);
    let (filtered, _) = sample(15, 200_000, 50_000, 5_000);

    let bcj = common::xz_encode_bcj(&filtered);
    let drained = full(&mut XzDecoder::new_multi_decoder(&bcj[..]));
    assert_eq!(drained.out, filtered);
    assert!(drained.copybacks.is_empty(), "a BCJ block reported copybacks");

    // A plain stream followed by a filtered one: only the plain stream's blocks report.
    let encoded = [common::xz_encode(&plain, 1 << 20), bcj].concat();
    let drained = full(&mut XzDecoder::new_multi_decoder(&encoded[..]));
    assert_eq!(drained.out, [&plain[..], &filtered[..]].concat());
    assert!(!drained.copybacks.is_empty());
    check_copybacks(&drained.out, &drained.copybacks);
    let end = drained.copybacks.last().map(|c| c.destination + c.len as u64).unwrap();
    assert!(end <= plain.len() as u64, "the filtered stream reported copybacks");
}

#[test]
fn gzip_file_decompressor_reports_what_the_push_one_does() {
    use lz_copyback::gzip::{GzipFileDecompressor, LibdeflateDecompressResult};
    let (data, _) = sample(16, 600_000, 20_000, 5_000);
    let encoded = common::gzip_encode(&data, 6);
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("copybacks_file.gz");
    std::fs::write(&path, &encoded).unwrap();

    let mut decompressor = GzipFileDecompressor::new(&path, 1 << 16);
    let mut out = Vec::new();
    let mut copybacks = Vec::new();
    loop {
        let delivered = out.len() as u64;
        let result = decompressor.decompress().unwrap();
        for copyback in decompressor.peek_copybacks().drain(..) {
            assert!(copyback.destination >= delivered, "{copyback:?} pushed late");
            copybacks.push(copyback);
        }
        let pending = decompressor.pending_output();
        let count = pending.len();
        out.extend_from_slice(pending);
        decompressor.consume_output(count);
        if let LibdeflateDecompressResult::EndOfStream = result {
            break;
        }
    }
    let _ = std::fs::remove_file(&path);

    assert_eq!(out, data);
    check_copybacks(&out, &copybacks);
    assert!(copybacks == full(&mut GzipDecoder::with_buffer_size(&encoded[..], 1 << 16)).copybacks);
}
