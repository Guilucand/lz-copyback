#![doc(html_root_url = "https://docs.rs/lzcb-lzma-sys/0.2.0")]
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
}
