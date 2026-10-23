//! Region handles: [`AppendRegion`], [`PageRegion`] and [`Slot`].
//!
//! Every handle is cheap to clone and safe to share between threads. Calls
//! block until their I/O is done.

use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use store_io_core::error::{Error, FirstCause, Named, NotWrittenCause, Op};
use store_io_core::id::Generation;
use store_io_format::meta::RegionKind;
use store_io_platform::Platform;

use crate::batch::{AppendBatch, PageBatch};
use crate::gate::Pass;
use crate::io::{ctx, ctx_range, not_written};
use crate::lifecycle::retired_error;
use crate::receipt::{DurableReceipt, RegionPos, WriteTicket};
use crate::scan::{ScanItem, ScanSummary};
use crate::settle::{Wrote, settle_pumped};
use crate::store::{LiveRegion, Store};

pub use crate::slot_handle::Slot;

/// Region lookups and provisioning.
impl<P: Platform> Store<P>
where
    P::Queue: Send,
{
    /// Looks up an existing append region.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if no ready append region has that name.
    pub fn append_region(&self, name: &str) -> Result<AppendRegion<P>, Error> {
        let state = self.find(name, RegionKind::Append, Named::Region)?;
        Ok(AppendRegion {
            store: self.clone(),
            state,
        })
    }

    /// Provisions a new append region of at least `size` bytes.
    ///
    /// # Errors
    ///
    /// [`Error::AlreadyExists`], [`Error::NoSpace`], or a platform error.
    pub fn provision_append_region(&self, name: &str, size: u64) -> Result<AppendRegion<P>, Error> {
        let state = self.provision(name, RegionKind::Append, size, None)?;
        Ok(AppendRegion {
            store: self.clone(),
            state,
        })
    }

    /// Looks up an existing page region.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if no ready page region has that name.
    pub fn page_region(&self, name: &str) -> Result<PageRegion<P>, Error> {
        let state = self.find(name, RegionKind::Page, Named::Region)?;
        Ok(PageRegion {
            store: self.clone(),
            state,
        })
    }

    /// Provisions a new page region of at least `size` bytes.
    ///
    /// # Errors
    ///
    /// [`Error::AlreadyExists`], [`Error::NoSpace`], or a platform error.
    pub fn provision_page_region(&self, name: &str, size: u64) -> Result<PageRegion<P>, Error> {
        let state = self.provision(name, RegionKind::Page, size, None)?;
        Ok(PageRegion {
            store: self.clone(),
            state,
        })
    }

    /// Looks up an existing small-object slot.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if no slot has that name.
    pub fn slot(&self, name: &str) -> Result<Slot<P>, Error> {
        let state = self.find(name, RegionKind::Slot, Named::Slot)?;
        Ok(Slot::from_parts(self.clone(), state))
    }

    /// Provisions a new small-object slot. Its capacity is the block size
    /// minus 176 bytes of headers ([`Slot::capacity`]).
    ///
    /// # Errors
    ///
    /// [`Error::AlreadyExists`], [`Error::NoSpace`], or a platform error.
    pub fn provision_slot(&self, name: &str) -> Result<Slot<P>, Error> {
        let state = self.provision(name, RegionKind::Slot, 0, None)?;
        Ok(Slot::from_parts(self.clone(), state))
    }
}

// ---------------------------------------------------------------------------
// Append regions
// ---------------------------------------------------------------------------

/// A region store-io appends to. Positions are chosen by store-io from an
/// atomic tail, so appends from many threads never overlap and a written
/// block is never rewritten.
///
/// Each append occupies whole blocks: the bytes from the end of the data to
/// the next block boundary are zero and are not caller data. Pack small
/// records into one append (or use a batch) to avoid the padding.
pub struct AppendRegion<P: Platform> {
    store: Store<P>,
    state: Arc<LiveRegion>,
}

impl<P: Platform> AppendRegion<P> {
    pub(crate) fn from_parts(store: Store<P>, state: Arc<LiveRegion>) -> Self {
        Self { store, state }
    }
}

impl<P: Platform> Clone for AppendRegion<P> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            state: Arc::clone(&self.state),
        }
    }
}

impl<P: Platform> AppendRegion<P>
where
    P::Queue: Send,
{
    fn frontier(&self) -> Result<&crate::frontier::AppendFrontier, Error> {
        self.state
            .frontier
            .as_ref()
            .ok_or_else(|| not_written(NotWrittenCause::NotReady, ctx(&self.state)))
    }

    /// The checks every append makes before touching the device, and the
    /// pass that keeps a recycle or release out until the append is done.
    pub(crate) fn writable(&self) -> Result<(&crate::frontier::AppendFrontier, Pass<'_>), Error> {
        let pass = self.pass()?;
        self.store.ready_for(&self.state)?;
        let f = self.frontier()?;
        if !self.state.positioned.load(Ordering::Acquire) {
            return Err(not_written(
                NotWrittenCause::NotPositioned,
                ctx(&self.state),
            ));
        }
        Ok((f, pass))
    }

    fn pass(&self) -> Result<Pass<'_>, Error> {
        self.state
            .gate
            .enter()
            .map_err(|why| retired_error(why, &self.state))
    }

    /// The generation this handle writes and reads. Recycling starts the
    /// next one; embed it in your records to tell generations apart after a
    /// crash.
    #[must_use]
    pub fn generation(&self) -> Generation {
        self.state.generation
    }

    /// Starts a new generation: waits for every operation in flight on this
    /// region, rewrites the region header with the next generation, makes it
    /// durable, and returns a handle to the new generation with its append
    /// position at 0. No other I/O: the old bytes stay on disk, so records
    /// must carry their generation. Every handle, ticket and position of the
    /// old generation is refused from then on (`StaleGeneration`).
    ///
    /// Must not be called from inside this region's own scan visitor, nor by
    /// a thread whose own [`crate::Submitter`] has operations on this region
    /// it has not reaped: recycle waits for them, and only that thread's
    /// `reap` or `wait` finishes them (the same holds for `release`).
    ///
    /// # Errors
    ///
    /// `NotWritten(StaleGeneration | NotReady)` if the region was already
    /// recycled or released; `Io` if the store's queue could not be prepared
    /// (nothing was written; the region stays usable); `Poisoned`;
    /// `DurabilityUnknown`.
    pub fn recycle(&self) -> Result<Self, Error> {
        let state = self.store.recycle_region(&self.state)?;
        Ok(Self {
            store: self.store.clone(),
            state,
        })
    }

    /// Releases the region: waits for every operation in flight, marks it
    /// released in the region table and its header (both durable), and gives
    /// its space back to the file system. Its contents are undefined from
    /// then on; every handle to it is refused (`NotReady`), and its space is
    /// reused by later provisioning. Not from a thread with unreaped
    /// [`crate::Submitter`] operations on it (see [`Self::recycle`]).
    ///
    /// # Errors
    ///
    /// `NotWritten(StaleGeneration | NotReady)`, `Poisoned`,
    /// `DurabilityUnknown`, or `Io` if the space could not be deallocated
    /// (the region is released either way).
    pub fn release(self) -> Result<(), Error> {
        self.store.release_region(&self.state)
    }

    pub(crate) fn store(&self) -> &Store<P> {
        &self.store
    }

    pub(crate) fn state(&self) -> &Arc<LiveRegion> {
        &self.state
    }

    /// Appends `data` (any length of at least one byte) and returns a ticket
    /// once the write has completed. The data is not yet durable: pass the
    /// ticket to [`Self::sync_through`], or use [`Self::append_durable`].
    ///
    /// The append starts at a block boundary chosen by store-io and is
    /// zero-padded to the next one. Data larger than the largest pooled
    /// buffer is written in several pieces, kept in flight together.
    ///
    /// # Errors
    ///
    /// `NotWritten` (empty data, region full, pool exhausted, read-only, not
    /// positioned), `Io` if the store's queue could not be prepared (nothing
    /// was reserved or written), `Poisoned`, or `DurabilityUnknown` (the
    /// domain is now poisoned).
    pub fn append(&self, data: &[u8]) -> Result<WriteTicket, Error> {
        let s = &self.store;
        let (f, _pass) = self.writable()?;
        let c = ctx(&self.state);
        if data.is_empty() {
            return Err(not_written(NotWrittenCause::Empty, c));
        }
        // Everything that can fail without touching the device happens first:
        // the first piece is staged before any space is reserved.
        let mut stream = s.stage(data, c)?;
        let mut lane = s.lane(&self.state, Op::Write)?;
        let reserved = s.reserve_append(f, data.len() as u64, c)?;
        let r = reserved.range();
        stream.at(self.state.map.locate(r.start, r.end - r.start));
        // Until the pump returns the guard poisons on any exit (a panic
        // included); then the settle step completes the reservation, or
        // poisons: past it every failure leaves a hole.
        let outcome = s.pump(&mut lane, &mut stream);
        let r = reserved.disarm();
        settle_pumped(s, &self.state, Wrote::Append(r), outcome)
    }

    /// Makes everything appended up to and including `ticket` durable.
    ///
    /// # Errors
    ///
    /// `NotWritten(ForeignPosition | StaleGeneration)` for a ticket of another
    /// region or generation; `Poisoned`; `DurabilityUnknown` if the flush
    /// failed.
    pub fn sync_through(&self, ticket: &WriteTicket) -> Result<DurableReceipt, Error> {
        let s = &self.store;
        let f = self.frontier()?;
        let c = ctx(&self.state);
        self.state
            .gate
            .check()
            .map_err(|why| retired_error(why, &self.state))?;
        if ticket.epoch != s.inner.epoch
            || ticket.volume != s.inner.volume
            || ticket.region != self.state.id
        {
            return Err(not_written(NotWrittenCause::ForeignPosition, c));
        }
        if ticket.generation != self.state.generation {
            return Err(not_written(NotWrittenCause::StaleGeneration, c));
        }
        // Already durable: no barrier. A forced region on a power-safe store
        // counts only what a data flush made durable (D-102).
        let flushes = s.inner.policy.flush;
        let synced = flushes || self.state.forced();
        let durable = if synced && !flushes {
            f.synced()
        } else {
            f.durable()
        };
        if durable >= ticket.end {
            s.inner.domain.count_already_durable();
            return Ok(s.receipt(&self.state, 0, durable, 0, true, synced));
        }
        let domain = &s.inner.domain;
        if !f.wait_completed(ticket.end, || domain.poisoned().is_some()) {
            return Err(Error::Poisoned {
                first: domain.poisoned().unwrap_or(FirstCause {
                    op: None,
                    raw: None,
                }),
            });
        }
        // Snapshot the completed prefix BEFORE the barrier takes its ticket.
        let through = f.completed();
        let (need, synced) = s.barrier_for(&self.state)?;
        if synced {
            f.publish_synced(through);
        } else {
            f.publish_durable(through);
        }
        Ok(s.receipt(&self.state, 0, through, need, true, synced))
    }

    /// Appends `data` and makes it durable before returning.
    ///
    /// # Errors
    ///
    /// As [`Self::append`] and [`Self::sync_through`].
    pub fn append_durable(&self, data: &[u8]) -> Result<(RegionPos, DurableReceipt), Error> {
        let t = self.append(data)?;
        let pos = t.pos();
        let r = self.sync_through(&t)?;
        Ok((pos, r))
    }

    /// A batch: many records packed back to back, written together and made
    /// durable with one barrier. See [`AppendBatch`].
    #[must_use]
    pub fn batch(&self) -> AppendBatch<P> {
        AppendBatch::new(self.clone())
    }

    /// Reads raw bytes at a block-aligned offset into `out` (any length).
    /// Returns the bytes read, fewer than `out.len()` only at the end of the
    /// region's data area.
    ///
    /// # Errors
    ///
    /// `NotWritten` for a misaligned or out-of-range read;
    /// `Corruption(MediaError)` if the device reports an unreadable range,
    /// with the exact range in the error's context; `Io` with the raw code
    /// for any other failure.
    pub fn read(&self, offset: u64, out: &mut [u8]) -> Result<usize, Error> {
        let _pass = self.pass()?;
        self.store.read_at(&self.state, offset, out)
    }

    /// Reads the region in order from the block-aligned offset `from` to the
    /// end of its data area, handing every piece to `visit`; return
    /// [`ControlFlow::Break`] to stop (typically where your valid data ends).
    ///
    /// The scan reads with direct I/O and several large reads in flight. It
    /// never stops at bad data: a range the device cannot read is narrowed
    /// to whole sectors (the device's logical blocks where the probe
    /// reported the direct-I/O alignment, else store blocks), reported as
    /// [`ScanItem::Unreadable`], and skipped.
    /// Scans work on read-only and poisoned stores.
    ///
    /// `visit` runs while the scan holds one of the store's queues. A call
    /// from `visit` into the same store takes another queue, and waits for
    /// one when all are busy: if every queue is held by such a scan (one scan
    /// is enough with `StoreOptions::queues` = 1) it waits forever. Collect
    /// what you need and act on the store after the scan returns.
    ///
    /// # Errors
    ///
    /// `NotWritten(Misaligned | OutOfBounds | PoolExhausted)` before any I/O;
    /// `Io` if a read fails for a reason other than the media.
    pub fn scan(
        &self,
        from: u64,
        visit: impl FnMut(ScanItem<'_>) -> ControlFlow<()>,
    ) -> Result<ScanSummary, Error> {
        let _pass = self.pass()?;
        self.store
            .scan_reported(&self.state, from, self.state.map.len(), visit)
    }

    /// Moves the append position forward to `offset` (rounded up to a block),
    /// typically after recovery found where valid data ends. Never moves
    /// backwards, and only while no append is in flight.
    ///
    /// The skipped range counts as durable: a writable open flushed the
    /// device before reading anything, so whatever recovery found is on
    /// media. Bytes appended in this open and not yet made durable never do:
    /// then the durable point stays where it is, and the next sync that
    /// flushes covers both (`durable_through` reads lower until then).
    /// A read-only open flushes nothing, so there `resume_at` is refused
    /// (D-072); appends are refused there anyway, and so is a poisoned
    /// store.
    ///
    /// # Errors
    ///
    /// `NotWritten(ReadOnly | TooManyInFlight | OutOfBounds)`, `Poisoned`.
    pub fn resume_at(&self, offset: u64) -> Result<u64, Error> {
        if self.store.inner.read_only || self.state.read_only {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::ReadOnly,
                ctx: ctx(&self.state),
            });
        }
        // A poison landing after this check is harmless: a failed append
        // leaves the prefix below the tail (refused below), a failed flush
        // leaves the durable point below it (not raised).
        if let Some(first) = self.store.inner.domain.poisoned() {
            return Err(Error::Poisoned { first });
        }
        let _pass = self.pass()?;
        let at = self
            .frontier()?
            .resume_at(offset)
            .map_err(|cause| Error::NotWritten {
                cause,
                ctx: ctx(&self.state),
            })?;
        self.state
            .positioned
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(at)
    }

    /// Where the next append will start.
    #[must_use]
    pub fn tail(&self) -> u64 {
        self.state
            .frontier
            .as_ref()
            .map_or(0, crate::frontier::AppendFrontier::tail)
    }

    /// Every byte before this offset is durable.
    #[must_use]
    pub fn durable_through(&self) -> u64 {
        self.state
            .frontier
            .as_ref()
            .map_or(0, crate::frontier::AppendFrontier::durable)
    }

    /// Data-area size in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.state.map.len()
    }

    /// Whether the data area is empty (never for a provisioned region).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.state.map.len() == 0
    }

    /// Block size: appends are padded to it and reads must be aligned to it.
    #[must_use]
    pub fn block_size(&self) -> u64 {
        self.store.inner.layout.block()
    }
}

// ---------------------------------------------------------------------------
// Page regions
// ---------------------------------------------------------------------------

/// A region of caller-addressed blocks, overwritten in place.
pub struct PageRegion<P: Platform> {
    store: Store<P>,
    state: Arc<LiveRegion>,
}

impl<P: Platform> PageRegion<P> {
    pub(crate) fn from_parts(store: Store<P>, state: Arc<LiveRegion>) -> Self {
        Self { store, state }
    }
}

impl<P: Platform> Clone for PageRegion<P> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            state: Arc::clone(&self.state),
        }
    }
}

impl<P: Platform> PageRegion<P>
where
    P::Queue: Send,
{
    /// A checked position: `offset` must be block-aligned and inside the
    /// region.
    ///
    /// # Errors
    ///
    /// `NotWritten(Misaligned | OutOfBounds)`.
    pub fn pos(&self, offset: u64) -> Result<RegionPos, Error> {
        let c = ctx(&self.state);
        if offset % self.store.block() != 0 {
            return Err(not_written(NotWrittenCause::Misaligned, c));
        }
        if offset >= self.state.map.len() {
            return Err(not_written(NotWrittenCause::OutOfBounds, c));
        }
        Ok(RegionPos {
            volume: self.store.inner.volume,
            region: self.state.id,
            generation: self.state.generation,
            offset,
        })
    }

    /// Checks a write of `len` bytes at `pos`; returns its end.
    pub(crate) fn check(&self, pos: &RegionPos, len: usize) -> Result<u64, Error> {
        let c = ctx(&self.state);
        if pos.volume != self.store.inner.volume || pos.region != self.state.id {
            return Err(not_written(NotWrittenCause::ForeignPosition, c));
        }
        if pos.generation != self.state.generation {
            return Err(not_written(NotWrittenCause::StaleGeneration, c));
        }
        if len == 0 {
            return Err(not_written(NotWrittenCause::Empty, c));
        }
        if (len as u64) % self.store.block() != 0 {
            return Err(not_written(NotWrittenCause::Misaligned, c));
        }
        match pos.offset.checked_add(len as u64) {
            Some(end) if end <= self.state.map.len() => Ok(end),
            _ => Err(not_written(NotWrittenCause::OutOfBounds, c)),
        }
    }

    pub(crate) fn store(&self) -> &Store<P> {
        &self.store
    }

    pub(crate) fn state(&self) -> &Arc<LiveRegion> {
        &self.state
    }

    pub(crate) fn pass(&self) -> Result<Pass<'_>, Error> {
        self.state
            .gate
            .enter()
            .map_err(|why| retired_error(why, &self.state))
    }

    /// The generation this handle writes and reads.
    #[must_use]
    pub fn generation(&self) -> Generation {
        self.state.generation
    }

    /// Starts a new generation (see [`AppendRegion::recycle`]); positions and
    /// tickets of the old generation are refused from then on.
    ///
    /// # Errors
    ///
    /// As [`AppendRegion::recycle`].
    pub fn recycle(&self) -> Result<Self, Error> {
        let state = self.store.recycle_region(&self.state)?;
        Ok(Self {
            store: self.store.clone(),
            state,
        })
    }

    /// Releases the region (see [`AppendRegion::release`]).
    ///
    /// # Errors
    ///
    /// As [`AppendRegion::release`].
    pub fn release(self) -> Result<(), Error> {
        self.store.release_region(&self.state)
    }

    /// Writes whole blocks at `pos` and returns a ticket once the write has
    /// completed. `data.len()` must be a multiple of [`Self::block_size`];
    /// any size is accepted (large writes go out in several pieces).
    ///
    /// # Errors
    ///
    /// `NotWritten` (foreign, stale, misaligned, out of range, pool
    /// exhausted), `Io` (refused by the platform before reaching the device;
    /// nothing is poisoned), `Poisoned`, or `DurabilityUnknown`.
    pub fn write(&self, pos: RegionPos, data: &[u8]) -> Result<WriteTicket, Error> {
        let s = &self.store;
        let _pass = self.pass()?;
        s.ready_for(&self.state)?;
        let end = self.check(&pos, data.len())?;
        let c = ctx_range(&self.state, pos.offset, end);
        let mut stream = s.stage(data, c)?;
        stream.at(self.state.map.locate(pos.offset, end - pos.offset));
        let mut lane = s.lane(&self.state, Op::Write)?;
        let outcome = s.pump(&mut lane, &mut stream);
        let wrote = Wrote::Page {
            start: pos.offset,
            end,
        };
        settle_pumped(s, &self.state, wrote, outcome)
    }

    /// Makes the ticket's write durable, together with every other write to
    /// this store that completed before the call.
    ///
    /// # Errors
    ///
    /// `NotWritten(ForeignPosition | StaleGeneration)`, `Poisoned`, or
    /// `DurabilityUnknown`.
    pub fn sync_through(&self, ticket: &WriteTicket) -> Result<DurableReceipt, Error> {
        let s = &self.store;
        let c = ctx(&self.state);
        if ticket.epoch != s.inner.epoch
            || ticket.volume != s.inner.volume
            || ticket.region != self.state.id
        {
            return Err(not_written(NotWrittenCause::ForeignPosition, c));
        }
        if ticket.generation != self.state.generation {
            return Err(not_written(NotWrittenCause::StaleGeneration, c));
        }
        self.state
            .gate
            .check()
            .map_err(|why| retired_error(why, &self.state))?;
        let (need, synced) = s.barrier_for(&self.state)?;
        Ok(s.receipt(&self.state, ticket.start, ticket.end, need, false, synced))
    }

    /// Writes whole blocks at `pos` and makes them durable before returning.
    ///
    /// # Errors
    ///
    /// As [`Self::write`] and [`Self::sync_through`].
    pub fn write_durable(&self, pos: RegionPos, data: &[u8]) -> Result<DurableReceipt, Error> {
        let t = self.write(pos, data)?;
        self.sync_through(&t)
    }

    /// A batch: many page writes submitted together and made durable with
    /// one barrier. See [`PageBatch`].
    #[must_use]
    pub fn batch(&self) -> PageBatch<P> {
        PageBatch::new(self.clone())
    }

    /// Reads at `pos` into `out` (any length; reads whole blocks internally).
    /// Returns the bytes read, fewer than `out.len()` only at the end of the
    /// region.
    ///
    /// # Errors
    ///
    /// `NotWritten` for a foreign, stale or out-of-range position;
    /// `Corruption(MediaError)` for an unreadable range, with the exact range
    /// in the error's context; `Io` with the raw code for any other failure.
    pub fn read(&self, pos: RegionPos, out: &mut [u8]) -> Result<usize, Error> {
        let c = ctx(&self.state);
        if pos.volume != self.store.inner.volume || pos.region != self.state.id {
            return Err(not_written(NotWrittenCause::ForeignPosition, c));
        }
        if pos.generation != self.state.generation {
            return Err(not_written(NotWrittenCause::StaleGeneration, c));
        }
        let _pass = self.pass()?;
        self.store.read_at(&self.state, pos.offset, out)
    }

    /// Reads the region in order from the block-aligned offset `from` to the
    /// end of its data area, handing every piece to `visit`; return
    /// [`ControlFlow::Break`] to stop (typically where your valid data ends).
    ///
    /// The scan reads with direct I/O and several large reads in flight. It
    /// never stops at bad data: a range the device cannot read is narrowed
    /// to whole sectors (the device's logical blocks where the probe
    /// reported the direct-I/O alignment, else store blocks), reported as
    /// [`ScanItem::Unreadable`], and skipped.
    /// Scans work on read-only and poisoned stores.
    ///
    /// `visit` runs while the scan holds one of the store's queues. A call
    /// from `visit` into the same store takes another queue, and waits for
    /// one when all are busy: if every queue is held by such a scan (one scan
    /// is enough with `StoreOptions::queues` = 1) it waits forever. Collect
    /// what you need and act on the store after the scan returns.
    ///
    /// # Errors
    ///
    /// `NotWritten(Misaligned | OutOfBounds | PoolExhausted)` before any I/O;
    /// `Io` if a read fails for a reason other than the media.
    pub fn scan(
        &self,
        from: u64,
        visit: impl FnMut(ScanItem<'_>) -> ControlFlow<()>,
    ) -> Result<ScanSummary, Error> {
        let _pass = self.pass()?;
        self.store
            .scan_reported(&self.state, from, self.state.map.len(), visit)
    }

    /// Block size: writes must be whole blocks.
    #[must_use]
    pub fn block_size(&self) -> u64 {
        self.store.inner.layout.block()
    }

    /// Data-area size in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.state.map.len()
    }

    /// Whether the data area is empty (never for a provisioned region).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.state.map.len() == 0
    }
}

impl core::fmt::Debug for LiveRegion {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Region")
            .field("name", &self.name.as_str())
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("generation", &self.generation)
            .field("size", &self.map.len())
            .finish()
    }
}

impl<P: Platform> core::fmt::Debug for AppendRegion<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("AppendRegion").field(&*self.state).finish()
    }
}

impl<P: Platform> core::fmt::Debug for PageRegion<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("PageRegion").field(&*self.state).finish()
    }
}
