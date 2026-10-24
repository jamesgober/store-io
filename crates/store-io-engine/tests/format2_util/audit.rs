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

use super::{DIR, block, media_in};

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
    audit_with(pf, DIR, false)
}

/// [`audit`] of the store in `dir`.
pub(crate) fn audit_in(pf: &SimPlatform, dir: &str) -> Vec<Found> {
    audit_with(pf, dir, false)
}

/// [`audit`], also allowing a region header one extent ahead of its entry
/// (a growth beyond capacity that stopped before its commit): that extent
/// is not the region's (it may hold anything, and overlap anything).
pub(crate) fn audit_allowing_extra(pf: &SimPlatform) -> Vec<Found> {
    audit_with(pf, DIR, true)
}

fn audit_with(pf: &SimPlatform, dir: &str, allow_extra: bool) -> Vec<Found> {
    let m = media_in(pf, dir);
    let b = block(pf);
    let volume: [u8; 16] = m[32..48].try_into().unwrap();
    let (vbytes, _) = winner(&m, volume, 0, [0, b], 12, true).expect("no volume record");
    let vm = VolumeMeta2::decode(&vbytes).expect("volume record");
    let table_end = vm.table_end();
    // The geometry as the volume record gives it (any table's).
    let log2_table = vm.base.log2_table_slot_size;
    let t = 1u64 << log2_table;
    assert_eq!(table_end, 2 * b + 2 * u64::from(vm.table_pages) * t);
    assert_eq!(
        vm.entries_per_page,
        entries_per_page(log2_table),
        "entries per page"
    );
    let file_len = m.len() as u64;
    let mut found = Vec::new();
    let mut ranges: Vec<(u64, u64, u64)> = Vec::new();
    let mut names = std::collections::HashSet::new();
    let mut scratch = vec![(0, 0); footprint_len(12)];
    for p in 0..vm.table_pages {
        let at = [
            2 * b + 2 * u64::from(p) * t,
            2 * b + (2 * u64::from(p) + 1) * t,
        ];
        let (bytes, page_meta_seq) =
            winner(&m, volume, (1 << 61) | u64::from(p), at, log2_table, true)
                .unwrap_or_else(|| panic!("page {p}: no valid copy"));
        let page = decode_page(&bytes, p, entries_per_page(log2_table))
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
    header_in(pf, DIR, id, at)
}

/// [`header_of`] in the store in `dir`.
pub(crate) fn header_in(pf: &SimPlatform, dir: &str, id: u64, at: [u64; 2]) -> Option<Header> {
    let m = media_in(pf, dir);
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

/// What the format crate's judgement (spec §7) makes of the media as they
/// are, before any open, the slots a fault makes unreadable read as
/// `ReadError`; then, as the open must (§9), a page whose winner fails the
/// page checks is lost, and a page's heal is dropped (the page stale) when
/// any page is so lost or the page holds an entry past its own write.
/// `None` when the open must fail: a pair refused, no volume record, or a
/// file shorter than its metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Judged {
    pub(crate) volume_stale: bool,
    pub(crate) stale: Vec<u32>,
    pub(crate) lost: Vec<u32>,
    /// The torn write a writable open heals: `None` the volume record,
    /// `Some(p)` page `p`.
    pub(crate) heal: Option<Option<u32>>,
    /// A writable open may write (the judgement's).
    pub(crate) writable: bool,
    /// The clean-close rule applied (the last writer closed cleanly).
    pub(crate) clean: bool,
    /// A winner carries a read-only-compatible feature this version does
    /// not know (the store opens read only).
    pub(crate) foreign: bool,
}

/// The store's id and its volume pair as the open finds them: the first
/// copy (slot A, then B) whose header verifies at this block size names the
/// id, and is kept only if the pair then has a winner.
fn volume_of(
    m: &[u8],
    read: &dyn Fn([u8; 16], u64, u64, u8, SlotIndex) -> store_io_format::judge::SlotRead,
    b: u64,
) -> Option<([u8; 16], store_io_format::judge::PairRead)> {
    for off in [0, b] {
        let s = m.get(off as usize..(off + b) as usize)?;
        let id: [u8; 16] = s[32..48].try_into().unwrap();
        if s[..8] != store_io_format::slot::MAGIC[..]
            || s[78] != 12
            || store_io_format::slot::header_facts(
                s,
                &Expect {
                    volume_uuid: id,
                    object_id: 0,
                    slot_offset: off,
                    slot_index: if off == 0 { SlotIndex::A } else { SlotIndex::B },
                    log2_slot_size: 12,
                    format_major: FORMAT_2,
                    numbered: true,
                },
            )
            .and_then(|f| f.numbered)
            .is_none()
        {
            continue;
        }
        let pair = store_io_format::judge::PairRead {
            a: read(id, 0, 0, 12, SlotIndex::A),
            b: read(id, b, 0, 12, SlotIndex::B),
        };
        if matches!(pair.a.status, SlotStatus::Valid(_))
            || matches!(pair.b.status, SlotStatus::Valid(_))
        {
            return Some((id, pair));
        }
    }
    None
}

/// The store's id, as [`volume_of`] finds it on intact-enough media.
fn store_id(m: &[u8], b: u64) -> [u8; 16] {
    let read = |id: [u8; 16], off: u64, object: u64, log2: u8, index: SlotIndex| {
        read_slot(m, &[], id, off, object, log2, index)
    };
    volume_of(m, &read, b).expect("no volume record").0
}

fn read_slot(
    m: &[u8],
    bad: &[(u64, u64)],
    id: [u8; 16],
    off: u64,
    object: u64,
    log2: u8,
    index: SlotIndex,
) -> store_io_format::judge::SlotRead {
    use store_io_format::judge::SlotRead;
    let len = 1u64 << log2;
    if bad.iter().any(|&(s, e)| s < off + len && off < e) {
        return SlotRead {
            status: SlotStatus::ReadError,
            facts: None,
        };
    }
    let slot = &m[off as usize..(off + len) as usize];
    let e = Expect {
        volume_uuid: id,
        object_id: object,
        slot_offset: off,
        slot_index: index,
        log2_slot_size: log2,
        format_major: FORMAT_2,
        numbered: true,
    };
    SlotRead {
        status: validate(slot, &e),
        facts: store_io_format::slot::header_facts(slot, &e),
    }
}

pub(crate) fn judged(pf: &SimPlatform) -> Option<Judged> {
    judged_in(pf, DIR)
}

pub(crate) fn judged_in(pf: &SimPlatform, dir: &str) -> Option<Judged> {
    use store_io_format::judge::{PairRead, PairVerdict, SlotVerdict, Standing, judge};
    let m = media_in(pf, dir);
    let b = block(pf);
    if (m.len() as u64) < 2 * b {
        // Shorter than the volume record: no store (a crashed create).
        return None;
    }
    let file = pf.with_world(|w| w.file_id(&dir.into(), super::CONTAINER).unwrap());
    let bad: Vec<(u64, u64)> = pf.with_world(|w| {
        w.faults()
            .bad_ranges
            .iter()
            .filter(|r| r.0 == file)
            .map(|r| (r.1, r.2))
            .collect()
    });
    let read = |id: [u8; 16], off: u64, object: u64, log2: u8, index: SlotIndex| {
        read_slot(&m, &bad, id, off, object, log2, index)
    };
    let (volume, vpair) = volume_of(&m, &read, b)?;
    // The geometry and the clean-close flag: the volume record's winner
    // (the higher Valid generation), as the open reads it.
    let (vbytes, _) = winner_of(&m, &vpair, [0, b])?;
    let vm = VolumeMeta2::decode(&vbytes).ok()?;
    if (m.len() as u64) < vm.table_end() {
        // Shorter than its metadata: the open fails (corrupt).
        return None;
    }
    let log2_table = vm.base.log2_table_slot_size;
    let t = 1u64 << log2_table;
    let at = |p: u32| {
        [
            2 * b + 2 * u64::from(p) * t,
            2 * b + (2 * u64::from(p) + 1) * t,
        ]
    };
    let mut pairs = vec![vpair];
    for p in 0..vm.table_pages {
        let [x, y] = at(p);
        let object = (1 << 61) | u64::from(p);
        pairs.push(PairRead {
            a: read(volume, x, object, log2_table, SlotIndex::A),
            b: read(volume, y, object, log2_table, SlotIndex::B),
        });
    }
    let mut out = vec![
        PairVerdict {
            standing: Standing::Lost,
            winner: None,
            a: SlotVerdict::Damage,
            b: SlotVerdict::Damage,
        };
        pairs.len()
    ];
    let j = judge(&pairs, vm.clean_close, &mut out).unwrap();
    if j.refused || out[0].winner.is_none() {
        return None;
    }
    let mut standing: Vec<Standing> = out[1..].iter().map(|v| v.standing).collect();
    let mut ahead = vec![false; standing.len()];
    let mut decode_lost = false;
    for p in 0..vm.table_pages {
        let i = p as usize;
        if standing[i] == Standing::Lost {
            continue;
        }
        let (bytes, ms) = winner_of(&m, &pairs[i + 1], at(p)).unwrap();
        match decode_page(&bytes, p, entries_per_page(log2_table)) {
            Ok(page) => ahead[i] = page.entries().any(|e| e.footprint_seq > ms),
            Err(_) => {
                standing[i] = Standing::Lost;
                decode_lost = true;
            }
        }
    }
    let mut heal = j
        .heal
        .map(|(pair, _)| pair.checked_sub(1).map(|p| p as u32));
    if let Some(Some(p)) = heal {
        if decode_lost || ahead[p as usize] {
            heal = None;
            if standing[p as usize] == Standing::Confirmed {
                standing[p as usize] = Standing::Unconfirmed;
            }
        }
    }
    let foreign = pairs.iter().zip(&out).any(|(pr, v)| {
        let status = match v.winner {
            Some(SlotIndex::A) => pr.a.status,
            Some(SlotIndex::B) => pr.b.status,
            None => return false,
        };
        matches!(status, SlotStatus::Valid(info) if !info.writable())
    });
    let pages = |lost: bool| -> Vec<u32> {
        (0..vm.table_pages)
            .filter(|&p| {
                let s = standing[p as usize];
                if lost {
                    matches!(s, Standing::Lost | Standing::Refused)
                } else {
                    s == Standing::Unconfirmed
                }
            })
            .collect()
    };
    Some(Judged {
        volume_stale: out[0].standing != Standing::Confirmed,
        stale: pages(false),
        lost: pages(true),
        heal,
        writable: j.writable,
        clean: j.clean,
        foreign,
    })
}

/// A pair's winning copy as read (the higher Valid generation), as
/// `(payload, meta_seq)`.
fn winner_of(
    m: &[u8],
    pair: &store_io_format::judge::PairRead,
    at: [u64; 2],
) -> Option<(Vec<u8>, u64)> {
    let mut best: Option<(u64, Vec<u8>, u64)> = None;
    for (read, off) in [(pair.a, at[0]), (pair.b, at[1])] {
        if let SlotStatus::Valid(info) = read.status {
            let slot = &m[off as usize..];
            if best.as_ref().is_none_or(|b| info.generation > b.0) {
                best = Some((info.generation, info.payload(slot).to_vec(), info.meta_seq));
            }
        }
    }
    best.map(|(_, p, s)| (p, s))
}

/// The open's report agrees with [`judged`] of the media it found
/// (`before`): the volume record stale, the stale and lost pages, the heal
/// (made only by a writable open), the clean close, and read only exactly
/// where the judgement forbids writing (for that reason) or a winner
/// carries a feature this version may not write.
pub(crate) fn report_agrees(before: &Judged, report: &store_io_engine::StoreReport, what: &str) {
    let m = &report.metadata;
    assert_eq!(
        (
            m.volume_stale,
            m.stale_pages.as_slice(),
            m.lost_pages.as_slice()
        ),
        (
            before.volume_stale,
            before.stale.as_slice(),
            before.lost.as_slice()
        ),
        "{what}: the open's standings against the media's"
    );
    // The volume record's heal as `None`, a page's as `Some(p)`.
    let healed = m.healed.map(|h| {
        if let store_io_engine::Healed::Page(p) = h {
            Some(p)
        } else {
            None
        }
    });
    let want = if report.read_only { None } else { before.heal };
    assert_eq!(healed, want, "{what}: the heal against the media's");
    assert_eq!(m.clean_close, before.clean, "{what}: the clean close");
    if !before.writable {
        assert!(
            report.read_only,
            "{what}: writable where the judgement forbids it"
        );
        // A read-only open asked for gives no reason.
        if let Some(reason) = m.read_only_reason {
            assert_eq!(
                reason, "the volume record is damaged and a table page holds a damaged copy",
                "{what}: read only as the judgement says"
            );
        }
    } else if before.foreign {
        assert!(report.read_only, "{what}: a foreign feature written over");
    }
}

/// The store's space accounting against the media (spec §9 steps 5 and 6:
/// free space is the complement of the table area, every usable region's
/// footprint and every unavailable region's span): the container its file's
/// length, the bytes free plus reserved, and the bytes in usable regions,
/// counted over the media (the store says only which entries are
/// unavailable); every unavailable region's name reserved. For a writable,
/// unfrozen store with nothing running (no provisioning or growth marked):
/// an extra extent a growth left is free (its page is not stale).
pub(crate) fn audit_space(pf: &SimPlatform, s: &store_io_engine::Store<SimPlatform>) {
    audit_space_in(pf, DIR, s);
}

pub(crate) fn audit_space_in(pf: &SimPlatform, dir: &str, s: &store_io_engine::Store<SimPlatform>) {
    assert!(
        !s.frozen() && !s.report().read_only,
        "audit_space: a writable, unfrozen store"
    );
    // The store's view: the bytes as visible (a growth may not be durable
    // yet), the container their length.
    let file = pf.with_world(|w| w.file_id(&dir.into(), super::CONTAINER).unwrap());
    let m = pf.with_world(|w| w.visible(file).unwrap().to_vec());
    let b = block(pf);
    let volume = store_id(&m, b);
    let (vbytes, _) = winner(&m, volume, 0, [0, b], 12, true).unwrap();
    let vm = VolumeMeta2::decode(&vbytes).unwrap();
    let table_end = vm.table_end();
    let log2_table = vm.base.log2_table_slot_size;
    let t = 1u64 << log2_table;
    let space = s.space();
    assert_eq!(space.container, m.len() as u64, "the container is the file");
    let invalid = s.report().metadata.invalid_entries.clone();
    let unavailable: Vec<u64> = s.unavailable_regions().iter().map(|u| u.id.get()).collect();
    // Every allocated byte, as a union of ranges.
    let mut ranges: Vec<(u64, u64)> = vec![(0, table_end)];
    let mut in_regions = 0u64;
    let mut seen_unavailable = 0usize;
    let mut scratch = vec![(0, 0); footprint_len(12)];
    for p in 0..vm.table_pages {
        let at = [
            2 * b + 2 * u64::from(p) * t,
            2 * b + (2 * u64::from(p) + 1) * t,
        ];
        let (bytes, page_meta_seq) =
            winner(&m, volume, (1 << 61) | u64::from(p), at, log2_table, true)
                .unwrap_or_else(|| panic!("page {p}: no valid copy"));
        let page = decode_page(&bytes, p, entries_per_page(log2_table))
            .unwrap_or_else(|e| panic!("page {p}: {e}"));
        let lim = EntryLimits {
            log2_block: 12,
            table_end,
            file_len: m.len() as u64,
            page_meta_seq,
        };
        for raw in page.entries() {
            let id = (u64::from(p) << 32) | u64::from(raw.seq);
            let is_unavailable = unavailable.contains(&id);
            if is_unavailable || invalid.contains(&(p, raw.seq)) {
                // Its span stays allocated, whatever it covers (§9 step 5):
                // an entry that passes the entry checks keeps its span and
                // its usage (capacity and headers) in the store's count;
                // one that fails them its span widened to whole blocks and
                // clamped to the table and the file (`reserved_span`).
                match raw.check(&lim) {
                    Ok(e) => {
                        ranges.push((e.span_start, e.span_end));
                        in_regions += e.capacity + 2 * b;
                    }
                    Err(_) => ranges.extend(raw.reserved_span(&lim)),
                }
                seen_unavailable += usize::from(is_unavailable);
                continue;
            }
            let e = raw
                .check(&lim)
                .unwrap_or_else(|err| panic!("region {id:#x}: served, entry {err}"));
            let (hbytes, _) = winner(
                &m,
                volume,
                (1 << 62) | id,
                [e.header_a, e.header_b],
                12,
                false,
            )
            .unwrap_or_else(|| panic!("region {id:#x}: served, no valid header"));
            let view = decode_region_header(&hbytes, 12, m.len() as u64)
                .unwrap_or_else(|err| panic!("region {id:#x}: served, header {err}"));
            let fp = check_region(id, &e, &view, &lim, &mut scratch)
                .unwrap_or_else(|err| panic!("region {id:#x}: served, {err}"));
            for &(off, len) in &scratch[..fp.ranges] {
                ranges.push((off, off + len));
                in_regions += len;
            }
        }
    }
    assert_eq!(
        seen_unavailable,
        unavailable.len(),
        "every unavailable region has its entry"
    );
    ranges.sort_unstable();
    let mut allocated = 0u64;
    let mut end = 0u64;
    for (lo, hi) in ranges {
        let lo = lo.max(end);
        if hi > lo {
            allocated += hi - lo;
            end = hi;
        }
    }
    assert_eq!(space.regions, in_regions, "the bytes in regions");
    assert_eq!(
        space.tags.iter().map(|t| t.physical).sum::<u64>(),
        space.regions,
        "the tags' charges are the bytes in regions"
    );
    assert_eq!(
        space.released + space.reserved,
        space.container - allocated,
        "free space is the complement of what is allocated"
    );
    for u in s.unavailable_regions() {
        let taken = s.provision_slot(&u.name).map(drop);
        assert!(
            matches!(
                taken,
                Err(store_io_core::error::Error::AlreadyExists { .. })
            ),
            "the unavailable region {:?}'s name not reserved: {taken:?}",
            u.name
        );
    }
}

/// [`audit`], and the store closed cleanly: the media judged clean, the
/// volume record Confirmed with nothing to heal.
pub(crate) fn audit_closed(pf: &SimPlatform) -> Vec<Found> {
    let found = audit(pf);
    assert_closed_in(pf, DIR);
    found
}

/// The store in `dir` closed cleanly: the media judged clean, the volume
/// record Confirmed with nothing to heal (whatever the pages' standing).
pub(crate) fn assert_closed_in(pf: &SimPlatform, dir: &str) {
    let j = judged_in(pf, dir).expect("a closed store that does not open");
    assert!(
        j.clean && !j.volume_stale && j.heal.is_none(),
        "a writable store closed without a clean close: {j:?}"
    );
}
