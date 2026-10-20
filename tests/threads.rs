//! Many decoders at once, on as many threads: the hooks and liblzma's in-place state are
//! per thread, so each thread's decoders must read their own bytes and copybacks, whatever
//! the others do.

mod common;

use common::{Codec, check_copybacks, decoder, drain, gzip_encode, lz4_encode, sample, xz_encode, zstd_encode};

#[test]
fn decoders_on_sixteen_threads_at_once_read_their_own_streams() {
    std::thread::scope(|scope| {
        for thread in 0..16u64 {
            scope.spawn(move || {
                for round in 0..3u64 {
                    let seed = thread * 100 + round;
                    let (data, _) = sample(seed, 400_000 + (seed as usize % 7) * 50_000, 100_000, 10_000);
                    let codec = [Codec::Xz, Codec::Zstd, Codec::Lz4, Codec::Gzip][((thread + round) % 4) as usize];
                    let encoded = match codec {
                        Codec::Xz => xz_encode(&data, 1 << 20),
                        Codec::Zstd => zstd_encode(&data, 20),
                        Codec::Lz4 => lz4_encode(&data, thread % 2 == 0, lz4::BlockSize::Max64KB),
                        Codec::Gzip => gzip_encode(&data, 6),
                    };
                    // Small chunks and look-ahead, so that each decoder makes many calls into C
                    // while the other threads make theirs.
                    let (chunk, ahead) = (1000 + thread as usize * 37, 20_000);
                    let mut decoder = decoder(codec, &encoded, chunk + ahead);
                    decoder.set_copybacks_min_len(8);
                    let drained = drain(&mut *decoder, chunk, ahead);
                    assert!(drained.out == data, "thread {thread}, {codec:?}: the bytes differ");
                    check_copybacks(&drained.out, &drained.copybacks);
                    assert!(
                        drained.copybacks.iter().all(|copyback| copyback.len >= 8),
                        "thread {thread}: another thread's threshold"
                    );
                }
            });
        }
    });
}
