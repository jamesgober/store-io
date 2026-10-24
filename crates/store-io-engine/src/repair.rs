//! Repairs of a format-2 store (v0.5 E6; spec §12, D-112, Q6):
//! `repair_unavailable` releases an unavailable region's table entry (the
//! page written without it, the commit; then free space is the complement
//! again: the entry's span is never added back, other regions' bytes inside
//! it stay allocated), its bytes untouched; `accept_page_rollback` makes a
//! stale page's winner its content (whatever its lost commits held is
//! given up), its regions writable and, once no page is stale, allocation
//! free again.

use store_io_core::error::{Capability, Error, ErrorContext, Named, NotWrittenCause};
use store_io_core::id::RegionId;
use store_io_format::judge::Standing;
use store_io_format::meta2::{PAGE_HEADER_LEN, PageHeader, encode_page};
use store_io_platform::Platform;

use crate::io::not_written;
use crate::report::Repair;
use crate::store::*;

/// Whether removing the entry `removing` (a repair of an unavailable one,
/// or a release of a live region) would lift a verdict of the open at the
/// next one (spec §9 step 4): a region demoted by an overlap or a name
/// that would no longer be, or a live region found possibly superseded
/// that would no longer be. The rule is judged again over what remains:
/// the live regions' footprints and every unavailable entry's (a region
/// demoted by step 4 counts as usable there; one that failed step 3 by
/// what it may have written). Returns the first such region. Nothing to
/// judge (no region demoted, none superseded: the common case) costs a
/// scan of the holds and the live list only.
pub(crate) fn would_lift<P: Platform>(
    store: &Store<P>,
    v2: &crate::v2::Meta2,
    removing: u64,
) -> Option<RegionId>
where
    P::Queue: Send,
{
    use crate::footprint::{Unavailable as Gone, Usable, settle};
    let live = store
        .inner
        .regions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let demoted = v2
        .holds
        .iter()
        .any(|h| h.demoted.is_some() && h.id() != removing);
    let superseded = live.iter().any(|r| r.superseded && r.id.get() != removing);
    if !demoted && !superseded {
        return None;
    }
    let block = store.inner.layout.block();
    // Live regions' footprints and sequences, as their entries stand now.
    let seq_of = |id: u64| {
        v2.pages
            .get(usize::try_from(id >> 32).ok()?)?
            .entries
            .iter()
            .find(|e| e.seq == (id & 0xFFFF_FFFF) as u32)
            .map(|e| e.footprint_seq)
    };
    // (id, superseded now, demoted now).
    let mut usable_ids: Vec<(u64, bool, bool)> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut seqs: Vec<u64> = Vec::new();
    let mut footprints: Vec<Vec<(u64, u64)>> = Vec::new();
    for r in live.iter().filter(|r| r.id.get() != removing) {
        let mut f = vec![(r.header_at[0], block), (r.header_at[1], block)];
        f.extend(r.map.extents());
        // A live region's entry is always on its page under the metadata
        // lock; were it not, judging it is not possible: refused.
        let Some(seq) = seq_of(r.id.get()) else {
            return Some(r.id);
        };
        usable_ids.push((r.id.get(), r.superseded, false));
        names.push(r.name.as_str().to_owned());
        seqs.push(seq);
        footprints.push(f);
    }
    drop(live);
    for h in v2.holds.iter().filter(|h| h.id() != removing) {
        if let Some((name, ranges)) = &h.demoted {
            usable_ids.push((h.id(), false, true));
            names.push(name.as_str().to_owned());
            seqs.push(h.footprint_seq);
            footprints.push(ranges.clone());
        }
    }
    let usable: Vec<Usable<'_>> = (0..usable_ids.len())
        .map(|i| Usable {
            name: &names[i],
            seq: seqs[i],
            ranges: &footprints[i],
        })
        .collect();
    let gone: Vec<Gone<'_>> = v2
        .holds
        .iter()
        .filter(|h| h.id() != removing && h.demoted.is_none())
        .map(|h| Gone {
            seq: h.footprint_seq,
            own: &h.own,
            name: h
                .entry_name
                .as_ref()
                .map(store_io_format::meta::RegionName::as_str),
        })
        .collect();
    let again = settle(&usable, &gone);
    usable_ids
        .iter()
        .zip(again)
        .find(|((_, superseded, demoted), s)| {
            (*demoted && !(s.overlapped || s.renamed)) || (*superseded && !s.superseded)
        })
        .map(|((id, _, _), _)| RegionId::from_raw(*id))
}

impl<P: Platform> Store<P>
where
    P::Queue: Send,
{
    /// Releases the table entry of an unavailable region, one that
    /// [`Self::unavailable_regions`] lists (or, named by its id
    /// `page << 32 | seq`, an entry of a kind this version does not know,
    /// `report().metadata.invalid_entries`: given up deliberately). The
    /// entry's page is written without it (durable on return); the region's
    /// bytes are not touched, and its space becomes free where no other
    /// region's footprint lies. Its name is free again; the repair is
    /// recorded ([`Self::repairs`]). Container format 2.
    ///
    /// # Errors
    ///
    /// `NotFound(Region)` if `id` names no unavailable entry (a usable
    /// region is released with `release`); `NotWritten(Overlap)`, naming a
    /// region whose open-time verdict rests on this entry (spec §9 step 4:
    /// a live region it supersedes, which may read another's bytes, or an
    /// unavailable one it demoted by an overlap or a name): the entry stays
    /// while that region does (repair a demoted one first; a superseded
    /// one is read-only and cannot be released: the way out is a copy of
    /// the store that leaves unavailable entries out, `copy_store`'s
    /// `accept_stale_pages`, v0.5 E6 C4); `NotWritten(Frozen)` also for a page holding an entry
    /// past its own write that would be kept (repair that one first, when
    /// it can be: one that supersedes a live region cannot, and two on one
    /// page block each other; the way out is then the copy);
    /// `NotWritten(Frozen)` while
    /// allocation is frozen (a table page is stale or lost: free space
    /// cannot be trusted); `NotWritten(ReadOnly)`; `Unsupported(FormatVersion)`
    /// for format 1; `Poisoned`; `DurabilityUnknown` if the page write
    /// failed after submission (the store is then poisoned; the entry may or
    /// may not be gone at the next open).
    pub fn repair_unavailable(&self, id: RegionId) -> Result<(), Error> {
        self.ready()?;
        let inner = &self.inner;
        let ctx = ErrorContext {
            region: Some(id),
            ..ErrorContext::default()
        };
        if inner.layout.format != store_io_format::slot::FORMAT_2 {
            return Err(Error::Unsupported {
                what: Capability::FormatVersion,
            });
        }
        if inner.frozen.load(std::sync::atomic::Ordering::Acquire) {
            return Err(not_written(NotWrittenCause::Frozen, ctx));
        }
        let mut meta = lock_meta(&inner.meta);
        let (p, payload, at) = {
            let v2 = meta.v2.as_deref().ok_or_else(corrupt)?;
            let Some(at) = v2.holds.iter().position(|h| h.id() == id.get()) else {
                return Err(Error::NotFound {
                    what: Named::Region,
                });
            };
            let hold = &v2.holds[at];
            if let Some(region) = crate::repair::would_lift(self, v2, id.get()) {
                return Err(not_written(
                    NotWrittenCause::Overlap,
                    ErrorContext {
                        region: Some(region),
                        ..ErrorContext::default()
                    },
                ));
            }
            let page = v2.pages.get(hold.page).ok_or_else(corrupt)?;
            if page.standing != Standing::Confirmed {
                return Err(not_written(NotWrittenCause::Frozen, ctx));
            }
            let ph = page.header.ok_or_else(corrupt)?;
            let mut raws = page.entries.clone();
            let i = raws
                .iter()
                .position(|e| e.seq == hold.seq)
                .ok_or_else(corrupt)?;
            let _removed = raws.remove(i);
            let next = PageHeader {
                page_index: ph.page_index,
                // Never decreases: the id is never handed out again.
                next_seq: ph.next_seq,
                count: raws.len() as u16,
            };
            let mut buf = vec![0u8; PAGE_HEADER_LEN + 96 * raws.len()];
            let n = encode_page(&mut buf, &next, &raws).map_err(|_| corrupt())?;
            buf.truncate(n);
            (hold.page, buf, at)
        };
        // The commit.
        let meta_seq = crate::v2::next_meta_seq_of(&meta)?;
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
            )?;
        }
        // Committed: memory follows. Exactly what the open added for the
        // entry is taken away; free space is the complement again.
        let len = meta.container_len;
        let v2 = meta.v2.as_deref_mut().ok_or_else(corrupt)?;
        let hold = v2.holds.swap_remove(at);
        if let Some(span) = hold.span {
            if let Some(i) = v2.allocated.iter().position(|a| *a == span) {
                let _gone = v2.allocated.swap_remove(i);
            }
        }
        if let Some(usage) = hold.usage {
            if let Some(i) = v2.usage.iter().position(|u| *u == usage) {
                let _gone = v2.usage.swap_remove(i);
            }
            crate::space::lock_acct(&inner.acct).remove_usage(usage.0, usage.1, usage.2);
        }
        if let Some(name) = &hold.name {
            if let Some(i) = v2.reserved_names.iter().position(|n| n == name) {
                let _gone = v2.reserved_names.swap_remove(i);
            }
        }
        // Growth was refused while an entry named space past the end.
        v2.cut_short = v2.holds.iter().any(|h| h.short);
        v2.rebuild_free(len);
        let record = match meta.unavailable.iter().position(|u| u.id == id) {
            Some(i) => Repair::EntryReleased {
                region: meta.unavailable.remove(i),
            },
            None => Repair::InvalidEntryReleased {
                page: (id.get() >> 32) as u32,
                seq: (id.get() & 0xFFFF_FFFF) as u32,
            },
        };
        meta.repairs.push(record);
        Ok(())
    }

    /// Accepts the rollback of stale table pages (spec §12): for each page
    /// of `pages` that is Unconfirmed (`report().metadata.stale_pages`),
    /// its winner is rewritten byte for byte into its failed slot (even an
    /// unreadable one: writing usually remaps the sector) at the next
    /// `meta_seq`, durably, and read back; once the new copy reads back as
    /// the winner the page counts as Confirmed: whatever its lost commits
    /// held is given up (space they used may be handed out again) and the
    /// extra extents counted allocated for it are free. Every page is
    /// checked before any is written (an error from a check accepts
    /// none). Each accepted page is recorded ([`Self::repairs`]); a
    /// Confirmed page is left as it is.
    ///
    /// Once no page is stale or lost, allocation is no longer frozen
    /// ([`Self::frozen`]) and the regions become writable (not one possibly
    /// superseded, or with a header feature this version may not write).
    /// Not before: a region of an accepted page may be one a lost commit
    /// released, its space then given to a region of another stale page's
    /// lost commit, whose bytes it would overwrite. The same holds across
    /// a reopen: an accepted page's two slots then hold the same payload
    /// (as after a heal), and while any page is stale or lost the open
    /// keeps such a page's regions read-only (also once its older copy is
    /// damaged: any Confirmed page with a damaged older copy counts). A store with a Lost page
    /// stays frozen, the regions of the pages accepted read-only: it is
    /// copied (`copy_store`). An accepted page's `next_seq` counts higher
    /// (raised by the open that found it stale and by each open while its
    /// trace lasts, written by its next commit): the region ids its lost
    /// commit gave out are not given out
    /// again (unless more than 4,096 provisionings failed on that page
    /// between its last two commits). Container format 2.
    ///
    /// # Errors
    ///
    /// `NotWritten(NotReady)` for a Lost page, its slots named (no winner
    /// to keep); `NotWritten(OutOfBounds)` for a page the table does not
    /// have; `NotWritten(Frozen)` for a page holding an entry whose
    /// `footprint_seq` is past the page's own write, the entry named (the
    /// rewrite would change how the next open counts it, and no repair
    /// runs while allocation is frozen: such a store is copied).
    /// `Corruption(MediaError)` if the page does not read back with both
    /// slots Valid and the same, and `Io` if it does not read: the write was
    /// durable, so the store is poisoned (what the next open finds is the
    /// page's state: Confirmed if the new copy reads, else stale, to be
    /// accepted again). `NotWritten(ReadOnly)`; `Unsupported(FormatVersion)`
    /// for format 1; `Poisoned`; `DurabilityUnknown` if the write failed
    /// after submission (the store poisoned; at the next open the page is
    /// still stale, or Confirmed if the new copy reached the media).
    pub fn accept_page_rollback(&self, pages: &[u32]) -> Result<(), Error> {
        self.ready()?;
        let inner = &self.inner;
        if inner.layout.format != store_io_format::slot::FORMAT_2 {
            return Err(Error::Unsupported {
                what: Capability::FormatVersion,
            });
        }
        let mut meta = lock_meta(&inner.meta);
        let mut todo: Vec<(u32, usize)> = Vec::new();
        {
            let v2 = meta.v2.as_deref().ok_or_else(corrupt)?;
            for &page in pages {
                let p = usize::try_from(page).map_err(|_| corrupt())?;
                let Some(pg) = v2.pages.get(p) else {
                    return Err(not_written(
                        NotWrittenCause::OutOfBounds,
                        ErrorContext::default(),
                    ));
                };
                match pg.standing {
                    Standing::Confirmed => continue,
                    Standing::Unconfirmed => {}
                    Standing::Lost | Standing::Refused => {
                        return Err(not_written(
                            NotWrittenCause::NotReady,
                            ErrorContext {
                                range: Some(crate::v2::page_range(&inner.layout, page)),
                                ..ErrorContext::default()
                            },
                        ));
                    }
                }
                if let Some((seq, _)) = pg.ahead().next() {
                    return Err(not_written(
                        NotWrittenCause::Frozen,
                        ErrorContext {
                            region: Some(RegionId::from_raw(
                                (u64::from(page) << 32) | u64::from(seq),
                            )),
                            ..ErrorContext::default()
                        },
                    ));
                }
                if !todo.iter().any(|&(_, at)| at == p) {
                    todo.push((page, p));
                }
            }
        }
        for (page, p) in todo {
            {
                let mut q = inner.queues.get(0);
                crate::v2::accept_page::<P>(
                    &mut q,
                    &inner.file,
                    &inner.pool,
                    &inner.layout,
                    inner.volume,
                    &inner.policy,
                    &inner.domain,
                    &mut meta,
                    p,
                    inner.owner_generation,
                )?;
            }
            // Confirmed: the extra extents counted for it are free (a
            // growth beyond capacity that never committed), free space is
            // the complement again.
            let len = meta.container_len;
            let v2 = meta.v2.as_deref_mut().ok_or_else(corrupt)?;
            let freed: Vec<(u64, u64)> = v2
                .stale_extra
                .iter()
                .filter(|(at, _, _)| *at == p)
                .map(|&(_, _, r)| r)
                .collect();
            v2.stale_extra.retain(|(at, _, _)| *at != p);
            for range in &freed {
                if let Some(i) = v2.allocated.iter().position(|a| a == range) {
                    let _gone = v2.allocated.swap_remove(i);
                }
            }
            v2.rebuild_free(len);
            meta.repairs.push(Repair::PageAccepted { page });
        }
        // No page stale or lost: every region the open served read-only for
        // its page is writable, and allocation unfrozen.
        let v2 = meta.v2.as_deref().ok_or_else(corrupt)?;
        if v2.pages.iter().all(|pg| pg.standing == Standing::Confirmed) {
            for r in inner
                .regions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter(|r| !r.superseded && !r.foreign)
            {
                r.read_only
                    .store(false, std::sync::atomic::Ordering::Release);
            }
            inner
                .frozen
                .store(false, std::sync::atomic::Ordering::Release);
        }
        Ok(())
    }
}
