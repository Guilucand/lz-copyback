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
    /// `None`; matches shorter than `min_length` bytes are not reported, but for one that
    /// starts at `last_end`, `last_offset` back: where the last match reported ended, which
    /// the context follows from there on. Destinations
    /// count every byte the context has decoded since it was created through
    /// `ZSTD_decompressContinue` (the buffer-less API, which the streaming one runs on but
    /// for whole frames that fit its output): the count is never reset, not between frames
    /// nor by `ZSTD_DCtx_reset`.
    #[link_name = "lzcb_ZSTD_DCtx_setLzMatchHook"]
    pub fn ZSTD_DCtx_setLzMatchHook(
        dctx: *mut ZSTD_DCtx,
        hook: ZSTD_lzMatchHook,
        ctx: *mut ::core::ffi::c_void,
        min_length: ::core::ffi::c_uint,
        last_end: ::core::ffi::c_ulonglong,
        last_offset: ::core::ffi::c_uint,
    );

    /// Bytes `dctx` has decoded since it was created (the base of the hook's offsets).
    #[link_name = "lzcb_ZSTD_DCtx_getLzTotalOut"]
    pub fn ZSTD_DCtx_getLzTotalOut(dctx: *const ZSTD_DCtx) -> ::core::ffi::c_ulonglong;
}

/// `ZSTD_FrameHeader` from zstd's static-linking-only API, filled by [`ZSTD_getFrameHeader`].
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ZSTD_FrameHeader {
    /// `ZSTD_CONTENTSIZE_UNKNOWN` when the frame does not tell.
    pub frameContentSize: ::core::ffi::c_ulonglong,
    pub windowSize: ::core::ffi::c_ulonglong,
    pub blockSizeMax: ::core::ffi::c_uint,
    /// [`ZSTD_frame`] or [`ZSTD_skippableFrame`].
    pub frameType: ::core::ffi::c_uint,
    pub headerSize: ::core::ffi::c_uint,
    pub dictID: ::core::ffi::c_uint,
    pub checksumFlag: ::core::ffi::c_uint,
    pub _reserved1: ::core::ffi::c_uint,
    pub _reserved2: ::core::ffi::c_uint,
}

pub const ZSTD_frame: ::core::ffi::c_uint = 0;
pub const ZSTD_skippableFrame: ::core::ffi::c_uint = 1;

/// `ZSTD_nextInputType_e` values.
pub const ZSTDnit_frameHeader: ::core::ffi::c_uint = 0;
pub const ZSTDnit_blockHeader: ::core::ffi::c_uint = 1;
pub const ZSTDnit_block: ::core::ffi::c_uint = 2;
pub const ZSTDnit_lastBlock: ::core::ffi::c_uint = 3;
pub const ZSTDnit_checksum: ::core::ffi::c_uint = 4;
pub const ZSTDnit_skippableFrame: ::core::ffi::c_uint = 5;

// zstd's buffer-less streaming decompression (static-linking-only API): the caller decodes
// each block straight into a buffer of its own, typically a ring, see zstd.h.
extern "C" {
    #[link_name = "lzcb_ZSTD_getFrameHeader"]
    pub fn ZSTD_getFrameHeader(
        zfhPtr: *mut ZSTD_FrameHeader,
        src: *const ::core::ffi::c_void,
        srcSize: usize,
    ) -> usize;

    #[link_name = "lzcb_ZSTD_decodingBufferSize_min"]
    pub fn ZSTD_decodingBufferSize_min(
        windowSize: ::core::ffi::c_ulonglong,
        frameContentSize: ::core::ffi::c_ulonglong,
    ) -> usize;

    #[link_name = "lzcb_ZSTD_decompressBegin"]
    pub fn ZSTD_decompressBegin(dctx: *mut ZSTD_DCtx) -> usize;

    #[link_name = "lzcb_ZSTD_nextSrcSizeToDecompress"]
    pub fn ZSTD_nextSrcSizeToDecompress(dctx: *mut ZSTD_DCtx) -> usize;

    #[link_name = "lzcb_ZSTD_decompressContinue"]
    pub fn ZSTD_decompressContinue(
        dctx: *mut ZSTD_DCtx,
        dst: *mut ::core::ffi::c_void,
        dstCapacity: usize,
        src: *const ::core::ffi::c_void,
        srcSize: usize,
    ) -> usize;

    /// One of the `ZSTDnit_*` values.
    #[link_name = "lzcb_ZSTD_nextInputType"]
    pub fn ZSTD_nextInputType(dctx: *mut ZSTD_DCtx) -> ::core::ffi::c_uint;
}
