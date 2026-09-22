//! Attribution of LZ copies to the members of a tar archive.
//!
//! The tracked stream is the tar itself, so a copy's source and destination may fall in
//! different members, in a header, or in padding. [`TarTracker`] parses headers
//! incrementally as windows go by, keeps the members that can still be touched by a copy
//! and splits every [`crate::LzCopy`] into member-relative [`MemberCopy`] pieces.

mod attribute;
mod header;

use crate::copy::LzCopy;
use crate::stats::Stat;
use crate::decoder::Window;
use attribute::MemberTable;
use header::HeaderParser;
use std::fmt;

/// Why a member's bytes are not sequence data at the tar-stream level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpaqueReason {
    /// Directory, link, device or other non-file entry.
    NotRegularFile,
    /// The member is itself compressed (`.fa.gz` inside a `.tar.xz`): the outer stream's
    /// copies repeat compressed bytes, which say nothing about the sequences.
    CompressedName,
    /// Rejected by the caller's filter.
    Filtered,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberInfo {
    /// Position in the archive, counting every entry (opaque ones included).
    pub index: u32,
    /// Raw path bytes (PAX or GNU long name applied).
    pub name: Vec<u8>,
    /// Absolute offset of the first header block belonging to this member.
    pub header_start: u64,
    pub content_start: u64,
    pub content_end: u64,
    pub opaque: Option<OpaqueReason>,
}

impl MemberInfo {
    pub fn len(&self) -> u64 {
        self.content_end - self.content_start
    }
}

/// A copy between member contents: `len` bytes at `src_off` of `src_member` appear again
/// at `dst_off` of `dst_member`.
///
/// The two offsets and the length are `u32`, which makes a piece 20 bytes rather than 32:
/// an offset is bounded by the size of its member and a length by the tracking window, so
/// the only thing this rules out is a tar member of 4 GiB or more, whose pieces the tar
/// layer drops and counts in [`TarStats::dropped_oversized`] rather than truncating.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MemberCopy {
    pub src_member: u32,
    pub dst_member: u32,
    src_off: u32,
    dst_off: u32,
    len: u32,
}

impl MemberCopy {
    /// `None` when an offset or the length does not fit in a `u32`.
    #[inline]
    pub fn try_new(src_member: u32, src_off: u64, dst_member: u32, dst_off: u64, len: u64) -> Option<Self> {
        Some(Self {
            src_member,
            dst_member,
            src_off: u32::try_from(src_off).ok()?,
            dst_off: u32::try_from(dst_off).ok()?,
            len: u32::try_from(len).ok()?,
        })
    }

    /// Panics when an offset or the length does not fit in a `u32`.
    #[inline]
    pub fn new(src_member: u32, src_off: u64, dst_member: u32, dst_off: u64, len: u64) -> Self {
        Self::try_new(src_member, src_off, dst_member, dst_off, len)
            .expect("member copy out of the representable range")
    }

    #[inline(always)]
    pub fn src_off(&self) -> u64 {
        self.src_off as u64
    }

    #[inline(always)]
    pub fn dst_off(&self) -> u64 {
        self.dst_off as u64
    }

    #[inline(always)]
    pub fn len(&self) -> u64 {
        self.len as u64
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl fmt::Debug for MemberCopy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemberCopy")
            .field("src_member", &self.src_member)
            .field("src_off", &self.src_off)
            .field("dst_member", &self.dst_member)
            .field("dst_off", &self.dst_off)
            .field("len", &self.len)
            .finish()
    }
}

/// The part of a member's content that lies inside an emitted window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemberSpan {
    pub member: u32,
    pub window_off: usize,
    pub len: usize,
    pub member_off: u64,
}

/// Every field is a [`Stat`], so the whole struct is zero-sized without the `stats`
/// feature and the counting compiles away with it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TarStats {
    pub members: Stat,
    pub opaque_members: Stat,
    /// Member copies produced.
    pub pieces: Stat,
    pub cross_member: Stat,
    pub intra_member: Stat,
    /// Pieces touching header or padding bytes on either side.
    pub dropped_header_touch: Stat,
    pub dropped_opaque: Stat,
    pub dropped_short_piece: Stat,
    /// Pieces whose offsets or length did not fit in the `u32` fields of a
    /// [`MemberCopy`], which needs a member of 4 GiB or more.
    pub dropped_oversized: Stat,
    /// Bytes after the end-of-archive marker.
    pub trailing_bytes: Stat,
}

pub struct TarConfig {
    /// Pieces shorter than this are dropped (re-applied after splitting).
    pub min_len: u64,
    /// Decides from the member name whether its content is opaque.
    pub opaque_member: Box<dyn Fn(&[u8]) -> bool + Send>,
    /// How far back of the stream must stay attributable. Members ending more than this
    /// many bytes before the end of the processed window are dropped. `0` (the default)
    /// suits [`crate::Direction::Forward`], where a copy is reported while its source is
    /// still in the window; under [`crate::Direction::Backward`] the source lies in the
    /// past, so this must be at least the stream's maximum copy distance.
    pub retain_back: u64,
    /// Error when the stream ends without the two zero blocks.
    pub strict_end: bool,
}

/// Compression suffixes that make a member opaque by default (mirrors
/// `crates/io/src/sequences_stream/tar.rs`).
pub const COMPRESSION_SUFFIXES: [&[u8]; 6] = [b".gz", b".bz2", b".xz", b".zst", b".zstd", b".lz4"];

pub fn has_compression_suffix(name: &[u8]) -> bool {
    COMPRESSION_SUFFIXES.iter().any(|s| name.ends_with(s))
}

impl Default for TarConfig {
    fn default() -> Self {
        Self {
            min_len: crate::config::DEFAULT_MIN_LEN,
            opaque_member: Box::new(has_compression_suffix),
            retain_back: 0,
            strict_end: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TarError {
    BadChecksum { abs: u64 },
    BadField { abs: u64, what: &'static str },
    TruncatedHeader { abs: u64 },
    TruncatedContent { member: u32, missing: u64 },
    UnsupportedEntry { abs: u64, typeflag: u8 },
    BadPax { abs: u64 },
    /// Windows must be fed in stream order without gaps.
    OutOfOrder { expected: u64, got: u64 },
    MissingEndBlocks,
}

impl fmt::Display for TarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TarError::BadChecksum { abs } => write!(f, "tar header checksum mismatch at offset {abs}"),
            TarError::BadField { abs, what } => write!(f, "bad tar header field {what} at offset {abs}"),
            TarError::TruncatedHeader { abs } => write!(f, "tar stream ends inside a header at offset {abs}"),
            TarError::TruncatedContent { member, missing } => {
                write!(f, "tar stream ends inside member {member} ({missing} bytes missing)")
            }
            TarError::UnsupportedEntry { abs, typeflag } => {
                write!(f, "unsupported tar entry type {:?} at offset {abs}", *typeflag as char)
            }
            TarError::BadPax { abs } => write!(f, "malformed PAX extended header at offset {abs}"),
            TarError::OutOfOrder { expected, got } => {
                write!(f, "tar windows out of order: expected offset {expected}, got {got}")
            }
            TarError::MissingEndBlocks => write!(f, "tar stream ends without end-of-archive blocks"),
        }
    }
}

impl std::error::Error for TarError {}

/// Everything the tar layer knows about one emitted window.
pub struct TarWindow<'a> {
    /// Member-relative copies derived from the window's copies, in the same order.
    pub copies: &'a [MemberCopy],
    /// Under [`crate::Direction::Both`], the window's destination-keyed copies clipped to
    /// the content of a non-opaque member, in *absolute* coordinates and still sorted by
    /// `(dst, src)`. Only the destination is attributed, so the source member need not be
    /// live and `retain_back` can stay small. Empty under the other directions.
    pub back_copies: &'a [LzCopy],
    /// Member content ranges intersecting the window's `data`, in stream order.
    pub spans: &'a [MemberSpan],
    /// Members whose header was completed while processing this window.
    pub new_members: &'a [MemberInfo],
    pub stats: TarStats,
}

pub struct TarTracker {
    config: TarConfig,
    parser: HeaderParser,
    table: MemberTable,
    copies: Vec<MemberCopy>,
    back_copies: Vec<LzCopy>,
    spans: Vec<MemberSpan>,
    new_members: Vec<MemberInfo>,
    stats: TarStats,
}

impl TarTracker {
    pub fn new(config: TarConfig) -> Self {
        Self {
            config,
            parser: HeaderParser::new(),
            table: MemberTable::new(),
            copies: Vec::new(),
            back_copies: Vec::new(),
            spans: Vec::new(),
            new_members: Vec::new(),
            stats: TarStats::default(),
        }
    }

    pub fn stats(&self) -> TarStats {
        self.stats
    }

    /// A member that can still be touched by copies (retired members return `None`).
    pub fn member(&self, index: u32) -> Option<&MemberInfo> {
        self.table.get(index)
    }

    /// Process the next window of the tracked tar stream. Windows must arrive in order.
    pub fn process(&mut self, w: &Window<'_>) -> Result<TarWindow<'_>, TarError> {
        self.new_members.clear();
        self.copies.clear();
        self.back_copies.clear();
        self.spans.clear();

        // Parse headers up to the end of the decoded bytes, so every copy destination
        // (which may lie in the lookahead) is attributable.
        let opaque = &*self.config.opaque_member;
        let data_end = w.start_abs + w.data.len() as u64;
        self.parser.feed(w.start_abs, w.data, opaque, &mut self.new_members)?;
        self.parser.feed(data_end, w.lookahead, opaque, &mut self.new_members)?;
        for m in &self.new_members {
            self.stats.members.inc();
            if m.opaque.is_some() {
                self.stats.opaque_members.inc();
            }
            self.table.push(m.clone());
        }

        for c in w.copies {
            self.table.split_copy(*c, self.config.min_len, &mut self.stats, &mut self.copies);
        }
        for c in w.back_copies {
            self.table.clip_dst(*c, self.config.min_len, &mut self.stats, &mut self.back_copies);
        }
        self.table.spans(w.start_abs, data_end, &mut self.spans);
        self.table.retire(data_end.saturating_sub(self.config.retain_back));

        if w.is_last {
            self.parser.finish(self.config.strict_end)?;
            self.stats.trailing_bytes.set(self.parser.trailing_bytes());
        }

        Ok(TarWindow {
            copies: &self.copies,
            back_copies: &self.back_copies,
            spans: &self.spans,
            new_members: &self.new_members,
            stats: self.stats,
        })
    }

    /// Check that the archive ended cleanly (also done automatically on the last window).
    pub fn finish(&self) -> Result<(), TarError> {
        self.parser.finish(self.config.strict_end)
    }

    /// Split one absolute copy into member pieces without processing a window (the
    /// members must already be known).
    pub fn split_copy(&mut self, c: LzCopy, out: &mut Vec<MemberCopy>) {
        self.table.split_copy(c, self.config.min_len, &mut self.stats, out);
    }
}
