//! Provisioning a region in container format 2 (spec §10, §12), in three
//! phases (v0.5 E4 C13): under the metadata lock, its name and a table slot
//! claimed, its space placed by first fit over the free space (else past
//! everything allocated) and marked allocated, its tag charged (or the
//! bytes taken from its reservation, which keeps counting them reserved
//! until the commit); without the lock, filled, flushed and verified as in
//! format 1 and its header written in slot A; under the lock again,
//! committed by one table-page write carrying its entry (the page as it is
//! then), whose `footprint_seq` is that write's own `meta_seq`, and the
//! region made live in the same hold of the lock. Other provisionings,
//! growths, releases, reservations and space reports run during the fill.
//! A failure (or a panic) before the commit gives every hold back
//! ([`Holds`]).
//!
//! A capped region (`RegionOptions::max_size`, v0.5 E4 C10) is one extent
//! of its cap, flagged `FLAG_CAPPED`, charged whole: its data filled and
//! verified as any region's; the rest (the slack) allocated without being
//! written, or written too where the system cannot allocate it unwritten or
//! pays for it later (Windows, macOS: [`Store::writes_slack`]).

use std::sync::{Arc, Mutex, PoisonError};

use store_io_core::error::{Error, ErrorContext, Named, NotWrittenCause, ReserveTag};
use store_io_core::id::{Generation, RegionId};
use store_io_format::judge::Standing;
use store_io_format::meta::{FillPattern, RegionKind, RegionName, RegionState};
use store_io_format::meta2::{
    Entry2, FLAG_CAPPED, PAGE_HEADER_LEN, PageHeader, RegionHeader2, encode_page,
    encode_region_header, entries_per_page,
};
use store_io_format::slot::FORMAT_2;
use store_io_platform::Platform;

use crate::frontier::AppendFrontier;
use crate::gate::Gate;
use crate::io::not_written;
use crate::layout::DATA_ALIGN;
use crate::slots::{PairAt, PairRead, plan_next};
use crate::space::{Reservation, lock_acct, no_space};
use crate::store::*;

impl<P: Platform> Store<P>
where
    P::Queue: Send,
{
    /// Provisions a format-2 region (the caller checked read-only and
    /// poison). See [`Store::provision`].
    pub(crate) fn provision2(
        &self,
        name: &str,
        kind: RegionKind,
        opts: crate::RegionOptions,
        from: Option<&mut Reservation<P>>,
        copied: Option<&crate::copy::CopyAttrs>,
        data: Option<&mut crate::provision::Content<'_>>,
    ) -> Result<Arc<LiveRegion>, Error> {
        let inner = &self.inner;
        let data_size = opts.size_bytes();
        // A region `copy_store` recreates keeps its tag, generation, fill
        // pattern and key.
        let tag = copied.map_or_else(|| from.as_ref().map_or(0, |r| r.tag()), |c| c.tag);
        let (fill, fill_key) = match copied {
            Some(c) => (c.fill, c.fill_key),
            None if inner.opts.keyed_fill => (FillPattern::KeyedV1, random_uuid()),
            None => (FillPattern::Zeros, [0; 16]),
        };
        let generation =
            Generation::from_raw(copied.map_or(1, |c| c.generation)).ok_or_else(corrupt)?;
        // Refusals before any I/O (spec §9 step 2, §10).
        if inner.frozen.load(std::sync::atomic::Ordering::Acquire) {
            return Err(not_written(
                NotWrittenCause::Frozen,
                ErrorContext::default(),
            ));
        }
        let too_large = || not_written(NotWrittenCause::TooLarge, ErrorContext::default());
        let rname = RegionName::new(name)
            .map_err(|_| not_written(NotWrittenCause::InvalidName, ErrorContext::default()))?;
        let block = inner.layout.block();
        let slot = kind == RegionKind::Slot;
        let data_size = if slot || (data_size == 0 && copied.is_some()) {
            0
        } else {
            store_io_core::align::align_up(data_size.max(1), block).ok_or_else(too_large)?
        };
        // A capped region: one extent of its cap (at least its size).
        let capped = opts.max_size_bytes().filter(|_| !slot);
        let capacity = match capped {
            Some(cap) => store_io_core::align::align_up(cap, block)
                .ok_or_else(too_large)?
                .max(data_size),
            None if slot => 0,
            None => data_size.max(block),
        };
        if capacity / block >= u64::from(u32::MAX) {
            return Err(too_large());
        }
        let extent = capacity.checked_add(2 * block).ok_or_else(too_large)?;
        let flags = if capped.is_some() { FLAG_CAPPED } else { 0 };
        let mut from = from;
        // Phase 1, under the metadata lock (spec §12, "choose and mark"):
        // the name and a table slot claimed, the space placed and marked,
        // the tag charged or the reservation's bytes held.
        let mut meta = lock_meta(&inner.meta);
        self.name_taken2(&meta, &rname)?;
        let (p, seq) = {
            let v2 = meta.v2.as_deref().ok_or_else(corrupt)?;
            let per_page = entries_per_page(inner.layout.log2_table);
            v2.pages
                .iter()
                .enumerate()
                .find_map(|(p, page)| {
                    let h = page.header?;
                    // Slots other provisionings hold count as taken; a seq
                    // past every one claimed (a seq never committed may be
                    // handed out again, as before: the next fill zeroes any
                    // header a failed one left).
                    let held = v2.claims.iter().filter(|c| c.page == p);
                    let claimed = held.clone().count();
                    let seq = held.map(|c| c.seq + 1).fold(h.next_seq, u32::max);
                    // Not a page holding an entry past its own write (see
                    // `Page2::ahead`): it is written only to remove it.
                    (page.standing == Standing::Confirmed
                        && usize::from(h.count) + claimed < usize::from(per_page)
                        && seq < u32::MAX
                        && page.ahead().next().is_none())
                    .then_some((p, seq))
                })
                .ok_or(Error::NoSpace {
                    tag: ReserveTag::Table,
                    ctx: ErrorContext::default(),
                })?
        };
        {
            let mut acct = lock_acct(&inner.acct);
            match &from {
                Some(r) if extent > r.remaining() => return Err(no_space(tag)),
                None if !acct.fits(tag, extent) => return Err(no_space(tag)),
                _ => {}
            }
        }
        // Placement (spec §10): first fit over the free space, else past
        // everything allocated. A large region's data area is aligned as in
        // format 1; a reservation's growth that finds no room for the
        // alignment places the region unaligned instead (format 1's rule).
        let reserved_after = {
            let acct = lock_acct(&inner.acct);
            match &from {
                Some(_) => acct.reserved().saturating_sub(extent),
                None => acct.reserved(),
            }
        };
        let mut align = if capacity >= DATA_ALIGN {
            DATA_ALIGN
        } else {
            block
        };
        let offset = loop {
            let v2 = meta.v2.as_deref().ok_or_else(corrupt)?;
            let offset = match v2.free.first_fit(extent, align, 2 * block) {
                Some(at) => at,
                None => v2.free.tail(align, 2 * block).ok_or_else(too_large)?,
            };
            let end = offset.checked_add(extent).ok_or_else(too_large)?;
            let target = self
                .tail(&meta)
                .max(end)
                .checked_add(reserved_after)
                .ok_or_else(too_large)?;
            match self.grow_to(&mut meta, target, tag) {
                Ok(()) => break offset,
                Err(Error::NoSpace { .. }) if align > block && from.is_some() => align = block,
                Err(e) => return Err(e),
            }
        };
        let extents: Vec<(u64, u64)> = if slot {
            Vec::new()
        } else {
            vec![(offset + 2 * block, capacity)]
        };
        // As the open records a footprint: the header blocks, then the data
        // extent (release removes exactly these).
        let mut footprint = vec![(offset, block), (offset + block, block)];
        footprint.extend(extents.iter().copied());
        // The holds, all at once (nothing below fails until phase 2). A
        // reservation's bytes stay counted reserved until the commit, so
        // every placement meanwhile still sizes the container past them.
        if let Some(r) = from.as_deref_mut() {
            r.hold(extent)?;
        }
        let container = meta.container_len;
        if let Some(v2) = meta.v2.as_deref_mut() {
            v2.claims.push(crate::v2::Claim {
                page: p,
                seq,
                name: rname,
            });
            v2.mark(&footprint, container);
        }
        let charged = if from.is_none() {
            lock_acct(&inner.acct).add_usage(tag, 0, extent);
            extent
        } else {
            0
        };
        drop(meta);
        let mut holds = Holds {
            store: self,
            ranges: footprint,
            claim: Some((p, seq)),
            tag,
            charged,
            from: from.map(|r| (r, extent)),
            armed: true,
        };
        let placed = Placed {
            kind,
            name: rname,
            tag,
            flags,
            id: ((p as u64) << 32) | u64::from(seq),
            page: p,
            seq,
            offset,
            data_size,
            capacity,
            extent,
            extents: &extents,
            fill_whole: capped.is_none() || self.writes_slack(),
            fill,
            fill_key,
            generation,
        };
        // Any return before the commit drops `holds` armed: every hold
        // given back.
        self.fill_and_commit2(&placed, &mut holds, data)
    }

    /// Phases 2 and 3 of a format-2 provisioning placed and marked by
    /// phase 1: filled, made durable and verified, its header written, all
    /// without the metadata lock (other provisionings, growths, releases,
    /// reservations and space reports run meanwhile); then, under it, the
    /// commit: its page as it is now with the entry added, at the next
    /// `meta_seq` (the entry's `footprint_seq`), and in the same hold of the
    /// lock memory follows (the claim ends, the mark becomes the footprint,
    /// the usage counted, `holds` disarmed) and the region becomes live, so
    /// no other provisioning can take its name between the two.
    fn fill_and_commit2(
        &self,
        pl: &Placed<'_>,
        holds: &mut Holds<'_, P>,
        data: Option<&mut crate::provision::Content<'_>>,
    ) -> Result<Arc<LiveRegion>, Error> {
        let inner = &self.inner;
        let block = inner.layout.block();
        let (fill, fill_key) = (pl.fill, pl.fill_key);
        // 1-3. Fill, make durable, verify (as format 1); a capped region's
        // slack allocated, not written (but where the slack is written),
        // never verified.
        let filled = if pl.fill_whole {
            pl.extent
        } else {
            2 * block + pl.data_size
        };
        match data {
            None => self.fill(pl.offset, filled, block, fill, &fill_key)?,
            // A region `copy_store` recreates (D-137): its data area is the
            // source's bytes instead of the pattern; the header blocks zero
            // and any slack the pattern, as a fill writes them.
            Some(data) => {
                let head = 2 * block + pl.data_size;
                self.write_range(pl.offset, head, 2 * block, 0, data)?;
                if filled > head {
                    self.fill_range(
                        pl.offset + head,
                        filled - head,
                        0,
                        fill,
                        &fill_key,
                        pl.data_size,
                    )?;
                }
            }
        }
        if filled < pl.extent {
            inner
                .platform
                .allocate_range(&inner.file, pl.offset + filled, pl.extent - filled)
                .map_err(|raw| {
                    crate::store::space_err(store_io_core::error::Op::Allocate, raw, pl.tag)
                })?;
        }
        self.flush_all_accounted()?;
        self.verify_ready(pl.offset, filled)?;
        // 4. The header in slot A, generation 1, Ready.
        let h = RegionHeader2 {
            region_id: pl.id,
            kind: pl.kind,
            state: RegionState::Ready,
            fill,
            flags: pl.flags,
            compat_flags: 0,
            log2_block: inner.layout.log2_block,
            extent_count: pl.extents.len() as u16,
            region_generation: pl.generation.get(),
            fill_key,
            object_len: 0,
        };
        let mut hbuf = vec![0u8; block as usize];
        let hlen = encode_region_header(&mut hbuf, &h, pl.extents, &[]).map_err(|_| meta_err())?;
        let at = PairAt {
            volume: *inner.volume.as_bytes(),
            object: inner.layout.region_object(pl.id),
            a: pl.offset,
            b: pl.offset + block,
            log2: inner.layout.log2_block,
            format: FORMAT_2,
            numbered: false,
        };
        let blank = PairRead::blank();
        let next = plan_next(&blank, &hbuf[..hlen], inner.owner_generation)?;
        let header = {
            let mut q = inner.queues.get(pl.id as usize);
            durable_slot::<P>(
                &mut q,
                &inner.file,
                &inner.pool,
                &at,
                &blank,
                &next,
                &inner.policy,
                &inner.domain,
            )
            .inspect_err(|_| self.wake_frontiers())?
        };
        // 5. The commit: the page as it is now (other operations may have
        // written it meanwhile) with the entry added, at the next
        // `meta_seq`.
        let mut meta = lock_meta(&inner.meta);
        let meta_seq = crate::v2::next_meta_seq_of(&meta)?;
        let entry = Entry2 {
            seq: pl.seq,
            kind: pl.kind,
            flags: pl.flags,
            compat_flags: 0,
            name: pl.name,
            tag: pl.tag,
            extent_count: pl.extents.len() as u16,
            header_a: pl.offset,
            header_b: pl.offset + block,
            data_size: pl.data_size,
            capacity: pl.capacity,
            span_start: pl.offset,
            span_end: pl.offset + pl.extent,
            footprint_seq: meta_seq,
        };
        let payload = {
            let v2 = meta.v2.as_deref().ok_or_else(corrupt)?;
            let page = v2.pages.get(pl.page).ok_or_else(corrupt)?;
            let ph = page.header.ok_or_else(corrupt)?;
            if page.standing != Standing::Confirmed {
                return Err(not_written(
                    NotWrittenCause::Frozen,
                    ErrorContext::default(),
                ));
            }
            let mut raws = page.entries.clone();
            raws.push(entry.to_raw());
            let next = PageHeader {
                page_index: pl.page as u32,
                // Past this seq and every one committed meanwhile.
                next_seq: ph.next_seq.max(pl.seq + 1),
                count: raws.len() as u16,
            };
            let mut buf = vec![0u8; PAGE_HEADER_LEN + 96 * raws.len()];
            let n = encode_page(&mut buf, &next, &raws).map_err(|_| meta_err())?;
            buf.truncate(n);
            buf
        };
        {
            let mut q = inner.queues.get(0);
            crate::v2::write_page::<P>(
                &mut q,
                &inner.file,
                &inner.pool,
                &inner.layout,
                inner.volume,
                &inner.policy,
                &inner.domain,
                &mut meta,
                pl.page,
                &payload,
                meta_seq,
                inner.owner_generation,
            )
            .inspect_err(|_| self.wake_frontiers())?;
        }
        // Durable: memory follows, in the same hold of the lock.
        holds.armed = false;
        if let Some(v2) = meta.v2.as_deref_mut() {
            v2.settle(pl.extent);
            v2.usage.push((pl.tag, pl.data_size, pl.extent));
        }
        {
            let mut acct = lock_acct(&inner.acct);
            if holds.from.is_some() {
                // The reservation's bytes, counted reserved until now.
                acct.unreserve(pl.tag, pl.extent);
                acct.add_usage(pl.tag, pl.data_size, pl.extent);
            } else {
                acct.add_usage(pl.tag, pl.data_size, 0);
            }
        }
        let state = Arc::new(LiveRegion {
            id: RegionId::from_raw(pl.id),
            name: pl.name,
            kind: pl.kind,
            generation: pl.generation,
            map: match pl.kind == RegionKind::Slot {
                true => crate::regionmap::RegionMap::single(pl.offset + 2 * block, 0),
                false => crate::regionmap::RegionMap::from_extents(pl.extents, pl.data_size),
            }
            .with_room(store_io_format::meta2::max_extents(inner.layout.log2_block) - 1),
            header_at: [pl.offset, pl.offset + block],
            header: Mutex::new(header),
            frontier: (pl.kind == RegionKind::Append).then(|| {
                AppendFrontier::new(
                    0,
                    pl.data_size,
                    u32::from(inner.layout.log2_block),
                    crate::frontier::ring_for(
                        inner.opts.append_window,
                        crate::frontier::ring_reach(pl.capacity, pl.flags),
                        u32::from(inner.layout.log2_block),
                    ),
                )
            }),
            positioned: std::sync::atomic::AtomicBool::new(true),
            force_sync: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            read_only: std::sync::atomic::AtomicBool::new(false),
            superseded: false,
            foreign: false,
            gate: Gate::new(),
        });
        // Live before its claim ends and the metadata lock is released:
        // `name_taken2` (under that lock) finds the name one way or the
        // other throughout.
        inner
            .regions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::clone(&state));
        if let Some(v2) = meta.v2.as_deref_mut() {
            v2.claims
                .retain(|c| !(c.page == pl.page && c.seq == pl.seq));
        }
        drop(meta);
        Ok(state)
    }

    /// A name is taken by a live region, by an unavailable one, by an
    /// entry of a kind this version does not know (spec §9 step 5), or by a
    /// provisioning in flight.
    fn name_taken2(&self, meta: &Meta, name: &RegionName) -> Result<(), Error> {
        let taken = || Error::AlreadyExists {
            what: Named::Region,
        };
        let live = self
            .inner
            .regions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|r| r.name == *name);
        if live
            || meta.unavailable.iter().any(|u| u.name == name.as_str())
            || meta.v2.as_deref().is_some_and(|v| {
                v.reserved_names.contains(name) || v.claims.iter().any(|c| c.name == *name)
            })
        {
            return Err(taken());
        }
        Ok(())
    }
}

/// What phase 1 of a format-2 provisioning or growth beyond capacity holds
/// until its commit: its marked space, its claim on a name and a table slot,
/// its tag's charge or its reservation's bytes. Dropped armed by a failure
/// before the commit, it gives them all back; the commit disarms it. Once a
/// write was submitted and failed (the store poisoned), or on a panic
/// (which poisons the store), nothing is given back: that commit may have
/// landed, so the space stays held until the store is reopened (a
/// reservation's held bytes then stay counted reserved too). While a
/// reservation-backed provisioning fills, `space()` counts its extent both
/// in `regions` (marked) and in `reserved` (held): `released` is low by
/// that much until the commit. Built after phase 1 releases the metadata
/// lock (its drop takes that lock).
pub(crate) struct Holds<'a, P: Platform>
where
    P::Queue: Send,
{
    pub(crate) store: &'a Store<P>,
    /// The ranges marked allocated.
    pub(crate) ranges: Vec<(u64, u64)>,
    /// A provisioning's claimed table slot `(page, seq)` (its name with it).
    pub(crate) claim: Option<(usize, u32)>,
    pub(crate) tag: u32,
    /// Physical bytes charged to the tag at the mark.
    pub(crate) charged: u64,
    /// The reservation the bytes were held from, and how many.
    pub(crate) from: Option<(&'a mut Reservation<P>, u64)>,
    pub(crate) armed: bool,
}

impl<P: Platform> Drop for Holds<'_, P>
where
    P::Queue: Send,
{
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // A panic may come after a write was submitted (the commit may have
        // landed): fail-stop, keeping every hold, as a failure after
        // submission does.
        if std::thread::panicking() {
            self.store
                .inner
                .domain
                .poison(store_io_core::error::FirstCause {
                    op: None,
                    raw: None,
                });
            return;
        }
        if self.store.inner.domain.poisoned().is_some() {
            return;
        }
        let inner = &self.store.inner;
        let mut meta = lock_meta(&inner.meta);
        let container = meta.container_len;
        if let Some(v2) = meta.v2.as_deref_mut() {
            if let Some((page, seq)) = self.claim {
                v2.claims.retain(|c| !(c.page == page && c.seq == seq));
            }
            v2.unmark(&self.ranges, container);
        }
        drop(meta);
        if self.charged > 0 {
            lock_acct(&inner.acct).remove_usage(self.tag, 0, self.charged);
        }
        if let Some((r, bytes)) = self.from.take() {
            r.unhold(bytes);
        }
    }
}

/// A format-2 provisioning placed, marked and claimed (phase 1), for
/// [`Store::fill_and_commit2`].
struct Placed<'a> {
    kind: RegionKind,
    name: RegionName,
    tag: u32,
    flags: u8,
    id: u64,
    /// Its claimed table slot.
    page: usize,
    seq: u32,
    /// Its header blocks' offset; its data's size and capacity; its whole
    /// footprint's length.
    offset: u64,
    data_size: u64,
    capacity: u64,
    extent: u64,
    extents: &'a [(u64, u64)],
    /// The whole footprint written (else the slack allocated).
    fill_whole: bool,
    /// Its fill pattern and key, and its region generation (1, or the
    /// source's for a region `copy_store` recreates).
    fill: FillPattern,
    fill_key: [u8; 16],
    generation: Generation,
}
