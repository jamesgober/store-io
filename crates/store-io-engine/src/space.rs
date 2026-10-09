//! Space: reservations, hard caps and per-tag accounting.
//!
//! A [`Reservation`] sets space aside before it is needed: `reserve` checks
//! the tag's hard cap and grows the container so that provisioning from the
//! reservation can never run out of space. It is the only place where a full
//! file system or quota surfaces as [`Error::NoSpace`], and nothing is ever
//! poisoned by it.
//!
//! The container always holds every region and every outstanding
//! reservation: `reserve` grows it to keep that true, provisioning from a
//! reservation turns reserved bytes into used bytes without allocating, and
//! provisioning without one grows the container past the outstanding
//! reservations so it can never take space promised to someone else.
//!
//! Tags are opaque to store-io (a tenant, a class). Each region records its
//! tag in the region table, so per-tag usage is derived again on every open;
//! caps and outstanding reservations live in memory.

use std::sync::{MutexGuard, PoisonError};

use store_io_core::error::{Error, ErrorContext, NotWrittenCause, ReserveTag};
use store_io_format::meta::{RegionKind, RegionState, TableEntry};
use store_io_platform::Platform;

use crate::io::not_written;
use crate::region::{AppendRegion, PageRegion, Slot};
use crate::store::{Meta, Store, lock_meta};

/// Usage of one tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TagSpace {
    /// The tag.
    pub tag: u32,
    /// Data bytes of the tag's ready regions.
    pub logical: u64,
    /// Bytes the tag's ready regions occupy in the container, headers
    /// included.
    pub physical: u64,
    /// Bytes reserved for the tag and not yet provisioned.
    pub reserved: u64,
    /// The tag's hard cap on `physical + reserved`, if any.
    pub cap: Option<u64>,
}

/// Where a store's space goes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SpaceReport {
    /// Bytes the container file holds.
    pub container: u64,
    /// Bytes held by ready regions, headers included.
    pub regions: u64,
    /// Bytes of released regions: reused by a later region of exactly the
    /// same size.
    pub released: u64,
    /// Bytes reserved and not yet provisioned.
    pub reserved: u64,
    /// Usage per tag, in tag order.
    pub tags: Vec<TagSpace>,
}

/// In-memory accounting, guarded by `Inner::acct`. Lock order: `meta`, then
/// `acct`.
#[derive(Debug, Default)]
pub(crate) struct Accounting {
    tags: Vec<TagSpace>,
    reserved: u64,
}

impl Accounting {
    /// Usage derived from the ready entries of a region table.
    pub(crate) fn from_entries(entries: &[TableEntry], block: u64) -> Self {
        let mut a = Self::default();
        for e in entries.iter().filter(|e| e.state == RegionState::Ready) {
            a.add(e, block);
        }
        a
    }

    fn tag(&mut self, tag: u32) -> &mut TagSpace {
        let i = match self.tags.binary_search_by_key(&tag, |t| t.tag) {
            Ok(i) => i,
            Err(i) => {
                self.tags.insert(
                    i,
                    TagSpace {
                        tag,
                        ..TagSpace::default()
                    },
                );
                i
            }
        };
        &mut self.tags[i]
    }

    /// Counts a newly ready region.
    pub(crate) fn add(&mut self, e: &TableEntry, block: u64) {
        let physical = e.extent_len(block).unwrap_or(u64::MAX);
        let t = self.tag(e.tag);
        t.logical = t.logical.saturating_add(e.data_size);
        t.physical = t.physical.saturating_add(physical);
    }

    /// Stops counting a released region.
    pub(crate) fn remove(&mut self, e: &TableEntry, block: u64) {
        let physical = e.extent_len(block).unwrap_or(0);
        let t = self.tag(e.tag);
        t.logical = t.logical.saturating_sub(e.data_size);
        t.physical = t.physical.saturating_sub(physical);
    }

    /// Whether `extra` more bytes fit under the tag's cap.
    pub(crate) fn fits(&mut self, tag: u32, extra: u64) -> bool {
        let t = self.tag(tag);
        t.cap.is_none_or(|cap| {
            t.physical
                .checked_add(t.reserved)
                .and_then(|u| u.checked_add(extra))
                .is_some_and(|u| u <= cap)
        })
    }

    /// Bytes reserved and not yet provisioned, all tags.
    pub(crate) fn reserved(&self) -> u64 {
        self.reserved
    }

    /// Moves `bytes` of `tag` from reserved to provisioned (or back to free
    /// when a reservation is dropped).
    pub(crate) fn unreserve(&mut self, tag: u32, bytes: u64) {
        self.reserved = self.reserved.saturating_sub(bytes);
        let t = self.tag(tag);
        t.reserved = t.reserved.saturating_sub(bytes);
    }

    fn reserve(&mut self, tag: u32, bytes: u64) {
        self.reserved = self.reserved.saturating_add(bytes);
        let t = self.tag(tag);
        t.reserved = t.reserved.saturating_add(bytes);
    }
}

pub(crate) fn lock_acct(a: &std::sync::Mutex<Accounting>) -> MutexGuard<'_, Accounting> {
    a.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn no_space(tag: u32) -> Error {
    Error::NoSpace {
        tag: ReserveTag::Caller(tag),
        ctx: ErrorContext::default(),
    }
}

/// Space set aside for provisioning under one tag. Regions provisioned from
/// it can never fail for lack of space; whatever is left when it is dropped
/// returns to the free pool.
pub struct Reservation<P: Platform>
where
    P::Queue: Send,
{
    store: Store<P>,
    tag: u32,
    bytes: u64,
}

impl<P: Platform> core::fmt::Debug for Reservation<P>
where
    P::Queue: Send,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Reservation")
            .field("tag", &self.tag)
            .field("remaining", &self.bytes)
            .finish()
    }
}

impl<P: Platform> Drop for Reservation<P>
where
    P::Queue: Send,
{
    fn drop(&mut self) {
        lock_acct(&self.store.inner.acct).unreserve(self.tag, self.bytes);
    }
}

impl<P: Platform> Reservation<P>
where
    P::Queue: Send,
{
    /// The reservation's tag.
    #[must_use]
    pub fn tag(&self) -> u32 {
        self.tag
    }

    /// Bytes still reserved. A region uses its data size plus two header
    /// blocks (see [`Store::extent_of`]).
    #[must_use]
    pub fn remaining(&self) -> u64 {
        self.bytes
    }

    /// Takes `extent` bytes for a region being provisioned.
    pub(crate) fn consume(&mut self, extent: u64) -> Result<(), Error> {
        if extent > self.bytes {
            return Err(no_space(self.tag));
        }
        self.bytes -= extent;
        lock_acct(&self.store.inner.acct).unreserve(self.tag, extent);
        Ok(())
    }

    /// Provisions an append region from this reservation.
    ///
    /// # Errors
    ///
    /// As [`Store::provision_append_region`]; `NoSpace` if the reservation
    /// is too small.
    pub fn provision_append_region(
        &mut self,
        name: &str,
        size: u64,
    ) -> Result<AppendRegion<P>, Error> {
        let store = self.store.clone();
        let state = store.provision(name, RegionKind::Append, size, Some(self))?;
        Ok(AppendRegion::from_parts(store, state))
    }

    /// Provisions a page region from this reservation.
    ///
    /// # Errors
    ///
    /// As [`Store::provision_page_region`]; `NoSpace` if the reservation is
    /// too small.
    pub fn provision_page_region(&mut self, name: &str, size: u64) -> Result<PageRegion<P>, Error> {
        let store = self.store.clone();
        let state = store.provision(name, RegionKind::Page, size, Some(self))?;
        Ok(PageRegion::from_parts(store, state))
    }

    /// Provisions a small-object slot from this reservation.
    ///
    /// # Errors
    ///
    /// As [`Store::provision_slot`]; `NoSpace` if the reservation is too
    /// small.
    pub fn provision_slot(&mut self, name: &str) -> Result<Slot<P>, Error> {
        let store = self.store.clone();
        let state = store.provision(name, RegionKind::Slot, 0, Some(self))?;
        Ok(Slot::from_parts(store, state))
    }
}

impl<P: Platform> Store<P>
where
    P::Queue: Send,
{
    /// Bytes a region of `data_size` data bytes occupies in the container:
    /// the data size rounded up to the block, plus two header blocks.
    #[must_use]
    pub fn extent_of(&self, data_size: u64) -> Option<u64> {
        let block = self.block();
        store_io_core::align::align_up(data_size, block).and_then(|d| self.inner.layout.extent(d))
    }

    /// Sets (or with `None` removes) the hard cap of `tag`: the most bytes
    /// its regions and reservations may hold together. Caps live in memory;
    /// set them again after every open.
    pub fn set_cap(&self, tag: u32, cap: Option<u64>) {
        lock_acct(&self.inner.acct).tag(tag).cap = cap;
    }

    /// Reserves `bytes` for later provisioning under `tag`: checks the tag's
    /// cap and grows the container so that provisioning from the reservation
    /// cannot run out of space.
    ///
    /// # Errors
    ///
    /// `NoSpace { tag }` if the cap or the file system is exceeded (nothing
    /// changes and nothing is poisoned); `NotWritten(ReadOnly)`; `Poisoned`;
    /// `Io` for other allocation failures.
    pub fn reserve(&self, bytes: u64, tag: u32) -> Result<Reservation<P>, Error> {
        self.ready()?;
        let inner = &self.inner;
        let mut meta = lock_meta(&inner.meta);
        let mut acct = lock_acct(&inner.acct);
        if !acct.fits(tag, bytes) {
            return Err(no_space(tag));
        }
        let target = self
            .tail(&meta)
            .checked_add(acct.reserved())
            .and_then(|t| t.checked_add(bytes))
            .ok_or_else(|| not_written(NotWrittenCause::TooLarge, ErrorContext::default()))?;
        self.grow_to(&mut meta, target, tag)?;
        acct.reserve(tag, bytes);
        Ok(Reservation {
            store: self.clone(),
            tag,
            bytes,
        })
    }

    /// Where space goes: the container, regions, released extents,
    /// reservations, and usage per tag.
    #[must_use]
    pub fn space(&self) -> SpaceReport {
        let inner = &self.inner;
        let meta = lock_meta(&inner.meta);
        let acct = lock_acct(&inner.acct);
        let block = self.block();
        let sum = |state| {
            meta.entries
                .iter()
                .filter(|e| e.state == state)
                .filter_map(|e| e.extent_len(block))
                .sum()
        };
        SpaceReport {
            container: meta.container_len,
            regions: sum(RegionState::Ready),
            released: sum(RegionState::Released),
            reserved: acct.reserved(),
            tags: acct.tags.clone(),
        }
    }

    /// The end of the last extent in the table (the start of free tail
    /// space).
    pub(crate) fn tail(&self, meta: &Meta) -> u64 {
        self.inner.layout.tail(&meta.entries)
    }

    /// Grows the container to at least `target` bytes.
    pub(crate) fn grow_to(&self, meta: &mut Meta, target: u64, tag: u32) -> Result<(), Error> {
        if target <= meta.container_len {
            return Ok(());
        }
        let inner = &self.inner;
        inner
            .platform
            .allocate(&inner.file, target)
            .map_err(|raw| crate::store::space_err(store_io_core::error::Op::Allocate, raw, tag))?;
        meta.container_len = target;
        Ok(())
    }
}
