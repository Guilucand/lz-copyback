//! Every global symbol the built archive defines must carry the `lzcb_` prefix, or this
//! fork cannot be linked into a binary that also links the genuine `liblzma-sys`.
//!
//! This is the guard for an upstream bump that adds a symbol, a symbol that only appears
//! under some feature combination, and a silently dropped `-include` flag. On failure it
//! names the offenders, so regenerating `lzcb_prefix.h` is mechanical.
mod common;

#[test]
fn every_exported_symbol_is_prefixed() {
    common::assert_all_prefixed("liblzma.a");
}
