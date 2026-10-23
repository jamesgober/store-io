//! Provisioning a region in container format 2 (spec §10, §12): placed by
//! first fit over the free space (else past everything allocated), filled,
//! flushed and verified as in format 1, its header written in slot A, then
//! committed by one table-page write carrying its entry, whose
//! `footprint_seq` is that write's own `meta_seq`. Memory changes only after
//! the commit is durable.

use std::sync::{Arc, Mutex, PoisonError};

use store_io_core::error::{Error, ErrorContext, Named, NotWrittenCause, ReserveTag};
use store_io_core::id::{Generation, RegionId};
use store_io_format::judge::Standing;
use store_io_format::meta::{FillPattern, RegionKind, RegionName, RegionState};
use store_io_format::meta2::{
    Entry2, PAGE_HEADER_LEN, PageHeader, RegionHeader2, encode_page, encode_region_header,
    entries_per_page,
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
        data_size: u64,
        from: Option<&mut Reservation<P>>,
    ) -> Result<Arc<LiveRegion>, Error> {
        let inner = &self.inner;
        let tag = from.as_ref().map_or(0, |r| r.tag());
        // Refusals before any I/O (spec §9 step 2, §10).
        if inner.report.metadata.frozen {
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
        let data_size = if slot {
            0
        } else {
            store_io_core::align::align_up(data_size.max(1), block).ok_or_else(too_large)?
        };
        if data_size / block >= u64::from(u32::MAX) {
            return Err(too_large());
        }
        let extent = data_size.checked_add(2 * block).ok_or_else(too_large)?;
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
                    (page.standing == Standing::Confirmed
                        && h.count < per_page
                        && h.next_seq < u32::MAX)
                        .then_some((p, h.next_seq))
                })
                .ok_or(Error::NoSpace {
                    tag: ReserveTag::Table,
                    ctx: ErrorContext::default(),
                })?
        };
        // The commit's `meta_seq` (overflow refused here, before any I/O):
        // the entry's `footprint_seq`.
        let meta_seq = crate::v2::next_meta_seq_of(&meta)?;
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
        let mut align = if data_size >= DATA_ALIGN {
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
        let id = ((p as u64) << 32) | u64::from(seq);
        let fill = if inner.opts.keyed_fill {
            FillPattern::KeyedV1
        } else {
            FillPattern::Zeros
        };
        let fill_key = if fill == FillPattern::KeyedV1 {
            random_uuid()
        } else {
            [0; 16]
        };
        // 1-3. Fill, make durable, verify (as format 1).
        self.fill(offset, extent, block, fill, &fill_key)?;
        self.flush_all_accounted()?;
        self.verify_ready(offset, extent)?;
        // 4. The header in slot A, generation 1, Ready.
        let extents: Vec<(u64, u64)> = if slot {
            Vec::new()
        } else {
            vec![(offset + 2 * block, data_size)]
        };
        let h = RegionHeader2 {
            region_id: id,
            kind,
            state: RegionState::Ready,
            fill,
            flags: 0,
            compat_flags: 0,
            log2_block: inner.layout.log2_block,
            extent_count: extents.len() as u16,
            region_generation: 1,
            fill_key,
            object_len: 0,
        };
        let mut hbuf = vec![0u8; block as usize];
        let hlen = encode_region_header(&mut hbuf, &h, &extents, &[]).map_err(|_| meta_err())?;
        let at = PairAt {
            volume: *inner.volume.as_bytes(),
            object: inner.layout.region_object(id),
            a: offset,
            b: offset + block,
            log2: inner.layout.log2_block,
            format: FORMAT_2,
            numbered: false,
        };
        let blank = PairRead::blank();
        let next = plan_next(&blank, &hbuf[..hlen], inner.owner_generation)?;
        let header = {
            let mut q = inner.queues.get(id as usize);
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
        // 5. The commit: page `p` with the entry added.
        let entry = Entry2 {
            seq,
            kind,
            flags: 0,
            compat_flags: 0,
            name: rname,
            tag,
            extent_count: extents.len() as u16,
            header_a: offset,
            header_b: offset + block,
            data_size,
            capacity: data_size,
            span_start: offset,
            span_end: offset + extent,
            footprint_seq: meta_seq,
        };
        let payload = {
            let v2 = meta.v2.as_deref().ok_or_else(corrupt)?;
            let page = &v2.pages[p];
            let mut raws = page.entries.clone();
            raws.push(entry.to_raw());
            let ph = PageHeader {
                page_index: p as u32,
                next_seq: seq + 1,
                count: raws.len() as u16,
            };
            let mut buf = vec![0u8; PAGE_HEADER_LEN + 96 * raws.len()];
            let n = encode_page(&mut buf, &ph, &raws).map_err(|_| meta_err())?;
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
                p,
                &payload,
                meta_seq,
                inner.owner_generation,
            )
            .inspect_err(|_| self.wake_frontiers())?;
        }
        // Durable: memory follows.
        if let Some(v2) = meta.v2.as_deref_mut() {
            // As the open records a footprint: the header blocks, then the
            // data extent (release removes exactly these).
            v2.allocated.push((offset, block));
            v2.allocated.push((offset + block, block));
            v2.allocated.extend(extents.iter().copied());
            v2.usage.push((tag, data_size, extent));
        }
        let len = meta.container_len;
        if let Some(v2) = meta.v2.as_deref_mut() {
            v2.rebuild_free(len);
        }
        if let Some(r) = from {
            r.consume(extent)?;
        }
        lock_acct(&inner.acct).add_usage(tag, data_size, extent);
        let state = Arc::new(LiveRegion {
            id: RegionId::from_raw(id),
            name: rname,
            kind,
            generation: Generation::FIRST,
            map: crate::regionmap::RegionMap::single(offset + 2 * block, data_size)
                .with_room(store_io_format::meta2::max_extents(inner.layout.log2_block) - 1),
            header_at: [offset, offset + block],
            header: Mutex::new(header),
            frontier: (kind == RegionKind::Append).then(|| {
                AppendFrontier::new(
                    0,
                    data_size,
                    u32::from(inner.layout.log2_block),
                    crate::frontier::ring_for(
                        inner.opts.append_window,
                        data_size,
                        u32::from(inner.layout.log2_block),
                    ),
                )
            }),
            positioned: std::sync::atomic::AtomicBool::new(true),
            force_sync: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            read_only: false,
            gate: Gate::new(),
        });
        inner
            .regions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::clone(&state));
        Ok(state)
    }

    /// A name is taken by a live region, by an unavailable one, or by an
    /// entry of a kind this version does not know (spec §9 step 5).
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
            || self
                .inner
                .report
                .unavailable
                .iter()
                .any(|u| u.name == name.as_str())
            || meta
                .v2
                .as_deref()
                .is_some_and(|v| v.reserved_names.contains(name))
        {
            return Err(taken());
        }
        Ok(())
    }
}
