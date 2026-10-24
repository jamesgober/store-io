//! Container format 2 (`.dev/design/format2-draft.md`): the paged region
//! table's metadata, create, the open's tear judgement, region checks
//! (`open2`) and healing, and the clean close. Built beside format 1 (D-116
//! makes format 1 read-only once creation switches, v0.5 E5).

use store_io_core::error::{Capability, Error, ErrorContext, Op};
use store_io_core::id::{RegionId, VolumeId};
use store_io_format::judge::{self, PairVerdict, SlotVerdict, Standing};
use store_io_format::meta::VolumeMeta;
use store_io_format::meta2::{
    PageHeader, RawEntry, VOLUME_META2_LEN, VolumeMeta2, decode_page, encode_page, entries_per_page,
};
use store_io_format::slot::{FORMAT_2, SlotIndex, SlotStatus};
use store_io_platform::Platform;

use crate::domain::Domain;
use crate::exec::Lane;
use crate::layout::Layout;
use crate::slots::{PairAt, PairRead, plan_next, read_pair, read_pairs, reread_failed, write_slot};
use crate::store::*;

/// The most regions a format-2 store can be created for (a table of
/// 24,819 pages, two copies each: 776 MiB at 16 KiB pages, all read at
/// every open); a larger `region_capacity` is
/// clamped, and [`StoreReport::table_capacity`] says what was made.
pub const MAX_REGION_CAPACITY: u32 = 1 << 22;

/// What a format-2 open found in its metadata pairs (the volume record and
/// the table pages).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MetadataReport {
    /// The last writer closed cleanly: its final volume record is intact
    /// and says so (a damaged or in-flight volume record voids it).
    pub clean_close: bool,
    /// Pages whose newest copy is damaged (Unconfirmed): their regions are
    /// read-only and allocation is frozen.
    pub stale_pages: Vec<u32>,
    /// Pages with no usable copy (Lost), or whose copy fails the page
    /// checks: their regions are unknown and allocation is frozen.
    pub lost_pages: Vec<u32>,
    /// The volume record's newest copy was damaged (a writable open
    /// replaces it).
    pub volume_stale: bool,
    /// The one metadata write a crash left torn, which this open rewrote.
    pub healed: Option<Healed>,
    /// Allocation is frozen: some page is stale or lost (as the open found
    /// it; now: `Store::frozen`).
    pub frozen: bool,
    /// Why a writable open was made read-only, if it was.
    pub read_only_reason: Option<&'static str>,
    /// Metadata pairs read a second time because a slot read failed
    /// (each failed read is retried once).
    pub retried_reads: u32,
    /// Table entries whose kind or name do not decode, as `(page, seq)`:
    /// unavailable, their spans reserved (the regions with a readable name
    /// are in `StoreReport::unavailable`).
    pub invalid_entries: Vec<(u32, u32)>,
    /// Extents a region header lists past its entry's count (a growth
    /// beyond capacity cut short): not part of the region; allocated while
    /// the region's page is stale.
    pub extra_extents: Vec<(RegionId, (u64, u64))>,
    /// Regions that may read another region's bytes (spec §9 step 4): an
    /// unavailable region with a newer footprint overlaps them. Read-only.
    pub superseded: Vec<RegionId>,
    /// Regions an unavailable region's span overlaps (those bytes stay
    /// reserved).
    pub shares_reserved: Vec<RegionId>,
    /// For each stale page, whether every region header on it agrees with
    /// its entry. Agreement does not prove nothing was lost (spec §9): a
    /// growth within capacity changes only the entry, and a lost release
    /// can leave a Ready header over reused space.
    pub stale_page_agreement: Vec<(u32, bool)>,
    /// Regions whose header pair shows a newer, unreadable version than the
    /// one the open uses (spec §8): a recycle, slot commit or growth's
    /// header possibly lost to a crash. The older version is served.
    pub lost_header_writes: Vec<RegionId>,
    /// A list above was cut at its limit (256 items).
    pub truncated: bool,
}

/// Which metadata write a format-2 open healed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Healed {
    /// The volume record.
    VolumeRecord,
    /// Table page `p`.
    Page(u32),
}

impl core::fmt::Display for MetadataReport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let yes = |b: bool| if b { "yes" } else { "no" };
        writeln!(f, "clean close      {}", yes(self.clean_close))?;
        if self.volume_stale {
            writeln!(f, "volume record    stale")?;
        }
        if !self.stale_pages.is_empty() {
            writeln!(f, "stale pages      {:?}", self.stale_pages)?;
        }
        if !self.lost_pages.is_empty() {
            writeln!(f, "lost pages       {:?}", self.lost_pages)?;
        }
        if let Some(h) = self.healed {
            writeln!(f, "healed           {h:?}")?;
        }
        if self.frozen {
            writeln!(f, "allocation       frozen")?;
        }
        if let Some(r) = self.read_only_reason {
            writeln!(f, "read-only        {r}")?;
        }
        if self.retried_reads > 0 {
            writeln!(f, "retried reads    {}", self.retried_reads)?;
        }
        if !self.invalid_entries.is_empty() {
            writeln!(f, "invalid entries  {:?}", self.invalid_entries)?;
        }
        for (id, (off, len)) in &self.extra_extents {
            writeln!(f, "extra extent     region {id}: {len} bytes at {off}")?;
        }
        if !self.superseded.is_empty() {
            writeln!(f, "superseded       {:?}", self.superseded)?;
        }
        if !self.shares_reserved.is_empty() {
            writeln!(f, "shares reserved  {:?}", self.shares_reserved)?;
        }
        for (p, agrees) in &self.stale_page_agreement {
            let word = if *agrees { "agree" } else { "disagree" };
            writeln!(f, "stale page {p}     headers {word} with entries")?;
        }
        if !self.lost_header_writes.is_empty() {
            writeln!(f, "lost header      {:?}", self.lost_header_writes)?;
        }
        if self.truncated {
            writeln!(f, "(lists cut at 256 items)")?;
        }
        Ok(())
    }
}

/// Object id of table page `p`.
pub(crate) const fn page_object(p: u32) -> u64 {
    (1u64 << 61) | p as u64
}

/// Where page `p`'s pair lives.
pub(crate) fn pair_page(volume: VolumeId, l: &Layout, p: u32) -> PairAt {
    let (a, b) = l.page_slots(p);
    PairAt {
        volume: *volume.as_bytes(),
        object: page_object(p),
        a,
        b,
        log2: l.log2_table,
        format: FORMAT_2,
        numbered: true,
    }
}

/// One table page as the open found it.
#[derive(Debug, Clone)]
pub(crate) struct Page2 {
    pub(crate) pair: PairRead,
    pub(crate) standing: Standing,
    /// The winner's page header (none when Lost).
    pub(crate) header: Option<PageHeader>,
    /// The winner's entries, unchecked (none yet in E2a).
    pub(crate) entries: Vec<RawEntry>,
}

/// Format 2's metadata state.
#[derive(Debug, Clone)]
pub(crate) struct Meta2 {
    pub(crate) volume: VolumeMeta2,
    pub(crate) pages: Vec<Page2>,
    /// Free space past the table area, rebuilt as a complement on every
    /// change (spec §9 step 6).
    pub(crate) free: crate::footprint::FreeMap,
    /// Per-tag usage `(tag, logical, physical)` found at open.
    pub(crate) usage: Vec<(u32, u64, u64)>,
    /// Names of entries that failed the entry checks with a kind this
    /// version does not know (not in `StoreReport::unavailable`): reserved
    /// all the same.
    pub(crate) reserved_names: Vec<store_io_format::meta::RegionName>,
    /// An entry names space past the file's end: growth is refused (it
    /// would make zeros of the lost tail).
    pub(crate) cut_short: bool,
    /// Every allocated range: free space is rebuilt from it (never by
    /// adding a range back).
    pub(crate) allocated: Vec<(u64, u64)>,
    /// Provisionings between their placement and their commit: the name
    /// and the table slot each holds (v0.5 E4 C13).
    pub(crate) claims: Vec<Claim>,
    /// Bytes marked allocated by provisionings and growths not yet
    /// committed (in `allocated`, not yet in `usage`).
    pub(crate) marked: u64,
    /// What each unavailable entry (and each of a kind this version does
    /// not know) holds: exactly what the open added for it, so
    /// `repair_unavailable` takes away no more and no less (v0.5 E6).
    pub(crate) holds: Vec<Hold>,
    /// The extra extents counted allocated because their page is stale
    /// (spec §9 step 3): `(page, region id, range)`, freed when the page is
    /// accepted (and copied with its region by `copy_store` when it
    /// overlaps nothing else).
    pub(crate) stale_extra: Vec<(usize, u64, (u64, u64))>,
}

/// How far the open raises a page's `next_seq`, in memory, on every stale
/// page and every page whose pair may carry an accepted rollback
/// ([`PairRead::possibly_accepted`]): an
/// accepted page's lost commit may have handed out seqs past its winner's
/// `next_seq` (a provisioning's claim takes one past every claim held), and
/// the region ids it gave out are never handed out again unless more than
/// this many provisionings failed on that page between its last two
/// commits. The page's next commit writes the raised value.
pub(crate) const SEQ_MARGIN: u32 = 1 << 12;

impl Page2 {
    /// Raises the page's `next_seq` past what a lost commit may have given
    /// out (see [`SEQ_MARGIN`]).
    pub(crate) fn raise_next_seq(&mut self) {
        if let Some(h) = self.header.as_mut() {
            h.next_seq = h.next_seq.saturating_add(SEQ_MARGIN);
        }
    }

    /// The `meta_seq` of the page's winning copy (0 when none).
    pub(crate) fn meta_seq(&self) -> u64 {
        self.pair
            .report
            .winner_info()
            .and_then(|(i, _)| self.pair.facts[usize::from(i.as_u8())])
            .and_then(|f| f.numbered)
            .map_or(0, |n| n.meta_seq)
    }

    /// Entries whose `footprint_seq` is past the page's own write (only
    /// damage writes one): the open counts them newest of all (spec §9
    /// step 4). A rewrite of the page at a later `meta_seq` would count
    /// them otherwise, changing the verdicts that rest on them, so a page
    /// holding one is written only to remove it.
    pub(crate) fn ahead(&self) -> impl Iterator<Item = (u32, u64)> + '_ {
        let ms = self.meta_seq();
        self.entries
            .iter()
            .filter(move |e| e.footprint_seq > ms)
            .map(|e| (e.seq, e.footprint_seq))
    }
}

/// What an unavailable table entry holds (see [`Meta2::holds`]).
#[derive(Debug, Clone)]
pub(crate) struct Hold {
    pub(crate) page: usize,
    pub(crate) seq: u32,
    /// Its range in `allocated` (its reserved span, as the open found it),
    /// if it has one.
    pub(crate) span: Option<(u64, u64)>,
    /// Its usage in `usage` (an entry that passed the entry checks).
    pub(crate) usage: Option<(u32, u64, u64)>,
    /// Its name in `reserved_names` (an entry of an unknown kind).
    pub(crate) name: Option<store_io_format::meta::RegionName>,
    /// It names space past the file's end (`cut_short`).
    pub(crate) short: bool,
    /// Its `footprint_seq` as settled (spec §9 step 4) and what its writes
    /// may have touched (its header blocks and extents when its header
    /// decoded, else its span; none for a region demoted by an overlap).
    pub(crate) footprint_seq: u64,
    pub(crate) own: Vec<(u64, u64)>,
    /// A region that passed step 3 and was demoted in step 4 (by an
    /// overlap or a name): its name and footprint, for judging it again.
    pub(crate) demoted: Option<(store_io_format::meta::RegionName, Vec<(u64, u64)>)>,
    /// Its entry's name, when that decodes (it counts for names in step 4).
    pub(crate) entry_name: Option<store_io_format::meta::RegionName>,
}

impl Hold {
    /// The region id of the entry (`page << 32 | seq`).
    pub(crate) fn id(&self) -> u64 {
        ((self.page as u64) << 32) | u64::from(self.seq)
    }
}

/// A provisioning's hold, from its placement to its commit or its undo:
/// no other provisioning takes its name or its table slot meanwhile.
#[derive(Debug, Clone)]
pub(crate) struct Claim {
    pub(crate) page: usize,
    pub(crate) seq: u32,
    pub(crate) name: store_io_format::meta::RegionName,
}

impl Meta2 {
    /// Marks `ranges` allocated for a provisioning or a growth that fills
    /// them outside the metadata lock (spec §12: "choose and mark"): no
    /// other placement takes them; free space is the complement again in a
    /// container of `len` bytes.
    pub(crate) fn mark(&mut self, ranges: &[(u64, u64)], len: u64) {
        self.allocated.extend_from_slice(ranges);
        self.marked += ranges.iter().map(|r| r.1).sum::<u64>();
        self.rebuild_free(len);
    }

    /// Undoes [`Self::mark`] of exactly `ranges` (an operation that failed
    /// before its commit).
    pub(crate) fn unmark(&mut self, ranges: &[(u64, u64)], len: u64) {
        for r in ranges {
            if let Some(i) = self.allocated.iter().position(|a| a == r) {
                let _gone = self.allocated.swap_remove(i);
            }
        }
        let bytes = ranges.iter().map(|r| r.1).sum::<u64>();
        debug_assert!(self.marked >= bytes, "unmarked more than was marked");
        self.marked = self.marked.saturating_sub(bytes);
        self.rebuild_free(len);
    }

    /// A marked operation committed: its `bytes` are now a footprint (they
    /// stay in `allocated`).
    pub(crate) fn settle(&mut self, bytes: u64) {
        debug_assert!(self.marked >= bytes, "settled more than was marked");
        self.marked = self.marked.saturating_sub(bytes);
    }

    /// Free space again, as the complement of `allocated` in a container
    /// of `len` bytes.
    pub(crate) fn rebuild_free(&mut self, len: u64) {
        let mut all = self.allocated.clone();
        self.free = crate::footprint::FreeMap::complement(self.volume.table_end(), len, &mut all);
    }
}

/// The next `meta_seq` of the store's metadata (overflow refused).
pub(crate) fn next_meta_seq_of(meta: &Meta) -> Result<u64, Error> {
    let v2 = meta.v2.as_deref().ok_or_else(corrupt)?;
    next_meta_seq(&meta.volume_pair, &v2.pages, None)
}

/// Writes table page `p`'s next copy, `payload` at `meta_seq` (chosen by
/// the caller first, so the entries it carries can name it), durably; only
/// a Confirmed page is ever written (spec §9 step 2). The page's state in
/// `meta` follows once the write is durable.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_page<P: Platform>(
    q: &mut Lane<P::Queue>,
    file: &P::File,
    pool: &store_io_buf::BufPool,
    layout: &Layout,
    volume: VolumeId,
    policy: &Policy,
    domain: &Domain,
    meta: &mut Meta,
    p: usize,
    payload: &[u8],
    meta_seq: u64,
    owner_generation: u64,
) -> Result<(), Error> {
    let v2 = meta.v2.as_deref_mut().ok_or_else(corrupt)?;
    let page = v2.pages.get_mut(p).ok_or_else(corrupt)?;
    if page.standing != Standing::Confirmed {
        return Err(Error::NotWritten {
            cause: store_io_core::error::NotWrittenCause::Frozen,
            ctx: ErrorContext::default(),
        });
    }
    let pi = u32::try_from(p).map_err(|_| corrupt())?;
    let decoded = decode_page(payload, pi, entries_per_page(layout.log2_table))
        .map(|d| (d.header, d.entries().collect::<Vec<_>>()))
        .map_err(|_| corrupt())?;
    // An entry past the page's write, kept as it is, would read as an older
    // one after this write (see `Page2::ahead`): refused before any I/O.
    let ahead: Vec<(u32, u64)> = page.ahead().collect();
    if let Some(kept) = decoded
        .1
        .iter()
        .find(|e| ahead.contains(&(e.seq, e.footprint_seq)))
    {
        // Named: the entry to repair first.
        return Err(Error::NotWritten {
            cause: store_io_core::error::NotWrittenCause::Frozen,
            ctx: ErrorContext {
                region: Some(store_io_core::id::RegionId::from_raw(
                    (u64::from(pi) << 32) | u64::from(kept.seq),
                )),
                ..ErrorContext::default()
            },
        });
    }
    let mut next = plan_next(&page.pair, payload, owner_generation)?;
    next.meta_seq = meta_seq;
    let at = pair_page(volume, layout, pi);
    page.pair = durable_slot::<P>(q, file, pool, &at, &page.pair, &next, policy, domain)?;
    page.header = Some(decoded.0);
    page.entries = decoded.1;
    Ok(())
}

/// The `meta_seq` a slot of `pair` counts for (`Valid`, or `BadPayloadCrc`
/// with a numbered header).
fn numbered(pair: &PairRead, slot: SlotIndex) -> Option<u64> {
    let i = usize::from(slot.as_u8());
    let status = if slot == SlotIndex::A {
        pair.report.a
    } else {
        pair.report.b
    };
    match status {
        SlotStatus::Valid(info) => Some(info.meta_seq),
        SlotStatus::BadPayloadCrc => pair.facts[i].and_then(|f| f.numbered).map(|n| n.meta_seq),
        _ => None,
    }
}

/// One above every numbered slot of the volume record and the pages, except
/// the slot `over` (pair 0 the volume record, `p + 1` page `p`) the write
/// replaces (spec §7).
fn next_meta_seq(
    volume: &PairRead,
    pages: &[Page2],
    over: Option<(usize, SlotIndex)>,
) -> Result<u64, Error> {
    let pairs = core::iter::once(volume).chain(pages.iter().map(|p| &p.pair));
    let mut max = 0u64;
    for (i, pair) in pairs.enumerate() {
        for slot in [SlotIndex::A, SlotIndex::B] {
            if over == Some((i, slot)) {
                continue;
            }
            if let Some(v) = numbered(pair, slot) {
                max = max.max(v);
            }
        }
    }
    max.checked_add(1).ok_or(Error::Unsupported {
        what: Capability::FormatVersion,
    })
}

fn page_payload(p: u32) -> Result<Vec<u8>, Error> {
    let mut buf = vec![0u8; store_io_format::meta2::PAGE_HEADER_LEN];
    let n = encode_page(
        &mut buf,
        &PageHeader {
            page_index: p,
            next_seq: 0,
            count: 0,
        },
        &[],
    )
    .map_err(|_| meta_err())?;
    buf.truncate(n);
    Ok(buf)
}

fn volume_payload(vm: &VolumeMeta2) -> Result<[u8; VOLUME_META2_LEN], Error> {
    let mut buf = [0u8; VOLUME_META2_LEN];
    let _n = vm.encode(&mut buf).map_err(|_| meta_err())?;
    Ok(buf)
}

/// The format-2 layout for a container with blocks of `1 << log2_block`,
/// table pages of `1 << log2_table` and room for `capacity` regions.
pub(crate) fn layout_for(log2_block: u8, log2_table: u8, capacity: u32) -> Layout {
    let e = u32::from(entries_per_page(log2_table)).max(1);
    let pages = capacity
        .min(MAX_REGION_CAPACITY)
        .div_ceil(e)
        .clamp(1, store_io_format::meta2::MAX_TABLE_PAGES - 1);
    Layout {
        log2_block,
        log2_table,
        format: FORMAT_2,
        pages,
    }
}

/// Writes a fresh format-2 container's metadata (spec §12, create): the
/// file sized to the end of page 0's slot A (a one-page table: the whole
/// table), page 0's slot A (generation 1, `meta_seq` 1, empty), for P > 1
/// a full flush, the file sized for the whole table and every other page's
/// slot A (`meta_seq` 2..P), then a full flush, the volume record's slot A (`meta_seq`
/// P + 1), a full flush. The caller flushes the directory. Growing the file
/// only once page 0 is durable means a crash never leaves a file past
/// format 1's size bound without a page a later create recognises.
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_metadata<P: Platform>(
    platform: &P,
    file: &P::File,
    q: &mut Lane<P::Queue>,
    pool: &store_io_buf::BufPool,
    layout: &Layout,
    volume: VolumeId,
    created_unix_s: u64,
) -> Result<Meta, Error> {
    let allocate = |len: u64| {
        platform
            .allocate(file, len)
            .map_err(|e| space_err(Op::Allocate, e, 0))
    };
    // Page 0's slot A; a one-page table is within format 1's bound whole.
    allocate(if layout.pages == 1 {
        layout.data_start()
    } else {
        layout.page_slots(0).1
    })?;
    let mut pages = Vec::with_capacity(layout.pages as usize);
    for p in 0..layout.pages {
        if p == 1 {
            full_flush(platform, file)?;
            allocate(layout.data_start())?;
        }
        let payload = page_payload(p)?;
        let mut next = plan_next(&PairRead::blank(), &payload, 1)?;
        next.meta_seq = u64::from(p) + 1;
        let w = write_slot(q, file, pool, &pair_page(volume, layout, p), &next, false)?;
        pages.push(Page2 {
            pair: PairRead::blank().after(w),
            standing: Standing::Confirmed,
            header: Some(PageHeader {
                page_index: p,
                next_seq: 0,
                count: 0,
            }),
            entries: Vec::new(),
        });
    }
    full_flush(platform, file)?;
    let vm = VolumeMeta2 {
        base: VolumeMeta {
            log2_block_size: layout.log2_block,
            log2_table_slot_size: layout.log2_table,
            device_index: 0,
            owner_generation: 1,
            table_offset: 2 * layout.block(),
            created_unix_s,
            features: 0,
        },
        table_pages: layout.pages,
        entries_per_page: entries_per_page(layout.log2_table),
        clean_close: false,
        flags: 0,
    };
    let vbuf = volume_payload(&vm)?;
    let mut next = plan_next(&PairRead::blank(), &vbuf, 1)?;
    next.meta_seq = u64::from(layout.pages) + 1;
    let w = write_slot(q, file, pool, &pair_volume(volume, layout), &next, false)?;
    full_flush(platform, file)?;
    Ok(Meta {
        volume: vm.base,
        volume_pair: PairRead::blank().after(w),
        table_pair: PairRead::blank(),
        next_region_id: 0,
        entries: Vec::new(),
        container_len: layout.data_start(),
        v2: Some(Box::new(Meta2 {
            volume: vm,
            pages,
            free: crate::footprint::FreeMap::complement(
                layout.data_start(),
                layout.data_start(),
                &mut [],
            ),
            usage: Vec::new(),
            reserved_names: Vec::new(),
            cut_short: false,
            allocated: Vec::new(),
            claims: Vec::new(),
            marked: 0,
            holds: Vec::new(),
            stale_extra: Vec::new(),
        })),
        unavailable: Vec::new(),
        repairs: Vec::new(),
    })
}

fn full_flush<P: Platform>(platform: &P, file: &P::File) -> Result<(), Error> {
    platform
        .flush_all(file)
        .map_err(|e| Error::DurabilityUnknown {
            op: Op::FlushAll,
            raw: Some(e),
            ctx: ErrorContext::default(),
        })
}

/// What a format-2 open decided.
pub(crate) struct Opened {
    pub(crate) meta: Meta,
    pub(crate) owner_generation: u64,
    pub(crate) read_only: bool,
    pub(crate) report: MetadataReport,
    pub(crate) regions: Vec<std::sync::Arc<LiveRegion>>,
    pub(crate) unavailable: Vec<UnavailableRegion>,
}

/// Decodes a format-2 volume record: a longer record, or a `features` or
/// `flags` bit this version does not know, is a newer format (refused);
/// anything else that fails is corruption.
pub(crate) fn decode_volume(bytes: &[u8]) -> Result<VolumeMeta2, Error> {
    if bytes.len() > VOLUME_META2_LEN {
        // A longer record is a newer layout (spec §14).
        return Err(Error::Unsupported {
            what: Capability::FormatVersion,
        });
    }
    VolumeMeta2::decode(bytes).map_err(|e| match e {
        store_io_format::meta::MetaError::Field("features" | "flags") => Error::Unsupported {
            what: Capability::FormatVersion,
        },
        _ => corrupt(),
    })
}

/// Reads a metadata pair, each slot whose read failed again once (spec
/// §7), counting the retries.
pub(crate) fn read_retried<Q: store_io_platform::Queue>(
    q: &mut Lane<Q>,
    file: &Q::File,
    pool: &store_io_buf::BufPool,
    at: &PairAt,
    retried: &mut u32,
) -> Result<PairRead, Error> {
    let first = read_pair(q, file, pool, at)?;
    let (pair, again) = reread_failed(q, file, pool, at, first)?;
    *retried += u32::from(again);
    Ok(pair)
}

/// As [`read_retried`] for every pair in `ats`, many reads in flight at
/// once (the re-reads, rare, one at a time); in `ats`'s order.
pub(crate) fn read_all_retried<Q: store_io_platform::Queue>(
    q: &mut Lane<Q>,
    file: &Q::File,
    pool: &store_io_buf::BufPool,
    ats: &[PairAt],
    retried: &mut u32,
) -> Result<Vec<PairRead>, Error> {
    let first = read_pairs(q, file, pool, ats, usize::MAX)?;
    let mut out = Vec::with_capacity(first.len());
    for (at, pair) in ats.iter().zip(first) {
        let (pair, again) = reread_failed(q, file, pool, at, pair)?;
        *retried += u32::from(again);
        out.push(pair);
    }
    Ok(out)
}

fn judge_read(p: &PairRead) -> judge::PairRead {
    judge::PairRead {
        a: judge::SlotRead {
            status: p.report.a,
            facts: p.facts[0],
        },
        b: judge::SlotRead {
            status: p.report.b,
            facts: p.facts[1],
        },
    }
}

/// The format-2 open after the volume record was found (spec §9): reads
/// every page, judges the metadata, checks every region (`open2`), and only
/// then, when writable, heals the one torn write and writes the volume
/// record with the next owner generation: a read-only verdict from the
/// region checks comes before anything is written.
#[allow(clippy::too_many_arguments)]
pub(crate) fn open_metadata<P: Platform>(
    file: &P::File,
    q: &mut Lane<P::Queue>,
    pool: &store_io_buf::BufPool,
    layout: &Layout,
    volume: VolumeId,
    mut vm: VolumeMeta2,
    mut volume_pair: PairRead,
    container_len: u64,
    read_only: bool,
    policy: &Policy,
    append_window: u64,
    header_queue: &mut dyn FnMut() -> Option<(P::Queue, usize)>,
) -> Result<Opened, Error> {
    if layout.data_start() > container_len {
        return Err(corrupt());
    }
    let mut retried = 0u32;
    let (again, reread) = reread_failed(q, file, pool, &pair_volume(volume, layout), volume_pair)?;
    volume_pair = again;
    if reread {
        // A slot read again may be the newer winner: the record is read from
        // it then, and must describe the same table.
        retried += 1;
        // A newer copy read the second time is refused, not corrupt.
        vm = decode_volume(&crate::open::winner_payload(&volume_pair)?)?;
        if vm.table_pages != layout.pages
            || vm.base.log2_block_size != layout.log2_block
            || vm.base.log2_table_slot_size != layout.log2_table
        {
            return Err(corrupt());
        }
    }
    let mut pages = Vec::with_capacity(layout.pages as usize);
    for p in 0..layout.pages {
        let at = pair_page(volume, layout, p);
        let pair = read_retried(q, file, pool, &at, &mut retried)?;
        pages.push(Page2 {
            pair,
            standing: Standing::Lost,
            header: None,
            entries: Vec::new(),
        });
    }
    let reads: Vec<judge::PairRead> = core::iter::once(&volume_pair)
        .chain(pages.iter().map(|p| &p.pair))
        .map(judge_read)
        .collect();
    let mut verdicts = vec![
        PairVerdict {
            standing: Standing::Lost,
            winner: None,
            a: SlotVerdict::Damage,
            b: SlotVerdict::Damage,
        };
        reads.len()
    ];
    let j = judge::judge(&reads, vm.clean_close, &mut verdicts).map_err(|_| corrupt())?;
    if j.refused {
        return Err(Error::Unsupported {
            what: Capability::FormatVersion,
        });
    }
    let mut report = MetadataReport {
        clean_close: j.clean,
        volume_stale: verdicts[0].standing != Standing::Confirmed,
        retried_reads: retried,
        ..MetadataReport::default()
    };
    let mut decode_lost = false;
    for (i, page) in pages.iter_mut().enumerate() {
        let p = i as u32;
        let mut standing = verdicts[i + 1].standing;
        if standing != Standing::Lost {
            match page.pair.payload.as_deref().map(|b| {
                decode_page(b, p, entries_per_page(layout.log2_table))
                    .map(|d| (d.header, d.entries().collect::<Vec<_>>()))
            }) {
                Some(Ok((h, entries))) => {
                    page.header = Some(h);
                    page.entries = entries;
                    // A stale page (it may be accepted: its newer copy may
                    // even read Blank), or one possibly accepted already:
                    // ids its lost commit gave out stay given (see
                    // `SEQ_MARGIN`).
                    if standing == Standing::Unconfirmed || page.pair.possibly_accepted() {
                        page.raise_next_seq();
                    }
                }
                _ => {
                    standing = Standing::Lost;
                    decode_lost = true;
                }
            }
        }
        page.standing = standing;
    }
    // A page that fails the page checks is Lost, which the judgement could
    // not see: a page's torn write is healed only when nothing else is
    // lost (spec §7), so the heal is dropped and the torn copy is damage.
    // Likewise for a page holding an entry past its own write: the heal's
    // later `meta_seq` would change how the next open counts that entry
    // (see `Page2::ahead`), so the page is left stale, never written.
    let mut to_heal = j.heal;
    if let Some((pair, _)) = to_heal {
        if pair > 0 && (decode_lost || pages[pair - 1].ahead().next().is_some()) {
            to_heal = None;
            if pages[pair - 1].standing == Standing::Confirmed {
                pages[pair - 1].standing = Standing::Unconfirmed;
            }
        }
    }
    for (i, page) in pages.iter().enumerate() {
        match page.standing {
            Standing::Unconfirmed => report.stale_pages.push(i as u32),
            Standing::Lost | Standing::Refused => report.lost_pages.push(i as u32),
            Standing::Confirmed => {}
        }
    }
    report.frozen = !report.stale_pages.is_empty() || !report.lost_pages.is_empty();
    // A winner with a read-only-compatible feature this version does not
    // know may be read but never rewritten (spec §14).
    let foreign = core::iter::once(&volume_pair)
        .chain(pages.iter().map(|p| &p.pair))
        .any(|p| p.report.winner_info().is_some_and(|(_, i)| !i.writable()));
    let mut meta = Meta {
        volume: vm.base,
        volume_pair,
        table_pair: PairRead::blank(),
        next_region_id: 0,
        entries: Vec::new(),
        container_len,
        v2: Some(Box::new(Meta2 {
            volume: vm,
            pages,
            free: crate::footprint::FreeMap::default(),
            usage: Vec::new(),
            reserved_names: Vec::new(),
            cut_short: false,
            allocated: Vec::new(),
            claims: Vec::new(),
            marked: 0,
            holds: Vec::new(),
            stale_extra: Vec::new(),
        })),
        unavailable: Vec::new(),
        repairs: Vec::new(),
    };
    // Steps 3-6, before anything is written.
    let regions = {
        let v2 = meta.v2.as_deref().ok_or_else(corrupt)?;
        crate::open2::check_regions(
            q,
            file,
            pool,
            layout,
            volume,
            v2,
            container_len,
            append_window,
            &mut report,
            header_queue,
        )?
    };
    if let Some(v2) = meta.v2.as_deref_mut() {
        v2.free = regions.free;
        v2.usage = regions.usage;
        v2.reserved_names = regions.reserved_names;
        v2.cut_short = regions.cut_short;
        v2.allocated = regions.allocated;
        v2.holds = regions.holds;
        v2.stale_extra = regions.stale_extra;
    }
    let read_only = read_only || {
        report.read_only_reason = if !j.writable {
            Some("the volume record is damaged and a table page holds a damaged copy")
        } else if foreign {
            Some("the metadata carries a feature this version may read but not write")
        } else {
            regions.read_only_reason
        };
        report.read_only_reason.is_some()
    };
    if read_only {
        return Ok(Opened {
            meta,
            owner_generation: 0,
            read_only,
            report,
            regions: regions.live,
            unavailable: regions.unavailable,
        });
    }
    let og = vm
        .base
        .owner_generation
        .checked_add(1)
        .ok_or_else(corrupt)?;
    let domain = Domain::new();
    if let Some((pair, slot)) = to_heal {
        heal::<P>(
            q, file, pool, layout, volume, policy, &domain, &mut meta, pair, slot, og,
        )?;
        report.healed = Some(if pair == 0 {
            Healed::VolumeRecord
        } else {
            Healed::Page(pair as u32 - 1)
        });
    }
    write_volume::<P>(
        q, file, pool, layout, volume, policy, &domain, &mut meta, og, false,
    )?;
    Ok(Opened {
        meta,
        owner_generation: og,
        read_only,
        report,
        regions: regions.live,
        unavailable: regions.unavailable,
    })
}

/// Rewrites the one torn metadata write (`pair` 0 the volume record, `p + 1`
/// page `p`; `slot` its failed slot) with the winner's payload byte for
/// byte, at the torn copy's own `meta_seq` when it was readable (spec §7).
#[allow(clippy::too_many_arguments)]
fn heal<P: Platform>(
    q: &mut Lane<P::Queue>,
    file: &P::File,
    pool: &store_io_buf::BufPool,
    layout: &Layout,
    volume: VolumeId,
    policy: &Policy,
    domain: &Domain,
    meta: &mut Meta,
    pair: usize,
    slot: SlotIndex,
    og: u64,
) -> Result<(), Error> {
    let v2 = meta.v2.as_mut().ok_or_else(corrupt)?;
    let (at, prev) = if pair == 0 {
        (pair_volume(volume, layout), &meta.volume_pair)
    } else {
        let p = u32::try_from(pair - 1).map_err(|_| corrupt())?;
        (pair_page(volume, layout, p), &v2.pages[pair - 1].pair)
    };
    let payload = prev.payload.clone().ok_or_else(corrupt)?;
    let mut next = plan_next(prev, &payload, og)?;
    if next.index != slot {
        return Err(corrupt());
    }
    let own = prev.facts[usize::from(slot.as_u8())]
        .and_then(|f| f.numbered)
        .map(|n| n.meta_seq);
    next.meta_seq = match own {
        Some(m) => m,
        None => next_meta_seq(&meta.volume_pair, &v2.pages, Some((pair, slot)))?,
    };
    let healed = durable_slot::<P>(q, file, pool, &at, prev, &next, policy, domain)?;
    if pair == 0 {
        meta.volume_pair = healed;
    } else {
        v2.pages[pair - 1].pair = healed;
        v2.pages[pair - 1].standing = Standing::Confirmed;
    }
    Ok(())
}

/// The bytes of page `p`'s two slots (an error naming the page).
pub(crate) fn page_range(layout: &Layout, p: u32) -> store_io_core::error::ByteRange {
    let (a, b) = layout.page_slots(p);
    store_io_core::error::ByteRange {
        start: a.min(b),
        end: a.max(b) + (1u64 << layout.log2_table),
    }
}

/// Accepts stale page `p` (spec §12 `accept_page_rollback`): its winner
/// rewritten byte for byte into its failed slot at the next `meta_seq`,
/// durably, then the pair read back (retried once, §7): both slots must be
/// Valid with the same payload (the twin the next open will see, which
/// then counts the page Confirmed). On success the page is Confirmed (its
/// `next_seq` was raised by the open that found it stale, see
/// [`SEQ_MARGIN`]). Any failure after the write
/// poisons the store: the new copy is on the media, at a `meta_seq` this
/// state cannot count (its slot may read at the next open), so only the
/// next open knows the page.
#[allow(clippy::too_many_arguments)]
pub(crate) fn accept_page<P: Platform>(
    q: &mut Lane<P::Queue>,
    file: &P::File,
    pool: &store_io_buf::BufPool,
    layout: &Layout,
    volume: VolumeId,
    policy: &Policy,
    domain: &Domain,
    meta: &mut Meta,
    p: usize,
    og: u64,
) -> Result<(), Error> {
    let meta_seq = next_meta_seq_of(meta)?;
    let v2 = meta.v2.as_mut().ok_or_else(corrupt)?;
    let page = v2.pages.get_mut(p).ok_or_else(corrupt)?;
    let at = pair_page(volume, layout, u32::try_from(p).map_err(|_| corrupt())?);
    let payload = page.pair.payload.clone().ok_or_else(corrupt)?;
    let mut next = plan_next(&page.pair, &payload, og)?;
    next.meta_seq = meta_seq;
    let _written = durable_slot::<P>(q, file, pool, &at, &page.pair, &next, policy, domain)?;
    let mut retried = 0;
    let back = read_retried(q, file, pool, &at, &mut retried).and_then(|back| {
        if back.twin {
            Ok(back)
        } else {
            Err(Error::Corruption {
                kind: store_io_core::error::CorruptionKind::MediaError,
                ctx: ErrorContext {
                    range: Some(page_range(layout, u32::try_from(p).map_err(|_| corrupt())?)),
                    ..ErrorContext::default()
                },
            })
        }
    });
    match back {
        Ok(back) => {
            page.pair = back;
            page.standing = Standing::Confirmed;
            Ok(())
        }
        Err(e) => {
            domain.poison(store_io_core::error::FirstCause {
                op: Some(store_io_core::error::Op::Read),
                raw: None,
            });
            Err(e)
        }
    }
}

/// Writes the volume record's next version: `owner_generation` and
/// `clean_close` as given, the next `meta_seq`, durably.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_volume<P: Platform>(
    q: &mut Lane<P::Queue>,
    file: &P::File,
    pool: &store_io_buf::BufPool,
    layout: &Layout,
    volume: VolumeId,
    policy: &Policy,
    domain: &Domain,
    meta: &mut Meta,
    owner_generation: u64,
    clean_close: bool,
) -> Result<(), Error> {
    let v2 = meta.v2.as_mut().ok_or_else(corrupt)?;
    let mut vm = v2.volume;
    vm.base.owner_generation = owner_generation;
    vm.clean_close = clean_close;
    let vbuf = volume_payload(&vm)?;
    let mut next = plan_next(&meta.volume_pair, &vbuf, owner_generation)?;
    next.meta_seq = next_meta_seq(&meta.volume_pair, &v2.pages, Some((0, next.index)))?;
    let at = pair_volume(volume, layout);
    meta.volume_pair =
        durable_slot::<P>(q, file, pool, &at, &meta.volume_pair, &next, policy, domain)?;
    v2.volume = vm;
    meta.volume = vm.base;
    Ok(())
}

impl<P: Platform> Drop for Inner<P> {
    /// Format 2's clean close: once every handle is gone (so every write has
    /// completed), a writable, unpoisoned store records `clean_close` in a
    /// final durable volume-record write. Not after a panic (the in-memory
    /// metadata may be half-changed). Best effort: a failure leaves the
    /// next open to judge the store as not closed cleanly, which is safe.
    fn drop(&mut self) {
        if self.read_only
            || self.domain.poisoned().is_some()
            || self.meta.is_poisoned()
            || std::thread::panicking()
        {
            return;
        }
        let mut meta = lock_meta(&self.meta);
        if meta.v2.is_none() {
            return;
        }
        let mut q = self.queues.get(0);
        let _closed = write_volume::<P>(
            &mut q,
            &self.file,
            &self.pool,
            &self.layout,
            self.volume,
            &self.policy,
            &self.domain,
            &mut meta,
            self.owner_generation,
            true,
        );
    }
}
