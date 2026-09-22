//! The live member table and the splitting of absolute copies into member pieces.

use super::{MemberCopy, MemberInfo, TarStats};
use crate::copy::LzCopy;
use std::cell::Cell;

/// Members a cursor may walk over before it gives up and searches instead. A cursor that
/// follows the end of a copy that arrives in stream order stays on its member or steps to
/// the next one, so a low limit keeps what that end gains while wasting almost nothing on
/// the other one.
const SCAN_LIMIT: usize = 2;

pub(crate) struct MemberTable {
    /// The members a copy can still be attributed to, oldest first. A `Vec` drained at
    /// the front rather than a `VecDeque`, because every lookup indexes it and a deque
    /// pays for the wrap-around on each access; the drain moves only the live members,
    /// once per window.
    members: Vec<MemberInfo>,
    first_index: u32,
    /// Where the last source lookup landed, and the last destination one. The copies of a
    /// window arrive sorted by one of the two ends -- the source under `Forward`, the
    /// destination under `Backward` -- so that end's cursor only ever walks forward and
    /// costs nothing amortised, while the other end uses its cursor as a hint and falls
    /// back to a search.
    src_cursor: Cell<usize>,
    dst_cursor: Cell<usize>,
}

#[derive(Clone, Copy)]
enum Loc {
    /// Position in `members`.
    Member(usize),
    /// Header or padding bytes up to `until`.
    Gap { until: u64 },
}

impl MemberTable {
    pub(crate) fn new() -> Self {
        Self {
            members: Vec::new(),
            first_index: 0,
            src_cursor: Cell::new(0),
            dst_cursor: Cell::new(0),
        }
    }

    pub(crate) fn push(&mut self, m: MemberInfo) {
        debug_assert_eq!(m.index, self.first_index + self.members.len() as u32);
        self.members.push(m);
    }

    pub(crate) fn get(&self, index: u32) -> Option<&MemberInfo> {
        index.checked_sub(self.first_index).and_then(|i| self.members.get(i as usize))
    }

    /// Drop members whose content ends at or before `before`.
    pub(crate) fn retire(&mut self, before: u64) {
        let popped = self.members.partition_point(|m| m.content_end <= before);
        if popped > 0 {
            self.members.drain(..popped);
            self.first_index += popped as u32;
            // The cursors are positions in `members`, which just shifted down.
            self.src_cursor.set(self.src_cursor.get().saturating_sub(popped));
            self.dst_cursor.set(self.dst_cursor.get().saturating_sub(popped));
        }
    }

    /// Where `abs` falls, given that `i` is the first member ending after it.
    #[inline(always)]
    fn loc_at(&self, i: usize, abs: u64) -> Loc {
        match self.members.get(i) {
            Some(m) if m.content_start <= abs => Loc::Member(i),
            Some(m) => Loc::Gap { until: m.content_start },
            None => Loc::Gap { until: u64::MAX },
        }
    }

    /// Locate `abs` by binary search, leaving `cursor` on the answer.
    fn locate_search(&self, abs: u64, cursor: &Cell<usize>) -> Loc {
        let idx = self.members.partition_point(|m| m.content_end <= abs);
        cursor.set(idx);
        self.loc_at(idx, abs)
    }

    /// Locate `abs` starting from `cursor`, which ends up on the answer.
    ///
    /// The cursor is only ever walked forward, which is what makes the end that arrives in
    /// stream order amortised free: over a whole stream it passes each member once. It is
    /// a hint and nothing more, though -- when it sits past `abs`, or too far before it,
    /// the search takes over -- so an unordered end, or a caller handing copies over in
    /// any order at all, still gets the right answer.
    #[inline]
    fn locate_from(&self, abs: u64, cursor: &Cell<usize>) -> Loc {
        let n = self.members.len();
        let mut i = cursor.get().min(n);
        if i > 0 && self.members[i - 1].content_end > abs {
            return self.locate_search(abs, cursor);
        }
        let mut steps = 0;
        while i < n && self.members[i].content_end <= abs {
            i += 1;
            steps += 1;
            if steps == SCAN_LIMIT {
                return self.locate_search(abs, cursor);
            }
        }
        cursor.set(i);
        self.loc_at(i, abs)
    }

    fn end_of(&self, loc: Loc) -> u64 {
        match loc {
            Loc::Member(i) => self.members[i].content_end,
            Loc::Gap { until } => until,
        }
    }

    /// Cut `c` at every member boundary on both sides and emit the homogeneous pieces.
    pub(crate) fn split_copy(&self, c: LzCopy, min_len: u64, stats: &mut TarStats, out: &mut Vec<MemberCopy>) {
        let mut off = 0;
        while off < c.len() {
            let (s, d) = (c.src() + off, c.dst() + off);
            let (ls, ld) = (
                self.locate_from(s, &self.src_cursor),
                self.locate_from(d, &self.dst_cursor),
            );
            let piece = (c.len() - off).min(self.end_of(ls) - s).min(self.end_of(ld) - d);
            debug_assert!(piece > 0);
            match (ls, ld) {
                (Loc::Member(a), Loc::Member(b)) => {
                    let (ma, mb) = (&self.members[a], &self.members[b]);
                    if ma.opaque.is_some() || mb.opaque.is_some() {
                        stats.dropped_opaque.inc();
                    } else if piece < min_len {
                        stats.dropped_short_piece.inc();
                    } else if let Some(mc) = MemberCopy::try_new(
                        ma.index,
                        s - ma.content_start,
                        mb.index,
                        d - mb.content_start,
                        piece,
                    ) {
                        out.push(mc);
                        stats.pieces.inc();
                        if a == b {
                            stats.intra_member.inc();
                        } else {
                            stats.cross_member.inc();
                        }
                    } else {
                        // A member of 4 GiB or more: the piece cannot be expressed.
                        stats.dropped_oversized.inc();
                    }
                }
                _ => stats.dropped_header_touch.inc(),
            }
            off += piece;
        }
    }

    /// Cut `c` at the member boundaries of its *destination* only and emit the pieces
    /// that land in the content of a non-opaque member, in absolute coordinates.
    ///
    /// Unlike [`Self::split_copy`] this says nothing about the source, which therefore
    /// needs no live member entry: a consumer that only has to know *where* the source
    /// bytes were (rather than which member they belonged to) can use this and keep
    /// `retain_back` small.
    pub(crate) fn clip_dst(&self, c: LzCopy, min_len: u64, stats: &mut TarStats, out: &mut Vec<LzCopy>) {
        let mut off = 0;
        while off < c.len() {
            let d = c.dst() + off;
            let ld = self.locate_from(d, &self.dst_cursor);
            let piece = (c.len() - off).min(self.end_of(ld) - d);
            debug_assert!(piece > 0);
            match ld {
                Loc::Member(b) if self.members[b].opaque.is_none() => {
                    if piece < min_len {
                        stats.dropped_short_piece.inc();
                    } else {
                        out.push(c.piece(off, piece));
                    }
                }
                Loc::Member(_) => stats.dropped_opaque.inc(),
                Loc::Gap { .. } => stats.dropped_header_touch.inc(),
            }
            off += piece;
        }
    }

    /// Member content ranges intersecting `[start, end)`, as window-relative spans.
    pub(crate) fn spans(&self, start: u64, end: u64, out: &mut Vec<super::MemberSpan>) {
        for m in &self.members {
            if m.content_end <= start || m.content_start >= end {
                continue;
            }
            let lo = m.content_start.max(start);
            let hi = m.content_end.min(end);
            out.push(super::MemberSpan {
                member: m.index,
                window_off: (lo - start) as usize,
                len: (hi - lo) as usize,
                member_off: lo - m.content_start,
            });
        }
    }
}
