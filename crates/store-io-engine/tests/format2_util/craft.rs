//! An independent writer of format-2 regions for the open's tests: table
//! entries, region header slots and the space they name, encoded with the
//! format crate alone (never the engine's writer), so the open's region
//! checks are tested against media they did not produce.

use store_io_format::meta::{FillPattern, RegionKind, RegionName, RegionState};
use store_io_format::meta2::{
    Entry2, PAGE_HEADER_LEN, PageHeader, RegionHeader2, encode_page, encode_region_header,
};
use store_io_format::slot::{FORMAT_2, SlotFields, SlotIndex, encode};
use store_io_sim::SimPlatform;

use super::{LOG2_TABLE, block, max_meta_seq, media, page_at, put, u32_at, u64_at};

/// One region to write: its entry, and the header it gets.
#[derive(Debug, Clone)]
pub(crate) struct Region {
    /// The entry as the table stores it (`footprint_seq` 0: the page
    /// write's own `meta_seq`).
    pub(crate) entry: Entry2,
    /// The header's fixed part (`region_id` 0: the entry's id).
    pub(crate) header: RegionHeader2,
    /// The extents the header lists.
    pub(crate) extents: Vec<(u64, u64)>,
    /// A slot region's object.
    pub(crate) object: Vec<u8>,
    /// Whether a header slot is written at all.
    pub(crate) write_header: bool,
    /// The header slot's read-only-compatible feature bits.
    pub(crate) ro_compat: u32,
    /// Which slot of the header pair is written, and its generation.
    pub(crate) header_slot: SlotIndex,
    pub(crate) header_generation: u64,
    /// The entry's kind code as stored, when not the kind's own (a kind
    /// from a newer release).
    pub(crate) kind_raw: Option<u8>,
    /// Whether the container is extended to hold the span and extents.
    pub(crate) extend: bool,
}

/// A region of `kind` named `name` with sequence `seq` on its page, at
/// `offset`: two header blocks, then one extent of `capacity` bytes, half
/// of it holding data.
pub(crate) fn region(seq: u32, kind: RegionKind, name: &str, offset: u64, capacity: u64) -> Region {
    let b = 4096;
    let slot = kind == RegionKind::Slot;
    let (capacity, extents) = if slot {
        (0, Vec::new())
    } else {
        (capacity, vec![(offset + 2 * b, capacity)])
    };
    Region {
        entry: Entry2 {
            seq,
            kind,
            flags: 0,
            compat_flags: 0,
            name: RegionName::new(name).unwrap(),
            tag: 0,
            extent_count: extents.len() as u16,
            header_a: offset,
            header_b: offset + b,
            data_size: capacity / 2 / b * b,
            capacity,
            span_start: offset,
            span_end: offset + 2 * b + capacity,
            footprint_seq: 0,
        },
        header: RegionHeader2 {
            region_id: 0,
            kind,
            state: RegionState::Ready,
            fill: FillPattern::Zeros,
            flags: 0,
            compat_flags: 0,
            log2_block: 12,
            extent_count: extents.len() as u16,
            region_generation: 1,
            fill_key: [0; 16],
            object_len: if slot { 5 } else { 0 },
        },
        extents,
        object: if slot { b"hello".to_vec() } else { Vec::new() },
        write_header: true,
        ro_compat: 0,
        header_slot: SlotIndex::A,
        header_generation: 1,
        kind_raw: None,
        extend: true,
    }
}

/// Encodes a metadata slot of `1 << log2` bytes for `object` at `at`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn slot(
    pf: &SimPlatform,
    object: u64,
    at: u64,
    index: SlotIndex,
    log2: u8,
    generation: u64,
    prev_crc: u32,
    meta_seq: u64,
    payload: &[u8],
    ro_compat: u32,
) -> Vec<u8> {
    let m = media(pf);
    let mut out = vec![0u8; 1 << log2];
    encode(
        &mut out,
        &SlotFields {
            volume_uuid: m[32..48].try_into().unwrap(),
            object_id: object,
            generation,
            owner_generation: 1,
            slot_offset: at,
            slot_index: index,
            log2_slot_size: log2,
            prev_header_crc32c: prev_crc,
            format_major: FORMAT_2,
            meta_seq,
            compat_flags: 0,
        },
        payload,
    )
    .unwrap();
    if ro_compat != 0 {
        // `ro_compat_flags` (bytes 20..24 of the slot header), then the
        // header CRC again.
        out[20..24].copy_from_slice(&ro_compat.to_le_bytes());
        super::rehash(&mut out);
    }
    out
}

/// The page payload holding `regions` (`next_seq` one past the highest).
pub(crate) fn page_payload(p: u32, regions: &[Region], footprint_seq: u64) -> Vec<u8> {
    let raws: Vec<_> = regions
        .iter()
        .map(|r| {
            let mut e = r.entry;
            if e.footprint_seq == 0 {
                e.footprint_seq = footprint_seq;
            }
            let mut raw = e.to_raw();
            if let Some(k) = r.kind_raw {
                raw.kind = k;
            }
            raw
        })
        .collect();
    let next_seq = regions.iter().map(|r| r.entry.seq + 1).max().unwrap_or(0);
    let mut payload = vec![0u8; PAGE_HEADER_LEN + 96 * raws.len()];
    let h = PageHeader {
        page_index: p,
        next_seq,
        count: raws.len() as u16,
    };
    let n = encode_page(&mut payload, &h, &raws).unwrap();
    payload.truncate(n);
    payload
}

/// Writes `regions` into page `p`'s slot B, chained to slot A (the page
/// as create left it), at the next `meta_seq`; then each region's header
/// in its slot A, and one zero block at the end of its span and of every
/// extent its header lists, so the container holds them.
pub(crate) fn write_regions(pf: &SimPlatform, p: u32, regions: &[Region]) {
    let meta_seq = max_meta_seq(pf) + 1;
    let m = media(pf);
    let a = page_at(pf, p, 0) as usize;
    let page = slot(
        pf,
        (1 << 61) | u64::from(p),
        page_at(pf, p, 1),
        SlotIndex::B,
        LOG2_TABLE,
        u64_at(&m, a + 56) + 1,
        u32_at(&m, a + 8),
        meta_seq,
        &page_payload(p, regions, meta_seq),
        0,
    );
    put(pf, page_at(pf, p, 1), &page);
    let b = block(pf);
    for r in regions {
        let id = (u64::from(p) << 32) | u64::from(r.entry.seq);
        if r.extend {
            put(pf, r.entry.span_end - b, &vec![0u8; b as usize]);
            for &(off, len) in &r.extents {
                put(pf, off + len - b, &vec![0u8; b as usize]);
            }
        }
        if !r.write_header {
            continue;
        }
        let mut h = r.header;
        if h.region_id == 0 {
            h.region_id = id;
        }
        let mut payload = vec![0u8; b as usize];
        let n = encode_region_header(&mut payload, &h, &r.extents, &r.object).unwrap();
        let at = if r.header_slot == SlotIndex::A {
            r.entry.header_a
        } else {
            r.entry.header_b
        };
        let s = slot(
            pf,
            (1 << 62) | id,
            at,
            r.header_slot,
            12,
            r.header_generation,
            0,
            0,
            &payload[..n],
            r.ro_compat,
        );
        put(pf, at, &s);
    }
}

/// Tears both copies of page `p`: it is Lost, its regions unknown.
pub(crate) fn lose_page(pf: &SimPlatform, p: u32) {
    super::tear(pf, page_at(pf, p, 0));
    super::tear(pf, page_at(pf, p, 1));
}

/// Writes a newer copy of page `p` into its older slot, chained to the
/// newer one, torn: the page becomes stale (Unconfirmed), its winner the
/// newer copy.
pub(crate) fn tear_newer_copy_of(pf: &SimPlatform, p: u32) {
    let m = media(pf);
    let (a, b) = (page_at(pf, p, 0) as usize, page_at(pf, p, 1) as usize);
    // Slot A newer (a valid copy of a higher generation): the torn copy
    // goes to B.
    let a_newer = u64_at(&m, a + 56) > u64_at(&m, b + 56)
        && store_io_format::slot::validate(
            &m[a..a + (1 << LOG2_TABLE)],
            &store_io_format::slot::Expect {
                volume_uuid: m[32..48].try_into().unwrap(),
                object_id: (1 << 61) | u64::from(p),
                slot_offset: a as u64,
                slot_index: SlotIndex::A,
                log2_slot_size: LOG2_TABLE,
                format_major: FORMAT_2,
                numbered: true,
            },
        )
        .is_valid();
    let (from, to, index) = if a_newer {
        (a, b, SlotIndex::B)
    } else {
        (b, a, SlotIndex::A)
    };
    let mut s = slot(
        pf,
        (1 << 61) | u64::from(p),
        to as u64,
        index,
        LOG2_TABLE,
        u64_at(&m, from + 56) + 1,
        u32_at(&m, from + 8),
        max_meta_seq(pf) + 1,
        &super::empty_page(p),
        0,
    );
    s[128] ^= 0xff;
    put(pf, to as u64, &s);
}
