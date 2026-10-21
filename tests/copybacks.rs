//! The copyback contract, checked on each of the four decoders: the output is unchanged,
//! every copyback is a correct repeat pushed no later than its first byte, the matches
//! continuing one another merged into it, the list does not depend on how far each
//! `decode_to` goes, and the threshold and the tracking switch behave.

mod common;

use common::{
    Codec, Drained, check_copybacks, continues, covers, decoder, decoder_hard, drain, drain_min, full, gzip_encode,
    lz4_encode,
    merged, read_out, sample, xz_encode, zstd_encode,
};
use lz_copyback::Copyback;

/// The longest match each format can encode, when it has a limit: a longer repeat is cut into
/// matches, which the decoders merge back.
const XZ_MAX_MATCH: u32 = 273;
const DEFLATE_MAX_MATCH: u32 = 258;

/// One test input, and the facts the generic tests check it against.
struct Case {
    codec: Codec,
    data: Vec<u8>,
    planted: (u64, u64, u64),
    encoded: Vec<u8>,
    max_match: Option<u32>,
}

macro_rules! codec_tests {
    ($name:ident, $case:expr) => {
        mod $name {
            use super::*;

            fn case() -> Case {
                $case
            }

            #[test]
            fn output_is_unchanged_and_copybacks_are_correct_repeats() {
                let case = case();
                let drained = full(case.codec, &case.encoded);
                assert_eq!(drained.out, case.data);
                assert!(!drained.copybacks.is_empty());
                check_copybacks(&drained.out, &drained.copybacks);
                let (src, dst, len) = case.planted;
                assert!(covers(&drained.copybacks, src, dst, len, 0.9), "planted repeat not found");
                if let Some(max) = case.max_match {
                    let longest = drained.copybacks.iter().map(|c| c.len).max().unwrap();
                    assert!(longest > max, "the matches of the planted repeat were not merged: {longest} bytes");
                }
            }

            #[test]
            fn matches_continuing_the_last_copyback_extend_it() {
                let case = case();
                let mut decoder = decoder(case.codec, &case.encoded, 1 << 20);
                // Nothing taken out of the queue: every continuation finds its copyback there.
                assert_eq!(read_out(&mut *decoder, 1 << 16), case.data);
                let all: Vec<Copyback> = decoder.peek_copybacks().iter().copied().collect();
                check_copybacks(&case.data, &all);
                assert!(!all.windows(2).any(|pair| continues(&pair[0], &pair[1])), "a continuation not merged");
            }

            #[test]
            fn copybacks_do_not_depend_on_how_far_each_step_decodes() {
                let case = case();
                let reference = full(case.codec, &case.encoded).copybacks;
                for (chunk, ahead) in [(7, 0), (7, 4099), (4099, 1 << 16), (1 << 20, 0)] {
                    let drained = drain(&mut *decoder(case.codec, &case.encoded, chunk + ahead), chunk, ahead);
                    assert_eq!(drained.out, case.data, "chunk {chunk}, ahead {ahead}");
                    // Taken out at other times, the queue merged other continuations.
                    assert!(merged(&drained.copybacks) == merged(&reference), "chunk {chunk}, ahead {ahead} changed the copybacks");
                }
            }

            #[test]
            fn min_len_filters_the_merged_copybacks() {
                let case = case();
                let all = merged(&full(case.codec, &case.encoded).copybacks);
                for min_len in [4, 40, 300] {
                    let mut decoder = decoder(case.codec, &case.encoded, 1 << 20);
                    let filtered = drain_min(&mut *decoder, 1 << 16, 0, min_len).copybacks;
                    // The planted repeat, longer than any match a format writes, passes merged.
                    let (src, dst, len) = case.planted;
                    assert!(covers(&filtered, src, dst, len, 0.9), "min_len {min_len}: planted repeat not found");
                    // Each of at least min_len, or the rest of the one before, taken out of the
                    // queue as it grew, within a copyback of all the matches.
                    for (index, copyback) in filtered.iter().enumerate() {
                        let rest = index > 0 && continues(&filtered[index - 1], copyback);
                        assert!(copyback.len >= min_len || rest, "{copyback:?} with min_len {min_len}");
                        let within = all.iter().any(|outer| {
                            outer.offset == copyback.offset
                                && outer.destination <= copyback.destination
                                && copyback.destination + copyback.len as u64 <= outer.destination + outer.len as u64
                        });
                        assert!(within, "{copyback:?} with min_len {min_len} is no part of a copyback of all matches");
                    }
                }
            }

            #[test]
            fn tracking_off_records_nothing_and_leaves_the_output_alone() {
                let case = case();
                let mut decoder = decoder(case.codec, &case.encoded, 1 << 20);
                decoder.set_copybacks_tracking(false);
                let drained = drain(&mut *decoder, 1 << 16, 0);
                assert_eq!(drained.out, case.data);
                assert!(drained.copybacks.is_empty());
            }

            #[test]
            fn turning_tracking_off_keeps_what_was_recorded_and_stops_there() {
                let case = case();
                let all = full(case.codec, &case.encoded).copybacks;
                let mut decoder = decoder(case.codec, &case.encoded, 1 << 20);
                decoder.decode_to(1 << 16).unwrap();
                let before: Vec<Copyback> = decoder.peek_copybacks().iter().copied().collect();
                decoder.set_copybacks_tracking(false);
                assert_eq!(read_out(&mut *decoder, 1 << 16), case.data);
                let after: Vec<Copyback> = decoder.peek_copybacks().iter().copied().collect();
                assert!(after == before, "the queue changed while tracking was off");
                // All of them merged, but the last, which matches decoded later can extend.
                let (all, before) = (merged(&all), merged(&before));
                let (last, earlier) = before.split_last().expect("copybacks recorded");
                assert!(all.starts_with(earlier));
                let next = &all[earlier.len()];
                assert!((last.destination, last.offset) == (next.destination, next.offset) && last.len <= next.len);
            }
        }
    };
}

codec_tests!(xz, {
    let (data, planted) = sample(1, 600_000, 300_000, 20_000);
    let encoded = common::xz_encode(&data, 1 << 20);
    Case { codec: Codec::Xz, data, planted, encoded, max_match: Some(XZ_MAX_MATCH) }
});

codec_tests!(zstd, {
    let (data, planted) = sample(2, 600_000, 300_000, 20_000);
    let encoded = common::zstd_encode(&data, 20);
    Case { codec: Codec::Zstd, data, planted, encoded, max_match: None }
});

codec_tests!(lz4, {
    let (data, planted) = sample(3, 600_000, 40_000, 8_000);
    let encoded = common::lz4_encode(&data, true, ::lz4::BlockSize::Max256KB);
    Case { codec: Codec::Lz4, data, planted, encoded, max_match: None }
});

codec_tests!(gzip, {
    let (data, planted) = sample(4, 600_000, 20_000, 5_000);
    let encoded = common::gzip_encode(&data, 6);
    Case { codec: Codec::Gzip, data, planted, encoded, max_match: Some(DEFLATE_MAX_MATCH) }
});

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
    check_concatenated(&a, &b, &full(Codec::Xz, &encoded));
}

#[test]
fn zstd_offsets_run_on_across_concatenated_frames() {
    let (a, _) = sample(7, 200_000, 50_000, 5_000);
    let (b, _) = sample(8, 200_000, 50_000, 5_000);
    let encoded = [common::zstd_encode(&a, 20), common::zstd_encode(&b, 20)].concat();
    check_concatenated(&a, &b, &full(Codec::Zstd, &encoded));
}

#[test]
fn gzip_offsets_run_on_across_concatenated_members() {
    let (a, _) = sample(9, 200_000, 20_000, 5_000);
    let (b, _) = sample(10, 200_000, 20_000, 5_000);
    let encoded = [common::gzip_encode(&a, 6), common::gzip_encode(&b, 6)].concat();
    check_concatenated(&a, &b, &full(Codec::Gzip, &encoded));
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
    let drained = full(Codec::Lz4, &encoded);
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
    let drained = full(Codec::Xz, &encoded);
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
    let drained = full(Codec::Xz, &bcj);
    assert_eq!(drained.out, filtered);
    assert!(drained.copybacks.is_empty(), "a BCJ block reported copybacks");

    // A plain stream followed by a filtered one: only the plain stream's blocks report.
    let encoded = [common::xz_encode(&plain, 1 << 20), bcj].concat();
    let drained = full(Codec::Xz, &encoded);
    assert_eq!(drained.out, [&plain[..], &filtered[..]].concat());
    assert!(!drained.copybacks.is_empty());
    check_copybacks(&drained.out, &drained.copybacks);
    let end = drained.copybacks.last().map(|c| c.destination + c.len as u64).unwrap();
    assert!(end <= plain.len() as u64, "the filtered stream reported copybacks");
}

/// The tail of a repeat cut into matches is kept however short, below the hard threshold too: the
/// copyback of a planted repeat runs to its end. The lengths leave xz's and deflate's last match
/// of all sizes, those under the hard threshold (128 bytes) included.
#[test]
fn the_tails_of_repeats_are_kept_however_short() {
    for codec in [Codec::Xz, Codec::Gzip, Codec::Zstd, Codec::Lz4] {
        for len in (2000..2600).step_by(23) {
            let (data, (src, dst, len)) = sample(7, 120_000, 30_000, len);
            let encoded = match codec {
                Codec::Xz => xz_encode(&data, 1 << 20),
                Codec::Gzip => gzip_encode(&data, 6),
                Codec::Zstd => zstd_encode(&data, 20),
                Codec::Lz4 => lz4_encode(&data, true, ::lz4::BlockSize::Max64KB),
            };
            let copybacks = drain_min(&mut *decoder_hard(codec, &encoded, 1 << 20, 128), 1 << 16, 0, 128).copybacks;
            // Random bytes follow the repeat, so its copyback ends with it, or a byte or two later.
            let end = dst + len;
            assert!(
                copybacks.iter().any(|c| c.offset as u64 == dst - src && c.destination + c.len as u64 >= end),
                "{codec:?}, len {len}: the repeat's copyback stops before its end"
            );
        }
    }
}
