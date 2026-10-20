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

/// Called once for every LZ match the decoder decodes, with the absolute offset of its
/// destination in the decoded stream, its distance and its full length.
pub type ZSTD_lzMatchHook = ::core::option::Option<
    unsafe extern "C" fn(
        ctx: *mut ::core::ffi::c_void,
        dst_abs: ::core::ffi::c_ulonglong,
        distance: ::core::ffi::c_uint,
        match_length: ::core::ffi::c_uint,
    ),
>;

extern "C" {
    /// Installs `hook` on `dctx`, for every decoding call until it is cleared with
    /// `None`; matches shorter than `min_length` bytes are not reported. Destinations
    /// count every byte the context has decoded since it was created: the count is never
    /// reset, not between frames nor by `ZSTD_DCtx_reset`.
    #[link_name = "lzcb_ZSTD_DCtx_setLzMatchHook"]
    pub fn ZSTD_DCtx_setLzMatchHook(
        dctx: *mut ZSTD_DCtx,
        hook: ZSTD_lzMatchHook,
        ctx: *mut ::core::ffi::c_void,
        min_length: ::core::ffi::c_uint,
    );

    /// Bytes `dctx` has decoded since it was created (the base of the hook's offsets).
    #[link_name = "lzcb_ZSTD_DCtx_getLzTotalOut"]
    pub fn ZSTD_DCtx_getLzTotalOut(dctx: *const ZSTD_DCtx) -> ::core::ffi::c_ulonglong;
}
