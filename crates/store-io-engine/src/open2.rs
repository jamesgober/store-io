//! The format-2 open's regions (spec §9 steps 3-6): every entry on a page
//! that is not Lost is checked with its region's header pair; the regions
//! that pass are settled against each other and against those that did not
//! (`footprint`); free space is the complement of everything allocated.
//! Nothing here writes: a read-only verdict reached here comes before the
//! open's first write (§9 step 7).

use std::sync::{Arc, Mutex};

use store_io_core::error::Error;
use store_io_core::id::{Generation, RegionId, VolumeId};
use store_io_format::footprint::{check_region, footprint_len};
use store_io_format::judge::Standing;
use store_io_format::meta::{MetaError, RegionKind};
use store_io_format::meta2::{Entry2, EntryLimits, decode_region_header};
use store_io_format::slot::FORMAT_2;
use store_io_platform::Queue;

use crate::exec::{Lane, solo_lane};
use crate::footprint::{Extent, FreeMap, Unavailable as Gone, Usable, settle, shares_reserved};
use crate::frontier::AppendFrontier;
use crate::gate::Gate;
use crate::layout::Layout;
use crate::slots::{PairAt, PairRead};
use crate::store::*;
use crate::v2::{Meta2, MetadataReport, read_all_retried};

/// The most items each list of [`MetadataReport`] the region checks fill
/// holds; past it the report says it was cut.
pub(crate) const REPORT_LIMIT: usize = 256;

/// What the region checks found.
pub(crate) struct Regions2 {
    /// The usable regions, ready to serve.
    pub(crate) live: Vec<Arc<LiveRegion>>,
    /// Regions the table names that cannot be used (their names stay
    /// reserved).
    pub(crate) unavailable: Vec<UnavailableRegion>,
    /// Free space past the table area.
    pub(crate) free: FreeMap,
    /// Per-tag usage `(tag, logical, physical)` of every entry that passed
    /// the entry checks.
    pub(crate) usage: Vec<(u32, u64, u64)>,
    /// Why the store must open read-only, if a region requires it.
    pub(crate) read_only_reason: Option<&'static str>,
    /// Names of entries listed only by place (`invalid_entries`): their
    /// kind is unknown, their name reserved.
    pub(crate) reserved_names: Vec<store_io_format::meta::RegionName>,
    /// An entry names space past the file's end.
    pub(crate) cut_short: bool,
    /// Every allocated range `free` is the complement of.
    pub(crate) allocated: Vec<Extent>,
    /// What each unavailable or unknown-kind entry holds.
    pub(crate) holds: Vec<crate::v2::Hold>,
    /// The extra extents counted allocated on stale pages: `(page,
    /// range)`, as they are in `allocated`.
    pub(crate) stale_extra: Vec<(usize, Extent)>,
}

/// A region whose entry passed the entry checks.
struct Checked {
    id: u64,
    page: usize,
    entry: Entry2,
    /// The header pair as read.
    header: PairRead,
    /// The header's region generation and whether its winner may be
    /// rewritten, when it decoded.
    decoded: Option<(u64, bool)>,
    /// The header's committed extents, when it decoded (data order).
    extents: Vec<Extent>,
    /// Every extent the header lists, decoded without the file's length
    /// (`None` if it does not decode even so, or lists fewer than its
    /// entry): what its writes may
    /// have touched, judged by the header alone, so that a later growth of
    /// the file never changes the answer step 4 gives (spec §9 step 4).
    written: Option<Vec<Extent>>,
    /// An extent the header lists past the entry's count.
    extra: Option<Extent>,
    /// The header's data blocks are 512 B (spec §13, not built).
    small: bool,
    /// The footprint (step 3 passed), or why not.
    outcome: Result<Vec<Extent>, Unavailable>,
}

/// Lists in the report stop at [`REPORT_LIMIT`]; the report says so.
fn pushed<T>(list: &mut Vec<T>, item: T, truncated: &mut bool) {
    if list.len() < REPORT_LIMIT {
        list.push(item);
    } else {
        *truncated = true;
    }
}

/// Step 3's failures as the region's reason.
fn reason_of(e: MetaError) -> Unavailable {
    match e {
        MetaError::Field("region_id" | "kind" | "flags") => Unavailable::HeaderMismatch,
        MetaError::Field("state") => Unavailable::HeaderReleased,
        MetaError::Field("extent" | "capacity") | MetaError::Overlap => Unavailable::ExtentsInvalid,
        _ => Unavailable::HeaderInvalid,
    }
}

/// Region header pairs read at a time: the reads in flight are bounded by
/// the lane's depth; this bounds what a batch holds besides the pairs kept
/// (their places, and each losing slot's payload until it is decided).
const HEADER_BATCH: usize = 4096;

/// Header pairs (entries that passed the entry checks) from which the open
/// reads them on a queue of their own (`header_queue`): a blocking call's
/// lane may run one operation at a time (on Linux, D-104), which would cost
/// every pair a device round trip. Made as a parallel lane's queue is, so
/// on Linux's synchronous tier it starts the platform's submit pool, as the
/// first parallel read would.
const OWN_QUEUE_FROM: usize = 16;

/// Checks every region of the format-2 table in `v2` (steps 3-6), reading
/// the header pairs many at a time, and adds what it found to `report`.
/// The pairs are read through `q`, or, from [`OWN_QUEUE_FROM`] of them, on
/// a queue `header_queue` makes (one that keeps several operations in flight;
/// `None` if it cannot, and `q` serves).
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_regions<Q: Queue>(
    q: &mut Lane<Q>,
    file: &Q::File,
    pool: &store_io_buf::BufPool,
    layout: &Layout,
    volume: VolumeId,
    v2: &Meta2,
    container_len: u64,
    append_window: u64,
    report: &mut MetadataReport,
    header_queue: &mut dyn FnMut() -> Option<(Q, usize)>,
) -> Result<Regions2, Error> {
    let table_end = v2.volume.table_end();
    let block = layout.block();
    let mut scratch = vec![(0u64, 0u64); footprint_len(layout.log2_block)];
    let mut checked: Vec<Checked> = Vec::new();
    // Entries that failed the entry checks: their `footprint_seq` as stored
    // and what they reserve.
    let mut bad_entries: Vec<(
        u64,
        Option<Extent>,
        Option<store_io_format::meta::RegionName>,
    )> = Vec::new();
    let mut usage: Vec<(u32, u64, u64)> = Vec::new();
    let mut unavailable = Vec::new();
    let mut read_only_reason = None;
    let mut reserved_names = Vec::new();
    let mut cut_short = false;
    let mut holds = Vec::new();
    let mut stale_extra = Vec::new();
    let mut truncated = false;
    // Whether every region header on a stale page agrees with its entry.
    let mut agrees = vec![true; v2.pages.len()];
    // Entries that passed the entry checks, their header pairs to read:
    // (id, page, entry, limits).
    let mut pending: Vec<(u64, usize, Entry2, EntryLimits)> = Vec::new();
    for (p, page) in v2.pages.iter().enumerate() {
        if !matches!(page.standing, Standing::Confirmed | Standing::Unconfirmed) {
            // A Lost page's regions are unknown (allocation is frozen).
            continue;
        }
        let page_meta_seq = page
            .pair
            .report
            .winner_info()
            .and_then(|(i, _)| page.pair.facts[usize::from(i.as_u8())])
            .and_then(|f| f.numbered)
            .map_or(0, |n| n.meta_seq);
        let lim = EntryLimits {
            log2_block: layout.log2_block,
            table_end,
            file_len: container_len,
            page_meta_seq,
        };
        for raw in &page.entries {
            let id = ((p as u64) << 32) | u64::from(raw.seq);
            // Past the file's end: growth must not make zeros of the tail.
            let short = raw.span_end > container_len;
            cut_short |= short;
            let Ok(entry) = raw.check(&lim) else {
                agrees[p] = false;
                // A sequence the entry checks refuse may be newest: one that
                // is 0, or past its page's write, supersedes as the highest.
                let seq = if raw.footprint_seq == 0 || raw.footprint_seq > page_meta_seq {
                    u64::MAX
                } else {
                    raw.footprint_seq
                };
                let span = raw
                    .reserved_span(&lim)
                    .map(|(start, end)| (start, end - start));
                let entry_name = raw.name_and_kind().map(|(n, _)| n).or_else(|| raw.name());
                bad_entries.push((seq, span, entry_name));
                holds.push(crate::v2::Hold {
                    page: p,
                    seq: raw.seq,
                    span,
                    usage: None,
                    name: if raw.name_and_kind().is_none() {
                        raw.name()
                    } else {
                        None
                    },
                    short,
                    footprint_seq: seq,
                    own: span.into_iter().collect(),
                    demoted: None,
                    entry_name,
                });
                // Every unavailable region is listed, never cut: the list
                // keeps their names reserved.
                match raw.name_and_kind() {
                    Some((name, kind)) => unavailable.push(UnavailableRegion {
                        name: name.as_str().to_owned(),
                        id: RegionId::from_raw(id),
                        kind,
                        reason: if short {
                            Unavailable::ContainerShort {
                                needed: raw.span_end,
                                actual: container_len,
                            }
                        } else {
                            Unavailable::EntryInvalid
                        },
                    }),
                    None => {
                        // A kind from a newer release: its name stays
                        // reserved though it cannot be listed as a region.
                        if let Some(name) = raw.name() {
                            reserved_names.push(name);
                        }
                        pushed(
                            &mut report.invalid_entries,
                            (p as u32, raw.seq),
                            &mut truncated,
                        );
                    }
                }
                continue;
            };
            // Bounded: the entry checks keep the capacity inside the span.
            usage.push((
                entry.tag,
                entry.data_size,
                entry.capacity.saturating_add(2 * block),
            ));
            pending.push((id, p, entry, lim));
        }
    }
    let mut own = if pending.len() >= OWN_QUEUE_FROM {
        header_queue().map(|(queue, depth)| solo_lane(queue, depth))
    } else {
        None
    };
    let lane = match own.as_mut() {
        Some(own) => own,
        None => &mut *q,
    };
    for batch in pending.chunks(HEADER_BATCH) {
        let ats: Vec<PairAt> = batch
            .iter()
            .map(|&(id, _, entry, _)| PairAt {
                volume: *volume.as_bytes(),
                object: layout.region_object(id),
                a: entry.header_a,
                b: entry.header_b,
                log2: layout.log2_block,
                format: FORMAT_2,
                numbered: false,
            })
            .collect();
        let headers = read_all_retried(lane, file, pool, &ats, &mut report.retried_reads)?;
        for (&(id, p, entry, lim), header) in batch.iter().zip(headers) {
            let page = &v2.pages[p];
            let mut c = Checked {
                id,
                page: p,
                entry,
                header,
                decoded: None,
                extents: Vec::new(),
                written: None,
                extra: None,
                small: false,
                outcome: Err(Unavailable::HeaderUnreadable),
            };
            if let (Some(bytes), Some((won, info))) =
                (c.header.payload.as_deref(), c.header.report.winner_info())
            {
                // The other slot's header verifies with a newer generation,
                // its version unread (a torn write explains the survivor, so
                // the winner is still Confirmed): a recycle or slot commit
                // possibly lost (spec §8).
                let other = usize::from(1 - won.as_u8());
                let newer = c.header.facts[other]
                    .and_then(|f| f.numbered)
                    .is_some_and(|n| n.generation > info.generation);
                if newer {
                    pushed(
                        &mut report.lost_header_writes,
                        RegionId::from_raw(id),
                        &mut truncated,
                    );
                }
                c.outcome = Err(Unavailable::HeaderInvalid);
                // Decoded by the header alone: the file's length is never an
                // input (a later growth of the file would change the
                // verdict). The entry checks keep the span inside the file
                // and `check_region` each committed extent inside the span;
                // only the one extent a header may list past its entry's (a
                // lost growth, ignored or counted allocated) may lie past
                // the file's end.
                if let Ok(view) = decode_region_header(bytes, layout.log2_block, u64::MAX) {
                    let committed = usize::from(entry.extent_count);
                    // What its writes may have touched: the extents its
                    // header lists, unless it lists fewer than the entry (a
                    // header rolled back: the span then).
                    if usize::from(view.header.extent_count) >= committed {
                        // Every extent it lists: one past its entry's (a
                        // lost growth) was filled before the header named it.
                        c.written = Some(view.extents().collect());
                    }
                    c.extents = view.extents().take(committed).collect();
                    if usize::from(view.header.extent_count) == committed + 1 {
                        c.extra = view.extents().nth(committed);
                    }
                    c.decoded = Some((view.header.region_generation, info.writable()));
                    c.small = view.header.log2_block == 9;
                    c.outcome = check_region(id, &entry, &view, &lim, &mut scratch)
                        .map(|fp| scratch[..fp.ranges].to_vec())
                        .map_err(reason_of);
                }
            }
            // A header one extent ahead of its entry on a stale page is a
            // lost growth: it does not agree either.
            if c.outcome.is_err() || (c.extra.is_some() && page.standing == Standing::Unconfirmed) {
                agrees[p] = false;
            }
            if let Some(extra) = c.extra {
                pushed(
                    &mut report.extra_extents,
                    (RegionId::from_raw(id), extra),
                    &mut truncated,
                );
            }
            checked.push(c);
        }
    }

    // Step 4: settle the usable regions against each other and against
    // those that are not. An unavailable region may have written over its
    // own extents (from its header, when that decodes), else its span.
    let usable_at: Vec<usize> = (0..checked.len())
        .filter(|&i| checked[i].outcome.is_ok())
        .collect();
    // (footprint_seq, span reserved, what it may have written, name).
    #[allow(clippy::type_complexity)]
    let mut gone: Vec<(
        u64,
        Option<Extent>,
        Vec<Extent>,
        Option<store_io_format::meta::RegionName>,
    )> = bad_entries
        .iter()
        .map(|&(seq, span, name)| (seq, span, span.into_iter().collect(), name))
        .collect();
    for c in checked.iter().filter(|c| c.outcome.is_err()) {
        let e = &c.entry;
        let span = (e.span_start, e.span_end - e.span_start);
        // What its writes may have touched: its header blocks and the
        // extents its header lists, when the header decodes (by the header
        // alone: see `written`), else the whole span.
        let own = match &c.written {
            Some(written) => {
                let mut own = vec![(e.header_a, block), (e.header_b, block)];
                own.extend_from_slice(written);
                own
            }
            None => vec![span],
        };
        gone.push((e.footprint_seq, Some(span), own, Some(e.name)));
    }
    let settled = {
        let usable: Vec<Usable<'_>> = usable_at
            .iter()
            .map(|&i| Usable {
                name: checked[i].entry.name.as_str(),
                seq: checked[i].entry.footprint_seq,
                ranges: checked[i].outcome.as_deref().unwrap_or(&[]),
            })
            .collect();
        let others: Vec<Gone<'_>> = gone
            .iter()
            .map(|(seq, _, own, name)| Gone {
                seq: *seq,
                own,
                name: name.as_ref().map(store_io_format::meta::RegionName::as_str),
            })
            .collect();
        settle(&usable, &others)
    };
    let mut verdicts = vec![None; checked.len()];
    for (&i, s) in usable_at.iter().zip(settled) {
        verdicts[i] = Some(s);
    }

    // Step 6: free space is the complement of every usable footprint, every
    // unavailable span, and every extra extent on a stale page.
    let mut allocated: Vec<Extent> = gone.iter().filter_map(|g| g.1).collect();
    // Every unavailable region's span: those above, then the demoted.
    let mut reserved: Vec<Extent> = allocated.clone();
    // Each live region's footprint, for the reserved-overlap report.
    let mut footprints: Vec<(u64, String, u64, Vec<Extent>)> = Vec::new();
    let mut live = Vec::new();
    let settled = v2.pages.iter().all(|p| p.standing == Standing::Confirmed);
    for (c, verdict) in checked.into_iter().zip(verdicts) {
        let e = c.entry;
        let unconfirmed = v2.pages[c.page].standing == Standing::Unconfirmed;
        if unconfirmed {
            // It may be the newest growth (spec §9 step 3). Bytes past the
            // file's end hold nothing: clamped, so free space never ends
            // there (a header is decoded without the file's length).
            if let Some((off, len)) = c.extra {
                if off < container_len {
                    let range = (off, len.min(container_len - off));
                    allocated.push(range);
                    stale_extra.push((c.page, range));
                }
            }
        }
        let reason = match (&c.outcome, verdict) {
            (Err(r), _) => Some(*r),
            (Ok(_), Some(s)) if s.overlapped => Some(Unavailable::Overlaps),
            (Ok(_), Some(s)) if s.renamed => Some(Unavailable::NameTaken),
            _ => None,
        };
        if let Some(reason) = reason {
            if c.outcome.is_ok() {
                // Demoted in step 4: its span stays allocated.
                allocated.push((e.span_start, e.span_end - e.span_start));
                reserved.push((e.span_start, e.span_end - e.span_start));
            }
            // Its span is in `allocated` (above, or with the failed ones)
            // and its usage was counted. What it may have written, as step
            // 4 judged it (a demoted region supersedes nothing).
            let own = match (&c.outcome, &c.written) {
                (Err(_), Some(written)) => {
                    let mut own = vec![(e.header_a, block), (e.header_b, block)];
                    own.extend_from_slice(written);
                    own
                }
                (Err(_), None) => vec![(e.span_start, e.span_end - e.span_start)],
                (Ok(_), _) => Vec::new(),
            };
            holds.push(crate::v2::Hold {
                page: c.page,
                seq: e.seq,
                span: Some((e.span_start, e.span_end - e.span_start)),
                usage: Some((e.tag, e.data_size, e.capacity.saturating_add(2 * block))),
                name: None,
                short: false,
                footprint_seq: e.footprint_seq,
                own,
                demoted: c
                    .outcome
                    .as_ref()
                    .ok()
                    .map(|ranges| (e.name, ranges.clone())),
                entry_name: Some(e.name),
            });
            unavailable.push(UnavailableRegion {
                name: e.name.as_str().to_owned(),
                id: RegionId::from_raw(c.id),
                kind: e.kind,
                reason,
            });
            continue;
        }
        let s = verdict.unwrap_or_default();
        if c.small {
            // §13 (512 B append blocks) is not built yet; only a region that
            // stays usable decides it.
            read_only_reason = Some(
                "an append region uses 512-byte blocks, which this version reads but does not write",
            );
        }
        let ranges = c.outcome.unwrap_or_default();
        allocated.extend_from_slice(&ranges);
        if s.superseded {
            pushed(
                &mut report.superseded,
                RegionId::from_raw(c.id),
                &mut truncated,
            );
        }
        footprints.push((c.id, e.name.as_str().to_owned(), e.footprint_seq, ranges));
        // A usable region's header decoded, and a decoded header's
        // generation is never 0.
        let (raw_generation, writable) = c.decoded.ok_or_else(corrupt)?;
        let generation = Generation::from_raw(raw_generation).ok_or_else(corrupt)?;
        let frontier = (e.kind == RegionKind::Append).then(|| {
            AppendFrontier::new(
                0,
                e.data_size,
                u32::from(layout.log2_block),
                crate::frontier::ring_for(
                    append_window,
                    crate::frontier::ring_reach(e.capacity, e.flags),
                    u32::from(layout.log2_block),
                ),
            )
        });
        // Room for every run the header may list (a growth adds them).
        let room = store_io_format::meta2::max_extents(layout.log2_block) - 1;
        let map =
            crate::regionmap::RegionMap::from_extents(&c.extents, e.data_size).with_room(room);
        live.push(Arc::new(LiveRegion {
            id: RegionId::from_raw(c.id),
            name: e.name,
            kind: e.kind,
            generation,
            map,
            header_at: [e.header_a, e.header_b],
            header: Mutex::new(c.header),
            frontier,
            positioned: std::sync::atomic::AtomicBool::new(false),
            force_sync: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            // On a stale page; on a page possibly accepted (a twin, or its
            // older copy damaged) while
            // another is stale or lost (D-130: its lost commit's release
            // may have given another stale page's lost commit its space);
            // possibly superseded; or with a header whose winner carries a
            // feature this version may not write.
            read_only: std::sync::atomic::AtomicBool::new(
                unconfirmed
                    || (!settled && v2.pages[c.page].pair.possibly_accepted())
                    || s.superseded
                    || !writable,
            ),
            superseded: s.superseded,
            foreign: !writable,
            gate: Gate::new(),
        }));
    }
    let usable: Vec<Usable<'_>> = footprints
        .iter()
        .map(|(_, name, seq, ranges)| Usable {
            name,
            seq: *seq,
            ranges,
        })
        .collect();
    for (f, shares) in footprints.iter().zip(shares_reserved(&usable, &reserved)) {
        if shares {
            pushed(
                &mut report.shares_reserved,
                RegionId::from_raw(f.0),
                &mut truncated,
            );
        }
    }
    for (p, page) in v2.pages.iter().enumerate() {
        if page.standing == Standing::Unconfirmed {
            pushed(
                &mut report.stale_page_agreement,
                (p as u32, agrees[p]),
                &mut truncated,
            );
        }
    }
    report.truncated |= truncated;
    let free = FreeMap::complement(table_end, container_len, &mut allocated.clone());
    Ok(Regions2 {
        live,
        unavailable,
        free,
        usage,
        read_only_reason,
        reserved_names,
        cut_short,
        allocated,
        holds,
        stale_extra,
    })
}
