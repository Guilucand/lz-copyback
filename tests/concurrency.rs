//! Decoding several streams on different threads must give exactly the result of decoding
//! them one after another: backends are independent and the C hooks are per-thread or
//! per-instance.

mod common;

use common::{fasta, gzip_encode, lz4_encode, xz_encode, zstd_encode};
use lz_copyback::{Codec, LzCopy, TrackerConfig, TrackerStats, open_reader};

fn decode_all(bytes: Vec<u8>, codec: Codec) -> (Vec<u8>, Vec<LzCopy>, TrackerStats) {
    let mut dec = open_reader(std::io::Cursor::new(bytes), codec, TrackerConfig::default().with_window(256 << 10)).unwrap();
    let mut out = Vec::new();
    let mut copies = Vec::new();
    loop {
        let w = dec.next_window().unwrap().unwrap();
        out.extend_from_slice(w.data);
        copies.extend_from_slice(w.copies);
        if w.is_last {
            break;
        }
    }
    (out, copies, dec.stats())
}

#[test]
fn eight_streams_decoded_concurrently_match_sequential_results() {
    let streams: Vec<(Vec<u8>, Codec, Vec<u8>)> = (0..8u64)
        .map(|i| {
            let data = fasta(900 + i, 120, 4096, 0.7, 4);
            let (bytes, codec) = match i % 4 {
                0 => (gzip_encode(&data, 6), Codec::Gzip),
                1 => (xz_encode(&data, 1 << 20), Codec::Xz),
                2 => (zstd_encode(&data, 20), Codec::Zstd),
                _ => (lz4_encode(&data, true), Codec::Lz4),
            };
            (bytes, codec, data)
        })
        .collect();

    let sequential: Vec<_> = streams.iter().map(|(b, c, _)| decode_all(b.clone(), *c)).collect();
    let concurrent: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = streams.iter().map(|(b, c, _)| scope.spawn(move || decode_all(b.clone(), *c))).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for (i, ((seq, con), (_, _, data))) in sequential.iter().zip(&concurrent).zip(&streams).enumerate() {
        assert_eq!(&seq.0, data, "stream {i} bytes");
        assert_eq!(seq.0, con.0, "stream {i} bytes differ between runs");
        assert_eq!(seq.1, con.1, "stream {i} copies differ between runs");
        assert_eq!(seq.2, con.2, "stream {i} stats differ between runs");
        assert!(!seq.1.is_empty(), "stream {i} found copies");
    }
}
