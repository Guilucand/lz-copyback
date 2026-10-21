# zstd-sys, vendored and patched for ggcat

Upstream: crates.io `zstd-sys` 2.1.2+zstd.1.5.7 (zstd 1.5.7, BSD-3-Clause OR GPL-2.0;
bindings BSD-3-Clause since 2.1). Crate file sha256: `481ada9c543b2323cbb7bbdbb5249177fbf0188ed959ed2c15e4b22421c84ed3`.

## Why

`lz-copyback` reports every LZ match the decoder executes. zstd's sequence
execution has no hook, so the block decoder is patched. With no hook installed the change is
one NULL check per sequence.

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
* `build.rs`'s pkg-config path (and with it 2.1's `ZSTD_SYS_USE_PKG_CONFIG`), 2.1's `cmake`
  and `vendored` features and its `ZSTD_SYS_NO_HIDE_SYMBOLS` switch: a system libzstd has no
  LZ hook and the cmake build would bypass `lzcb_prefix.h`, so the bundled source is always
  built by `cc`. From 2.1's `build.rs` the fork keeps the assembly being built on x86_64
  only and `-fvisibility=hidden` being skipped for MSVC targets.

Deleting the `bindgen` feature is not cleanup but a correctness requirement: unlike
liblzma-sys's same-named feature, it really does regenerate bindings at build time, and those
would carry **unprefixed** names and bind silently to the stock library.

That takes the archive from 581 exported symbols to 348. A legacy v0.x frame is rejected by
`ZSTD_decompressStream` itself.

## Patch (`ggcat-lz-hook.patch`)

* `zstd/lib/decompress/zstd_decompress_internal.h`: `lzTotalOut`, `lzHook`, `lzHookCtx` and
  `lzHookMinLen` in `ZSTD_DCtx_s`; the `ZSTD_LZ_HOOK(dctx, ostart, op, seq)` macro reporting
  `lzTotalOut + (op - ostart) + seq.litLength`, `seq.offset` (the resolved distance) and
  `seq.matchLength` when the match is at least `lzHookMinLen` long (`SIZE_MAX` without a
  hook, so a sequence costs a comparison and a NULL check, and the short matches never leave
  C), or when it continues the last match reported, from `lzHookLastEnd` at distance
  `lzHookLastOffset`, which it then updates, so the tail of a repeat cut into matches is
  kept however short; prototypes of
  `ZSTD_DCtx_setLzMatchHook(dctx, hook, ctx, minLength, lastEnd, lastOffset)` and
  `ZSTD_DCtx_getLzTotalOut`.
* `zstd/lib/decompress/zstd_decompress_block.c`: the macro at the eight sequence execution
  sites (`ZSTD_decompressSequences_bodySplitLitBuffer` x3, `ZSTD_decompressSequences_body`
  x1, `ZSTD_decompressSequencesLong_body` x4), right after the `ZSTD_execSequence*` error
  check, so reports are in destination order even for the look-ahead Long variant. A
  sequence is executed whole and never spans blocks, so every match is reported once, with
  its full length.
* `zstd/lib/decompress/zstd_decompress.c`: fields initialised in `ZSTD_initDCtx_internal`;
  `lzTotalOut` advanced per block in `ZSTD_decompressContinue`, the buffer-less API that
  `ZstdInPlaceDecoder` decodes with (and that the streaming API runs on); the two new API
  functions. The one-shot `ZSTD_decompressFrame`, which the streaming API also takes when a
  whole frame fits its output, does not count: its matches are reported with wrong offsets,
  and only the buffer-less API is supported. Nothing else resets `lzTotalOut` -- not
  `ZSTD_DCtx_reset`, not the start of a frame -- so the offsets run on across frames for the
  life of the context.
* `src/lib.rs`: hand-written declarations of `ZSTD_DCtx_setLzMatchHook` and
  `ZSTD_DCtx_getLzTotalOut`, and of the buffer-less streaming API (`ZSTD_getFrameHeader`,
  `ZSTD_decodingBufferSize_min`, `ZSTD_decompressBegin`, `ZSTD_nextSrcSizeToDecompress`,
  `ZSTD_decompressContinue`, `ZSTD_nextInputType`, `ZSTD_FrameHeader`), which is in
  zstd's static-linking-only section and so not in the pregenerated bindings; they are
  untouched. `ZstdInPlaceDecoder` decodes with it, each block straight into its own ring.
* `build.rs`: re-runs when the patched files change.

The Rust side (`../../src/zstd.rs`) installs the hook around every `ZSTD_decompressContinue`
call and clears it afterwards.

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
4. **The archive is `liblzcb_zstd.a`**, not `libzstd.a`. Its directory goes on the search path of the
   final link, where a `libzstd.a` would be taken for the `-lzstd` of a crate linking the system
   library, whose symbols then come out undefined.

`lzcb_prefix.h` is generated, not hand-written: `tools/gen-symbol-prefix.sh zstd-sys liblzcb_zstd.a`
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

1. Copy the new upstream tree from `~/.cargo/registry/src/*/`, prune as described above. The
   zstd C sources of 2.1.2 are identical to 2.0.16's, so that bump only took the Rust side.
2. `git apply -p1 ../../tools/../external/zstd-sys/ggcat-lz-hook.patch` from the crate
   directory and resolve any conflicts.
3. Re-run `../../tools/gen-symbol-prefix.sh zstd-sys liblzcb_zstd.a` -- the symbol list is not
   stable across versions.
4. Re-run `python3 ../../tools/add-link-names.py external/zstd-sys/src/bindings_zstd.rs` if the bindings were
   regenerated.
5. `cargo test -p lzcb-zstd-sys` (the symbol audit) and `cargo test` at the workspace root (the
   coexistence check, which links this fork next to the stock library).
6. `../../tools/gen-patch.sh zstd-sys <upstream-dir>` to refresh this patch.
