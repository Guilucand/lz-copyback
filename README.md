# lz-copyback

Decoders for the LZ formats ggcat reads (xz, zstd, lz4, gzip) that also report the LZ
matches ("copybacks") they decode: which output bytes are verbatim repeats of earlier output.
Each has the interface of the library decoder ggcat used for that format, so switching is
mostly a matter of `use` lines.

| codec | decoder | interface of | runs on |
|---|---|---|---|
| xz (`.xz`, `.lzma`, `.lz`) | `xz::XzDecoder` | `liblzma::read::XzDecoder` | `external/lzma-sys`: liblzma-sys 0.4.9 (XZ 5.8.4), patched |
| zstd | `zstd::ZstdDecoder` | `zstd::stream::read::Decoder` | `external/zstd-sys`: zstd-sys 2.1.2 (zstd 1.5.7), patched |
| lz4 | `lz4::Lz4Decoder` | `lz4::Decoder` | `external/lz4-sys`: lz4-sys 1.11.1 (liblz4 1.10.0), patched |
| gzip | `gzip::GzipFileDecompressor`, `gzip::GzipPushDecompressor` | `LibdeflateGzipFileDecompressor`, `LibdeflateGzipPushDecompressor` | `streaming-libdeflate-rs` 0.3.0, unpatched |

`gzip::GzipDecoder` adds a `Read` over the push decompressor. Codecs without an LZ stage
(bzip2, plain files) are out of scope.

| decoder | methods besides the copyback ones |
|---|---|
| `XzDecoder` | `new`, `new_multi_decoder`, `get_ref`, `get_mut`, `into_inner`, `total_in`, `total_out`, `Read` |
| `ZstdDecoder` | `new`, `with_buffer`, `single_frame`, `window_log_max`, `recommended_output_size`, `get_ref`, `get_mut`, `finish`, `Read` |
| `Lz4Decoder` | `new`, `reader`, `finish`, `Read` |
| `GzipFileDecompressor` | `new`, `try_new`, `decompress`, `pending_output`, `consume_output`, `increase_buffer_size` |
| `GzipPushDecompressor` | the same, plus `push_input`, `input_space_mut`, `commit_input`, `finish_input`, `push`, `finish` |
| `GzipDecoder` | `new`, `with_buffer_size`, `get_ref`, `into_inner`, `Read` |

## Copybacks

Every decoder has the same three extra methods:

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

```rust
use lz_copyback::xz::XzDecoder;
use std::io::Read;

let mut decoder = XzDecoder::new_multi_decoder(file);
decoder.set_copybacks_min_len(62);
let mut buffer = vec![0; 1 << 20];
while decoder.read(&mut buffer)? > 0 {
    while let Some(copyback) = decoder.peek_copybacks().pop_front() {
        // copyback.len bytes at copyback.destination repeat those at copyback.source().
    }
}
```

The decoder only ever pushes to the back of the queue; removing entries is up to the caller.

* **Raw matches.** Every match exactly as the decoder decoded it: never merged with the next
  one, never clipped, never split. A long repeat is as many matches as the compressor
  wrote (deflate stops at 258 bytes, LZMA at 273), and a match may overlap its own output
  (`len > offset`, a periodic repeat). `set_copybacks_min_len` filters on this raw length.
* **Absolute destinations,** counted from the first byte the decoder produced, across
  concatenated xz streams, zstd frames and gzip members.
* **No later than the bytes.** A copyback is in the queue by the time the call that returns
  its first destination byte returns:

  | codec | when a match is pushed |
  |---|---|
  | gzip | while `decompress` decodes it, before its bytes are in `pending_output` |
  | xz | in the `read` call that returns its first byte; a match cut by the end of the caller's buffer is still reported once, whole |
  | zstd | when its block (at most 128 KiB) is decoded, possibly some `read` calls before its bytes |
  | lz4 | when its block (64 KiB to 4 MiB, as the frame says) is decoded, possibly some `read` calls before its bytes |

* **xz filters.** A Block whose filter chain is not a lone LZMA2 (BCJ, delta) reports
  nothing: those filters rewrite the LZ output, so its matches do not repeat output bytes.
  Such chains are only written on request (`xz --x86`, `--delta`, `--filters=`), never for
  sequence data.
* **lz4 frames.** Like upstream, `Lz4Decoder` stops after the first frame.

## How it works

The C libraries are patched where they decode a match. Each decoder installs the hook, with
a pointer to its queue, right before every native decoding call and removes it right after
(the decoders move, so no pointer outlives a call). With tracking off the hook is not
installed: the C decoders pay one NULL check per match, and the gzip decoder runs the stock
decoding loop, with no hook at all.

* **xz** (`external/lzma-sys`): reported from the LZMA decoder when a match's length is
  decoded, not from `dict_repeat`, which stops at the end of the caller's buffer and resumes
  the copy in the next call. The block decoder suppresses the hook for filtered Blocks.
* **zstd** (`external/zstd-sys`): reported at the eight sequence-execution sites; a sequence
  never spans blocks.
* **lz4** (`external/lz4-sys`): reported at the four places `LZ4_decompress_generic` completes
  a match; the frame decoder publishes the hook and the block's absolute offset around each
  block it decodes.
* **gzip**: `streaming-libdeflate-rs` already calls `DeflateOutput::on_lz_copy` for every
  match when the output asks for it; the decompressors here pass an output that does, only
  while tracking.

Each fork's `PATCH.md` describes its change and how to replay it on a newer upstream, and
`ggcat-lz-hook.patch` holds it as a diff.

## Coexisting with the stock crates

The C forks can be linked into the same binary as the genuine `liblzma-sys`, `zstd-sys` and
`lz4-sys`: they claim their own `links` keys (`lzcb_lzma`, `lzcb_zstd`, `lzcb_lz4`) and every
exported C symbol is renamed into the `lzcb_` namespace by a generated header,
`lzcb_prefix.h`, force-included into every translation unit. Each fork's `tests/symbols.rs`
fails if a symbol of its built archive lost the prefix, and this crate's tests link the
stock crates next to the forks.

## Development

* `cargo test --workspace`:
  * `tests/copybacks.rs`: on all four decoders, correct raw matches, pushed in time,
    independent of the read size, the threshold and the switch;
  * `tests/lz4_frames.rs`: the lz4 matches equal the ones read straight from the frames;
  * `tests/interface.rs`: truncated and concatenated inputs end as with the stock decoders,
    and the extra methods behave;
  * the symbol audits of the three forks.
* `cargo bench --bench decode -- [--synthetic-mib N] [--read-size BYTES] [--real FILE.xz
  --scratch DIR]` decodes each input once per configuration (stock decoder, tracking off,
  tracking on at min 62 and at min 1) and prints throughput, copybacks and bytes covered.
* `tools/gen-symbol-prefix.sh`, `tools/add-link-names.py` and `tools/gen-patch.sh`
  regenerate a fork's prefix header, its `#[link_name]`s and its patch after an upstream
  bump.
