//! `open` must pick the backend from the file extension the way ggcat does and decode
//! every supported format to the same bytes.

mod common;

use common::{drain, fasta, gzip_encode, lz4_encode, temp_path, xz_encode, zstd_encode};
use lz_copyback::{Codec, TrackerConfig, codec_of, is_archive, open};
use std::io::Write;
use std::path::Path;

#[test]
fn every_codec_is_selected_by_extension_and_decodes() {
    let data = fasta(51, 60, 4096, 0.6, 4);
    let mut bz = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
    bz.write_all(&data).unwrap();
    let bz = bz.finish().unwrap();
    let cases: Vec<(&str, Vec<u8>, Codec, bool)> = vec![
        ("x.fa.gz", gzip_encode(&data, 6), Codec::Gzip, true),
        ("x.fa.xz", xz_encode(&data, 1 << 20), Codec::Xz, true),
        ("x.fa.zst", zstd_encode(&data, 18), Codec::Zstd, true),
        ("x.fa.zstd", zstd_encode(&data, 18), Codec::Zstd, true),
        ("x.fa.lz4", lz4_encode(&data, true), Codec::Lz4, true),
        ("x.fa.bz2", bz, Codec::Bzip2, false),
        ("x.fa", data.clone(), Codec::Plain, false),
    ];
    for (name, bytes, codec, lz) in cases {
        let path = temp_path(name);
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(codec_of(&path), codec, "{name}");
        let mut dec = open(&path, TrackerConfig::default()).unwrap();
        let snaps = drain(&mut dec, &data, 62);
        let copies: usize = snaps.iter().map(|s| s.copies.len()).sum();
        assert_eq!(copies > 0, lz, "{name}: {copies} copies");
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn archive_rules_match_ggcat() {
    for name in ["a.tar", "a.tar.xz", "a.tar.gz", "a.tgz", "a.tbz2", "a.txz", "a.tar.zst", "a.tar.lz4"] {
        assert!(is_archive(Path::new(name)), "{name}");
    }
    for name in ["a.fa.gz", "a.fa", "tar.gz", "a.tarx"] {
        assert!(!is_archive(Path::new(name)), "{name}");
    }
    assert_eq!(codec_of(Path::new("a.tgz")), Codec::Gzip);
    assert_eq!(codec_of(Path::new("a.txz")), Codec::Xz);
}
