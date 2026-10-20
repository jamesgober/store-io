//! Where a region's data lies in the container.
//!
//! Every engine site that turns a region offset into a file offset goes
//! through [`RegionMap::locate`], so a region whose data is split across
//! extents (format 2's growth) changes this module, not the I/O paths. Today
//! every region has exactly one extent.

/// A region's data placement: its extents, in data order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RegionMap {
    /// The file offset of region offset 0.
    base: u64,
    /// The data's length.
    len: u64,
}

impl RegionMap {
    /// One extent: `len` bytes at file offset `base`.
    pub(crate) const fn single(base: u64, len: u64) -> Self {
        Self { base, len }
    }

    /// The file offset of `[pos, pos + len)`, which the caller has already
    /// bounded by the region's size, and which lies in one extent (one
    /// device operation never crosses extents). A range outside the map is
    /// a bug that would write or read the wrong bytes: checked in release
    /// builds too (one comparison per operation).
    #[inline]
    pub(crate) fn locate(&self, pos: u64, len: u64) -> u64 {
        assert!(
            pos.checked_add(len).is_some_and(|end| end <= self.len),
            "region range {pos}+{len} outside the map ({} bytes)",
            self.len
        );
        self.base + pos
    }

    /// The extents, `(file offset, length)`, in data order.
    pub(crate) fn extents(&self) -> impl Iterator<Item = (u64, u64)> {
        core::iter::once((self.base, self.len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_a_single_extent_maps_region_offsets_by_its_base() {
        let m = RegionMap::single(1 << 20, 64 << 10);
        assert_eq!(m.locate(0, 4096), 1 << 20);
        assert_eq!(m.locate(60 << 10, 4096), (1 << 20) + (60 << 10));
        assert_eq!(m.extents().collect::<Vec<_>>(), [(1 << 20, 64 << 10)]);
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
}
