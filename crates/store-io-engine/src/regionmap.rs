//! Where a region's data lies in the container.
//!
//! Every engine site that turns a region offset into a file offset goes
//! through [`RegionMap::locate`], so a region whose data is split across
//! extents (format 2) changes this module, not the I/O paths. A region's
//! extents, in data order, are coalesced into runs (adjacent in the file,
//! spec §11); one device operation never crosses a run. A region in one run
//! (every format-1 region, and a format-2 region that never grew elsewhere)
//! pays one comparison more than its bounds check.

use store_io_format::footprint::runs;
use store_io_format::meta2::Extent;

/// A region's data placement: its runs, in data order, and its length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegionMap {
    /// The file offset of region offset 0 (the first run's).
    first: u64,
    /// The data's length (`data_size`): only `[0, len)` is addressable.
    len: u64,
    /// The runs' total length (the capacity): `len` at most.
    capacity: u64,
    /// The runs after the first, `(region offset, file offset)`, ascending:
    /// empty (no allocation) for a region in one run.
    more: Box<[(u64, u64)]>,
}

impl RegionMap {
    /// One extent: `len` bytes at file offset `base`, all of them data.
    pub(crate) fn single(base: u64, len: u64) -> Self {
        Self {
            first: base,
            len,
            capacity: len,
            more: Box::default(),
        }
    }

    /// `extents` (a region's committed extents in data order, checked by
    /// the open or the provisioning), of which the first `len` bytes are
    /// data.
    pub(crate) fn from_extents(extents: &[Extent], len: u64) -> Self {
        let mut all = runs(extents.iter().copied());
        // The decoder and the open's checks refuse data past the capacity;
        // one getting through would map past the last extent, so it stops
        // here in every build (once a region, at open).
        let Some(head) = all.next() else {
            assert_eq!(len, 0, "data without extents");
            return Self::single(0, 0);
        };
        let more: Box<[(u64, u64)]> = all.map(|r| (r.start, r.file)).collect();
        let capacity = extents.iter().map(|e| e.1).sum();
        assert!(len <= capacity, "data past the capacity");
        Self {
            first: head.file,
            len,
            capacity,
            more,
        }
    }

    /// The data's length (`data_size`).
    #[inline]
    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    /// How many runs the data lies in.
    pub(crate) fn runs(&self) -> usize {
        1 + self.more.len()
    }

    /// The runs' total length.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn capacity(&self) -> u64 {
        self.capacity
    }

    /// The run holding region offset `pos`: its region offset, file offset
    /// and end (region offset).
    fn run_of(&self, pos: u64) -> (u64, u64, u64) {
        let k = self.more.partition_point(|r| r.0 <= pos);
        let (start, file) = match k {
            0 => (0, self.first),
            _ => self.more[k - 1],
        };
        let end = self.more.get(k).map_or(self.capacity, |r| r.0);
        (start, file, end)
    }

    /// The file offset of `[pos, pos + len)`, which the caller has already
    /// bounded by the region's size, and which lies in one run (one device
    /// operation never crosses runs: see [`Self::run_end`]). A range outside
    /// the map or across a run is a bug that would write or read the wrong
    /// bytes: checked in release builds too.
    #[inline]
    pub(crate) fn locate(&self, pos: u64, len: u64) -> u64 {
        assert!(
            pos.checked_add(len).is_some_and(|end| end <= self.len),
            "region range {pos}+{len} outside the map ({} bytes)",
            self.len
        );
        if self.more.is_empty() {
            return self.first + pos;
        }
        let (start, file, end) = self.run_of(pos);
        assert!(
            pos + len <= end,
            "region range {pos}+{len} crosses the run ending at {end}"
        );
        file + (pos - start)
    }

    /// Where the run holding region offset `pos` ends (a region offset),
    /// at most the data's length: an operation from `pos` stays in one run
    /// up to here.
    #[inline]
    pub(crate) fn run_end(&self, pos: u64) -> u64 {
        if self.more.is_empty() {
            return self.len;
        }
        self.run_of(pos).2.min(self.len)
    }

    /// The runs, `(file offset, length)`, in data order, over the whole
    /// capacity.
    pub(crate) fn extents(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        let starts = core::iter::once((0, self.first)).chain(self.more.iter().copied());
        let ends = self
            .more
            .iter()
            .map(|r| r.0)
            .chain(core::iter::once(self.capacity));
        starts
            .zip(ends)
            .map(|((start, file), end)| (file, end - start))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: u64 = 4096;

    #[test]
    fn test_a_single_extent_maps_region_offsets_by_its_base() {
        let m = RegionMap::single(1 << 20, 64 << 10);
        assert_eq!(m.locate(0, 4096), 1 << 20);
        assert_eq!(m.locate(60 << 10, 4096), (1 << 20) + (60 << 10));
        assert_eq!(m.extents().collect::<Vec<_>>(), [(1 << 20, 64 << 10)]);
        assert_eq!(m.run_end(0), 64 << 10);
        assert_eq!(m.capacity(), 64 << 10);
    }

    /// A range outside the map is refused, in every build.
    #[test]
    #[should_panic(expected = "outside the map")]
    fn test_a_range_past_the_map_is_refused() {
        let _ = RegionMap::single(0, 8192).locate(4096, 8192);
    }

    #[test]
    #[should_panic(expected = "outside the map")]
    fn test_an_overflowing_range_is_refused() {
        let _ = RegionMap::single(0, 8192).locate(u64::MAX, 2);
    }

    /// Past the data's length but inside the capacity: refused.
    #[test]
    #[should_panic(expected = "outside the map")]
    fn test_a_range_past_the_data_inside_the_capacity_is_refused() {
        let _ =
            RegionMap::from_extents(&[(0, 4 * B), (100 * B, 4 * B)], 6 * B).locate(5 * B, 2 * B);
    }

    /// A range across two runs is refused, in every build.
    #[test]
    #[should_panic(expected = "crosses the run")]
    fn test_a_range_across_runs_is_refused() {
        let _ =
            RegionMap::from_extents(&[(0, 4 * B), (100 * B, 4 * B)], 8 * B).locate(3 * B, 2 * B);
    }

    /// Adjacent extents are one run: a range across them is one operation.
    #[test]
    fn test_adjacent_extents_are_one_run() {
        let m = RegionMap::from_extents(&[(10 * B, 2 * B), (12 * B, 2 * B)], 4 * B);
        assert_eq!(m.locate(B, 2 * B), 11 * B);
        assert_eq!(m.run_end(0), 4 * B);
        assert_eq!(m.extents().collect::<Vec<_>>(), [(10 * B, 4 * B)]);
    }

    /// Against the extents byte by byte (at block grain): `locate` of every
    /// block, `run_end` of every offset, `extents` over the capacity, and
    /// the data's length bounding `run_end`.
    #[test]
    fn test_locate_and_run_end_match_the_extents() {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move |m: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % m
        };
        for _ in 0..300 {
            let mut extents = Vec::new();
            let mut at = next(64) * B;
            for _ in 0..1 + next(6) {
                if next(2) == 0 {
                    at = next(512) * B;
                }
                let len = (1 + next(4)) * B;
                extents.push((at, len));
                at += len;
            }
            let capacity: u64 = extents.iter().map(|e| e.1).sum();
            // Anything from no data to all of it.
            let len = capacity - next(capacity / B + 1) * B;
            let m = RegionMap::from_extents(&extents, len);
            assert_eq!((m.len(), m.capacity()), (len, capacity));
            // Each block's file offset and its run's end, from the extents.
            let mut blocks = Vec::new();
            for &(file, n) in &extents {
                for k in (0..n).step_by(B as usize) {
                    blocks.push(file + k);
                }
            }
            for (i, &file) in blocks.iter().enumerate() {
                let pos = i as u64 * B;
                let mut end = i + 1;
                while end < blocks.len() && blocks[end] == blocks[end - 1] + B {
                    end += 1;
                }
                let end = end as u64 * B;
                if pos < len {
                    assert_eq!(m.locate(pos, B), file, "{extents:?} at {pos}");
                    assert_eq!(m.run_end(pos), end.min(len), "{extents:?} at {pos}");
                    assert_eq!(m.locate(pos, end.min(len) - pos), file);
                }
            }
            let mut from_runs = Vec::new();
            for (file, n) in m.extents() {
                for k in (0..n).step_by(B as usize) {
                    from_runs.push(file + k);
                }
            }
            assert_eq!(from_runs, blocks, "extents() over the capacity");
        }
    }
}
