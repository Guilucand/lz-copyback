# liblzma-sys, vendored and patched for ggcat

Upstream: crates.io `liblzma-sys` 0.4.7 (XZ Utils 5.8.3, 0BSD; bindings MIT OR Apache-2.0).
Crate file sha256: `a046c7f353ba30f810545151e04f63545833803f5b86ee3ddf1517247fe560a5`.

## Why

`lz-copyback` needs to know every LZ match liblzma executes (destination offset,
distance, length) to report forward copies. liblzma has no such hook and the LZ coder is
not reachable through the public `lzma_stream` API, so the decoder is patched and the
crate is built from this tree through `[patch.crates-io]` in the workspace `Cargo.toml`.
With no hook installed the only change on the hot path is one predictable NULL check per
match, so ggcat's existing xz readers behave exactly as before.

## Pruning

Only what the build needs is kept: `build.rs`, `Cargo.toml`, `config.h`, `src/`,
`wasm-shim/`, `xz/src/liblzma/`, `xz/src/common/`, and the licence files `LICENSE-*`,
`xz/COPYING*`, `xz/AUTHORS`, `xz/README`. Dropped: `xz/po*`, `xz/tests`, `xz/doc`, the
autotools/cmake scaffolding and the command line tools.

This fork additionally drops `src/manual.rs` and the `bindgen` feature that selected between
it and `src/bindgen.rs`: `manual.rs` lacks a good half of what `lz_copyback`'s xz backend
uses (`lzma_filters_free`, `lzma_block_header_decode`, `LZMA_FILTERS_MAX`,
`LZMA_STREAM_HEADER_SIZE`, `LZMA_BLOCK_HEADER_SIZE_MAX`, the `lzma_ret_LZMA_*` variants), so
keeping it would only mean a second set of `#[link_name]`s to hold in sync. The `wasm`,
`uncheck_liblzma_version`, `parallel` and `static` features and the `pkg-config`
build-dependency went too (`want_static` has been hardcoded `true` since the hook landed, so
`static` was a lie, and a system liblzma would lack `lzma_lz_set_match_hook`).

The encoder sources are deliberately **kept**. Dropping them would mean extending
`SKIP_FILENAMES`, flipping `HAVE_ENCODER_*` in `config.h` and then chasing the `common/` glue
that still references the removed initialisers -- a hand-maintained divergence to redo on
every re-vendor, for about 60 symbols that the `lzcb_` prefix already isolates.

## Patch (`ggcat-lz-hook.patch`, `diff -ruN` against the pruned pristine tree)

* `xz/src/liblzma/lz/lz_decoder.h`: `lzma_lz_match_hook` typedef; `hook`, `hook_ctx` and
  `abs_base` fields in `lzma_dict`; the call in `dict_repeat()` right after the copy
  length is known (`distance + 1` is the real distance, `left` the bytes copied by this
  call; a match split at the dictionary limit reports each part); the prototype of
  `lzma_lz_set_match_hook`.
* `xz/src/liblzma/lz/lz_decoder.c`: thread-local hook state and the exported
  `lzma_lz_set_match_hook(hook, ctx, out_abs_base)`; `decode_buffer()` copies the state
  into the dictionary before each `lz.code()` call and sets
  `abs_base = out_abs_base + *out_pos - dict_start`, which is exact because the bytes
  `dict.buf[dict_start..pos]` are copied to `out[*out_pos..]` right after the call; fields
  initialised in `lzma_lz_decoder_init()`.
* `build.rs`: always builds the bundled source (a system liblzma would lack the symbol) and
  re-runs when the patched files change.

The Rust side (`../../src/backends/xz.rs`) installs the hook around every
`lzma_code` call with the running output total as `out_abs_base` and clears it afterwards.

## CLMUL CRC (2026-09-18)

`build.rs` defines `HAVE_USABLE_CLMUL` for `x86_64` non-MSVC targets, which makes
`check/crc_common.h` build the runtime-detected CLMUL CRC32/CRC64 kernels
(`crc_x86_clmul.h`) next to the generic tables; the generic code is still used when the
CPU lacks PCLMULQDQ. Upstream enables the same thing from its autoconf check. Measured on
the AllTheBacteria `.tar.xz` archives, the generic CRC64 took about as long as the LZMA
decoding of the (extremely repetitive) content.

## Packaging for crates.io (lzcb fork)

This tree is published as **`lzcb-lzma-sys`**, not as `liblzma-sys`, so that a project can depend on
`lz-copyback` and on the genuine `liblzma-sys` at the same time. Three things make that work:

1. **`links = "lzcb_lzma"`** instead of upstream's `links = "lzma"`. Cargo allows only one package
   per `links` value in a graph, so without this the two cannot coexist at all.
2. **Every exported C symbol is renamed into the `lzcb_` namespace.** `lzcb_prefix.h` holds
   one `#ifndef`-guarded `#define` per symbol and `build.rs` force-includes it into every
   translation unit (`-include<path>`, or `/FI<path>` under MSVC). Without this the linker
   sees two definitions of every symbol and either errors or, worse, silently binds one
   crate's calls to the other library.
3. **The Rust bindings carry `#[link_name = "lzcb_..."]`**, so the Rust-visible API keeps
   upstream's names.

`lzcb_prefix.h` is generated, not hand-written: `tools/gen-symbol-prefix.sh lzma-sys liblzma.a`
builds the archive with `LZCB_NO_PREFIX=1` (the escape hatch that breaks the bootstrap
cycle), reads its symbol table and writes the header. `tests/symbols.rs` re-reads the built
archive and fails, naming the offenders, if any global definition lost its prefix -- that is
the guard for an upstream bump that adds a symbol.

### One hazard specific to liblzma

`tuklib_*` cannot be renamed with a `#define`: `tuklib_common.h` pastes the name with `##`
through `TUKLIB_SYMBOL()`, which blocks macro expansion, so our macro would be silently
overridden. `build.rs` uses upstream's own `TUKLIB_SYMBOL_PREFIX=lzcb_` instead and the
generator keeps `tuklib_*` out of `lzcb_prefix.h`. For the same reason, do **not** enable
symbol versioning in this fork: the three `HAVE_SYMBOL_VERSIONS_LINUX` aliases would bypass
the prefix the same way.

## Upgrading

1. Copy the new upstream tree from `~/.cargo/registry/src/*/`, prune as described above.
2. `git apply -p1 ../../tools/../external/lzma-sys/ggcat-lz-hook.patch` from the crate
   directory and resolve any conflicts.
3. Re-run `../../tools/gen-symbol-prefix.sh lzma-sys liblzma.a` -- the symbol list is not
   stable across versions.
4. Re-run `python3 ../../tools/add-link-names.py external/lzma-sys/src/bindgen.rs` if the bindings were
   regenerated.
5. `cargo test -p lzcb-lzma-sys` (the symbol audit) and `cargo test` at the workspace root (the
   coexistence check, which links this fork next to the stock library).
6. `../../tools/gen-patch.sh lzma-sys <upstream-dir>` to refresh this patch.
