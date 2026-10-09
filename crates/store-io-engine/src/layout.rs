//! Container layout arithmetic. Pure: no I/O.
//!
//! ```text
//! 0          volume slot A          block
//! B          volume slot B          block
//! 2B         region-table slot A    table bytes
//! 2B + T     region-table slot B    table bytes
//! 2B + 2T    regions: header slot A, header slot B, data area (each B-aligned)
//! ```

use store_io_core::evidence::Evidence;
use store_io_format::meta::{TableEntry, VOLUME_META_LEN};

/// Smallest block (slot) size: 4 KiB.
pub(crate) const MIN_LOG2_BLOCK: u8 = 12;
/// Largest block (slot) size: 64 KiB.
pub(crate) const MAX_LOG2_BLOCK: u8 = 16;

/// Object ids of the two volume-level pairs.
pub(crate) const VOLUME_OBJECT: u64 = 0;
/// Region table object id.
pub(crate) const TABLE_OBJECT: u64 = 1;

/// Object id of region `id`'s header pair.
pub(crate) const fn region_object(id: u32) -> u64 {
    (1u64 << 32) | id as u64
}

/// Chooses the block size from the device and filesystem evidence: the
/// largest of 4 KiB, the logical and physical block, the preferred write
/// size and the direct-I/O alignments, rounded up to a power of two and
/// capped at 64 KiB. An explicit request is honoured if it is at least that.
pub(crate) fn choose_log2_block(ev: &Evidence, requested: Option<u32>) -> u8 {
    let d = &ev.device;
    let f = &ev.fs;
    let need = [
        4096,
        d.logical_block,
        d.physical_block,
        d.optimal_write,
        f.dio_mem_align,
        f.dio_offset_align,
    ]
    .into_iter()
    .max()
    .unwrap_or(4096)
    .max(requested.unwrap_or(0));
    let log2 = need.next_power_of_two().trailing_zeros() as u8;
    log2.clamp(MIN_LOG2_BLOCK, MAX_LOG2_BLOCK)
}

/// Geometry of one container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Layout {
    pub(crate) log2_block: u8,
    pub(crate) log2_table: u8,
}

impl Layout {
    pub(crate) fn block(&self) -> u64 {
        1u64 << self.log2_block
    }

    pub(crate) fn table(&self) -> u64 {
        1u64 << self.log2_table
    }

    /// Volume slots A and B.
    pub(crate) fn volume_slots(&self) -> (u64, u64) {
        (0, self.block())
    }

    /// Region-table slots A and B.
    pub(crate) fn table_slots(&self) -> (u64, u64) {
        let a = 2 * self.block();
        (a, a + self.table())
    }

    /// First byte available to regions.
    pub(crate) fn data_start(&self) -> u64 {
        2 * self.block() + 2 * self.table()
    }

    /// Bytes a region of `data` bytes occupies (two header blocks + data).
    pub(crate) fn extent(&self, data: u64) -> Option<u64> {
        data.checked_add(2 * self.block())
    }

    /// Table entries that fit in one table slot.
    pub(crate) fn table_capacity(&self) -> usize {
        let room = self.table() as usize
            - store_io_format::slot::HEADER_LEN
            - store_io_format::meta::TABLE_HEADER_LEN;
        room / store_io_format::meta::ENTRY_LEN
    }

    /// Placement for an extent of `len` bytes: a released extent of exactly
    /// that length is reused (its table entry is replaced by the new one, so
    /// the table never holds overlapping entries); otherwise the extent goes
    /// after every existing one. Returns the offset, the index of the reused
    /// released entry if any, and whether the container must grow. Released
    /// extents of other sizes stay reported as reclaimable space.
    pub(crate) fn place(
        &self,
        entries: &[TableEntry],
        container_len: u64,
        len: u64,
    ) -> Option<(u64, Option<usize>, bool)> {
        let block = self.block();
        for (i, e) in entries.iter().enumerate() {
            if e.state == store_io_format::meta::RegionState::Released
                && e.extent_len(block)? == len
                && !overlaps_ready(entries, e.offset, len, block)
            {
                return Some((e.offset, Some(i), false));
            }
        }
        let start = self.tail(entries);
        let grow = start.checked_add(len)? > container_len;
        Some((start, None, grow))
    }

    /// The end of the last extent in the table, or the data start of an
    /// empty container: where the free tail begins.
    pub(crate) fn tail(&self, entries: &[TableEntry]) -> u64 {
        let block = self.block();
        entries
            .iter()
            .filter_map(|e| e.offset.checked_add(e.extent_len(block)?))
            .fold(self.data_start(), u64::max)
    }
}

fn overlaps_ready(entries: &[TableEntry], start: u64, len: u64, block: u64) -> bool {
    let end = start.saturating_add(len);
    entries
        .iter()
        .filter(|e| e.state == store_io_format::meta::RegionState::Ready)
        .any(|e| {
            let e_end = e
                .offset
                .saturating_add(e.extent_len(block).unwrap_or(u64::MAX));
            start < e_end && e.offset < end
        })
}

/// Compile-time check that the volume record fits in the smallest slot.
const _: () = assert!(VOLUME_META_LEN + store_io_format::slot::HEADER_LEN <= 1 << MIN_LOG2_BLOCK);

#[cfg(test)]
mod tests {
    use super::*;
    use store_io_core::evidence::PlatformKind;
    use store_io_format::meta::{FillPattern, RegionKind, RegionName, RegionState};

    #[test]
    fn test_block_choice_follows_the_largest_constraint() {
        let mut ev = Evidence::unknown(PlatformKind::Sim);
        assert_eq!(choose_log2_block(&ev, None), 12);
        ev.device.physical_block = 16384;
        assert_eq!(choose_log2_block(&ev, None), 14);
        ev.fs.dio_offset_align = 1 << 20;
        assert_eq!(choose_log2_block(&ev, None), 16, "capped at 64 KiB");
        let ev = Evidence::unknown(PlatformKind::Sim);
        assert_eq!(choose_log2_block(&ev, Some(8192)), 13);
    }

    #[test]
    fn test_layout_offsets_and_capacity() {
        let l = Layout {
            log2_block: 12,
            log2_table: 14,
        };
        assert_eq!(l.volume_slots(), (0, 4096));
        assert_eq!(l.table_slots(), (8192, 8192 + 16384));
        assert_eq!(l.data_start(), 8192 + 32768);
        assert_eq!(l.table_capacity(), 253);
    }

    #[test]
    fn test_placement_appends_or_reuses_released_space() {
        let l = Layout {
            log2_block: 12,
            log2_table: 14,
        };
        let entry = |id, offset, data, state| TableEntry {
            region_id: id,
            kind: RegionKind::Append,
            state,
            name: RegionName::new("r").unwrap_or_else(|_| panic!("name")),
            offset,
            data_size: data,
            fill: FillPattern::Zeros,
            tag: 0,
        };
        let start = l.data_start();
        assert_eq!(l.place(&[], start, 3 * 4096), Some((start, None, true)));
        let used = vec![entry(0, start, 4096, RegionState::Ready)];
        assert_eq!(
            l.place(&used, start + 3 * 4096, 4096 * 3),
            Some((start + 3 * 4096, None, true))
        );
        let released = vec![entry(0, start, 4096, RegionState::Released)];
        assert_eq!(
            l.place(&released, start + 3 * 4096, 3 * 4096),
            Some((start, Some(0), false))
        );
        // A released extent of another size is not reused.
        assert_eq!(
            l.place(&released, start + 3 * 4096, 4 * 4096),
            Some((start + 3 * 4096, None, true))
        );
    }
}
