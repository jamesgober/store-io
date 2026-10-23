//! Settling the regions a format-2 open found (spec §9 steps 4-6): which
//! usable regions stay usable when footprints overlap or names repeat, and
//! the free space that is left.
//!
//! Every region that passed the step-3 checks is compared with every other
//! at once (plan §3 a): where two overlap or share a name, the one with the
//! lower `footprint_seq` becomes unavailable, both on a tie, whatever
//! happens to the other. An unavailable region (one that failed step 3)
//! never demotes anyone: its span stays allocated, and a usable region its
//! own extents overlap with a higher `footprint_seq` is **possibly
//! superseded** (read-only).
//!
//! The overlap queries cost O(n log n) in the number of ranges: for each
//! range, the highest `footprint_seq` among the others that overlap it is the
//! higher of those that started before it and are still open (a sweep with
//! a max-heap, expired lazily) and those that start inside it (a range
//! maximum over the ranges sorted by start). A crafted container cannot make
//! the open quadratic.

use std::collections::{BinaryHeap, HashMap};

/// A byte range `(offset, length)` in the container.
pub(crate) type Extent = (u64, u64);

/// A region that passed the step-3 checks.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Usable<'a> {
    /// Its name.
    pub name: &'a str,
    /// Its entry's `footprint_seq`.
    pub seq: u64,
    /// Its footprint: header blocks and committed extents, none overlapping
    /// another (`check_region`).
    pub ranges: &'a [Extent],
}

/// A region that failed the step-3 checks.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Unavailable<'a> {
    /// Its entry's `footprint_seq`.
    pub seq: u64,
    /// Its own extents (from its header, when that still decodes), else its
    /// span: what it may have written over. (What it reserves, its span, is
    /// the caller's: see [`shares_reserved`].)
    pub own: &'a [Extent],
}

/// What settling decided for one usable region.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Settled {
    /// Another usable region with an equal or higher `footprint_seq`
    /// overlaps it: now unavailable.
    pub overlapped: bool,
    /// Another usable region with an equal or higher `footprint_seq` has its
    /// name: now unavailable.
    pub renamed: bool,
    /// An unavailable region with a higher `footprint_seq` overlaps it with
    /// its own extents: read-only.
    pub superseded: bool,
}

#[cfg(test)]
impl Settled {
    /// Still usable after settling.
    pub(crate) fn usable(self) -> bool {
        !self.overlapped && !self.renamed
    }
}

/// One range of one region.
#[derive(Debug, Clone, Copy)]
struct Iv {
    start: u64,
    end: u64,
    seq: u64,
    owner: usize,
}

fn intervals<'a>(sets: impl Iterator<Item = (usize, u64, &'a [Extent])>) -> Vec<Iv> {
    let mut v: Vec<Iv> = sets
        .flat_map(|(owner, seq, ranges)| {
            ranges.iter().map(move |&(off, len)| Iv {
                start: off,
                end: off.saturating_add(len),
                seq,
                owner,
            })
        })
        .filter(|iv| iv.end > iv.start)
        .collect();
    v.sort_unstable_by_key(|iv| (iv.start, iv.end));
    v
}

/// A segment tree of maxima over a fixed array. `None` is below every
/// `Some`, so a `footprint_seq` of 0 and of `u64::MAX` both count.
struct MaxTree {
    n: usize,
    t: Vec<Option<u64>>,
}

impl MaxTree {
    fn new(values: impl ExactSizeIterator<Item = Option<u64>>) -> Self {
        let n = values.len().max(1);
        let mut t = vec![None; 2 * n];
        for (i, v) in values.enumerate() {
            t[n + i] = v;
        }
        for i in (1..n).rev() {
            t[i] = t[2 * i].max(t[2 * i + 1]);
        }
        Self { n, t }
    }

    /// The maximum over `[lo, hi)` (`None` when empty).
    fn max(&self, lo: usize, hi: usize) -> Option<u64> {
        let (mut l, mut r, mut m) = (lo + self.n, hi.min(self.n) + self.n, None);
        while l < r {
            if l & 1 == 1 {
                m = m.max(self.t[l]);
                l += 1;
            }
            if r & 1 == 1 {
                r -= 1;
                m = m.max(self.t[r]);
            }
            l /= 2;
            r /= 2;
        }
        m
    }
}

/// For every range in `query`, the highest `seq` among the ranges of `set`
/// that overlap it (`None` if none). With `same`, `query` and `set` are the
/// same sorted slice and each range skips itself (a region's own ranges
/// never overlap one another).
fn max_overlapping(query: &[Iv], set: &[Iv], same: bool) -> Vec<Option<u64>> {
    let tree = MaxTree::new(set.iter().map(|iv| Some(iv.seq)));
    let mut out = vec![None; query.len()];
    // Ranges of `set` that started strictly before the query's start and
    // end after it, by a sweep: queries in start order, `set` added as its
    // ranges start, expired from the top of the heap once ended.
    let mut order: Vec<usize> = (0..query.len()).collect();
    order.sort_unstable_by_key(|&i| query[i].start);
    let mut heap: BinaryHeap<(u64, u64)> = BinaryHeap::new();
    let mut next = 0;
    for &qi in &order {
        let q = query[qi];
        while next < set.len() && set[next].start < q.start {
            heap.push((set[next].seq, set[next].end));
            next += 1;
        }
        // The top, once open, is the highest of every open range (any
        // higher one below it would be the top).
        while heap.peek().is_some_and(|&(_, end)| end <= q.start) {
            let _expired = heap.pop();
        }
        let stab = heap.peek().map(|&(s, _)| s);
        // Ranges of `set` that start inside the query: a contiguous run of
        // the start-sorted `set`.
        let lo = set.partition_point(|iv| iv.start < q.start);
        let hi = set.partition_point(|iv| iv.start < q.end);
        let inside = if same {
            tree.max(lo, qi).max(tree.max(qi + 1, hi))
        } else {
            tree.max(lo, hi)
        };
        out[qi] = stab.max(inside);
    }
    out
}

/// Settles `usable` against each other and against `unavailable` (spec §9
/// step 4). The result is in `usable`'s order.
pub(crate) fn settle(usable: &[Usable<'_>], unavailable: &[Unavailable<'_>]) -> Vec<Settled> {
    let mut out = vec![Settled::default(); usable.len()];
    // Overlaps among usable regions.
    let ivs = intervals(usable.iter().enumerate().map(|(i, u)| (i, u.seq, u.ranges)));
    for (iv, m) in ivs.iter().zip(max_overlapping(&ivs, &ivs, true)) {
        if m >= Some(iv.seq) {
            out[iv.owner].overlapped = true;
        }
    }
    // Names: the highest `footprint_seq` keeps a name, unless two share it.
    let mut names: HashMap<&str, (u64, u32)> = HashMap::with_capacity(usable.len());
    for u in usable {
        let e = names.entry(u.name).or_insert((u.seq, 0));
        match u.seq.cmp(&e.0) {
            core::cmp::Ordering::Greater => *e = (u.seq, 1),
            core::cmp::Ordering::Equal => e.1 += 1,
            core::cmp::Ordering::Less => {}
        }
    }
    for (u, s) in usable.iter().zip(out.iter_mut()) {
        let (top, at_top) = names[u.name];
        s.renamed = u.seq < top || at_top > 1;
    }
    // Unavailable regions: their own extents against usable footprints.
    let own = intervals(
        unavailable
            .iter()
            .enumerate()
            .map(|(i, x)| (i, x.seq, x.own)),
    );
    for (iv, m) in ivs.iter().zip(max_overlapping(&ivs, &own, false)) {
        if m > Some(iv.seq) {
            out[iv.owner].superseded = true;
        }
    }
    out
}

/// Which of `usable` overlap any of `spans`: the spans every unavailable
/// region keeps reserved (those that failed step 3 and those demoted by
/// [`settle`]), whose overlaps are reported (spec §9 step 4).
pub(crate) fn shares_reserved(usable: &[Usable<'_>], spans: &[Extent]) -> Vec<bool> {
    let ivs = intervals(usable.iter().enumerate().map(|(i, u)| (i, u.seq, u.ranges)));
    let set = intervals(core::iter::once((0, 0, spans)));
    let mut out = vec![false; usable.len()];
    for (iv, m) in ivs.iter().zip(max_overlapping(&ivs, &set, false)) {
        if m.is_some() {
            out[iv.owner] = true;
        }
    }
    out
}

/// The first offset from `at` whose `offset + skew` is a multiple of
/// `align` (a power of two); `None` past `u64::MAX`.
pub(crate) fn aligned(at: u64, align: u64, skew: u64) -> Option<u64> {
    debug_assert!(align.is_power_of_two());
    Some((at.checked_add(skew)?.checked_add(align - 1)? & !(align - 1)) - skew)
}

/// Free space as the complement of everything allocated (spec §9 step 6,
/// §10): never built by adding a range back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FreeMap {
    /// Free ranges `[start, end)` past the table area and before the
    /// container's end, sorted, disjoint, none empty.
    free: Vec<(u64, u64)>,
    /// Where the tail starts: the end of the last allocated byte (at least
    /// the table's end).
    tail: u64,
}

impl FreeMap {
    /// The free space of a container of `len` bytes whose table area ends at
    /// `table_end`, given every allocated range (footprints, unavailable
    /// spans, extra extents counted allocated). Ranges may overlap and be in
    /// any order; `allocated` is sorted in place.
    pub(crate) fn complement(table_end: u64, len: u64, allocated: &mut [Extent]) -> Self {
        allocated.sort_unstable();
        let mut free = Vec::new();
        let mut at = table_end;
        for &(off, n) in allocated.iter().filter(|r| r.1 > 0) {
            let end = off.saturating_add(n);
            // A range inside one already seen moves nothing.
            if off > at {
                let gap_end = off.min(len);
                if gap_end > at {
                    free.push((at, gap_end));
                }
            }
            at = at.max(end);
        }
        let tail = at;
        if len > at {
            free.push((at, len));
        }
        Self { free, tail }
    }

    /// The first free range, in offset order, holding `len` bytes from an
    /// offset `at` whose `at + skew` is a multiple of `align` (a power of
    /// two; `skew` lets a region align its data after its header blocks):
    /// that offset.
    pub(crate) fn first_fit(&self, len: u64, align: u64, skew: u64) -> Option<u64> {
        debug_assert!(align.is_power_of_two());
        self.free.iter().find_map(|&(start, end)| {
            let at = (start.checked_add(skew)?.checked_add(align - 1)? & !(align - 1)) - skew;
            (at.checked_add(len)? <= end).then_some(at)
        })
    }

    /// Where a region placed past everything allocated starts (the
    /// container grows to hold it): the end of the last allocated byte,
    /// moved up so `tail + skew` is a multiple of `align` (a power of two;
    /// a clamped unavailable span may end anywhere). `None` past
    /// `u64::MAX`.
    pub(crate) fn tail(&self, align: u64, skew: u64) -> Option<u64> {
        aligned(self.tail, align, skew)
    }

    /// Whether `[at, at + len)` is free inside the container.
    pub(crate) fn fits_at(&self, at: u64, len: u64) -> bool {
        at.checked_add(len)
            .is_some_and(|end| self.free.iter().any(|&(s, e)| s <= at && end <= e))
    }

    /// The end of the last allocated byte (at least the table's end).
    pub(crate) fn end(&self) -> u64 {
        self.tail
    }

    /// Free bytes inside the container.
    pub(crate) fn free_bytes(&self) -> u64 {
        self.free.iter().map(|&(s, e)| e - s).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rules of spec §9 step 4 compared pair by pair: what `settle`
    /// must agree with.
    fn oracle(usable: &[Usable<'_>], unavailable: &[Unavailable<'_>]) -> Vec<Settled> {
        let hit = |a: &[Extent], b: &[Extent]| {
            a.iter().any(|&(ao, al)| {
                b.iter()
                    .any(|&(bo, bl)| ao < bo.saturating_add(bl) && bo < ao.saturating_add(al))
            })
        };
        usable
            .iter()
            .enumerate()
            .map(|(i, u)| {
                let others = usable.iter().enumerate().filter(|&(j, _)| j != i);
                Settled {
                    overlapped: others
                        .clone()
                        .any(|(_, o)| o.seq >= u.seq && hit(u.ranges, o.ranges)),
                    renamed: others
                        .clone()
                        .any(|(_, o)| o.seq >= u.seq && o.name == u.name),
                    superseded: unavailable
                        .iter()
                        .any(|x| x.seq > u.seq && hit(u.ranges, x.own)),
                }
            })
            .collect()
    }

    fn u<'a>(name: &'a str, seq: u64, ranges: &'a [Extent]) -> Usable<'a> {
        Usable { name, seq, ranges }
    }

    #[test]
    fn test_the_newer_of_two_overlapping_regions_stays() {
        let a = [(0, 10)];
        let b = [(5, 10)];
        let s = settle(&[u("a", 1, &a), u("b", 2, &b)], &[]);
        assert!(s[0].overlapped && !s[1].overlapped);
        assert!(!s[0].usable() && s[1].usable());
    }

    /// Either one newer: a range ending where the next starts is closed by
    /// then.
    #[test]
    fn test_touching_regions_do_not_overlap() {
        let a = [(0, 10)];
        let b = [(10, 10)];
        for (sa, sb) in [(1, 2), (2, 1)] {
            let s = settle(&[u("a", sa, &a), u("b", sb, &b)], &[]);
            assert!(s.iter().all(|x| x.usable()), "{sa} {sb}");
        }
    }

    /// The largest `footprint_seq` still counts, and still ties.
    #[test]
    fn test_the_largest_sequence_counts() {
        let r = [(8192, 4096)];
        let s = settle(&[u("a", u64::MAX, &r), u("b", 5, &r)], &[]);
        assert!(!s[0].overlapped && s[1].overlapped);
        let s = settle(&[u("a", u64::MAX, &r), u("b", u64::MAX, &r)], &[]);
        assert!(s[0].overlapped && s[1].overlapped);
        let newest = Unavailable {
            seq: u64::MAX,
            own: &r,
        };
        assert!(settle(&[u("a", 5, &r)], &[newest])[0].superseded);
        let s = settle(&[u("a", u64::MAX, &r)], &[newest]);
        assert!(!s[0].superseded, "equal is not newer");
        let s = settle(&[u("a", 0, &[(0, 1)]), u("b", 0, &[(0, 1)])], &[]);
        assert!(s[0].overlapped && s[1].overlapped, "zero ties too");
    }

    #[test]
    fn test_equal_sequences_demote_both() {
        let a = [(0, 10)];
        let b = [(9, 10)];
        let s = settle(&[u("a", 7, &a), u("b", 7, &b)], &[]);
        assert!(s[0].overlapped && s[1].overlapped);
        let s = settle(&[u("same", 7, &a), u("same", 7, &[(100, 1)])], &[]);
        assert!(s[0].renamed && s[1].renamed);
    }

    /// A > B > C in a chain, A not overlapping C: every step-3-usable
    /// region is compared at once, so C loses to B though B loses to A.
    #[test]
    fn test_a_chain_demotes_all_but_the_newest() {
        let a = [(0, 10)];
        let b = [(8, 10)];
        let c = [(16, 10)];
        let s = settle(&[u("a", 3, &a), u("b", 2, &b), u("c", 1, &c)], &[]);
        assert_eq!(
            s.iter().map(|x| x.usable()).collect::<Vec<_>>(),
            [true, false, false]
        );
    }

    /// A range nested inside another, starting after it: found from both
    /// sides.
    #[test]
    fn test_a_nested_range_is_found_from_both_sides() {
        let outer = [(0, 100)];
        let inner = [(40, 10)];
        let s = settle(&[u("o", 1, &outer), u("i", 2, &inner)], &[]);
        assert!(s[0].overlapped && !s[1].overlapped);
        let s = settle(&[u("o", 2, &outer), u("i", 1, &inner)], &[]);
        assert!(!s[0].overlapped && s[1].overlapped);
    }

    #[test]
    fn test_names_go_to_the_newest() {
        let s = settle(
            &[
                u("n", 1, &[(0, 1)]),
                u("n", 3, &[(10, 1)]),
                u("m", 2, &[(20, 1)]),
            ],
            &[],
        );
        assert_eq!(
            s.iter().map(|x| x.renamed).collect::<Vec<_>>(),
            [true, false, false]
        );
    }

    /// An unavailable span never demotes: it is reported. Its own extents,
    /// when newer, make the usable region possibly superseded; older, not.
    #[test]
    fn test_an_unavailable_region_never_demotes() {
        let a = [(0, 10)];
        let newer = Unavailable {
            seq: 9,
            own: &[(5, 1)],
        };
        let s = settle(&[u("a", 1, &a)], &[newer]);
        assert!(s[0].usable() && s[0].superseded);
        let older = Unavailable { seq: 0, ..newer };
        let s = settle(&[u("a", 1, &a)], &[older]);
        assert!(s[0].usable() && !s[0].superseded);
        // Equal is not newer.
        let equal = Unavailable { seq: 1, ..newer };
        assert!(!settle(&[u("a", 1, &a)], &[equal])[0].superseded);
        // The span overlaps, the own extents do not.
        let apart = Unavailable {
            own: &[(50, 1)],
            ..newer
        };
        let s = settle(&[u("a", 1, &a)], &[apart]);
        assert!(!s[0].superseded);
    }

    /// Reserved spans, whatever their region's sequence: an overlap is
    /// reported, touching is not.
    #[test]
    fn test_shares_reserved() {
        let a = [(0, 10)];
        let b = [(20, 10)];
        let usable = [u("a", 9, &a), u("b", 9, &b)];
        assert_eq!(shares_reserved(&usable, &[(5, 1)]), [true, false]);
        assert_eq!(shares_reserved(&usable, &[(10, 10)]), [false, false]);
        assert_eq!(shares_reserved(&usable, &[(0, 100)]), [true, true]);
        assert_eq!(shares_reserved(&usable, &[]), [false, false]);
    }

    /// A small deterministic generator (xorshift64*).
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// Random containers, against the pair-by-pair oracle. A region's own
    /// ranges never overlap (as `check_region` guarantees).
    #[test]
    fn test_settle_agrees_with_the_pairwise_rules() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let names = ["a", "b", "c", "d", "e", "f"];
        for round in 0..2000 {
            let n = 1 + rng.below(12) as usize;
            let space = 20 + rng.below(200);
            let mut ranges: Vec<Vec<Extent>> = Vec::new();
            for _ in 0..n {
                let k = 1 + rng.below(4);
                let mut own: Vec<Extent> = Vec::new();
                for _ in 0..k {
                    let off = rng.below(space);
                    let len = 1 + rng.below(12);
                    if own.iter().all(|&(o, l)| off + len <= o || o + l <= off) {
                        own.push((off, len));
                    }
                }
                ranges.push(own);
            }
            let usable: Vec<Usable<'_>> = ranges
                .iter()
                .map(|r| Usable {
                    name: names[rng.below(names.len() as u64) as usize],
                    seq: rng.below(6),
                    ranges: r,
                })
                .collect();
            // One to three extents each, which may overlap one another.
            let gone: Vec<Vec<Extent>> = (0..rng.below(4))
                .map(|_| {
                    (0..1 + rng.below(3))
                        .map(|_| (rng.below(space), 1 + rng.below(20)))
                        .collect()
                })
                .collect();
            let unavailable: Vec<Unavailable<'_>> = gone
                .iter()
                .map(|own| Unavailable {
                    seq: rng.below(8),
                    own,
                })
                .collect();
            assert_eq!(
                settle(&usable, &unavailable),
                oracle(&usable, &unavailable),
                "round {round}"
            );
        }
    }

    /// Many identical ranges (the worst case for a pairwise check) settle
    /// in one pass: all tie, all demoted.
    #[test]
    fn test_many_identical_footprints() {
        let r = [(0, 4096)];
        let usable: Vec<Usable<'_>> = (0..100_000).map(|i| u(["x", "y"][i % 2], 5, &r)).collect();
        let s = settle(&usable, &[]);
        assert!(s.iter().all(|x| x.overlapped && x.renamed));
    }

    #[test]
    fn test_free_space_is_the_complement() {
        let mut alloc = [(150, 50), (100, 30), (120, 40), (400, 10)];
        let f = FreeMap::complement(100, 1000, &mut alloc);
        assert_eq!(f.free, [(200, 400), (410, 1000)]);
        assert_eq!(f.tail(1, 0), Some(410));
        assert_eq!(f.tail(4096, 0), Some(4096));
        // Data two blocks after the start aligned to 8192.
        assert_eq!(f.tail(8192, 200), Some(8192 - 200));
        assert_eq!(f.end(), 410);
        assert_eq!(f.free_bytes(), 200 + 590);
        // Allocation before the table's end is inside the table area.
        let f = FreeMap::complement(100, 300, &mut [(0, 120)]);
        assert_eq!(f.free, [(120, 300)]);
        // A range inside another leaves the outer one's end in force.
        let f = FreeMap::complement(100, 300, &mut [(100, 100), (120, 10)]);
        assert_eq!(f.free, [(200, 300)]);
        // Nothing allocated: everything past the table is free.
        let f = FreeMap::complement(100, 300, &mut []);
        assert_eq!(
            (f.free.as_slice(), f.tail(1, 0)),
            ([(100, 300)].as_slice(), Some(100))
        );
        // A zero-length range splits nothing and moves no tail.
        let f = FreeMap::complement(1, 150, &mut [(31, 0), (5000, 0)]);
        assert_eq!(
            (f.free.as_slice(), f.tail(1, 0)),
            ([(1, 150)].as_slice(), Some(1))
        );
        // A gap that would start at the container's end is not a free range.
        let f = FreeMap::complement(100, 100, &mut [(150, 10)]);
        assert!(f.free.is_empty());
        // A tail that cannot be aligned within a u64.
        let f = FreeMap::complement(100, 100, &mut [(u64::MAX - 10, 5)]);
        assert_eq!(f.tail(4096, 0), None);
        // An allocation past the container's end (an entry past the file
        // end, kept by the growth guard) leaves nothing free there.
        let f = FreeMap::complement(100, 300, &mut [(250, 100)]);
        assert_eq!(
            (f.free.as_slice(), f.tail(1, 0)),
            ([(100, 250)].as_slice(), Some(350))
        );
    }

    #[test]
    fn test_first_fit_aligns_inside_a_gap() {
        let f = FreeMap::complement(4096, 1 << 20, &mut [(4096, 100), (8192, 4096)]);
        // The first gap, [4196, 8192), has no aligned 4 KiB block in it; the
        // next starts at 12288.
        assert_eq!(f.first_fit(4096, 4096, 0), Some(12288));
        assert_eq!(f.first_fit(3000, 1, 0), Some(4196));
        // Small enough for the first gap unaligned, not aligned.
        assert_eq!(f.first_fit(1000, 4096, 0), Some(12288));
        assert_eq!(f.first_fit(1 << 21, 4096, 0), None);
        // Skewed: `at + 100` aligned to 4096 inside [4196, 8192).
        assert_eq!(f.first_fit(90, 4096, 100), Some(8192 - 100));
        assert_eq!(f.first_fit(4000, 4096, 100), Some(12288 - 100 + 4096));
    }

    /// A range is free when one free range holds all of it: touching an
    /// allocation is fine, crossing one or the container's end is not.
    #[test]
    fn test_fits_at_needs_one_free_range_holding_it() {
        let f = FreeMap::complement(100, 1000, &mut [(200, 100)]);
        assert_eq!(f.free, [(100, 200), (300, 1000)]);
        assert!(f.fits_at(100, 100) && f.fits_at(300, 700) && f.fits_at(150, 1));
        assert!(!f.fits_at(150, 100), "crosses an allocation");
        assert!(!f.fits_at(250, 10), "inside one");
        assert!(!f.fits_at(300, 701), "past the container");
        assert!(!f.fits_at(50, 10), "inside the table area");
        assert!(!f.fits_at(u64::MAX, 2), "past u64::MAX");
    }
}
