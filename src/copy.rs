use std::fmt;

/// A forward LZ copy in absolute decompressed-stream coordinates: the `len` bytes at
/// `src..src_end` appear again, unchanged, at `dst..dst_end`.
///
/// Invariants of every copy a [`crate::TrackingDecoder`] emits: `dst > src`,
/// `dst - src >= len` (the two ranges never overlap) and `len >= min_len`.
///
/// Only the source is absolute. The destination is kept as its distance from the source
/// and the length beside it, both `u32`, so a copy is 16 bytes rather than 24: an LZ match
/// can never reach further back than the dictionary, whose size is a `u32` field in the
/// deflate, LZMA2, zstd and lz4 headers alike, and the length is clipped to the distance
/// before a copy is emitted. Everything is still read in absolute `u64` coordinates
/// through the accessors.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LzCopy {
    src: u64,
    /// `dst - src`.
    distance: u32,
    len: u32,
}

impl LzCopy {
    /// Panics when `dst` is behind `src`, or when the distance or the length does not fit
    /// in a `u32`; [`Self::try_new`] is the checked form.
    #[inline]
    pub fn new(src: u64, dst: u64, len: u64) -> Self {
        Self::try_new(src, dst, len).expect("copy out of the representable range")
    }

    /// `None` when `dst` is behind `src`, or when the distance or the length is larger
    /// than a `u32`, which no LZ match of a supported format can be.
    #[inline]
    pub fn try_new(src: u64, dst: u64, len: u64) -> Option<Self> {
        Some(Self {
            src,
            distance: u32::try_from(dst.checked_sub(src)?).ok()?,
            len: u32::try_from(len).ok()?,
        })
    }

    #[inline(always)]
    pub fn src(&self) -> u64 {
        self.src
    }

    #[inline(always)]
    pub fn dst(&self) -> u64 {
        self.src + self.distance as u64
    }

    #[inline(always)]
    pub fn len(&self) -> u64 {
        self.len as u64
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline(always)]
    pub fn distance(&self) -> u64 {
        self.distance as u64
    }

    #[inline(always)]
    pub fn src_end(&self) -> u64 {
        self.src + self.len as u64
    }

    #[inline(always)]
    pub fn dst_end(&self) -> u64 {
        self.dst() + self.len as u64
    }

    /// The first `len` bytes of this copy.
    #[inline]
    pub fn head(&self, len: u64) -> Self {
        debug_assert!(len <= self.len());
        Self { len: len as u32, ..*self }
    }

    /// This copy with its first `off` bytes cut away; both ends move by `off`, so the
    /// distance is the same.
    #[inline]
    pub fn tail(&self, off: u64) -> Self {
        debug_assert!(off <= self.len());
        Self { src: self.src + off, len: self.len - off as u32, ..*self }
    }

    /// The `len` bytes of this copy that start `off` bytes into it.
    #[inline]
    pub fn piece(&self, off: u64, len: u64) -> Self {
        self.tail(off).head(len)
    }
}

impl fmt::Debug for LzCopy {
    /// Printed in the absolute coordinates the accessors report, not as stored.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LzCopy")
            .field("src", &self.src)
            .field("dst", &self.dst())
            .field("len", &self.len())
            .finish()
    }
}
