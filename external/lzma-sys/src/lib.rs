#![doc(html_root_url = "https://docs.rs/lzcb-lzma-sys/0.3.0")]
#![allow(bad_style)]

//! Raw bindings to liblzma, forked for `lz-copyback`.
//!
//! This fork decodes in place only (see [`lzma_lz_in_place`]), exposes the LZ decoder's match
//! hook (`lzma_lz_set_match_hook`) and prefixes every C symbol with `lzcb_`, so it can be
//! linked into a binary that also links the genuine `liblzma-sys`. See `PATCH.md`.

mod bindgen;
mod bindgen_wrap;

#[cfg(target_arch = "wasm32")]
mod wasm_shim;

pub use bindgen_wrap::*;

/// Called once for every LZ match the decoder decodes, with the absolute offset of its
/// destination in the decoded stream, its distance and its full length.
pub type lzma_lz_match_hook =
    Option<unsafe extern "C" fn(ctx: *mut libc::c_void, dst_abs: u64, distance: u32, len: u32)>;

extern "C" {
    /// Installs `hook` for the calling thread, for every `lzma_code` call until it is
    /// cleared with `None`. `out_abs_base` is the absolute stream offset of the first byte
    /// the next `lzma_code` call writes; matches shorter than `min_len` bytes are not
    /// reported.
    #[link_name = "lzcb_lzma_lz_set_match_hook"]
    pub fn lzma_lz_set_match_hook(
        hook: lzma_lz_match_hook,
        ctx: *mut libc::c_void,
        out_abs_base: u64,
        min_len: u32,
    );

    /// Installs the calling thread's in-place state, for every `lzma_code` call until it is
    /// removed with a null pointer. See [`lzma_lz_in_place`].
    #[link_name = "lzcb_lzma_lz_set_in_place"]
    pub fn lzma_lz_set_in_place(state: *mut lzma_lz_in_place);

    /// Frees a dictionary handed over by [`lzma_lz_in_place::retire`].
    #[link_name = "lzcb_lzma_lz_in_place_free"]
    pub fn lzma_lz_in_place_free(buf: *mut u8);
}

/// In-place decoding, the only way this fork decodes: the decoded bytes are read straight
/// from the LZ decoder's dictionary, which `lzma_code` is given as its output buffer, at
/// `next`, instead of being copied out of it. A decoding step whose output buffer is not
/// there decodes nothing, so that the caller can call again with the right one. Without a
/// state installed, the LZ decoder cannot be initialised.
///
/// A reset keeps the decoded bytes in place, and a dictionary that liblzma would free (too
/// small for the next block, or its filter chain replaced) is handed to `retire` instead, to
/// be freed with [`lzma_lz_in_place_free`]. A block with a filter in front of LZMA2 sets
/// `filtered`: its output is not in the dictionary, and must be decoded into a buffer outside
/// it.
#[repr(C)]
pub struct lzma_lz_in_place {
    /// Set by the caller: the smallest dictionary allocated.
    pub min_size: usize,
    /// The current dictionary, with `size` usable bytes.
    pub buf: *mut u8,
    pub size: usize,
    /// Index in `buf` of the next byte to be decoded.
    pub next: usize,
    pub filtered: bool,
    pub retire: Option<unsafe extern "C" fn(ctx: *mut libc::c_void, buf: *mut u8, size: usize)>,
    pub retire_ctx: *mut libc::c_void,
}
