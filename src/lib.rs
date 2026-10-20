//! Decoders for the LZ formats ggcat reads (xz, zstd, lz4, gzip) that also report the LZ
//! matches ("copybacks") they decode: which output bytes are verbatim repeats of earlier
//! output.
//!
//! | codec | decoder | interface of |
//! |---|---|---|
//! | xz, also `.lzma` and `.lz` | [`xz::XzDecoder`] | `liblzma::read::XzDecoder` |
//! | zstd | [`zstd::ZstdDecoder`] | `zstd::stream::read::Decoder` |
//! | lz4 | [`lz4::Lz4Decoder`] | `lz4::Decoder` |
//! | gzip | [`gzip::GzipFileDecompressor`], [`gzip::GzipPushDecompressor`] | `streaming_libdeflate_rs::{LibdeflateGzipFileDecompressor, LibdeflateGzipPushDecompressor}` |
//!
//! [`gzip::GzipDecoder`] adds a [`std::io::Read`] over the push decompressor.
//!
//! xz, zstd and lz4 run on patched copies of their C libraries (`external/`), gzip on
//! `streaming-libdeflate-rs` and its match hook. Every decoder has the same three extra
//! methods:
//!
//! * `set_copybacks_tracking(bool)`: on at construction; while off, matches are ignored.
//! * `set_copybacks_min_len(u32)`: matches shorter than this are ignored; 1 at construction.
//! * `peek_copybacks() -> &mut VecDeque<Copyback>`: every match recorded so far, in stream
//!   order. The decoder only ever pushes to the back; removing entries is up to the caller.
//!
//! A match is pushed during the call that hands out its first destination byte, or earlier:
//! zstd and lz4 decode a whole block into a buffer of their own before handing it out, so
//! their matches can arrive up to one block ahead of the bytes.

mod copybacks;
pub mod gzip;
pub mod lz4;
pub mod xz;
pub mod zstd;

/// One LZ match exactly as the decoder decoded it, never merged, clipped or split: the match
/// writes `len` bytes at `destination`, copying them from `destination - offset` on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Copyback {
    /// Absolute offset of the first byte the match writes, counted from the first byte the
    /// decoder produced (across concatenated streams, frames and members).
    pub destination: u64,
    /// How far back the source starts. Smaller than `len` for a match that overlaps its own
    /// output (a periodic repeat).
    pub offset: u32,
    /// Number of bytes the match writes.
    pub len: u32,
}

impl Copyback {
    /// Absolute offset of the first byte the match copies.
    #[inline(always)]
    pub fn source(&self) -> u64 {
        self.destination - self.offset as u64
    }
}
