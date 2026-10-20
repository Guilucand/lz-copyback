# liblzma-sys, vendored and patched for ggcat

Upstream: crates.io `liblzma-sys` 0.4.9 (XZ Utils 5.8.4, 0BSD; bindings MIT OR Apache-2.0).
Crate file sha256: `7c7e3581f367a78d7b7e7ae948d023310556f0cfc13156c2e4e00e25616492b9`.

## Why

`lz-copyback` needs every LZ match liblzma decodes (destination offset, distance, length).
liblzma has no such hook and the LZ decoder is not reachable through the public
`lzma_stream` API, so the decoder is patched. With no hook installed the hot path gains one
predictable NULL check per match.

## Pruning

Only what the build needs is kept: `build.rs`, `Cargo.toml`, `config.h`, `src/`,
`wasm-shim/`, `xz/src/liblzma/`, `xz/src/common/`, and the licence files `LICENSE-*`,
`xz/COPYING*`, `xz/AUTHORS`, `xz/README`. Dropped: `xz/po*`, `xz/tests`, `xz/doc`, the
autotools/cmake scaffolding and the command line tools.

This fork additionally drops `src/manual.rs` and the `bindgen` feature that selected between
it and `src/bindgen.rs`: `manual.rs` lacks a good half of what the bindings are used for, so
keeping it would only mean a second set of `#[link_name]`s to hold in sync. The `wasm`,
`uncheck_liblzma_version`, `parallel` and `static` features and the `pkg-config`
build-dependency went too (`want_static` is hardcoded `true`, and a system liblzma would lack
the hook).

The encoder sources are deliberately **kept**. Dropping them would mean extending
`SKIP_FILENAMES`, flipping `HAVE_ENCODER_*` in `config.h` and then chasing the `common/` glue
that still references the removed initialisers -- a hand-maintained divergence to redo on
every re-vendor, for about 60 symbols that the `lzcb_` prefix already isolates.

## Patch (`ggcat-lz-hook.patch`, `diff -u` against the pristine crate)

* `xz/src/liblzma/lz/lz_decoder.h`: the `lzma_lz_match_hook` typedef; `hook`, `hook_ctx`,
  `hook_min_len` and `abs_base` fields in `lzma_dict`; `dict_report_match()`, which calls the
  hook with `abs_base + pos`, `distance + 1` and the full match length when the length
  reaches `hook_min_len` (`UINT32_MAX` without a hook, so a match costs one comparison and
  the short ones never leave C); the prototypes of `lzma_lz_set_match_hook` and
  `lzma_lz_suppress_match_hook`.
* `xz/src/liblzma/lzma/lzma_decoder.c`: `dict_report_match()` once per match, at the point
  where its length is known and before it is copied: the fast path's short rep and the call
  in front of its `dict_repeat()`; the slow path's short rep and the call in front of
  `case SEQ_COPY:`, both placed *before* the resume labels so that a copy resumed in a later
  call is not reported again.

  The match is **not** reported from `dict_repeat()`: that copies at most up to
  `dict.limit`, which `decode_buffer()` sets from the caller's free output space, and up to
  the end of the circular buffer, so reporting there split a match wherever the caller's
  buffer happened to end. `abs_base + pos` is the absolute offset of the next output byte
  even when the dictionary is full and the copy is resumed later, since `decode_buffer()`
  recomputes `abs_base` around every `lz.code()` call.
* `xz/src/liblzma/lz/lz_decoder.c`: the thread-local hook state, `lzma_lz_set_match_hook(hook,
  ctx, out_abs_base, min_len)` and `lzma_lz_suppress_match_hook(bool)`; `decode_buffer()`
  copies the state into the dictionary before each `lz.code()` call (no hook while
  suppressed) and sets
  `abs_base = out_abs_base + *out_pos - dict_start`, which is exact because
  `dict.buf[dict_start..pos]` is copied to `out[*out_pos..]` right after the call; the fields
  are initialised in `lzma_lz_decoder_init()`.
* `xz/src/liblzma/common/block_decoder.c`: a Block whose filter chain is not a lone LZMA2
  suppresses the hook while it is decoded. A BCJ or delta filter in front of LZMA2 rewrites
  the LZ output in place, so its matches would not repeat output bytes, and BCJ may decode
  into a buffer of its own, which would also make the offsets wrong. `.lzma` and `.lz`
  streams have no filter chain and keep reporting.
* `src/lib.rs`: the hand-written declaration of `lzma_lz_set_match_hook`.
* `build.rs`: always builds the bundled source and re-runs when the patched files change.

The Rust side (`../../src/xz.rs`) installs the hook around every `lzma_code` call with the
running output total as `out_abs_base` and clears it afterwards.

## CLMUL CRC

`build.rs` defines `HAVE_USABLE_CLMUL` for `x86_64` non-MSVC targets, which makes
`check/crc_common.h` build the runtime-detected CLMUL CRC32/CRC64 kernels
(`crc_x86_clmul.h`) next to the generic tables; the generic code is still used when the
CPU lacks PCLMULQDQ. Upstream enables the same thing from its autoconf check. On highly
repetitive input the table-driven CRC64 costs about as much as the LZMA decoding itself.

## Packaging for crates.io (lzcb fork)

This tree is published as **`lzcb-lzma-sys`**, not as `liblzma-sys`, so that a project can
depend on `lz-copyback` and on the genuine `liblzma-sys` at the same time:

1. **`links = "lzcb_lzma"`** instead of upstream's `links = "lzma"`. Cargo allows only one
   package per `links` value in a graph.
2. **Every exported C symbol is renamed into the `lzcb_` namespace.** `lzcb_prefix.h` holds
   one `#ifndef`-guarded `#define` per symbol and `build.rs` force-includes it into every
   translation unit (`-include<path>`, or `/FI<path>` under MSVC).
3. **The Rust bindings carry `#[link_name = "lzcb_..."]`**, so the Rust-visible API keeps
   upstream's names.

`lzcb_prefix.h` is generated: `tools/gen-symbol-prefix.sh lzma-sys liblzma.a` builds the
archive with `LZCB_NO_PREFIX=1`, reads its symbol table and writes the header.
`tests/symbols.rs` re-reads the built archive and fails, naming the offenders, if any global
definition lost its prefix.

### One hazard specific to liblzma

`tuklib_*` cannot be renamed with a `#define`: `tuklib_common.h` pastes the name with `##`
through `TUKLIB_SYMBOL()`, which blocks macro expansion. `build.rs` uses upstream's own
`TUKLIB_SYMBOL_PREFIX=lzcb_` instead and the generator keeps `tuklib_*` out of
`lzcb_prefix.h`. For the same reason, do **not** enable symbol versioning in this fork: the
`HAVE_SYMBOL_VERSIONS_LINUX` aliases would bypass the prefix the same way.

## Upgrading

1. Copy the new upstream tree from `~/.cargo/registry/src/*/`, prune as described above. For
   a point release it is simpler to copy over the files that changed between the two
   upstream versions and apply upstream's diff to the patched ones (that is how 0.4.7 became
   0.4.9).
2. `git apply -p1 ggcat-lz-hook.patch` from the crate directory and resolve any conflicts.
3. `tools/gen-symbol-prefix.sh lzma-sys liblzma.a` -- the symbol list is not stable across
   versions.
4. `python3 tools/add-link-names.py external/lzma-sys/src/bindgen.rs` if the bindings were
   regenerated.
5. `cargo test --workspace` (the symbol audit, and the copyback tests, which link this fork
   next to the stock library).
6. `tools/gen-patch.sh lzma-sys <upstream-dir>` to refresh this patch.
