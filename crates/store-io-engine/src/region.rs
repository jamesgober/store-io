//! Region handles: [`AppendRegion`], [`PageRegion`] and [`Slot`].
//!
//! Every handle is cheap to clone and safe to share between threads. Calls
//! block until their I/O is done.

use std::sync::{Arc, PoisonError};

use store_io_core::align::align_up;
use store_io_core::error::{Error, ErrorContext, FirstCause, Named, NotWrittenCause, Op};
use store_io_format::meta::{REGION_META_LEN, RegionKind, RegionMeta, RegionState as Lifecycle};
use store_io_format::slot::HEADER_LEN;
use store_io_platform::{IoBuf, IoOp, Platform};

use crate::exec::{Lane, run};
use crate::receipt::{DurableReceipt, RegionPos, WriteTicket};
use crate::slots::{PairAt, plan_next, read_pair};
use crate::store::{LiveRegion, Store, durable_slot, flush_data};

fn ctx(r: &LiveRegion) -> ErrorContext {
    ErrorContext {
        region: Some(r.id),
        range: None,
    }
}

fn ctx_range(r: &LiveRegion, start: u64, end: u64) -> ErrorContext {
    ErrorContext {
        region: Some(r.id),
        range: Some(store_io_core::error::ByteRange { start, end }),
    }
}

/// Shared write path: copy into an aligned buffer, check, write, classify.
impl<P: Platform> Store<P>
where
    P::Queue: Send,
{
    fn ready(&self) -> Result<(), Error> {
        if self.inner.read_only {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::ReadOnly,
                ctx: ErrorContext::default(),
            });
        }
        if let Some(first) = self.inner.domain.poisoned() {
            return Err(Error::Poisoned { first });
        }
        Ok(())
    }

    fn buffer(&self, data: &[u8], aligned: u64, c: ErrorContext) -> Result<IoBuf, Error> {
        let len = usize::try_from(aligned).map_err(|_| Error::NotWritten {
            cause: NotWrittenCause::TooLarge,
            ctx: c,
        })?;
        let mut buf = self.inner.pool.take(len).map_err(|_| Error::NotWritten {
            cause: NotWrittenCause::PoolExhausted,
            ctx: c,
        })?;
        let dst = buf.as_mut_slice();
        dst[..data.len()].copy_from_slice(data);
        dst[data.len()..].fill(0);
        Ok(buf)
    }

    /// Writes one buffer at an absolute container offset; any failure after
    /// submission poisons the domain.
    fn write_at(
        &self,
        lane: &mut Lane<P::Queue>,
        offset: u64,
        buf: IoBuf,
        c: ErrorContext,
    ) -> Result<(), Error> {
        let want = buf.len();
        let dsync = self.inner.policy.dsync;
        let done = run(
            lane,
            IoOp::Write {
                file: &self.inner.file,
                offset,
                buf,
                dsync,
            },
        )
        .map_err(|(_b, raw)| {
            // Refused before the device: media untouched. Callers that have
            // already reserved append space poison separately.
            match raw {
                Some(raw) => Error::Io {
                    op: Op::Write,
                    raw,
                    ctx: c,
                },
                None => Error::NotWritten {
                    cause: NotWrittenCause::QueueFull,
                    ctx: c,
                },
            }
        })?;
        match done.result {
            Ok(n) if n == want => Ok(()),
            other => {
                let raw = other.err();
                self.inner.domain.poison(FirstCause {
                    op: Some(Op::Write),
                    raw,
                });
                self.wake_frontiers();
                Err(Error::DurabilityUnknown {
                    op: Op::Write,
                    raw,
                    ctx: c,
                })
            }
        }
    }

    fn wake_frontiers(&self) {
        for r in self.regions_snapshot() {
            if let Some(f) = &r.frontier {
                f.wake_all();
            }
        }
    }

    /// The domain barrier: a flush on flush-required devices, nothing on
    /// power-safe ones (their writes were durable at completion).
    fn barrier(&self, lane: &mut Lane<P::Queue>) -> Result<(), Error> {
        if !self.inner.policy.flush {
            return match self.inner.domain.poisoned() {
                Some(first) => Err(Error::Poisoned { first }),
                None => Ok(()),
            };
        }
        let need = self.inner.domain.ticket();
        let file = &self.inner.file;
        self.inner
            .domain
            .barrier(need, || flush_data(lane, file))
            .inspect_err(|_| self.wake_frontiers())
    }

    fn receipt(&self, r: &LiveRegion, start: u64, through: u64) -> DurableReceipt {
        DurableReceipt {
            volume: self.inner.volume,
            region: r.id,
            generation: r.generation,
            class: self.inner.policy.class,
            label: self.inner.policy.label,
            start,
            through,
            untorn: false,
        }
    }

    fn read_at(&self, r: &LiveRegion, offset: u64, out: &mut [u8]) -> Result<usize, Error> {
        let block = self.inner.layout.block();
        let c = ctx_range(r, offset, offset.saturating_add(out.len() as u64));
        if offset % block != 0 {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::Misaligned,
                ctx: c,
            });
        }
        let len = align_up(out.len() as u64, block).ok_or(Error::NotWritten {
            cause: NotWrittenCause::TooLarge,
            ctx: c,
        })?;
        if offset.checked_add(len).is_none_or(|e| e > r.data_size) {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::OutOfBounds,
                ctx: c,
            });
        }
        let buf = self.buffer(&[], len, c)?;
        let mut lane = self.inner.queues.get(r.id.get() as usize);
        let done = run(
            &mut lane,
            IoOp::Read {
                file: &self.inner.file,
                offset: r.data_offset + offset,
                buf,
            },
        )
        .map_err(|(_b, raw)| match raw {
            Some(raw) => Error::Io {
                op: Op::Read,
                raw,
                ctx: c,
            },
            None => Error::NotWritten {
                cause: NotWrittenCause::QueueFull,
                ctx: c,
            },
        })?;
        let n = done.result.map_err(|_raw| Error::Corruption {
            kind: store_io_core::error::CorruptionKind::MediaError,
            ctx: c,
        })?;
        let Some(buf) = done.buf else { return Ok(0) };
        let n = n.min(out.len());
        out[..n].copy_from_slice(&buf.as_slice()[..n]);
        Ok(n)
    }

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
        self.state.frontier.as_ref().ok_or(Error::NotWritten {
            cause: NotWrittenCause::NotReady,
            ctx: ctx(&self.state),
        })
    }

    /// Appends `data` and returns a ticket once the write has completed. The
    /// data is not yet durable: pass the ticket to [`Self::sync_through`].
    ///
    /// # Errors
    ///
    /// `NotWritten` (empty data, region full, pool exhausted, read-only),
    /// `Poisoned`, or `DurabilityUnknown` (the domain is now poisoned).
    pub fn append(&self, data: &[u8]) -> Result<WriteTicket, Error> {
        let s = &self.store;
        s.ready()?;
        let f = self.frontier()?;
        let block = s.inner.layout.block();
        let c = ctx(&self.state);
        if !self
            .state
            .positioned
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::NotPositioned,
                ctx: c,
            });
        }
        if data.is_empty() {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::Empty,
                ctx: c,
            });
        }
        let aligned = align_up(data.len() as u64, block).ok_or(Error::NotWritten {
            cause: NotWrittenCause::TooLarge,
            ctx: c,
        })?;
        // Everything that can fail without touching the device happens first.
        let buf = s.buffer(data, aligned, c)?;
        let mut lane = s.inner.queues.get(self.state.id.get() as usize);
        let r = f
            .reserve(data.len() as u64)
            .map_err(|cause| Error::NotWritten { cause, ctx: c })?;
        let c = ctx_range(&self.state, r.start, r.data_end);
        match s.write_at(&mut lane, self.state.data_offset + r.start, buf, c) {
            Ok(()) => {
                f.complete(r);
                Ok(WriteTicket {
                    volume: s.inner.volume,
                    region: self.state.id,
                    generation: self.state.generation,
                    start: r.start,
                    end: r.end,
                })
            }
            Err(e) => {
                // A reserved range that was not written is a hole: no later
                // append may be acknowledged past it.
                s.inner.domain.poison(FirstCause {
                    op: Some(Op::Write),
                    raw: None,
                });
                s.wake_frontiers();
                Err(e)
            }
        }
    }

    /// Makes everything appended up to and including `ticket` durable.
    ///
    /// # Errors
    ///
    /// `NotWritten(ForeignPosition)` for a ticket of another region or
    /// generation; `Poisoned`; `DurabilityUnknown` if the flush failed.
    pub fn sync_through(&self, ticket: &WriteTicket) -> Result<DurableReceipt, Error> {
        let s = &self.store;
        let f = self.frontier()?;
        if ticket.volume != s.inner.volume || ticket.region != self.state.id {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::ForeignPosition,
                ctx: ctx(&self.state),
            });
        }
        if ticket.generation != self.state.generation {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::StaleGeneration,
                ctx: ctx(&self.state),
            });
        }
        if f.durable() >= ticket.end {
            return Ok(s.receipt(&self.state, 0, f.durable()));
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
        let mut lane = s.inner.queues.get(self.state.id.get() as usize);
        s.barrier(&mut lane)?;
        f.publish_durable(through);
        Ok(s.receipt(&self.state, 0, through))
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

    /// Reads raw bytes at a block-aligned offset into `out`.
    ///
    /// # Errors
    ///
    /// `NotWritten` for a misaligned or out-of-range read;
    /// `Corruption(MediaError)` if the device reports an unreadable range.
    pub fn read(&self, offset: u64, out: &mut [u8]) -> Result<usize, Error> {
        self.store.read_at(&self.state, offset, out)
    }

    /// Moves the append position forward to `offset` (rounded up to a block),
    /// typically after recovery found where valid data ends. Never moves
    /// backwards, and only while no append is in flight.
    ///
    /// # Errors
    ///
    /// `NotWritten(TooManyInFlight | OutOfBounds)`.
    pub fn resume_at(&self, offset: u64) -> Result<u64, Error> {
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
        if offset % self.store.inner.layout.block() != 0 {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::Misaligned,
                ctx: c,
            });
        }
        if offset >= self.state.data_size {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::OutOfBounds,
                ctx: c,
            });
        }
        Ok(RegionPos {
            volume: self.store.inner.volume,
            region: self.state.id,
            generation: self.state.generation,
            offset,
        })
    }

    fn check(&self, pos: &RegionPos, len: usize) -> Result<u64, Error> {
        let c = ctx(&self.state);
        if pos.volume != self.store.inner.volume || pos.region != self.state.id {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::ForeignPosition,
                ctx: c,
            });
        }
        if pos.generation != self.state.generation {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::StaleGeneration,
                ctx: c,
            });
        }
        let block = self.store.inner.layout.block();
        if len == 0 || (len as u64) % block != 0 {
            return Err(Error::NotWritten {
                cause: if len == 0 {
                    NotWrittenCause::Empty
                } else {
                    NotWrittenCause::Misaligned
                },
                ctx: c,
            });
        }
        let end = pos
            .offset
            .checked_add(len as u64)
            .ok_or(Error::NotWritten {
                cause: NotWrittenCause::OutOfBounds,
                ctx: c,
            })?;
        if end > self.state.data_size {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::OutOfBounds,
                ctx: c,
            });
        }
        Ok(end)
    }

    /// Writes whole blocks at `pos` and returns a ticket once the write has
    /// completed. `data.len()` must be a multiple of [`Self::block_size`].
    ///
    /// # Errors
    ///
    /// `NotWritten` (foreign, stale, misaligned, out of range, pool exhausted),
    /// `Poisoned`, or `DurabilityUnknown`.
    pub fn write(&self, pos: RegionPos, data: &[u8]) -> Result<WriteTicket, Error> {
        let s = &self.store;
        s.ready()?;
        let end = self.check(&pos, data.len())?;
        let c = ctx_range(&self.state, pos.offset, end);
        let buf = s.buffer(data, data.len() as u64, c)?;
        let mut lane = s.inner.queues.get(self.state.id.get() as usize);
        s.write_at(&mut lane, self.state.data_offset + pos.offset, buf, c)?;
        Ok(WriteTicket {
            volume: s.inner.volume,
            region: self.state.id,
            generation: self.state.generation,
            start: pos.offset,
            end,
        })
    }

    /// Makes the ticket's write durable.
    ///
    /// # Errors
    ///
    /// `NotWritten(ForeignPosition | StaleGeneration)`, `Poisoned`, or
    /// `DurabilityUnknown`.
    pub fn sync_through(&self, ticket: &WriteTicket) -> Result<DurableReceipt, Error> {
        let s = &self.store;
        let c = ctx(&self.state);
        if ticket.volume != s.inner.volume || ticket.region != self.state.id {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::ForeignPosition,
                ctx: c,
            });
        }
        if ticket.generation != self.state.generation {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::StaleGeneration,
                ctx: c,
            });
        }
        let mut lane = s.inner.queues.get(self.state.id.get() as usize);
        s.barrier(&mut lane)?;
        Ok(s.receipt(&self.state, ticket.start, ticket.end))
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

    /// Reads at `pos` into `out` (any length; reads whole blocks internally).
    ///
    /// # Errors
    ///
    /// `NotWritten` for a foreign, stale or out-of-range position;
    /// `Corruption(MediaError)` for an unreadable range.
    pub fn read(&self, pos: RegionPos, out: &mut [u8]) -> Result<usize, Error> {
        let c = ctx(&self.state);
        if pos.region != self.state.id || pos.generation != self.state.generation {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::ForeignPosition,
                ctx: c,
            });
        }
        self.store.read_at(&self.state, pos.offset, out)
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
                kind: store_io_core::error::CorruptionKind::Metadata,
                ctx: c,
            })?;
        let rm = RegionMeta::decode(&meta_bytes).map_err(|_| Error::Corruption {
            kind: store_io_core::error::CorruptionKind::Metadata,
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
                kind: store_io_core::error::CorruptionKind::Fork,
                ctx: c,
            })?;
        let generation = next.generation;
        let at = self.at();
        let mut lane = s.inner.queues.get(self.state.id.get() as usize);
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
        let mut receipt = s.receipt(&self.state, 0, data.len() as u64);
        if let Some(g) = store_io_core::id::Generation::from_raw(generation) {
            receipt.generation = g;
        }
        Ok(receipt)
    }
}
