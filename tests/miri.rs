//! Small inputs for Miri, which runs the Rust code of the gzip decoder (its ring, raw pointers
//! into it, the pieces handed out) and of the ring bookkeeping under its checks of undefined
//! behaviour; the C decoders cannot run under it. The ring wraps several times, with chunks
//! that straddle its end, across members and stored blocks. Natively, these run with the rest.
//!
//! ```text
//! cargo +nightly miri test --test miri
//! ```

mod common;

use common::{Codec, check_copybacks, decoder, drain_min, gzip_encode};

/// Repeats at a few distances, cheap to make under Miri.
fn data(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        if out.len() < 4096 || state % 3 == 0 {
            out.extend((0..64).map(|i| b"ACGT"[((state >> (i % 60)) & 3) as usize]));
        } else {
            let distance = 1 + (state >> 8) as usize % out.len().min(30_000);
            for _ in 0..200 + (state >> 40) as usize % 300 {
                out.push(out[out.len() - distance]);
            }
        }
    }
    out.truncate(len);
    out
}

#[test]
fn gzip_ring_wraps_with_chunks_across_its_end() {
    // The ring holds about 160 KiB besides the bytes to retain: 400 KB wrap it twice.
    let first = data(400_000, 1);
    let second = data(30_000, 2);
    let encoded = [gzip_encode(&first, 1), gzip_encode(&second, 0), gzip_encode(b"", 6)].concat();
    let expected = [first, second].concat();
    // 3001 shares no factor with the ring, so chunks straddle its end.
    for (chunk, ahead) in [(3001, 500), (64 << 10, 0)] {
        let mut decoder = decoder(Codec::Gzip, &encoded, chunk + ahead);
        let drained = drain_min(&mut *decoder, chunk, ahead, 20);
        assert!(drained.out == expected, "chunk {chunk}: the bytes differ");
        check_copybacks(&drained.out, &drained.copybacks);
        assert!(!drained.copybacks.is_empty());
    }
}

#[test]
fn gzip_truncated_and_empty_inputs() {
    let encoded = gzip_encode(&data(50_000, 3), 6);
    for cut in [0, 5, 18, encoded.len() / 2, encoded.len() - 1] {
        let mut decoder = decoder(Codec::Gzip, &encoded[..cut], 1 << 12);
        assert!(common::try_drain(&mut *decoder, 1000, 0).is_err(), "a cut at {cut} decoded");
    }
}
