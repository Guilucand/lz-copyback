//! The decoders end as the stock decoders do: a truncated input is an error, as is a zstd
//! window larger than zstd accepts by default, and each counts the input it took.

mod common;

use common::{Codec, decoder, full, try_drain};
use lz_copyback::xz::XzInPlaceDecoder;
use std::io::Read;

fn read_all(mut reader: impl Read) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    reader.read_to_end(&mut out)?;
    Ok(out)
}

fn data() -> Vec<u8> {
    common::sample(31, 300_000, 50_000, 5_000).0
}

fn fails(codec: Codec, encoded: &[u8]) -> std::io::Error {
    let mut decoder = decoder(codec, encoded, 1 << 20);
    match try_drain(&mut *decoder, 1 << 16, 0) {
        Ok(_) => panic!("{codec:?}: a truncated input decoded"),
        Err(error) => error,
    }
}

#[test]
fn truncated_inputs_fail_like_the_stock_decoders() {
    let data = data();

    let xz = common::xz_encode(&data, 1 << 20);
    let cut = &xz[..xz.len() / 2];
    assert!(read_all(liblzma::read::XzDecoder::new_multi_decoder(cut)).is_err());
    fails(Codec::Xz, cut);

    let zst = common::zstd_encode(&data, 20);
    let cut = &zst[..zst.len() / 2];
    assert!(read_all(zstd::stream::read::Decoder::new(cut).unwrap()).is_err());
    assert_eq!(fails(Codec::Zstd, cut).kind(), std::io::ErrorKind::UnexpectedEof);

    let gz = common::gzip_encode(&data, 6);
    let cut = &gz[..gz.len() / 2];
    assert!(read_all(flate2::read::GzDecoder::new(cut)).is_err());
    fails(Codec::Gzip, cut);

    // lz4's own decoder tells it only when finishing.
    let lz4 = common::lz4_encode(&data, true, ::lz4::BlockSize::Max64KB);
    let cut = &lz4[..lz4.len() / 2];
    let mut stock = ::lz4::Decoder::new(cut).unwrap();
    read_all(&mut stock).unwrap();
    assert!(stock.finish().1.is_err());
    assert_eq!(fails(Codec::Lz4, cut).kind(), std::io::ErrorKind::UnexpectedEof);
}

#[test]
fn every_decoder_counts_all_its_input() {
    let data = data();
    for (codec, encoded) in [
        (Codec::Xz, common::xz_encode(&data, 1 << 20)),
        (Codec::Zstd, common::zstd_encode(&data, 20)),
        (Codec::Lz4, common::lz4_encode(&data, true, ::lz4::BlockSize::Max64KB)),
        (Codec::Gzip, common::gzip_encode(&data, 6)),
    ] {
        let mut decoder = decoder(codec, &encoded, 1 << 20);
        assert_eq!(decoder.decode_to(u64::MAX).unwrap(), data.len() as u64, "{codec:?}");
        assert!(decoder.is_finished());
        assert_eq!(decoder.total_in(), encoded.len() as u64, "{codec:?}");
    }
}

#[test]
fn xz_single_stream_decoder_stops_after_the_first_stream() {
    let (a, b) = (data(), common::sample(32, 100_000, 20_000, 2_000).0);
    let encoded = [common::xz_encode(&a, 1 << 20), common::xz_encode(&b, 1 << 20)].concat();
    assert_eq!(full(Codec::Xz, &encoded).out, [&a[..], &b[..]].concat());
    let mut decoder = XzInPlaceDecoder::new(&encoded[..], 1 << 20);
    assert_eq!(common::drain(&mut decoder, 1 << 16, 0).out, a);
}

#[test]
fn zstd_refuses_windows_over_128_mib_as_zstd_does() {
    let data = data();
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
    encoder.long_distance_matching(true).unwrap();
    encoder.window_log(28).unwrap();
    std::io::Write::write_all(&mut encoder, &data).unwrap();
    let encoded = encoder.finish().unwrap();
    assert!(read_all(zstd::stream::read::Decoder::new(&encoded[..]).unwrap()).is_err());
    let mut decoder = decoder(Codec::Zstd, &encoded, 1 << 20);
    assert!(decoder.decode_to(u64::MAX).is_err());
}

/// A frame whose content falls short of the size its header declares is an error, also when it
/// ends with an empty last block, where zstd's buffer-less API does not check it (found by
/// fuzzing). zstd's own streaming decoder catches it only when the frame fits the caller's
/// buffer, and so takes its one-shot path, as the zstd command does.
#[test]
fn zstd_frame_shorter_than_its_content_size_is_an_error() {
    // A single-segment frame declaring 128 bytes, then an empty raw block, the last.
    let encoded = [0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x80, 0x01, 0x00, 0x00];
    let mut stock = zstd::stream::read::Decoder::new(&encoded[..]).unwrap();
    assert!(stock.read(&mut vec![0; 1 << 16]).is_err());
    let mut decoder = decoder(Codec::Zstd, &encoded, 1 << 20);
    assert!(decoder.decode_to(u64::MAX).is_err());
}

/// A `decode_to` that fails on a truncated input leaves readable the bytes it decoded before
/// the error, as a stock reader hands them out before it fails (found by fuzzing, for gzip).
fn check_bytes_before_the_error(codec: Codec, encoded: &[u8], data: &[u8]) {
    let cut = &encoded[..encoded.len() * 3 / 4];
    let mut decoder = decoder(codec, cut, 1 << 20);
    assert!(decoder.decode_to(u64::MAX).is_err(), "{codec:?}: a truncated input decoded");
    let decoded = decoder.decoded_len();
    assert!(decoded > 0, "{codec:?}: nothing readable before the error");
    let bytes: Vec<u8> = decoder.bytes(0..decoded).flatten().copied().collect();
    assert!(bytes == data[..decoded as usize], "{codec:?}: the bytes before the error differ");
}

#[test]
fn the_bytes_decoded_before_an_error_are_readable() {
    let data = data();
    check_bytes_before_the_error(Codec::Xz, &common::xz_encode(&data, 1 << 20), &data);
    check_bytes_before_the_error(Codec::Zstd, &common::zstd_encode(&data, 20), &data);
    check_bytes_before_the_error(Codec::Lz4, &common::lz4_encode(&data, true, ::lz4::BlockSize::Max64KB), &data);
}

#[test]
#[ignore = "streaming-libdeflate-rs 0.3.0 decodes the last instruction from the zero padding after a truncated input, and fails only after writing it"]
fn gzip_bytes_decoded_before_an_error_are_readable() {
    let data = data();
    check_bytes_before_the_error(Codec::Gzip, &common::gzip_encode(&data, 6), &data);
}
