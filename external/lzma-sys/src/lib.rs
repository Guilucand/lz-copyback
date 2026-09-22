#![doc(html_root_url = "https://docs.rs/lzcb-lzma-sys/0.1.0")]
#![allow(bad_style)]

//! Raw bindings to liblzma, forked for `lz-copyback`.
//!
//! This fork exposes the LZ decoder's match hook (`lzma_lz_set_match_hook`) and prefixes
//! every C symbol with `lzcb_`, so it can be linked into a binary that also links the
//! genuine `liblzma-sys`. See `PATCH.md`.

mod bindgen;
mod bindgen_wrap;

#[cfg(target_arch = "wasm32")]
mod wasm_shim;

pub use bindgen_wrap::*;
