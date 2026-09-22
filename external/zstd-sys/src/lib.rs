#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![no_std]
//! Low-level bindings to the [zstd] library, forked for `lz-copyback`.
//!
//! This fork exposes the decoder's LZ match hook (`ZSTD_DCtx_setLzMatchHook`,
//! `ZSTD_DCtx_getLzTotalOut`) and prefixes every C symbol with `lzcb_`, so it can be
//! linked into a binary that also links the genuine `zstd-sys`. See `PATCH.md`.
//!
//! [zstd]: https://facebook.github.io/zstd/

#[cfg(target_arch = "wasm32")]
extern crate alloc;

#[cfg(target_arch = "wasm32")]
mod wasm_shim;

include!("bindings_zstd.rs");
