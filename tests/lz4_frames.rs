//! The lz4 decoder must report exactly the matches its frame encodes: every sequence of
//! every compressed block, once, whole, at its position in the output. The format is simple
//! enough to read them straight from the compressed bytes, independently of liblz4.

mod common;

use lz_copyback::Copyback;

/// Every match of a single lz4 frame, as `(destination, offset, len)`, and the frame's
/// decoded size.
fn frame_matches(frame: &[u8]) -> (Vec<Copyback>, u64) {
    fn length(block: &[u8], i: &mut usize, mut value: usize) -> usize {
        if value == 15 {
            loop {
                let byte = block[*i];
                *i += 1;
                value += byte as usize;
                if byte != 255 {
                    break;
                }
            }
        }
        value
    }

    assert_eq!(u32::from_le_bytes(frame[0..4].try_into().unwrap()), 0x184D_2204, "not an lz4 frame");
    let flags = frame[4];
    let block_checksums = flags & 0x10 != 0;
    let mut p = 6; // magic, FLG, BD
    if flags & 0x08 != 0 {
        p += 8; // content size
    }
    if flags & 0x01 != 0 {
        p += 4; // dictionary id
    }
    p += 1; // header checksum

    let mut matches = Vec::new();
    let mut pos = 0u64;
    loop {
        let word = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap());
        p += 4;
        if word == 0 {
            break; // end mark
        }
        let size = (word & 0x7FFF_FFFF) as usize;
        let block = &frame[p..p + size];
        p += size + if block_checksums { 4 } else { 0 };
        if word & 0x8000_0000 != 0 {
            pos += size as u64; // stored uncompressed
            continue;
        }
        let mut i = 0;
        loop {
            let token = block[i];
            i += 1;
            let literals = length(block, &mut i, (token >> 4) as usize);
            i += literals;
            pos += literals as u64;
            if i == block.len() {
                break; // the last sequence has no match
            }
            let offset = u16::from_le_bytes([block[i], block[i + 1]]) as u32;
            i += 2;
            let len = length(block, &mut i, (token & 15) as usize) + 4;
            matches.push(Copyback { destination: pos, offset, len: len as u32 });
            pos += len as u64;
        }
    }
    (matches, pos)
}

#[test]
fn reported_matches_are_exactly_the_frame_sequences() {
    let (data, _) = common::sample(21, 900_000, 30_000, 10_000);
    for linked in [true, false] {
        for block_size in [lz4::BlockSize::Max64KB, lz4::BlockSize::Max256KB, lz4::BlockSize::Max4MB] {
            let encoded = common::lz4_encode(&data, linked, block_size.clone());
            let (expected, size) = frame_matches(&encoded);
            assert_eq!(size, data.len() as u64);
            assert!(!expected.is_empty());
            for (chunk, ahead) in [(7, 0), (1 << 16, 1 << 20), (8 << 20, 0)] {
                let mut decoder = lz_copyback::lz4::Lz4InPlaceDecoder::new(&encoded[..], chunk + ahead, u32::MAX).unwrap();
                let drained = common::drain(&mut decoder, chunk, ahead);
                assert_eq!(drained.out, data);
                assert!(
                    drained.copybacks == expected,
                    "linked {linked}, {block_size:?}, chunk {chunk}: {} reported, {} in the frame",
                    drained.copybacks.len(),
                    expected.len()
                );
            }
        }
    }
}
