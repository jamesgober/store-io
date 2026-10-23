//! An independent audit of a format-2 container's raw bytes: every table
//! page and region header decoded with the format crate alone (never the
//! engine's reader), and every rule a writer must keep checked. What the
//! engine wrote is judged by what is on the media, not by what the engine
//! says it wrote.

use store_io_format::footprint::{check_region, footprint_len};
use store_io_format::meta::RegionKind;
use store_io_format::meta2::{
    EntryLimits, VolumeMeta2, decode_page, decode_region_header, entries_per_page,
};
use store_io_format::slot::{Expect, FORMAT_2, SlotIndex, SlotStatus, validate};
use store_io_sim::SimPlatform;

use super::{LOG2_TABLE, PAGES, block, media, page_at};

/// A region the audit found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) name: String,
    pub(crate) kind: RegionKind,
    pub(crate) id: u64,
    pub(crate) data_offset: u64,
    pub(crate) data_size: u64,
    pub(crate) capacity: u64,
    /// The header pair's slots.
    pub(crate) header_at: [u64; 2],
    /// The committed extents, in data order.
    pub(crate) extents: Vec<(u64, u64)>,
    /// An extent the header lists past the entry's (only from
    /// [`audit_allowing_extra`]).
    pub(crate) extra: Option<(u64, u64)>,
    /// The entry's `footprint_seq`, and the `meta_seq` of its page's
    /// winning copy.
    pub(crate) footprint_seq: u64,
    pub(crate) page_meta_seq: u64,
}

/// The newer valid copy of a pair, as `(payload, meta_seq)`.
fn winner(
    m: &[u8],
    volume: [u8; 16],
    object: u64,
    at: [u64; 2],
    log2: u8,
    numbered: bool,
) -> Option<(Vec<u8>, u64)> {
    let mut best: Option<(u64, Vec<u8>, u64)> = None;
    for (i, &off) in at.iter().enumerate() {
        let slot = m.get(off as usize..off as usize + (1usize << log2))?;
        let e = Expect {
            volume_uuid: volume,
            object_id: object,
            slot_offset: off,
            slot_index: if i == 0 { SlotIndex::A } else { SlotIndex::B },
            log2_slot_size: log2,
            format_major: FORMAT_2,
            numbered,
        };
        if let SlotStatus::Valid(info) = validate(slot, &e) {
            if best.as_ref().is_none_or(|b| info.generation > b.0) {
                best = Some((info.generation, info.payload(slot).to_vec(), info.meta_seq));
            }
        }
    }
    best.map(|(_, p, s)| (p, s))
}

/// Audits the container: panics, naming the broken rule, if any rule fails.
/// Returns every region found, in table order.
pub(crate) fn audit(pf: &SimPlatform) -> Vec<Found> {
    audit_with(pf, false)
}

/// [`audit`], also allowing a region header one extent ahead of its entry
/// (a growth beyond capacity that stopped before its commit): that extent
/// is not the region's (it may hold anything, and overlap anything).
pub(crate) fn audit_allowing_extra(pf: &SimPlatform) -> Vec<Found> {
    audit_with(pf, true)
}

fn audit_with(pf: &SimPlatform, allow_extra: bool) -> Vec<Found> {
    let m = media(pf);
    let b = block(pf);
    let volume: [u8; 16] = m[32..48].try_into().unwrap();
    let (vbytes, _) = winner(&m, volume, 0, [0, b], 12, true).expect("no volume record");
    let vm = VolumeMeta2::decode(&vbytes).expect("volume record");
    let table_end = vm.table_end();
    assert_eq!(table_end, 2 * b + 2 * u64::from(PAGES) * (1 << LOG2_TABLE));
    let file_len = m.len() as u64;
    let mut found = Vec::new();
    let mut ranges: Vec<(u64, u64, u64)> = Vec::new();
    let mut names = std::collections::HashSet::new();
    let mut scratch = vec![(0, 0); footprint_len(12)];
    for p in 0..PAGES {
        let at = [page_at(pf, p, 0), page_at(pf, p, 1)];
        let (bytes, page_meta_seq) =
            winner(&m, volume, (1 << 61) | u64::from(p), at, LOG2_TABLE, true)
                .unwrap_or_else(|| panic!("page {p}: no valid copy"));
        let page = decode_page(&bytes, p, entries_per_page(LOG2_TABLE))
            .unwrap_or_else(|e| panic!("page {p}: {e}"));
        let lim = EntryLimits {
            log2_block: 12,
            table_end,
            file_len,
            page_meta_seq,
        };
        let mut count = 0u16;
        for raw in page.entries() {
            count += 1;
            assert!(
                raw.seq < page.header.next_seq,
                "page {p}: seq {} not below next_seq",
                raw.seq
            );
            let e = raw
                .check(&lim)
                .unwrap_or_else(|err| panic!("page {p} seq {}: entry {err}", raw.seq));
            let id = (u64::from(p) << 32) | u64::from(e.seq);
            let (hbytes, _) = winner(
                &m,
                volume,
                (1 << 62) | id,
                [e.header_a, e.header_b],
                12,
                false,
            )
            .unwrap_or_else(|| panic!("region {id:#x}: no valid header"));
            let view = decode_region_header(&hbytes, 12, file_len)
                .unwrap_or_else(|err| panic!("region {id:#x}: header {err}"));
            if !allow_extra {
                assert_eq!(
                    view.header.extent_count, e.extent_count,
                    "region {id:#x}: extra extent"
                );
            }
            let fp = check_region(id, &e, &view, &lim, &mut scratch)
                .unwrap_or_else(|err| panic!("region {id:#x}: {err}"));
            let r = &scratch[..fp.ranges];
            // Spans are tight: exactly the footprint's lowest and highest.
            let lo = r.iter().map(|x| x.0).min().unwrap();
            let hi = r.iter().map(|x| x.0 + x.1).max().unwrap();
            assert_eq!(
                (e.span_start, e.span_end),
                (lo, hi),
                "region {id:#x}: span not tight"
            );
            for &(off, len) in r {
                ranges.push((off, off + len, id));
            }
            assert!(
                names.insert(e.name.as_str().to_owned()),
                "name {} twice",
                e.name.as_str()
            );
            found.push(Found {
                name: e.name.as_str().to_owned(),
                kind: e.kind,
                id,
                data_offset: view.extents().next().map_or(e.span_end, |x| x.0),
                data_size: e.data_size,
                capacity: e.capacity,
                header_at: [e.header_a, e.header_b],
                extents: view.extents().take(usize::from(e.extent_count)).collect(),
                extra: fp.extra,
                footprint_seq: e.footprint_seq,
                page_meta_seq,
            });
        }
        assert_eq!(count, page.header.count, "page {p}: count");
    }
    // Every footprint change is its own page write: no two entries share
    // a `footprint_seq` (`RawEntry::check` already bounds each by its
    // page's write).
    let mut seqs: Vec<u64> = found.iter().map(|f| f.footprint_seq).collect();
    seqs.sort_unstable();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "footprint_seq repeated: {seqs:?}"
    );
    // No two regions share a byte.
    ranges.sort_unstable();
    for w in ranges.windows(2) {
        assert!(
            w[0].1 <= w[1].0,
            "regions {:#x} and {:#x} overlap",
            w[0].2,
            w[1].2
        );
    }
    found
}

/// A region header as its pair's winning copy holds it.
#[derive(Debug, Clone)]
pub(crate) struct Header {
    /// The winning slot's generation (commits count here).
    pub(crate) slot_generation: u64,
    pub(crate) header: store_io_format::meta2::RegionHeader2,
    pub(crate) extents: Vec<(u64, u64)>,
    pub(crate) object: Vec<u8>,
}

/// Region `id`'s header, read straight from the media at `at` (its two
/// slots); `None` if neither copy is valid.
pub(crate) fn header_of(pf: &SimPlatform, id: u64, at: [u64; 2]) -> Option<Header> {
    let m = media(pf);
    let volume: [u8; 16] = m[32..48].try_into().unwrap();
    let mut best: Option<Header> = None;
    for (i, &off) in at.iter().enumerate() {
        let slot = m.get(off as usize..off as usize + 4096)?;
        let e = Expect {
            volume_uuid: volume,
            object_id: (1 << 62) | id,
            slot_offset: off,
            slot_index: if i == 0 { SlotIndex::A } else { SlotIndex::B },
            log2_slot_size: 12,
            format_major: FORMAT_2,
            numbered: false,
        };
        if let SlotStatus::Valid(info) = validate(slot, &e) {
            let v = decode_region_header(info.payload(slot), 12, u64::MAX).ok()?;
            if best
                .as_ref()
                .is_none_or(|b| info.generation > b.slot_generation)
            {
                best = Some(Header {
                    slot_generation: info.generation,
                    header: v.header,
                    extents: v.extents().collect(),
                    object: v.object().to_vec(),
                });
            }
        }
    }
    best
}
