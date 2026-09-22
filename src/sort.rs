//! Ordering a window's copies by source.
//!
//! The backends report their matches in *destination* order, so the destination-keyed list
//! of a window comes out sorted already, while the source-keyed one does not: about half
//! of its neighbouring pairs are out of order on real data, since a match at any
//! destination may source from anywhere behind it.
//!
//! Sorting it with a comparison sort costs `log2(n)` passes over the whole list, and the
//! key is a plain integer, which is what a radix sort is for: `voracious_radix_sort` sorts
//! the copies in place, keyed on the source, in about half the time (`examples/sortbench.rs`
//! compares the two). The radix passes say nothing about copies that *share* a source,
//! though, so the runs that do are ordered by destination afterwards -- together that is
//! exactly `sort_unstable_by_key(|c| (c.src(), c.dst()))`.

use crate::copy::LzCopy;
use std::cmp::Ordering;
use voracious_radix_sort::{RadixSort, Radixable};

/// Below this the standard sort is simply faster.
const MIN_RADIX_LEN: usize = 64;

/// A copy keyed on its source for the radix sort. `#[repr(transparent)]`, so a slice of
/// copies is sorted in place rather than through a second array.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, PartialEq)]
struct SrcKeyed(LzCopy);

impl PartialOrd for SrcKeyed {
    /// Refines the radix key with the destination, which is the order the window promises;
    /// the sort falls back to this for the small runs it finishes by comparison.
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        (self.0.src(), self.0.dst()).partial_cmp(&(other.0.src(), other.0.dst()))
    }
}

impl Radixable<u64> for SrcKeyed {
    type Key = u64;

    #[inline]
    fn key(&self) -> u64 {
        self.0.src()
    }
}

/// Order `copies` by `(src, dst)`.
pub(crate) fn by_source(copies: &mut [LzCopy]) {
    if copies.len() < MIN_RADIX_LEN {
        copies.sort_unstable_by_key(|c| (c.src(), c.dst()));
        return;
    }

    // SAFETY: `SrcKeyed` is `#[repr(transparent)]` over `LzCopy`, so the two slices have
    // the same layout, and the borrow lasts no longer than the one it is made from.
    let keyed: &mut [SrcKeyed] =
        unsafe { std::slice::from_raw_parts_mut(copies.as_mut_ptr() as *mut SrcKeyed, copies.len()) };
    keyed.voracious_sort();

    // Copies that share a source are in no particular order after the radix passes.
    let mut i = 0;
    while i < copies.len() {
        let src = copies[i].src();
        let mut j = i + 1;
        while j < copies.len() && copies[j].src() == src {
            j += 1;
        }
        if j - i > 1 {
            copies[i..j].sort_unstable_by_key(|c| c.dst());
        }
        i = j;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Copies as a window holds them before sorting: destinations ascending, sources
    /// spread over `span`, and every `share`-th copy reusing the previous source so that
    /// the tie ordering is exercised.
    fn sample(n: usize, span: u64, base: u64, share: usize) -> Vec<LzCopy> {
        let mut state = 0x243f_6a88_85a3_08d3u64;
        let mut src = base;
        let mut dst = base + span;
        (0..n)
            .map(|i| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                if share == 0 || i % share != 0 {
                    src = base + (state >> 33) % span;
                }
                dst += 1 + (state >> 60);
                LzCopy::new(src, dst.max(src + 1), 1)
            })
            .collect()
    }

    #[test]
    fn matches_a_comparison_sort_over_many_shapes() {
        for (n, span, base, share) in [
            (0usize, 1u64 << 20, 0u64, 0usize),
            (1, 1 << 20, 7, 0),
            (MIN_RADIX_LEN - 1, 1 << 20, 1 << 40, 0),
            (MIN_RADIX_LEN, 1 << 20, 0, 0),
            (10_000, 1 << 26, 1 << 32, 0),
            (10_000, 3, 5, 0), // nearly every source shared
            (10_000, 1 << 10, 1 << 20, 2),
            (100_000, 64 << 20, 12345, 8),
        ] {
            let copies = sample(n, span, base, share);
            let mut want = copies.clone();
            want.sort_unstable_by_key(|c| (c.src(), c.dst()));
            let mut got = copies;
            by_source(&mut got);
            assert_eq!(got, want, "n={n} span={span} base={base} share={share}");
        }
    }

    #[test]
    fn handles_an_already_sorted_and_a_reversed_list() {
        let n = 50_000u64;
        let ascending: Vec<LzCopy> = (0..n).map(|i| LzCopy::new(i, i + 1000, 4)).collect();
        let mut got = ascending.clone();
        by_source(&mut got);
        assert_eq!(got, ascending);

        let mut got: Vec<LzCopy> = ascending.iter().rev().copied().collect();
        by_source(&mut got);
        assert_eq!(got, ascending);
    }

    #[test]
    fn orders_copies_that_share_a_source_by_destination() {
        let mut got: Vec<LzCopy> =
            (0..200u64).rev().map(|i| LzCopy::new(7, 1000 + i * 4, 2)).collect();
        got.push(LzCopy::new(3, 5000, 1));
        by_source(&mut got);
        let mut want = got.clone();
        want.sort_unstable_by_key(|c| (c.src(), c.dst()));
        assert_eq!(got, want);
        assert_eq!(got[0].src(), 3);
        assert!(got[1..].windows(2).all(|p| p[0].dst() < p[1].dst()));
    }
}
