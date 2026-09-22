use crate::recorder::CopyRecorder;

pub type BackendError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Bytes kept writable past the fill target of every `decode` call. Deflate refuses to
/// decode unless 520 bytes are writable and over-writes up to 40 bytes past a match, so
/// the target may be overshot by up to this amount.
pub const SLACK: usize = 1024;

/// A decompressor whose first stage is LZ77 (or a passthrough for formats without one).
pub trait LzBackend {
    /// Maximum back-reference distance of the stream, known at construction because the
    /// stream, frame or block header is parsed eagerly. `0` means the format has no LZ
    /// stage or tracking is not possible for this stream, and no copies are reported.
    fn max_distance(&self) -> u64;

    /// Bytes that must be addressable, and hold the previous output, immediately before
    /// the first output byte. Deflate reads `dst - distance` without checking that the
    /// stream has produced that many bytes, so a corrupt stream must land in a guard
    /// region instead of before the allocation. Deflate: 32768, others: 0.
    fn history_required(&self) -> usize {
        0
    }

    /// Decode into `buf[pos..]`.
    ///
    /// * `buf[..pos]` holds the previous output, contiguously (the deflate history).
    /// * Try to reach `target` (`target + SLACK <= buf.len()`), but the backend may
    ///   overshoot up to `buf.len()`; every byte in `buf[pos..pos + produced]` is committed
    ///   output.
    /// * Return `Ok(0)` exactly once, at the end of the stream; it is never called again.
    /// * Every LZ match executed (possibly partially) must be reported, in destination
    ///   order, through [`CopyRecorder::record`] with the absolute stream offset of its
    ///   destination. Backends may skip reporting entirely while
    ///   [`CopyRecorder::is_enabled`] is false.
    fn decode(
        &mut self,
        buf: &mut [u8],
        pos: usize,
        target: usize,
        rec: &mut CopyRecorder,
    ) -> Result<usize, BackendError>;
}

impl<B: LzBackend + ?Sized> LzBackend for Box<B> {
    fn max_distance(&self) -> u64 {
        (**self).max_distance()
    }

    fn history_required(&self) -> usize {
        (**self).history_required()
    }

    fn decode(
        &mut self,
        buf: &mut [u8],
        pos: usize,
        target: usize,
        rec: &mut CopyRecorder,
    ) -> Result<usize, BackendError> {
        (**self).decode(buf, pos, target, rec)
    }
}
