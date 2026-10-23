//! Growth of a format-2 region within its capacity (spec §12, "Growth
//! within capacity"; v0.5 E4 C8).
//!
//! Under the region lock: the range from the data size to the new size is
//! filled with the region's pattern (keyed by region offset), flushed and
//! verified, run by run; then one table page write carries the new
//! `data_size` (the commit; the entry's `footprint_seq` is unchanged: the
//! footprint did not move); then the new size is published, the region
//! map's length first and the append frontier's limit after. A crash before
//! the page write leaves the old size (the filled bytes are past it, with
//! no defined contents); after it, the new one.

use std::sync::PoisonError;

use store_io_core::align::align_up;
use store_io_core::error::{Capability, Error, NotWrittenCause};
use store_io_format::judge::Standing;
use store_io_format::meta2::{PAGE_HEADER_LEN, PageHeader, decode_region_header, encode_page};
use store_io_format::slot::FORMAT_2;
use store_io_platform::Platform;

use crate::io::{ctx, not_written};
use crate::store::*;

impl<P: Platform> Store<P>
where
    P::Queue: Send,
{
    /// Grows `r` to at least `new_size` bytes (rounded up to the block)
    /// within its capacity; returns the new size (the current one, with
    /// nothing written, when that is already as large). The caller holds a
    /// pass on the region's gate, so no recycle or release runs meanwhile.
    pub(crate) fn grow_region2(&self, r: &LiveRegion, new_size: u64) -> Result<u64, Error> {
        let inner = &*self.inner;
        let c = ctx(r);
        if inner.layout.format != FORMAT_2 {
            return Err(Error::Unsupported {
                what: Capability::FormatVersion,
            });
        }
        self.ready_for(r)?;
        // Frozen (a page Unconfirmed or Lost): refused, though the spec
        // freezes only growth beyond capacity. A page write now would take
        // a `meta_seq` from what the open could read; a Lost page may have
        // used a higher one, and a torn commit would then be judged
        // against a sequence that is not the newest.
        if inner.report.metadata.frozen {
            return Err(not_written(NotWrittenCause::Frozen, c));
        }
        let block = inner.layout.block();
        let too_large = || not_written(NotWrittenCause::TooLarge, c);
        let new = align_up(new_size, block).ok_or_else(too_large)?;
        // The region lock: one growth, recycle or slot commit at a time.
        let header = r.header.lock().unwrap_or_else(PoisonError::into_inner);
        let len = r.map.len();
        if new <= len {
            return Ok(len);
        }
        // Past the capacity is growth beyond it (E4 C9); past 2^32 blocks
        // the append frontier cannot address it.
        if new > r.map.capacity() || new / block >= u64::from(u32::MAX) {
            return Err(too_large());
        }
        let view = header
            .payload
            .as_deref()
            .and_then(|b| decode_region_header(b, inner.layout.log2_block, u64::MAX).ok())
            .ok_or_else(corrupt)?;
        let (fill, key) = (view.header.fill, view.header.fill_key);
        // 1. The new range, run by run: filled, flushed, verified.
        let pieces = r.map.pieces(len, new);
        for &(file, at, n) in &pieces {
            self.fill_range(file, n, 0, fill, &key, at)?;
        }
        self.flush_all_accounted()?;
        for &(file, _, n) in &pieces {
            self.verify_ready(file, n)?;
        }
        // 2. The commit: the page with the entry's new data size.
        let mut meta = lock_meta(&inner.meta);
        let id = r.id.get();
        let seq = (id & 0xFFFF_FFFF) as u32;
        let p = usize::try_from(id >> 32).map_err(|_| corrupt())?;
        let (payload, entry) = {
            let v2 = meta.v2.as_deref().ok_or_else(corrupt)?;
            let page = v2.pages.get(p).ok_or_else(corrupt)?;
            let ph = page.header.ok_or_else(corrupt)?;
            if page.standing != Standing::Confirmed {
                return Err(not_written(NotWrittenCause::Frozen, c));
            }
            let mut raws = page.entries.clone();
            let entry = raws.iter_mut().find(|e| e.seq == seq).ok_or_else(corrupt)?;
            debug_assert_eq!(entry.data_size, len, "the map's length is the entry's");
            let old = *entry;
            entry.data_size = new;
            let next = PageHeader {
                page_index: ph.page_index,
                next_seq: ph.next_seq,
                count: raws.len() as u16,
            };
            let mut buf = vec![0u8; PAGE_HEADER_LEN + 96 * raws.len()];
            let n = encode_page(&mut buf, &next, &raws).map_err(|_| meta_err())?;
            buf.truncate(n);
            (buf, old)
        };
        let meta_seq = crate::v2::next_meta_seq_of(&meta)?;
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
                // Not submitted: the old size stands. Submitted: poisoned.
                self.wake_frontiers();
                return Err(e);
            }
        }
        // 3. Memory follows the commit: usage, then the new size published
        // (the map, which every I/O bounds by, before the frontier, which
        // lets appends reserve into it).
        let physical = entry.capacity.saturating_add(2 * block);
        if let Some(v2) = meta.v2.as_deref_mut() {
            let u = v2
                .usage
                .iter_mut()
                .find(|u| **u == (entry.tag, entry.data_size, physical));
            debug_assert!(u.is_some(), "a usable region without its usage");
            if let Some(u) = u {
                u.1 = new;
            }
        }
        {
            let mut acct = crate::space::lock_acct(&inner.acct);
            acct.remove_usage(entry.tag, entry.data_size, 0);
            acct.add_usage(entry.tag, new, 0);
        }
        drop(meta);
        r.map.publish_len(new);
        if let Some(f) = &r.frontier {
            f.raise_limit(new);
        }
        drop(header);
        Ok(new)
    }
}
