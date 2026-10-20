//! Patched copies of the LZ decoders ggcat reads its inputs with, each reporting the LZ
//! matches ("copybacks") it decodes.
//!
//! | codec | fork | decoder |
//! |---|---|---|
//! | xz | `lzcb-liblzma` (liblzma 0.4.9) | `lzcb_liblzma::read::XzDecoder` |
//! | zstd | `lzcb-zstd` (zstd 0.13.3) | `lzcb_zstd::stream::read::Decoder` |
//! | lz4 | `lzcb-lz4` (lz4 1.28.1) | `lzcb_lz4::Decoder` |
//! | gzip | `streaming-libdeflate-rs` | `LibdeflateGzipFileDecompressor`, `LibdeflateGzipPushDecompressor` |
//!
//! Every one of those decoders has the same three extra methods:
//!
//! * `set_copybacks_tracking(bool)`: on at construction; while off, matches are ignored.
//! * `set_copybacks_min_len(u32)`: matches shorter than this are ignored; 1 at construction.
//! * `peek_copybacks() -> &mut VecDeque<(u64, u32, u32)>`: every match recorded so far, in
//!   stream order. The decoder only ever pushes to the back; removing entries is up to
//!   the caller.
//!
//! A match is pushed during the call that hands out its first destination byte, or
//! earlier: zstd and lz4 decode a whole block into a buffer of their own before handing
//! it out, so their matches can arrive up to one block ahead of the bytes.
//!
//! The forks push plain tuples so they need no common dependency; this crate only names
//! the tuple and its fields.

/// One LZ match exactly as the decoder decoded it, never merged, clipped or split:
/// `(destination, offset, len)`. See [`CopybackExt`] for the fields.
pub type Copyback = (u64, u32, u32);

/// The fields of a [`Copyback`] by name.
#[allow(clippy::len_without_is_empty)]
pub trait CopybackExt {
    /// Absolute offset of the first byte the match writes, counted from the first byte the
    /// decoder produced (across concatenated streams, frames and members).
    fn destination(&self) -> u64;

    /// How far back the source starts: the match copies the bytes from
    /// `destination - offset` on. Smaller than [`Self::len`] for a match that overlaps its
    /// own output (a periodic repeat).
    fn offset(&self) -> u32;

    /// Number of bytes the match writes.
    fn len(&self) -> u32;
}

impl CopybackExt for Copyback {
    #[inline(always)]
    fn destination(&self) -> u64 {
        self.0
    }

    #[inline(always)]
    fn offset(&self) -> u32 {
        self.1
    }

    #[inline(always)]
    fn len(&self) -> u32 {
        self.2
    }
}
