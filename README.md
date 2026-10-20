# lz-copyback

Decoders for the LZ formats ggcat reads (xz, zstd, lz4, gzip) that decode in place and also
report the LZ matches ("copybacks") they decode: which output bytes are verbatim repeats of
earlier output. A decoder never copies its output into the caller's buffer: it decodes into a
ring buffer of its own and hands the bytes out of it, so that each byte is written once, by
the decoder, and read where it is.

| codec | decoder | the ring | runs on |
|---|---|---|---|
| xz (`.xz`, `.lzma`, `.lz`) | `xz::XzInPlaceDecoder` | liblzma's own dictionary, patched to skip its copy out of it | `external/lzma-sys`: liblzma-sys 0.4.9 (XZ 5.8.4), patched |
| zstd | `zstd::ZstdInPlaceDecoder` | its own, filled a block at a time by zstd's buffer-less API | `external/zstd-sys`: zstd-sys 2.1.2 (zstd 1.5.7), patched |
| lz4 | `lz4::Lz4InPlaceDecoder` | its own, which liblz4 fills a block at a time, fed one block of input at a time | `external/lz4-sys`: lz4-sys 1.11.1 (liblz4 1.10.0), patched |
| gzip | `gzip::GzipInPlaceDecoder` | its own, with 32 KiB of history copied in front of it at each wrap | `streaming-libdeflate-rs` 0.3.0, unpatched |

Codecs without an LZ stage (bzip2, plain files) are out of scope.

## Decoding in place

All four decoders implement the `InPlaceDecoder` trait:

```rust
use lz_copyback::{InPlaceDecoder, xz::XzInPlaceDecoder};

// Keeps the last 1 MiB + 64 KiB readable behind what it was asked to decode.
let mut decoder = XzInPlaceDecoder::new_multi_decoder(file, (1 << 20) + (64 << 10));
decoder.set_copybacks_min_len(62);
let mut position = 0;
loop {
    // A chunk of 64 KiB, with every copyback up to 1 MiB past it.
    let decoded = decoder.decode_to(position + (64 << 10) + (1 << 20))?;
    let end = decoded.min(position + (64 << 10));
    if end == position {
        break;
    }
    let (pieces, copybacks) = decoder.bytes_and_copybacks(position..end);
    for piece in pieces {
        // The chunk, in one piece, or more where the ring wraps around.
    }
    while let Some(copyback) = copybacks.pop_front() {
        // copyback.len bytes at copyback.destination repeat those at copyback.source().
    }
    position = end;
}
```

* **`retain`.** After `decode_to(target)`, every byte from `target - retain` to the end of
  what was decoded is readable with `bytes` (in more than one piece where the ring wraps),
  until the next `decode_to`. The ring is sized for it: liblzma's dictionary is at least
  `retain` + 1 MiB, the other rings `retain` plus a few blocks.
* **Steps.** `decode_to` may decode up to a block past `target` (zstd, lz4) or a few hundred
  bytes (gzip); xz stops exactly there. The output and the copybacks do not depend on how far
  each step goes.
* **`total_in`**: the compressed bytes taken from the input, for the compression ratio.
* **Ends.** An input that ends inside a stream, frame or member is an error, as is a zstd
  window larger than the 128 MiB zstd accepts by default. Concatenated xz streams (and `.lzma`
  and `.lz` input with `new_multi_decoder`), zstd frames and gzip members are decoded one after
  the other; like upstream's, the lz4 decoder stops after the first frame.
* **xz specifics.** A reset of the dictionary (every Block starts with one) keeps the decoded
  bytes in place, and a dictionary liblzma replaces (a larger one for the next stream, or one
  freed with its filter chain) is kept until its bytes are no longer readable. A Block whose
  filter chain is not a lone LZMA2 is decoded, with a copy, into a ring of the decoder's own,
  as its output is not in the dictionary.

## Copybacks

```rust
decoder.set_copybacks_tracking(true);      // on at construction; off ignores further matches
decoder.set_copybacks_min_len(62);         // 1 at construction: every match is recorded
let queue: &mut VecDeque<Copyback> = decoder.peek_copybacks();
```

```rust
pub struct Copyback {
    pub destination: u64, // absolute offset of the first byte the match writes
    pub offset: u32,      // the match copies from destination - offset on (Copyback::source)
    pub len: u32,         // bytes written
}
```

The decoder only ever pushes to the back of the queue; removing entries is up to the caller.

* **Raw matches.** Every match exactly as the decoder decoded it: never merged with the next
  one, never clipped, never split. A long repeat is as many matches as the compressor
  wrote (deflate stops at 258 bytes, LZMA at 273), and a match may overlap its own output
  (`len > offset`, a periodic repeat). `set_copybacks_min_len` filters on this raw length.
* **Absolute destinations,** counted from the first byte the decoder produced, across
  concatenated xz streams, zstd frames and gzip members.
* **No later than the bytes.** A copyback is in the queue by the time the `decode_to` call
  that decodes its first destination byte returns:

  | codec | when a match is pushed |
  |---|---|
  | gzip | as it is decoded |
  | xz | as it is decoded; a match cut by the end of the step is still reported once, whole |
  | zstd | when its block (at most 128 KiB) is decoded, possibly a step before its bytes |
  | lz4 | when its block (64 KiB to 4 MiB, as the frame says) is decoded, possibly a step before its bytes |

* **xz filters.** A Block whose filter chain is not a lone LZMA2 (BCJ, delta) reports
  nothing: those filters rewrite the LZ output, so its matches do not repeat output bytes.
  Such chains are only written on request (`xz --x86`, `--delta`, `--filters=`), never for
  sequence data.

## How it works

The C libraries are patched where they decode a match. Each decoder installs the hook, with
a pointer to its queue, right before every native decoding call and removes it right after
(the decoders move, so no pointer outlives a call). With tracking off the hook is not
installed: the C decoders pay one NULL check per match, and the gzip decoder runs the stock
decoding loop, with no hook at all.

* **xz** (`external/lzma-sys`): reported from the LZMA decoder when a match's length is
  decoded, not from `dict_repeat`, which stops at the end of the step and resumes the copy
  in the next one. The block decoder suppresses the hook for filtered Blocks. The fork also
  decodes in place only: the caller hands liblzma its own dictionary as the output buffer,
  and liblzma skips the copy into it.
* **zstd** (`external/zstd-sys`): reported at the eight sequence-execution sites; a sequence
  never spans blocks. The decoder uses zstd's buffer-less API, which decodes each block into
  the place the caller gives.
* **lz4** (`external/lz4-sys`): reported at the four places `LZ4_decompress_generic` completes
  a match; the frame decoder publishes the hook and the block's absolute offset around each
  block it decodes, straight into the caller's buffer when it has room for a whole block.
* **gzip**: `streaming-libdeflate-rs` decodes into any `DeflateOutput`, and calls its
  `on_lz_copy` for every match when the output asks for it; the decoder's ring is such an
  output, which asks for them only while tracking.

Each fork's `PATCH.md` describes its change and how to replay it on a newer upstream, and
`ggcat-lz-hook.patch` holds it as a diff.

## Coexisting with the stock crates

The C forks can be linked into the same binary as the genuine `liblzma-sys`, `zstd-sys` and
`lz4-sys`: they claim their own `links` keys (`lzcb_lzma`, `lzcb_zstd`, `lzcb_lz4`), build
archives of their own names (`liblzcb_lzma.a`, `liblzcb_zstd.a`, `liblzcb_lz4.a`), and every
exported C symbol is renamed into the `lzcb_` namespace by a generated header,
`lzcb_prefix.h`, force-included into every translation unit. Each fork's `tests/symbols.rs`
fails if a symbol of its built archive lost the prefix, and this crate's tests link the
stock crates next to the forks.

## Development

* `cargo test --workspace`:
  * `tests/copybacks.rs`: on all four decoders, correct raw matches, pushed in time,
    independent of how far each step decodes, the threshold and the switch;
  * `tests/lz4_frames.rs`: the lz4 matches equal the ones read straight from the frames;
  * `tests/in_place.rs`: the decoders read the same bytes as the stock decoders, a chunk at
    a time with the retained bytes checked after every step, while their rings wrap, across
    frames, members, blocks, filter chains and dictionary sizes;
  * `tests/interface.rs`: truncated inputs and oversized zstd windows fail as with the stock
    decoders, and `total_in` counts all the input;
  * the symbol audits of the three forks.
* `cargo bench --bench decode -- [--synthetic-mib N] [--read-size BYTES] [--window BYTES]
  [--real FILE.xz --scratch DIR] [--threads N] [--pause SECONDS]` decodes each input once per
  configuration (stock decoder, tracking off, tracking on at min 62 and at min 1), on as many
  threads at once as asked, and prints throughput, copybacks and bytes covered.
* `tools/gen-symbol-prefix.sh`, `tools/add-link-names.py` and `tools/gen-patch.sh`
  regenerate a fork's prefix header, its `#[link_name]`s and its patch after an upstream
  bump.
