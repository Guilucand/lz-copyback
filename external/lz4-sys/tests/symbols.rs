//! Every global symbol the built archive defines must carry the `lzcb_` prefix, or this
//! fork cannot be linked into a binary that also links the genuine `lz4-sys`.
//!
//! This is the guard for an upstream bump that adds a symbol, a symbol that only appears
//! under some feature combination, and a silently dropped `-include` flag. On failure it
//! names the offenders, so regenerating `lzcb_prefix.h` is mechanical.
// The crate is edition 2015, where an external crate must be declared.
extern crate object;

mod common;

#[test]
fn every_exported_symbol_is_prefixed() {
    common::assert_all_prefixed("liblz4.a");
}
