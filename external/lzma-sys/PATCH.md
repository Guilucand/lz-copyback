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
  reaches `hook_min_len` (`UINT32_MAX` without a hook, so a match costs two comparisons and
  the short ones never leave C), or when the match continues the last one reported: its
  destination at `hook_last_end`, at distance `hook_last_distance`, which it then updates
  (`UINT64_MAX` without a hook), so the tail of a repeat cut into matches is kept however
  short; the prototypes of `lzma_lz_set_match_hook` and `lzma_lz_suppress_match_hook`.
* `xz/src/liblzma/lzma/lzma_decoder.c`: the local dictionary's `hook_last_end` and
  `hook_last_distance` written back with `pos` and `full`; `dict_report_match()` once per match, at the point
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
  ctx, out_abs_base, min_len, last_end, last_distance)` and
  `lzma_lz_suppress_match_hook(bool)`; `decode_buffer()` copies the state into the dictionary
  before each `lz.code()` call (no hook while suppressed), the last match's end and distance
  back after it, and sets
  `abs_base = out_abs_base + *out_pos - dict_start`, which is exact because
  `dict.buf[dict_start..pos]` is `out[*out_pos..]` (decoding in place, below; a filtered
  Block's bytes are copied there, and report nothing); the fields are initialised in
  `lzma_lz_decoder_init()`.
* `xz/src/liblzma/common/block_decoder.c`: a Block whose filter chain is not a lone LZMA2
  suppresses the hook while it is decoded. A BCJ or delta filter in front of LZMA2 rewrites
  the LZ output in place, so its matches would not repeat output bytes, and BCJ may decode
  into a buffer of its own, which would also make the offsets wrong. `.lzma` and `.lz`
  streams have no filter chain and keep reporting.
* `src/lib.rs`: the hand-written declarations of `lzma_lz_set_match_hook` and of the
  in-place API below.
* `build.rs`: always builds the bundled source and re-runs when the patched files change.

### In-place decoding

The only way this fork decodes, for `XzInPlaceDecoder`, which reads the decoded bytes straight
from the dictionary instead of having them copied out of it. The caller installs a
`lzma_lz_in_place` state for the thread with `lzma_lz_set_in_place()` around every
`lzma_code` call, like the hook; without one the LZ decoder cannot be initialised
(`LZMA_PROG_ERROR`). So this fork is no general-purpose liblzma anymore.

* `lz_decoder.h`: the `lzma_lz_in_place` state and its functions; a `full_base` field in
  `lzma_dict`, which the three updates of `full` subtract instead of `LZ_DICT_INIT_POS`.
* `lz_decoder.c`:
  * `decode_buffer()` decodes only if the output buffer is the dictionary at the decoding
    position (the caller passes exactly that, from the state's `next`), and then skips the
    copy: everything after it, the integrity checks included, reads the bytes where they
    are. Otherwise it returns without decoding, and the caller calls again with the right
    buffer: the position moves at a wrap, a reset or a new dictionary, never otherwise.
  * A reset keeps the bytes decoded so far in place and restarts right after them, at the
    next multiple of 16 with a zero in front: the LZMA decoder takes the position state from
    the low bits of `pos` and the previous byte from `buf[pos - 1]`. `full` then counts from
    there (`full_base`), across the wrap that may follow before the dictionary is full again;
    `decode_buffer()` stops where it becomes full, to set `has_wrapped`. A new dictionary
    starts at `LZ_DICT_INIT_POS`, as upstream's.
  * `lzma_lz_decoder_init()` allocates at least the state's `min_size`, plus the declared
    dictionary size when the state's `min_size_plus_dict` is set (the caller then keeps a whole
    dictionary readable, as far back as a match reaches), reports that dictionary size in the
    state's `dict_size`, and keeps a dictionary
    large enough, so that the bytes in it stay; one too small, or freed with its coder
    (`lz_decoder_end()`, when a Block's filter chain differs from the previous one), goes to
    the state's `retire` callback instead of being freed. Preset dictionaries (raw decoders
    only, never in `.xz`, `.lzma` or `.lz` files) are refused, and their copying is gone.
  * `in_place_report()` updates the state's `buf`, `size` and `next` after every decoding
    step and initialisation.
* `common/block_decoder.c`: a Block whose filter chain is not a lone LZMA2 (the one that
  suppresses the hook) does not decode in the dictionary, as its output is not there:
  `lzma_lz_in_place_block()` marks it `filtered` and refuses an output buffer inside the
  dictionary, so the caller decodes it into a buffer of its own.

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
4. **The archive is `liblzcb_lzma.a`**, not `liblzma.a`. Its directory goes on the search path of the
   final link, where a `liblzma.a` would be taken for the `-llzma` of a crate linking the system
   library, whose symbols then come out undefined.

`lzcb_prefix.h` is generated: `tools/gen-symbol-prefix.sh lzma-sys liblzcb_lzma.a` builds the
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
3. `tools/gen-symbol-prefix.sh lzma-sys liblzcb_lzma.a` -- the symbol list is not stable across
   versions.
4. `python3 tools/add-link-names.py external/lzma-sys/src/bindgen.rs` if the bindings were
   regenerated.
5. `cargo test --workspace` (the symbol audit, and the copyback tests, which link this fork
   next to the stock library).
6. `tools/gen-patch.sh lzma-sys <upstream-dir>` to refresh this patch.
