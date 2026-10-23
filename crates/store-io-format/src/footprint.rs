//! A region's footprint in container format 2 (spec §9 step 3): the rules
//! between a table entry, its region header and the container that decide
//! whether the region is usable, and the byte ranges it then occupies.
//!
//! A header lists at most [`max_extents`] extents (4,084 at 64 KiB blocks),
//! so the overlap rule sorts them instead of comparing every pair: a crafted
//! file would otherwise cost 4,084² comparisons per region. The caller lends
//! the scratch space, so nothing here allocates.

use crate::meta::MetaError;
use crate::meta2::{Entry2, EntryLimits, Extent, LOG2_RANGE, RegionHeaderView, max_extents};

/// Scratch [`check_region`] needs in a container of blocks of
/// `1 << log2_block` (the container's block, not a 512 B append region's):
/// every extent a header can list, and its two header blocks.
#[must_use]
pub const fn footprint_len(log2_block: u8) -> usize {
    max_extents(log2_block) + 2
}

/// What a usable region occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Footprint {
    /// How many byte ranges [`check_region`] left at the front of the
    /// scratch: the region's two header blocks and its committed extents,
    /// sorted by file offset, none overlapping another. This is what the
    /// region occupies, not its extent map: data order (how region offsets
    /// map to the file, spec §11) comes only from the header's extents.
    pub ranges: usize,
    /// An extent the header lists past the entry's count (a growth beyond
    /// capacity that crashed before its page write): not part of the
    /// footprint, and reported. It is free unless the region's page is
    /// Unconfirmed (the caller's rule).
    pub extra: Option<Extent>,
}

/// The rules of spec §9 step 3 for region `region_id`, whose `entry` passed
/// the per-entry checks under `lim` and whose header winner decoded as
/// `view`: the header agrees with the entry ([`Entry2::check_header`]); the
/// first `entry.extent_count` extents sum to `capacity`, lie inside the
/// entry's span and past the table area, and overlap neither each other nor
/// the header blocks. A header one extent ahead has its last extent returned
/// as [`Footprint::extra`]. On success `scratch[..ranges]` holds the
/// footprint; on error the scratch holds nothing meaningful, and a caller
/// that reports extra extents of unavailable regions reads them from
/// `view` (the last listed extent when the header lists one more).
///
/// `entry` must have passed [`crate::meta2::RawEntry::check`] and `view`
/// must have been decoded at `lim.log2_block` (debug builds assert what
/// they can); the overlap scan relies only on the sort, so a violation
/// cannot overflow here.
///
/// # Errors
///
/// What [`Entry2::check_header`] returns; [`MetaError::Field`] `"extent"`
/// for an extent outside the span or inside the table area, `"capacity"` for
/// extents that do not sum to the capacity; [`MetaError::Overlap`] for two
/// ranges of the footprint that overlap; [`MetaError::Length`] if `scratch`
/// is shorter than the footprint (see [`footprint_len`]).
pub fn check_region(
    region_id: u64,
    entry: &Entry2,
    view: &RegionHeaderView<'_>,
    lim: &EntryLimits,
    scratch: &mut [Extent],
) -> Result<Footprint, MetaError> {
    entry.check_header(region_id, &view.header)?;
    debug_assert!(
        LOG2_RANGE.contains(&lim.log2_block),
        "not a container block"
    );
    debug_assert!(
        view.header.log2_block == lim.log2_block || view.header.log2_block == 9,
        "header decoded at another block size"
    );
    let committed = usize::from(entry.extent_count);
    // Sized by `footprint_len` for this container, scratch is never short;
    // a short one is a caller's bug, not a property of the region.
    debug_assert!(scratch.len() >= committed + 2, "scratch too short");
    let Some(out) = scratch.get_mut(..committed + 2) else {
        return Err(MetaError::Length);
    };
    // The entry checks put both header blocks inside the span, past the
    // table area, block aligned and apart.
    let block = 1u64 << lim.log2_block;
    out[0] = (entry.header_a, block);
    out[1] = (entry.header_b, block);
    let mut sum = 0u64;
    let mut extra = None;
    // `check_header` allows exactly the entry's count or one more.
    for (i, (off, len)) in view.extents().enumerate() {
        if i == committed {
            extra = Some((off, len));
            break;
        }
        let end = off.checked_add(len).ok_or(MetaError::Field("extent"))?;
        if off < entry.span_start || end > entry.span_end || off < lim.table_end {
            return Err(MetaError::Field("extent"));
        }
        sum = sum.checked_add(len).ok_or(MetaError::Field("capacity"))?;
        out[i + 2] = (off, len);
    }
    if sum != entry.capacity {
        return Err(MetaError::Field("capacity"));
    }
    out.sort_unstable();
    // Sorted by offset, so only neighbours can overlap (no length is zero:
    // the decoder refuses one); touching is fine. The gap is computed from
    // the sort alone, so it cannot overflow.
    for pair in out.windows(2) {
        let (off, len) = pair[0];
        if pair[1].0 - off < len {
            return Err(MetaError::Overlap);
        }
    }
    Ok(Footprint {
        ranges: committed + 2,
        extra,
    })
}
