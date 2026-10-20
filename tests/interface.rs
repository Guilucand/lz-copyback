//! The decoders keep the behaviour of the library interfaces they reproduce: how a truncated
//! or concatenated input ends, and the extra methods callers rely on.

mod common;

use lz_copyback::gzip::{GzipDecoder, GzipPushDecompressor};
use lz_copyback::lz4::Lz4Decoder;
use lz_copyback::xz::XzDecoder;
use lz_copyback::zstd::ZstdDecoder;
use std::io::Read;

fn read_all(mut reader: impl Read) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    reader.read_to_end(&mut out)?;
    Ok(out)
}

fn data() -> Vec<u8> {
    common::sample(31, 300_000, 50_000, 5_000).0
}

#[test]
fn truncated_inputs_fail_like_the_stock_decoders() {
    let data = data();

    let xz = common::xz_encode(&data, 1 << 20);
    let cut = &xz[..xz.len() / 2];
    assert!(read_all(liblzma::read::XzDecoder::new_multi_decoder(cut)).is_err());
    assert!(read_all(XzDecoder::new_multi_decoder(cut)).is_err());

    let zst = common::zstd_encode(&data, 20);
    let cut = &zst[..zst.len() / 2];
    assert!(read_all(zstd::stream::read::Decoder::new(cut).unwrap()).is_err());
    let error = read_all(ZstdDecoder::new(cut).unwrap()).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);

    let gz = common::gzip_encode(&data, 6);
    assert!(read_all(GzipDecoder::new(&gz[..gz.len() / 2])).is_err());
}

#[test]
fn lz4_reports_truncation_only_when_finishing_like_upstream() {
    let data = data();
    let lz4 = common::lz4_encode(&data, true, ::lz4::BlockSize::Max64KB);
    let cut = &lz4[..lz4.len() / 2];

    let mut stock = ::lz4::Decoder::new(cut).unwrap();
    let mut decoder = Lz4Decoder::new(cut).unwrap();
    assert_eq!(read_all(&mut decoder).unwrap(), read_all(&mut stock).unwrap());
    assert!(stock.finish().1.is_err());
    assert!(decoder.finish().1.is_err());

    let mut decoder = Lz4Decoder::new(&lz4[..]).unwrap();
    assert_eq!(read_all(&mut decoder).unwrap(), data);
    assert!(decoder.finish().1.is_ok());
}

#[test]
fn xz_single_stream_decoder_and_counters() {
    let data = data();
    let xz = common::xz_encode(&data, 1 << 20);
    let mut decoder = XzDecoder::new(&xz[..]);
    assert_eq!(read_all(&mut decoder).unwrap(), data);
    assert_eq!(decoder.total_out(), data.len() as u64);
    assert_eq!(decoder.total_in(), xz.len() as u64);
}

#[test]
fn zstd_single_frame_stops_after_the_first_frame() {
    let (a, b) = (data(), common::sample(32, 100_000, 20_000, 2_000).0);
    let encoded = [common::zstd_encode(&a, 20), common::zstd_encode(&b, 20)].concat();
    assert_eq!(read_all(ZstdDecoder::new(&encoded[..]).unwrap()).unwrap(), [&a[..], &b[..]].concat());
    assert_eq!(read_all(ZstdDecoder::new(&encoded[..]).unwrap().single_frame()).unwrap(), a);
}

#[test]
fn zstd_window_log_max_admits_long_windows() {
    let data = data();
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
    encoder.long_distance_matching(true).unwrap();
    encoder.window_log(28).unwrap();
    std::io::Write::write_all(&mut encoder, &data).unwrap();
    let encoded = encoder.finish().unwrap();

    assert!(read_all(ZstdDecoder::new(&encoded[..]).unwrap()).is_err(), "a 256 MiB window by default");
    let mut decoder = ZstdDecoder::new(&encoded[..]).unwrap();
    decoder.window_log_max(28).unwrap();
    assert_eq!(read_all(decoder).unwrap(), data);
}

#[test]
fn gzip_push_and_finish_hand_out_everything_with_its_copybacks() {
    let data = data();
    let gz = common::gzip_encode(&data, 6);
    let mut decompressor = GzipPushDecompressor::new(1 << 16);
    let mut out = Vec::new();
    for chunk in gz.chunks(10_000) {
        decompressor.push(chunk, |bytes| {
            out.extend_from_slice(bytes);
            Ok(())
        })
        .unwrap();
    }
    decompressor.finish(|bytes| {
        out.extend_from_slice(bytes);
        Ok(())
    })
    .unwrap();
    assert_eq!(out, data);
    let copybacks: Vec<_> = decompressor.peek_copybacks().drain(..).collect();
    common::check_copybacks(&out, &copybacks);
    assert!(copybacks == common::drain(&mut GzipDecoder::new(&gz[..]), 1 << 16).copybacks);
}
