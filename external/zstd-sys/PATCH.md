# zstd-sys, vendored and patched for ggcat

Upstream: crates.io `zstd-sys` 2.0.16+zstd.1.5.7 (zstd 1.5.7, BSD-3-Clause OR GPL-2.0;
bindings MIT OR Apache-2.0). Crate file sha256: `91e19ebc2adc8f83e43039e79776e3fda8ca919132d68a1fed6a5faca2683748`.

## Why

`lz-copyback` reports every LZ match the decoder executes. zstd's sequence
execution has no hook, so the block decoder is patched and the crate is built from this
tree through `[patch.crates-io]`. With no hook installed the change is one NULL check per
sequence; ggcat's existing zstd readers behave as before.

## Pruning

The registry tree already contains only `zstd/lib`, `zstd/contrib` and licences; the
generated `Cargo.lock`, `.cargo-ok` and `Cargo.toml.orig` were dropped. This fork decodes
only, so the following were removed as well -- both the files and the features, since
leaving a feature that no longer has sources would break any downstream `--features legacy`:

* `zstd/lib/legacy` (v0.1-v0.7 decoders, ~1 MB, 243 exported symbols),
  `zstd/lib/dictBuilder` (~190 KB, 34 symbols), `zstd/lib/deprecated` (never compiled),
  `zstd/contrib` (the seekable format).
* features `legacy`, `zdict_builder`, `seekable`, `experimental`, `zstdmt`, `thin`,
  `non-cargo`, `pkg-config`, `debug`, and **`bindgen`**.
* the six unreachable `src/bindings_*.rs` files (two of which the vendored `src/lib.rs`
  already could not reach) and the `zdict.h` / `zstd_seekable.h` bindgen inputs.
* `build.rs`'s pkg-config path: a system libzstd has no LZ hook, so the bundled source must
  always be built.

Deleting the `bindgen` feature is not cleanup but a correctness requirement: unlike
liblzma-sys's same-named feature, it really does regenerate bindings at build time, and those
would carry **unprefixed** names and bind silently to the stock library.

That takes the archive from 581 exported symbols to 348. One behavioural consequence: a
legacy v0.x frame is now rejected by `ZSTD_decompressStream` itself rather than by
`lz_copyback`'s own `lzTotalOut` mismatch check.

## Patch (`ggcat-lz-hook.patch`)

* `zstd/lib/decompress/zstd_decompress_internal.h`: `lzTotalOut`, `lzHook`, `lzHookCtx` in
  `ZSTD_DCtx_s`; the `ZSTD_LZ_HOOK(dctx, ostart, op, seq)` macro reporting
  `lzTotalOut + (op - ostart) + seq.litLength`, `seq.offset` (the resolved distance) and
  `seq.matchLength`; prototypes of `ZSTD_DCtx_setLzMatchHook` and `ZSTD_DCtx_getLzTotalOut`.
* `zstd/lib/decompress/zstd_decompress_block.c`: the macro at the eight sequence execution
  sites (`ZSTD_decompressSequences_bodySplitLitBuffer` x3, `ZSTD_decompressSequences_body`
  x1, `ZSTD_decompressSequencesLong_body` x4), right after the `ZSTD_execSequence*` error
  check, so reports are in destination order even for the look-ahead Long variant.
* `zstd/lib/decompress/zstd_decompress.c`: fields initialised in `ZSTD_initDCtx_internal`;
  `lzTotalOut` advanced per block in `ZSTD_decompressContinue` (streaming) and
  `ZSTD_decompressFrame` (one-shot); the two new API functions. `ZSTD_DCtx_reset` leaves
  the hook alone. Legacy (v0.x) frames bypass the counter; the Rust side detects the
  mismatch and reports an error instead of mis-tracking.
* `build.rs`: re-runs when the patched files change.

The Rust side (`../../src/backends/zstd.rs`) declares the new functions and
`ZSTD_getFrameHeader` itself, so the pregenerated bindings are untouched.

## Packaging for crates.io (lzcb fork)

This tree is published as **`lzcb-zstd-sys`**, not as `zstd-sys`, so that a project can depend on
`lz-copyback` and on the genuine `zstd-sys` at the same time. Three things make that work:

1. **`links = "lzcb_zstd"`** instead of upstream's `links = "zstd"`. Cargo allows only one package
   per `links` value in a graph, so without this the two cannot coexist at all.
2. **Every exported C symbol is renamed into the `lzcb_` namespace.** `lzcb_prefix.h` holds
   one `#ifndef`-guarded `#define` per symbol and `build.rs` force-includes it into every
   translation unit (`-include<path>`, or `/FI<path>` under MSVC). Without this the linker
   sees two definitions of every symbol and either errors or, worse, silently binds one
   crate's calls to the other library.
3. **The Rust bindings carry `#[link_name = "lzcb_..."]`**, so the Rust-visible API keeps
   upstream's names.

`lzcb_prefix.h` is generated, not hand-written: `tools/gen-symbol-prefix.sh zstd-sys libzstd.a`
builds the archive with `LZCB_NO_PREFIX=1` (the escape hatch that breaks the bootstrap
cycle), reads its symbol table and writes the header. `tests/symbols.rs` re-reads the built
archive and fails, naming the offenders, if any global definition lost its prefix -- that is
the guard for an upstream bump that adds a symbol.

### Two hazards specific to zstd

* `common/zstd_internal.h` shadows `ZSTD_isError`, `FSE_isError` and `HUF_isError` with
  `ERR_isError` for inlining, which replaces our prefix macro; `common/zstd_common.c` then
  `#undef`s `ZSTD_isError` before defining the real exported function, which would export it
  *unprefixed* while the bindings ask for `lzcb_ZSTD_isError`. The patch adds `#undef`s ahead
  of the three shadowing defines (so they do not warn) and re-includes `lzcb_prefix.h` after
  the one in `zstd_common.c` (the guards make that restore exactly that entry).
* `decompress/huf_decompress_amd64.S` declares its two entry points under both
  `HUF_...` and `_HUF_...`, because Mach-O prepends an underscore. The generated header
  therefore maps the underscored spelling to `_lzcb_HUF_...` rather than `lzcb__HUF_...`.
  The `.S` file is preprocessed by the C compiler, so the force-included header reaches it;
  `build.rs` skips it on Windows anyway.

## Upgrading

1. Copy the new upstream tree from `~/.cargo/registry/src/*/`, prune as described above.
2. `git apply -p1 ../../tools/../external/zstd-sys/ggcat-lz-hook.patch` from the crate
   directory and resolve any conflicts.
3. Re-run `../../tools/gen-symbol-prefix.sh zstd-sys libzstd.a` -- the symbol list is not
   stable across versions.
4. Re-run `python3 ../../tools/add-link-names.py external/zstd-sys/src/bindings_zstd.rs` if the bindings were
   regenerated.
5. `cargo test -p lzcb-zstd-sys` (the symbol audit) and `cargo test` at the workspace root (the
   coexistence check, which links this fork next to the stock library).
6. `../../tools/gen-patch.sh zstd-sys <upstream-dir>` to refresh this patch.
