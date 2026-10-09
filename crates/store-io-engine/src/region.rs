//! Region handles: [`AppendRegion`], [`PageRegion`] and [`Slot`].
//!
//! Every handle is cheap to clone and safe to share between threads. Calls
//! block until their I/O is done.

use std::ops::ControlFlow;
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};

use store_io_core::error::{CorruptionKind, Error, FirstCause, Named, NotWrittenCause, Op};
use store_io_core::id::Generation;
use store_io_format::meta::{REGION_META_LEN, RegionKind, RegionMeta, RegionState as Lifecycle};
use store_io_format::slot::HEADER_LEN;
use store_io_platform::Platform;

use crate::batch::{AppendBatch, PageBatch};
use crate::exec::Pumped;
use crate::gate::Pass;
use crate::io::{ctx, ctx_range, not_written};
use crate::lifecycle::retired_error;
use crate::receipt::{DurableReceipt, RegionPos, WriteTicket};
use crate::scan::{ScanItem, ScanSummary};
use crate::slots::{PairAt, plan_next, read_pair};
use crate::store::{LiveRegion, Store, durable_slot};

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
        let state = self.provision(name, RegionKind::Append, size)?;
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
        let state = self.provision(name, RegionKind::Page, size)?;
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
        Ok(Slot {
            store: self.clone(),
            state,
        })
    }

    /// Provisions a new small-object slot. Its capacity is the block size
    /// minus 176 bytes of headers ([`Slot::capacity`]).
    ///
    /// # Errors
    ///
    /// [`Error::AlreadyExists`], [`Error::NoSpace`], or a platform error.
    pub fn provision_slot(&self, name: &str) -> Result<Slot<P>, Error> {
        let state = self.provision(name, RegionKind::Slot, 0)?;
        Ok(Slot {
            store: self.clone(),
            state,
        })
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
        self.store.ready()?;
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
    /// Must not be called from inside this region's own scan visitor.
    ///
    /// # Errors
    ///
    /// `NotWritten(StaleGeneration | NotReady)` if the region was already
    /// recycled or released; `Poisoned`; `DurabilityUnknown`.
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
    /// reused by later provisioning.
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

    pub(crate) fn state(&self) -> &LiveRegion {
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
    /// positioned), `Poisoned`, or `DurabilityUnknown` (the domain is now
    /// poisoned).
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
        let mut lane = s.lane(&self.state);
        let reserved = s.reserve(f, data.len() as u64, c)?;
        let r = reserved.range();
        stream.at(self.state.data_offset + r.start);
        match s.pump(&mut lane, &mut stream) {
            Pumped::All => {
                reserved.complete();
                Ok(s.ticket(&self.state, r.start, r.end))
            }
            // Past the reservation every failure leaves a hole: the guard
            // poisons when dropped, and the write's fate is unknown.
            Pumped::Refused(raw) | Pumped::Failed(raw) => {
                drop(reserved);
                Err(s.unknown(Op::Write, raw, ctx_range(&self.state, r.start, r.data_end)))
            }
        }
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
        let durable = f.durable();
        if durable >= ticket.end {
            return Ok(s.receipt(&self.state, 0, durable, 0, true));
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
        let mut lane = s.lane(&self.state);
        let need = s.barrier(&mut lane)?;
        f.publish_durable(through);
        Ok(s.receipt(&self.state, 0, through, need, true))
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
    /// `Corruption(MediaError)` if the device reports an unreadable range.
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
    /// to whole blocks, reported as [`ScanItem::Unreadable`], and skipped.
    /// Scans work on read-only and poisoned stores.
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
            .scan_region(&self.state, from, self.state.data_size, visit)
    }

    /// Moves the append position forward to `offset` (rounded up to a block),
    /// typically after recovery found where valid data ends. Never moves
    /// backwards, and only while no append is in flight.
    ///
    /// # Errors
    ///
    /// `NotWritten(TooManyInFlight | OutOfBounds)`.
    pub fn resume_at(&self, offset: u64) -> Result<u64, Error> {
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
        self.state.data_size
    }

    /// Whether the data area is empty (never for a provisioned region).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.state.data_size == 0
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
        if offset >= self.state.data_size {
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
            Some(end) if end <= self.state.data_size => Ok(end),
            _ => Err(not_written(NotWrittenCause::OutOfBounds, c)),
        }
    }

    pub(crate) fn store(&self) -> &Store<P> {
        &self.store
    }

    pub(crate) fn state(&self) -> &LiveRegion {
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
        s.ready()?;
        let end = self.check(&pos, data.len())?;
        let c = ctx_range(&self.state, pos.offset, end);
        let mut stream = s.stage(data, c)?;
        stream.at(self.state.data_offset + pos.offset);
        let mut lane = s.lane(&self.state);
        match s.pump(&mut lane, &mut stream) {
            Pumped::All => Ok(s.ticket(&self.state, pos.offset, end)),
            Pumped::Refused(raw) => Err(Store::<P>::refused(raw, c)),
            Pumped::Failed(raw) => Err(s.unknown(Op::Write, raw, c)),
        }
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
        let mut lane = s.lane(&self.state);
        let need = s.barrier(&mut lane)?;
        Ok(s.receipt(&self.state, ticket.start, ticket.end, need, false))
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
    /// `Corruption(MediaError)` for an unreadable range.
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
    /// to whole blocks, reported as [`ScanItem::Unreadable`], and skipped.
    /// Scans work on read-only and poisoned stores.
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
            .scan_region(&self.state, from, self.state.data_size, visit)
    }

    /// Block size: writes must be whole blocks.
    #[must_use]
    pub fn block_size(&self) -> u64 {
        self.store.inner.layout.block()
    }

    /// Data-area size in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.state.data_size
    }

    /// Whether the data area is empty (never for a provisioned region).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.state.data_size == 0
    }
}

// ---------------------------------------------------------------------------
// Small objects
// ---------------------------------------------------------------------------

/// A small object (a manifest, a superblock) committed atomically: after any
/// crash it reads back as exactly the old or exactly the new version.
///
/// One commit is one block write plus one barrier. Commits to one slot are
/// serialised (at most one slot write in flight).
pub struct Slot<P: Platform> {
    store: Store<P>,
    state: Arc<LiveRegion>,
}

impl<P: Platform> Clone for Slot<P> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            state: Arc::clone(&self.state),
        }
    }
}

impl<P: Platform> Slot<P>
where
    P::Queue: Send,
{
    /// Largest payload a commit accepts.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.store.inner.layout.block() as usize - HEADER_LEN - REGION_META_LEN
    }

    fn at(&self) -> PairAt {
        let l = &self.store.inner.layout;
        let a = self.state.data_offset - 2 * l.block();
        PairAt {
            volume: *self.store.inner.volume.as_bytes(),
            object: crate::layout::region_object(self.state.id.get()),
            a,
            b: a + l.block(),
            log2: l.log2_block,
        }
    }

    /// The current committed object, or `None` if never committed.
    ///
    /// Generation 1 of the slot pair is the provisioning header; commits start
    /// at generation 2.
    #[must_use]
    pub fn read(&self) -> Option<Vec<u8>> {
        let h = self
            .state
            .header
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (_, info) = h.report.winner_info()?;
        if info.generation <= 1 {
            return None;
        }
        h.payload
            .as_ref()
            .and_then(|p| p.get(REGION_META_LEN..))
            .map(<[u8]>::to_vec)
    }

    /// Commits a new version of the object.
    ///
    /// # Errors
    ///
    /// `NotWritten(TooLarge)` beyond [`Self::capacity`], `Poisoned`, or
    /// `DurabilityUnknown` (the domain is now poisoned).
    pub fn commit(&self, data: &[u8]) -> Result<DurableReceipt, Error> {
        let s = &self.store;
        s.ready()?;
        let c = ctx(&self.state);
        if data.len() > self.capacity() {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::TooLarge,
                ctx: c,
            });
        }
        let mut header = self
            .state
            .header
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let meta_bytes = header
            .payload
            .as_ref()
            .and_then(|p| p.get(..REGION_META_LEN))
            .map(<[u8]>::to_vec)
            .ok_or(Error::Corruption {
                kind: CorruptionKind::Metadata,
                ctx: c,
            })?;
        let rm = RegionMeta::decode(&meta_bytes).map_err(|_| Error::Corruption {
            kind: CorruptionKind::Metadata,
            ctx: c,
        })?;
        if rm.state != Lifecycle::Ready {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::NotReady,
                ctx: c,
            });
        }
        let mut payload = Vec::with_capacity(REGION_META_LEN + data.len());
        payload.extend_from_slice(&meta_bytes);
        payload.extend_from_slice(data);
        let next =
            plan_next(&header, &payload, s.inner.owner_generation).ok_or(Error::Corruption {
                kind: CorruptionKind::Fork,
                ctx: c,
            })?;
        let generation = next.generation;
        let at = self.at();
        let mut lane = s.lane(&self.state);
        durable_slot::<P>(
            &mut lane,
            &s.inner.file,
            &s.inner.pool,
            &at,
            &next,
            &s.inner.policy,
            &s.inner.domain,
        )
        .inspect_err(|_| s.wake_frontiers())?;
        *header = read_pair(&mut lane, &s.inner.file, &s.inner.pool, &at)?;
        // A slot has no tickets: its receipt covers the commit alone.
        let mut receipt = s.receipt(&self.state, 0, data.len() as u64, 0, false);
        if let Some(g) = store_io_core::id::Generation::from_raw(generation) {
            receipt.generation = g;
        }
        Ok(receipt)
    }
}

impl core::fmt::Debug for LiveRegion {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Region")
            .field("name", &self.name.as_str())
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("generation", &self.generation)
            .field("size", &self.data_size)
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

impl<P: Platform> core::fmt::Debug for Slot<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("Slot").field(&*self.state).finish()
    }
}
