//! Decoders for the LZ formats ggcat reads (xz, zstd, lz4, gzip) that decode in place and
//! report the LZ matches ("copybacks") they decode: which output bytes are verbatim repeats of
//! earlier output.
//!
//! | codec | decoder | the ring it decodes into |
//! |---|---|---|
//! | xz, also `.lzma` and `.lz` | [`xz::XzInPlaceDecoder`] | liblzma's own dictionary |
//! | zstd | [`zstd::ZstdInPlaceDecoder`] | its own, filled a block at a time by zstd's buffer-less API |
//! | lz4 | [`lz4::Lz4InPlaceDecoder`] | its own, filled a block at a time by liblz4 |
//! | gzip | [`gzip::GzipInPlaceDecoder`] | its own, with 32 KiB of history copied in front at each wrap |
//!
//! xz, zstd and lz4 run on patched copies of their C libraries (`external/`), gzip on
//! `streaming-libdeflate-rs` and its match hook. All of them are [`InPlaceDecoder`]s: the
//! output stays in a ring buffer of the decoder's own and is read there, never copied into the
//! caller's buffer, and the matches go to a queue:
//!
//! * `set_copybacks_tracking(bool)`: on at construction; while off, matches are ignored.
//! * `set_copybacks_min_len(u32)`: matches shorter than this are ignored; 1 at construction.
//! * `peek_copybacks() -> &mut VecDeque<Copyback>`: every match recorded so far, in stream
//!   order. The decoder only ever pushes to the back; removing entries is up to the caller.
//!
//! A match is pushed during the `decode_to` call that decodes its first destination byte, or
//! earlier: zstd and lz4 decode a whole block at once, so their matches can arrive up to one
//! block ahead of the bytes asked for.

mod copybacks;
pub mod gzip;
mod in_place;
pub mod lz4;
pub mod xz;
pub mod zstd;

pub use in_place::{InPlaceDecoder, Pieces};

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
