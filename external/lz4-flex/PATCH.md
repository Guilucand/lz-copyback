# lz4_flex, vendored and patched for ggcat

Upstream: crates.io `lz4_flex` 0.11.6 (MIT). Crate file sha256: `373f5eceeeab7925e0c1098212f2fbc4d416adec9d35051a6ab251e824c1854a`.
Used only by `lz-copyback` (ggcat's own lz4 paths keep using
the `lz4` C binding).

## Why

The lz4 frame backend of `lz-copyback` needs every match with an absolute output
offset. The C `LZ4_decompress_generic` is an inlined template shared with ggcat's
temp-file path and its stream state is opaque, so the pure-Rust decoder is forked instead.

## Pruning

Kept: `src/`, `Cargo.toml`, `LICENSE`, `README.md`. Tests and benches dropped
(they pull `lzzzz`, `proptest`, `binggan`).

## Patch (`ggcat-lz-hook.patch`)

* `src/block/mod.rs`: `MatchHook` trait and the no-op `NoHook`.
* `src/block/decompress.rs` and `src/block/decompress_safe.rs`: `decompress_internal` takes
  `hook: &mut H` and calls `on_match(output_len, offset, match_length)` at both match sites
  (after the offset check, before the external-dictionary split); public callers pass
  `NoHook`.
* `src/frame/decompress.rs`: `FrameDecoder<R, H = NoHook>` with `with_hook`, `hook_mut`,
  `total_out`; an `AbsHook` adapter adds the running total so the hook sees absolute stream
  offsets across blocks and frames.

## Lint fix (2026-09-18)

`src/fastcpy.rs`: the doc comment above the `#[cfg(target_feature = "avx")]` block became a
plain comment. With `-C target-cpu=native` the block is compiled and the crate's
`deny(unused_doc_comments)` (via `deny(warnings)`-style lints) turned the dangling doc
comment into a build error.

## Packaging for crates.io (lzcb fork)

This tree is published as **`lzcb-lz4-flex`**, not as `lz4_flex`, so a project can depend on
`lz-copyback` and on the genuine `lz4_flex` at the same time. It is pure Rust, so there is no
`links` key and no C symbol prefixing to do -- the package rename plus the `[lib] name =
"lzcb_lz4_flex"` is the whole story, and `lz-copyback` aliases it back to `lz4_flex` in its
manifest so no `use` statement changes.

The upstream dev-dependencies (`proptest`, `lzzzz`, `binggan`, `jemallocator`,
`lz4-compress`, `more-asserts`, `serde_json`, `snap`) were dropped along with the tests and
benches they served, and the inert `[profile.*]` tables were removed.

## Upgrading

1. Copy the new upstream tree from `~/.cargo/registry/src/*/`, prune as described above.
2. `git apply -p1 ggcat-lz-hook.patch` from the crate directory and resolve conflicts.
   Note the patch includes the `src/fastcpy.rs` lint fix below -- the first generation of
   this file did not, so re-applying it used to silently drop that change.
3. `cargo test -p lz-copyback --test roundtrip_lz4` at the workspace root.
4. `../../tools/gen-patch.sh lz4-flex <upstream-dir>` to refresh this patch.
