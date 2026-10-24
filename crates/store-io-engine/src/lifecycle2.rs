//! Releasing a region in container format 2 (spec §12): with the region
//! drained and its lock held (the header mutex, before the metadata lock,
//! D-113), and the metadata lock held throughout, the region's page is
//! written without its entry (the commit; every other entry byte for byte,
//! `next_seq` never reused), then its header says Released, its data
//! extents are deallocated, and free space is the complement again.

use std::sync::{Arc, PoisonError};

use store_io_core::error::{Error, NotWrittenCause, Op};
use store_io_core::id::Generation;
use store_io_format::judge::Standing;
use store_io_format::meta::RegionState;
use store_io_format::meta2::{
    PAGE_HEADER_LEN, PageHeader, decode_region_header, encode_page, encode_region_header,
};
use store_io_format::slot::FORMAT_2;
use store_io_platform::Platform;

use crate::gate::Retire;
use crate::io::{ctx, not_written};
use crate::slots::{PairAt, PairRead, plan_next};
use crate::store::*;

impl<P: Platform> Store<P>
where
    P::Queue: Send,
{
    /// Releases a format-2 region. See [`Store::release_region`].
    pub(crate) fn release_region2(&self, r: &Arc<LiveRegion>) -> Result<(), Error> {
        let inner = &self.inner;
        let c = ctx(r);
        if inner.frozen.load(std::sync::atomic::Ordering::Acquire) {
            return Err(not_written(NotWrittenCause::Frozen, c));
        }
        // Drained before any lock (a `Submitter` operation holds its pass
        // until its thread processes the completion; see format 1).
        self.retire(r, Retire::Release)?;
        // The region lock, then the metadata lock, both held throughout.
        let mut header = r.header.lock().unwrap_or_else(PoisonError::into_inner);
        let mut meta = lock_meta(&inner.meta);
        let id = r.id.get();
        let seq = (id & 0xFFFF_FFFF) as u32;
        // Every return before the commit gives the region back.
        let refused = |e: Error| {
            r.gate.restore();
            e
        };
        let p = usize::try_from(id >> 32).map_err(|_| refused(corrupt()))?;
        // A region an overlap or a name demoted rests on this one: released,
        // the next open would serve it (spec §9 step 4).
        if let Some(d) = meta
            .v2
            .as_deref()
            .and_then(|v2| crate::repair::would_lift(self, v2, id))
        {
            return Err(refused(not_written(
                NotWrittenCause::Overlap,
                store_io_core::error::ErrorContext {
                    region: Some(d),
                    ..store_io_core::error::ErrorContext::default()
                },
            )));
        }
        // 1. The commit: the page without the entry.
        let (payload, removed) = {
            let v2 = meta.v2.as_deref().ok_or_else(|| refused(corrupt()))?;
            let page = v2.pages.get(p).ok_or_else(|| refused(corrupt()))?;
            let ph = page.header.ok_or_else(|| refused(corrupt()))?;
            // A page holding an entry past its own write is written only to
            // remove that entry (`Page2::ahead`).
            if page.standing != Standing::Confirmed || page.ahead().next().is_some() {
                return Err(refused(not_written(NotWrittenCause::Frozen, c)));
            }
            let Some(at) = page.entries.iter().position(|e| e.seq == seq) else {
                return Err(refused(corrupt()));
            };
            let mut raws = page.entries.clone();
            let removed = raws.remove(at);
            let next = PageHeader {
                page_index: ph.page_index,
                // Never decreases: the id is never handed out again.
                next_seq: ph.next_seq,
                count: raws.len() as u16,
            };
            let mut buf = vec![0u8; PAGE_HEADER_LEN + 96 * raws.len()];
            let n = encode_page(&mut buf, &next, &raws).map_err(|_| refused(meta_err()))?;
            buf.truncate(n);
            (buf, removed)
        };
        let meta_seq = crate::v2::next_meta_seq_of(&meta).map_err(refused)?;
        {
            let mut q = inner.queues.get(0);
            if let Err(e) = crate::v2::write_page::<P>(
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
            ) {
                // Not submitted: nothing changed, the region serves again.
                // Submitted: the domain is poisoned (durability unknown).
                if inner.domain.poisoned().is_none() {
                    r.gate.restore();
                }
                self.wake_frontiers();
                return Err(e);
            }
        }
        // Committed: memory follows, before the header and the space.
        let block = inner.layout.block();
        let committed = usize::from(removed.extent_count);
        let extents = committed_extents(&header, inner.layout.log2_block, committed);
        let mut footprint = vec![(removed.header_a, block), (removed.header_b, block)];
        footprint.extend(extents.iter().copied());
        if let Some(v2) = meta.v2.as_deref_mut() {
            for range in &footprint {
                if let Some(i) = v2.allocated.iter().position(|a| a == range) {
                    let _gone = v2.allocated.swap_remove(i);
                }
            }
            let physical = removed.capacity.saturating_add(2 * block);
            if let Some(i) = v2
                .usage
                .iter()
                .position(|u| *u == (removed.tag, removed.data_size, physical))
            {
                let _gone = v2.usage.swap_remove(i);
            }
        }
        crate::space::lock_acct(&inner.acct).remove_usage(
            removed.tag,
            removed.data_size,
            removed.capacity.saturating_add(2 * block),
        );
        inner
            .regions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|x| x.id != r.id);
        // Free space: the complement, recomputed now (the metadata lock,
        // held until return, keeps it from reuse until the space is given
        // back below).
        let len = meta.container_len;
        if let Some(v2) = meta.v2.as_deref_mut() {
            v2.rebuild_free(len);
        }
        // 2. The header says Released: should the commit's page ever be
        //    rolled back (its newer copy found damaged), the region is then
        //    unavailable, not served. A header write refused before it was
        //    submitted leaves the data in place instead (that rollback would
        //    find the region whole); one that failed after submission has
        //    poisoned the store. Either way the release is committed.
        match self.rewrite_header2(r, &mut header, RegionState::Released, 0, committed, None) {
            Ok(_) => {}
            Err(Error::NotWritten { .. }) => return Ok(()),
            Err(e) => return Err(e),
        }
        // 3. The data extents, as format 1 does: every one tried.
        self.release_extents(extents.iter().copied(), c)
    }

    /// Recycles a format-2 region's header (spec §12): one header write
    /// under the region lock, Ready, its region generation one more, its
    /// entry's extents (never an extra one) and everything else kept.
    /// Returns the new region generation.
    pub(crate) fn recycle_header2(&self, r: &LiveRegion) -> Result<Generation, Error> {
        let mut header = r.header.lock().unwrap_or_else(PoisonError::into_inner);
        // The entry's extent count, read under the metadata lock (taken
        // after the region lock, D-113).
        let committed = {
            let meta = lock_meta(&self.inner.meta);
            let id = r.id.get();
            let p = usize::try_from(id >> 32).map_err(|_| corrupt())?;
            let seq = (id & 0xFFFF_FFFF) as u32;
            meta.v2
                .as_deref()
                .and_then(|v| v.pages.get(p))
                .and_then(|page| page.entries.iter().find(|e| e.seq == seq))
                .map(|e| usize::from(e.extent_count))
                .ok_or_else(corrupt)?
        };
        self.rewrite_header2(r, &mut header, RegionState::Ready, 1, committed, None)
    }

    /// Writes the region's header again with `state` and its region
    /// generation moved on by `generation_step`: its first `committed`
    /// extents (the entry's count, never an extra one a header may list),
    /// then `append` (a growth beyond capacity: the header is one extent
    /// ahead of its entry until the page commits), its fill and key, its
    /// may-ignore flags and its object as they were. The caller holds the
    /// region lock (`header`).
    pub(crate) fn rewrite_header2(
        &self,
        r: &LiveRegion,
        header: &mut PairRead,
        state: RegionState,
        generation_step: u64,
        committed: usize,
        append: Option<(u64, u64)>,
    ) -> Result<Generation, Error> {
        let inner = &self.inner;
        let c = ctx(r);
        let metadata = || Error::Corruption {
            kind: store_io_core::error::CorruptionKind::Metadata,
            ctx: c,
        };
        let bytes = header.payload.as_deref().ok_or_else(metadata)?;
        let view = decode_region_header(bytes, inner.layout.log2_block, u64::MAX)
            .map_err(|_| metadata())?;
        let mut extents: Vec<(u64, u64)> = view.extents().take(committed).collect();
        extents.extend(append);
        let mut h = view.header;
        h.state = state;
        h.extent_count = extents.len() as u16;
        h.region_generation = h
            .region_generation
            .checked_add(generation_step)
            .ok_or_else(metadata)?;
        // Converted before the write, so nothing can fail after it.
        let generation = Generation::from_raw(h.region_generation).ok_or_else(metadata)?;
        let object = view.object().to_vec();
        let mut buf = vec![0u8; inner.layout.block() as usize];
        let n = encode_region_header(&mut buf, &h, &extents, &object).map_err(|_| metadata())?;
        let next = plan_next(header, &buf[..n], inner.owner_generation)?;
        let [a, b] = r.header_at;
        let at = PairAt {
            volume: *inner.volume.as_bytes(),
            object: inner.layout.region_object(r.id.get()),
            a,
            b,
            log2: inner.layout.log2_block,
            format: FORMAT_2,
            numbered: false,
        };
        let mut lane = self.lane(r, Op::Write)?;
        let written = durable_slot::<P>(
            &mut lane,
            &inner.file,
            &inner.pool,
            &at,
            header,
            &next,
            &inner.policy,
            &inner.domain,
        )
        .inspect_err(|_| self.wake_frontiers())?;
        *header = written;
        Ok(generation)
    }
}

/// The first `committed` extents the header in `pair` lists (the entry's;
/// a slot region has none), as the open's footprint holds them.
fn committed_extents(pair: &PairRead, log2_block: u8, committed: usize) -> Vec<(u64, u64)> {
    pair.payload
        .as_deref()
        .and_then(|b| decode_region_header(b, log2_block, u64::MAX).ok())
        .map(|v| v.extents().take(committed).collect())
        .unwrap_or_default()
}
