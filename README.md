# lz-copyback

Streaming decompression that reports the LZ77 copies the decoder executes, so a consumer
can tell which byte ranges of the output are verbatim repeats of earlier ranges.

```rust
use lz_copyback::{open, Direction, TrackerConfig, tar::{TarConfig, TarTracker}};

let mut dec = open(path, TrackerConfig::default())?;      // gz, xz, zst, lz4, bz2, plain
let mut tar = TarTracker::new(TarConfig::default());        // only for *.tar[.xz] inputs
while let Some(w) = dec.next_window()? {
    // w.data: W bytes of output starting at w.start_abs (fewer on the last window)
    // w.copies: LzCopy { src, dst, len }; the bytes at dst repeat the bytes at src
    // w.lookahead: the already decoded bytes after w.data (destinations fall inside it)
    let tw = tar.process(&w)?;                              // MemberCopy pieces + member spans
    if w.is_last { break; }
}
```

## The three directions

A window always carries its `W` bytes of output; the `Direction` picks which end of a copy
attaches it to that window.

* **Forward** (`Direction::Forward`, the default) -- the copies whose *source* lies in the
  window, sorted by `(src, dst)`: "these bytes come back later". A copy can only be
  reported while its source is still inside the window, so copies farther than `W` are
  counted as `dropped_late` and lost.
* **Backward** (`Direction::Backward`) -- the copies whose *destination* lies in the
  window, sorted by `(dst, src)`: "these bytes repeat something earlier". The source is
  already decoded when the copy becomes known, so nothing is held back and no copy is lost
  whatever its distance; the consumer needs the older bytes, or just their offsets, to use
  it. `TarConfig::retain_back` keeps the member table alive that far back, so that a
  copy's source member can still be named.
* **Both** (`Direction::Both`) -- both at once: `w.copies` is the forward list and
  `w.back_copies` the backward one, so a copy is reported twice, at each end. The two
  lists are split at different window boundaries, so the two reports of one copy are
  generally not the same range; match them by interval, not by equality. Copies farther
  than `W` are still missing from the forward list (`dropped_late` counts them) while the
  backward list has them. Use `w.src_copies()` / `w.dst_copies()` to stay
  direction-agnostic.

Everything else is shared: the same decoders, the same merging and filtering, the same tar
layer.

## How it works

* `W` is the stream's maximum copy distance (32 KiB for deflate, the LZMA2 dictionary size
  for xz, the frame window for zstd, 64 KiB for lz4) clamped into
  `[min_window, max_window]` (defaults 1 MiB and 64 MiB).
* Raw matches are merged when contiguous with the same distance (compressors split long
  repeats into 258/273-byte matches), clipped to their distance when they overlap their
  own source (`SelfOverlap::Clip`, or `Drop`), then filtered by `min_len`.
* A window's copies come out ordered. The destination-keyed list is built in order --
  backends report matches in destination order, destinations never overlap, and a copy cut
  at the window boundary leaves its tail exactly there -- so it is only checked, in debug
  builds. The source-keyed list is not: sources jump around, so it is radix sorted in place
  on the source with `voracious_radix_sort` and the runs that share a source are then
  ordered by destination (`src/sort.rs`), which together is exactly what sorting by
  `(src, dst)` gives. That is about twice as fast as the standard sort on window-sized
  lists, which `cargo run --release --example sortbench` measures.
* `set_tracking(false)` disables the hooks at zero cost for a later dynamic cut-off.
* Backends live in `src/backends`: deflate writes straight into the window through the
  `on_lz_copy` hook of `streaming-libdeflate-rs`; xz, zstd and lz4 use the patched
  vendored forks under `external/` (see their `PATCH.md`); bzip2 and plain files pass
  through without copies.
* The decoder alternates between two buffers of `W + slack` bytes (plus a 32 KiB history
  guard for deflate), so no output byte is moved twice; the cost of tracking off is the
  cold window memory, the cost of tracking on is 1-3 ns per raw match event.
* A copy is 16 bytes and a member piece 20: only the source is absolute, the destination
  is its distance from it, and distances, offsets and lengths are `u32`. No LZ match can
  reach past its dictionary, whose size is a `u32` field in every format here, and a
  length is clipped to its distance before a copy is emitted; a tar member of 4 GiB or
  more is the one thing this rules out, and its pieces are dropped and counted rather
  than truncated. Everything is still read in absolute `u64` coordinates.
* The `tar` module maps copies onto archive members, splitting a copy at every member
  boundary on both sides and dropping the pieces that touch headers, padding or opaque
  members. Each end of a copy is located through its own cursor into the live member
  table: the end the window is sorted by walks forward and costs nothing amortised, the
  other one uses the cursor as a hint and falls back to a binary search, so nothing
  depends on the order copies arrive in.

## Measurements

`src/bin/analysis --min-len 62`, one thread, release profile, on the 200,000,000 byte tar
of AllTheBacteria batch 104 recompressed four ways, and on the batch as it ships:

| input | compressed | output | plain decode | forward | backward |
|---|---:|---:|---:|---:|---:|
| `.tar.xz` (8 MiB dict) | 19.4 MB | 200 MB | 0.52 s | 0.42 s, 34 MB | 0.41 s, 34 MB |
| `.tar.gz` (`-6`) | 58.0 MB | 200 MB | 0.72 s | 0.20 s, 13 MB | 0.20 s, 13 MB |
| `.tar.zst` (`-3`) | 58.0 MB | 200 MB | 0.13 s | 0.20 s, 16 MB | 0.20 s, 16 MB |
| `.tar.lz4` (`-9 -BD`) | 76.6 MB | 200 MB | 0.13 s | 0.10 s, 21 MB | 0.11 s, 21 MB |
| `batch.104.tar.xz` | 5.0 MB | 837 MB | 0.36 s | 0.38 s, 225 MB | 0.37 s, 220 MB |

Tracking costs almost nothing beyond decoding the stream, which has to happen anyway, and
both directions report the same copies -- 94.5% of the bytes of `batch.104.tar.xz` are
covered by copies of at least 62 bytes -- only attached to the other end.

## Vendored decompressors

The xz, zstd and lz4 backends need decoders that report every LZ match, which upstream does
not offer, so `external/` carries three forks:

| crate | upstream | what the fork adds |
|---|---|---|
| `lzcb-lzma-sys` | `liblzma-sys` 0.4.7 (XZ 5.8.3) | `lzma_lz_set_match_hook` in the LZ decoder |
| `lzcb-zstd-sys` | `zstd-sys` 2.0.16 (zstd 1.5.7) | `ZSTD_DCtx_setLzMatchHook` at the sequence sites |
| `lzcb-lz4-flex` | `lz4_flex` 0.11.6 | a `MatchHook` trait and `FrameDecoder::with_hook` |

Each has a `PATCH.md` describing the change and a `ggcat-lz-hook.patch` to replay it on a
newer upstream. The two `-sys` forks compile their C with every exported symbol renamed into
the `lzcb_` namespace (`lzcb_prefix.h`, force-included into every translation unit) and claim
their own `links` keys, so they can be linked into a binary that also links the genuine
`liblzma-sys` / `zstd-sys` -- which is exactly what this crate's own test suite does, since
its dev-dependencies pull the stock `liblzma`, `zstd` and `lz4` crates. A test in each fork
re-reads the built archive and fails if any symbol lost its prefix.

## Features

`stats` (on by default) keeps the counters of `TrackerStats` and `TarStats`. Built without
it -- `default-features = false` -- every `Stat` is a zero-sized type, so both structs are
zero-sized, every `inc`/`add` compiles away and reading a counter gives `0`; the API is
the same either way, and `Stat::ENABLED` says which build this is. `TrackingDecoder::
window_size()` reports the window whether or not the counters are there. The
`lz-copyback-stats` binary and every test that asserts on a counter declare
`required-features = ["stats"]`, so a build without the feature skips them instead of
failing.

## Tools

* `cargo run --bin lz-copyback-analysis` dumps tar members with the copies attached to
  them (`--mode forward|backward`).
* `cargo run --bin lz-copyback-stats` prints copy statistics for a set of files.
* `cargo run --example lz_bench` measures decode speed with tracking off/on against the
  plain readers -- the stock `liblzma`, `zstd`, `lz4` and `flate2` crates, which the test
  and example builds link alongside the prefixed forks.
