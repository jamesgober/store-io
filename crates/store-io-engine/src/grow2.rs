//! Growth of a format-2 region (spec §12; v0.5 E4 C8, C9).
//!
//! Within its capacity, under the region lock: the range from the data
//! size to the new size is filled with the region's pattern (keyed by
//! region offset), flushed and verified, run by run; then one table page
//! write carries the new `data_size` (the commit; the entry's
//! `footprint_seq` is unchanged: the footprint did not move); then the new
//! size is published, the region map's length first and the append
//! frontier's limit after. A crash before the page write leaves the old
//! size (the filled bytes are past it, with no defined contents); after it,
//! the new one.
//!
//! Beyond its capacity, under the region lock and the metadata lock
//! throughout (as provisioning: no other placement, reservation or space
//! report runs meanwhile, so the chosen extent needs no mark and a failure
//! no undo): a new extent is placed (after the region's last one when that
//! space is free, else by first fit, else past everything allocated), its
//! slack allocated, the range from the data size to the new size filled,
//! flushed and verified; then the header is written listing the extent
//! after the entry's (one extent ahead of its entry: an open finds that
//! extent free and reports it); then one table page write carries the new
//! `data_size`, `capacity`, `extent_count`, span and a `footprint_seq` of
//! its own `meta_seq` (the commit); then memory follows: the extent
//! allocated and charged, then the map's new run, its length and the
//! frontier's limit.

use std::sync::PoisonError;

use store_io_core::align::align_up;
use store_io_core::error::{Capability, Error, NotWrittenCause, Op, ReserveTag};
use store_io_core::evidence::PlatformKind;
use store_io_format::judge::Standing;
use store_io_format::meta::{FillPattern, RegionState};
use store_io_format::meta2::{
    FLAG_CAPPED, PAGE_HEADER_LEN, PageHeader, RawEntry, decode_region_header, encode_page,
    max_extents,
};
use store_io_format::slot::FORMAT_2;
use store_io_platform::Platform;

use crate::io::{ctx, not_written};
use crate::layout::DATA_ALIGN;
use crate::slots::PairRead;
use crate::space::lock_acct;
use crate::store::*;

/// The smallest extent a growth beyond capacity adds.
const MIN_EXTENT: u64 = 1 << 20;

impl<P: Platform> Store<P>
where
    P::Queue: Send,
{
    /// Grows `r` to at least `new_size` bytes (rounded up to the block);
    /// returns the new size (the current one, with nothing written, when
    /// that is already as large). The caller holds a pass on the region's
    /// gate, so no recycle or release runs meanwhile.
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
        let mut header = r.header.lock().unwrap_or_else(PoisonError::into_inner);
        let len = r.map.len();
        if new <= len {
            return Ok(len);
        }
        // Past 2^32 blocks the append frontier cannot address it.
        if new / block >= u64::from(u32::MAX) {
            return Err(too_large());
        }
        let (fill, key) = header
            .payload
            .as_deref()
            .and_then(|b| decode_region_header(b, inner.layout.log2_block, u64::MAX).ok())
            .map(|v| (v.header.fill, v.header.fill_key))
            .ok_or_else(corrupt)?;
        if new > r.map.capacity() {
            return self.grow_beyond2(r, &mut header, new, fill, &key);
        }
        // 1. The new range, run by run: filled, flushed, verified.
        self.fill_pieces(&r.map.pieces(len, new), fill, &key)?;
        // 2. The commit: the page with the entry's new data size.
        let mut meta = lock_meta(&inner.meta);
        let (p, payload, old) = page_with(&meta, r, |e| {
            debug_assert_eq!(e.data_size, len, "the map's length is the entry's");
            e.data_size = new;
        })?;
        let meta_seq = crate::v2::next_meta_seq_of(&meta)?;
        self.commit_page(&mut meta, p, &payload, meta_seq)?;
        // 3. Memory follows the commit: usage, then the new size published
        // (the map, which every I/O bounds by, before the frontier, which
        // lets appends reserve into it).
        self.count_growth(&mut meta, &old, new, 0);
        drop(meta);
        r.map.publish_len(new);
        if let Some(f) = &r.frontier {
            f.raise_limit(new);
        }
        drop(header);
        Ok(new)
    }

    /// Grows `r` past its capacity to `new` (rounded; more than the
    /// capacity) by one extent: see the module doc. The caller holds the
    /// region lock (`header`) and has made the checks every growth makes.
    fn grow_beyond2(
        &self,
        r: &LiveRegion,
        header: &mut PairRead,
        new: u64,
        fill: FillPattern,
        key: &[u8; 16],
    ) -> Result<u64, Error> {
        let inner = &*self.inner;
        let c = ctx(r);
        let block = inner.layout.block();
        let too_large = || not_written(NotWrittenCause::TooLarge, c);
        let no_space = |tag| Error::NoSpace { tag, ctx: c };
        let (len, old_cap) = (r.map.len(), r.map.capacity());
        let mut meta = lock_meta(&inner.meta);
        // Refusals before any I/O: a capped region, a full extent list, the
        // tag's cap.
        let (_, old) = entry_of(&meta, r)?;
        debug_assert_eq!(
            (old.data_size, old.capacity),
            (len, old_cap),
            "the map is the entry's"
        );
        if old.flags & FLAG_CAPPED != 0 {
            return Err(no_space(ReserveTag::RegionLimit));
        }
        if usize::from(old.extent_count) >= max_extents(inner.layout.log2_block) {
            return Err(no_space(ReserveTag::Extents));
        }
        // An eighth of the capacity at least, so a region growing by small
        // steps has few extents.
        let ext_len = align_up(MIN_EXTENT.max(old_cap / 8).max(new - old_cap), block)
            .ok_or_else(too_large)?;
        if !lock_acct(&inner.acct).fits(old.tag, ext_len) {
            return Err(no_space(ReserveTag::Caller(old.tag)));
        }
        // The commit's `meta_seq` (overflow refused here, before any I/O):
        // the entry's new `footprint_seq`.
        let meta_seq = crate::v2::next_meta_seq_of(&meta)?;
        // Placement (spec §10, §12): after the last extent when that space
        // is free or the tail (one run: I/O there is never split), else
        // first fit, else past everything allocated, aligned as a large
        // region's data area is (unaligned when only the alignment does not
        // fit); the container grown past outstanding reservations.
        let last_end = r.map.extents().last().map_or(0, |(f, n)| f + n);
        let reserved = lock_acct(&inner.acct).reserved();
        let mut align = DATA_ALIGN;
        let at = loop {
            let v2 = meta.v2.as_deref().ok_or_else(corrupt)?;
            let at = if v2.free.fits_at(last_end, ext_len) || last_end == v2.free.end() {
                last_end
            } else if let Some(at) = v2.free.first_fit(ext_len, align, 0) {
                at
            } else {
                v2.free.tail(align, 0).ok_or_else(too_large)?
            };
            let end = at.checked_add(ext_len).ok_or_else(too_large)?;
            let target = self
                .tail(&meta)
                .max(end)
                .checked_add(reserved)
                .ok_or_else(too_large)?;
            match self.grow_to(&mut meta, target, old.tag) {
                Ok(()) => break at,
                Err(Error::NoSpace { .. }) if align > block => align = block,
                Err(e) => return Err(growth_space(e)),
            }
        };
        let capacity = old_cap.checked_add(ext_len).ok_or_else(too_large)?;
        let (p, payload, _) = page_with(&meta, r, |e| {
            e.data_size = new;
            e.capacity = capacity;
            e.extent_count += 1;
            e.span_start = e.span_start.min(at);
            e.span_end = e.span_end.max(at + ext_len);
            e.footprint_seq = meta_seq;
        })?;
        // 1. The slack and the new range. On Windows the whole extent is
        // filled: a write past a file's valid data makes NTFS zero every
        // byte below it first, so slack left unwritten would cost a later
        // write (this region's, or one placed after it) that zeroing.
        // Elsewhere the slack is allocated, unwritten (never verified).
        let filled = if inner.report.evidence.platform == PlatformKind::Windows {
            ext_len
        } else {
            new - old_cap
        };
        if filled < ext_len {
            inner
                .platform
                .allocate_range(&inner.file, at + filled, ext_len - filled)
                .map_err(|raw| growth_space(space_err(Op::Allocate, raw, old.tag)))?;
        }
        let mut pieces = r.map.pieces(len, old_cap);
        pieces.push((at, old_cap, filled));
        self.fill_pieces(&pieces, fill, key)?;
        // 2. The header lists the extent after the entry's (refused before
        // submission: nothing on disk changed). A header one extent ahead
        // already (an earlier growth whose commit was lost) is first written
        // again with the entry's extents only: else both copies of the pair
        // would list as many extents as the entry once this growth commits,
        // the older one an extent the region never had (another region's by
        // now, perhaps), and a damaged newer copy would let an open take
        // the older one as the region's.
        let committed = usize::from(old.extent_count);
        let listed = header
            .payload
            .as_deref()
            .and_then(|b| decode_region_header(b, inner.layout.log2_block, u64::MAX).ok())
            .map_or(0, |v| usize::from(v.header.extent_count));
        if listed > committed {
            let _same = self.rewrite_header2(r, header, RegionState::Ready, 0, committed, None)?;
        }
        let _same = self.rewrite_header2(
            r,
            header,
            RegionState::Ready,
            0,
            committed,
            Some((at, ext_len)),
        )?;
        // 3. The commit. Refused before submission, the header stays one
        // extent ahead (on disk and here); every later header write lists
        // the entry's extents only, and an open finds the extent free.
        self.commit_page(&mut meta, p, &payload, meta_seq)?;
        // 4. Memory follows: the extent allocated, the usage, then the run
        // and the new size published (the map before the frontier).
        let container = meta.container_len;
        if let Some(v2) = meta.v2.as_deref_mut() {
            v2.allocated.push((at, ext_len));
            v2.rebuild_free(container);
        }
        self.count_growth(&mut meta, &old, new, ext_len);
        drop(meta);
        r.map.extend(at, ext_len);
        r.map.publish_len(new);
        if let Some(f) = &r.frontier {
            f.raise_limit(new);
        }
        Ok(new)
    }

    /// Fills `pieces` (`(file offset, region offset, length)`) with the
    /// region's pattern keyed by region offset, makes them durable and
    /// verifies them. A fill write that runs out of space is
    /// `NoSpace(Growth)`; other failures as [`Self::fill_range`].
    fn fill_pieces(
        &self,
        pieces: &[(u64, u64, u64)],
        fill: FillPattern,
        key: &[u8; 16],
    ) -> Result<(), Error> {
        for &(file, at, n) in pieces {
            self.fill_range(file, n, 0, fill, key, at)
                .map_err(growth_space)?;
        }
        self.flush_all_accounted()?;
        for &(file, _, n) in pieces {
            self.verify_ready(file, n)?;
        }
        Ok(())
    }

    /// Writes page `p` (a growth's commit). Not submitted: nothing changed;
    /// submitted and failed: poisoned.
    fn commit_page(
        &self,
        meta: &mut Meta,
        p: usize,
        payload: &[u8],
        meta_seq: u64,
    ) -> Result<(), Error> {
        let inner = &*self.inner;
        let mut q = inner.queues.get(0);
        crate::v2::write_page::<P>(
            &mut q,
            &inner.file,
            &inner.pool,
            &inner.layout,
            inner.volume,
            &inner.policy,
            &inner.domain,
            meta,
            p,
            payload,
            meta_seq,
            inner.owner_generation,
        )
        .inspect_err(|_| self.wake_frontiers())
    }

    /// Counts a committed growth of the region whose entry was `old`: its
    /// usage in the table's list and the tags' counts, logical `new`,
    /// physical `extra` more.
    fn count_growth(&self, meta: &mut Meta, old: &RawEntry, new: u64, extra: u64) {
        let physical = old.capacity.saturating_add(2 * self.inner.layout.block());
        if let Some(v2) = meta.v2.as_deref_mut() {
            let u = v2
                .usage
                .iter_mut()
                .find(|u| **u == (old.tag, old.data_size, physical));
            debug_assert!(u.is_some(), "a usable region without its usage");
            if let Some(u) = u {
                *u = (old.tag, new, physical.saturating_add(extra));
            }
        }
        let mut acct = lock_acct(&self.inner.acct);
        acct.remove_usage(old.tag, old.data_size, 0);
        acct.add_usage(old.tag, new, extra);
    }
}

/// Region `r`'s page index and entry. Refused `Frozen` unless its page is
/// Confirmed.
fn entry_of(meta: &Meta, r: &LiveRegion) -> Result<(usize, RawEntry), Error> {
    let id = r.id.get();
    let seq = (id & 0xFFFF_FFFF) as u32;
    let p = usize::try_from(id >> 32).map_err(|_| corrupt())?;
    let v2 = meta.v2.as_deref().ok_or_else(corrupt)?;
    let page = v2.pages.get(p).ok_or_else(corrupt)?;
    if page.standing != Standing::Confirmed {
        return Err(not_written(NotWrittenCause::Frozen, ctx(r)));
    }
    let e = page
        .entries
        .iter()
        .find(|e| e.seq == seq)
        .ok_or_else(corrupt)?;
    Ok((p, *e))
}

/// Region `r`'s page with `change` made to its entry, every other entry
/// byte for byte: the page index, the payload, and the entry as it was.
/// Refused as [`entry_of`].
fn page_with(
    meta: &Meta,
    r: &LiveRegion,
    change: impl FnOnce(&mut RawEntry),
) -> Result<(usize, Vec<u8>, RawEntry), Error> {
    let (p, old) = entry_of(meta, r)?;
    let v2 = meta.v2.as_deref().ok_or_else(corrupt)?;
    let page = &v2.pages[p];
    let ph = page.header.ok_or_else(corrupt)?;
    let mut raws = page.entries.clone();
    let e = raws
        .iter_mut()
        .find(|e| e.seq == old.seq)
        .ok_or_else(corrupt)?;
    change(e);
    let next = PageHeader {
        page_index: ph.page_index,
        next_seq: ph.next_seq,
        count: raws.len() as u16,
    };
    let mut buf = vec![0u8; PAGE_HEADER_LEN + 96 * raws.len()];
    let n = encode_page(&mut buf, &next, &raws).map_err(|_| meta_err())?;
    buf.truncate(n);
    Ok((p, buf, old))
}

/// Running out of space during a growth: `NoSpace(Growth)` (a tag's cap is
/// refused before, as `NoSpace(Caller)`).
fn growth_space(e: Error) -> Error {
    match e {
        Error::NoSpace { ctx, .. } => Error::NoSpace {
            tag: ReserveTag::Growth,
            ctx,
        },
        e => e,
    }
}
