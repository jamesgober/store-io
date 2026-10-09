//! The batch layer: many writes, one barrier.
//!
//! - [`AppendBatch`] packs records back to back (no per-record padding) into
//!   pooled aligned buffers as they are added, so committing costs one
//!   reservation, the fewest possible device writes (kept in flight
//!   together) and one barrier.
//! - [`PageBatch`] collects page writes, checks them, and submits them
//!   together before one barrier.
//!
//! Records are staged straight into the buffers the device reads from: an
//! append is copied once, and [`AppendBatch::append_with`] lets the caller
//! encode in place with no copy at all.

use store_io_core::error::{Error, NotWrittenCause, Op};
use store_io_platform::{IoBuf, Platform};

use crate::exec::Pumped;
use crate::io::{Chunks, Placed, ctx, ctx_range, not_written};
use crate::receipt::{DurableReceipt, RegionPos, WriteTicket};
use crate::region::{AppendRegion, PageRegion};
use crate::store::Store;

/// Many appends written together and made durable with one barrier.
///
/// Records are packed back to back: record `i` starts where record `i - 1`
/// ended, and only the end of the whole batch is padded to a block. Each
/// `append` returns the record's offset within the batch; after a commit,
/// the record's region offset is the commit position's offset plus that.
///
/// A batch reuses its buffer list across commits, so a long-lived batch
/// makes no allocation after its first commit. Staged bytes live in the
/// store's buffer pool: a batch holds at most what the pool can lend
/// (`StoreOptions::buffers_per_class × StoreOptions::max_io`), and an append
/// beyond that fails with `PoolExhausted`, leaving the batch as it was.
///
/// ```
/// # use std::path::Path;
/// # use store_io_engine::{Store, StoreOptions};
/// # use store_io_sim::{SimConfig, SimPlatform};
/// # let platform = SimPlatform::new(SimConfig::volatile(1));
/// # let store = Store::create(platform, Path::new("/db"), StoreOptions::default())?;
/// let wal = store.provision_append_region("wal", 1 << 20)?;
/// let mut batch = wal.batch();
/// let a = batch.append(b"first record")?;
/// let b = batch.append_with(4, |buf| buf.copy_from_slice(b"next"))?;
/// let (pos, receipt) = batch.commit()?; // one write, one barrier
///
/// let mut out = [0u8; 16];
/// wal.read(pos.offset(), &mut out)?;
/// assert_eq!(&out[a as usize..b as usize], b"first record");
/// assert_eq!(&out[b as usize..], b"next");
/// assert!(receipt.durable_through() >= pos.offset() + 16);
/// # Ok::<(), store_io_core::error::Error>(())
/// ```
pub struct AppendBatch<P: Platform> {
    region: AppendRegion<P>,
    /// Staged bytes. Every buffer but the last holds whole blocks.
    chunks: Vec<IoBuf>,
    bytes: u64,
    records: usize,
}

impl<P: Platform> AppendBatch<P>
where
    P::Queue: Send,
{
    pub(crate) fn new(region: AppendRegion<P>) -> Self {
        Self {
            region,
            chunks: Vec::new(),
            bytes: 0,
            records: 0,
        }
    }

    fn store(&self) -> &Store<P> {
        self.region.store()
    }

    /// Bytes still free in the last staged buffer.
    fn room(&self) -> usize {
        let chunk = self.store().chunk();
        self.chunks.last().map_or(0, |b| chunk - b.len())
    }

    /// Starts a new, empty staging buffer.
    fn grow(&mut self) -> Result<(), Error> {
        let mut buf = self
            .store()
            .take(self.store().chunk(), ctx(self.region.state()))?;
        // Length 0 always fits.
        let _empty = buf.set_len(0);
        self.chunks.push(buf);
        Ok(())
    }

    /// Restores the staged length to `len` bytes (undoing a failed append).
    fn truncate(&mut self, len: u64) {
        let mut total: u64 = self.chunks.iter().map(|b| b.len() as u64).sum();
        while let Some(last) = self.chunks.last_mut() {
            let l = last.len() as u64;
            if total - l >= len {
                total -= l;
                let _dropped = self.chunks.pop();
                continue;
            }
            // `len - (total - l) ≤ l`: the length fits.
            let _fits = last.set_len((len - (total - l)) as usize);
            break;
        }
    }

    /// Adds a record (at least one byte) and returns its offset within the
    /// batch. The bytes are copied once, into the buffer the device will
    /// read.
    ///
    /// # Errors
    ///
    /// `NotWritten(Empty)`, or `NotWritten(PoolExhausted)` (the batch is left
    /// exactly as it was).
    pub fn append(&mut self, data: &[u8]) -> Result<u64, Error> {
        if data.is_empty() {
            return Err(not_written(
                NotWrittenCause::Empty,
                ctx(self.region.state()),
            ));
        }
        let at = self.bytes;
        let mut rest = data;
        while !rest.is_empty() {
            let room = self.room();
            if room == 0 {
                if let Err(e) = self.grow() {
                    self.truncate(at);
                    return Err(e);
                }
                continue;
            }
            let n = room.min(rest.len());
            if let Some(last) = self.chunks.last_mut() {
                let l = last.len();
                last.as_mut_capacity()[l..l + n].copy_from_slice(&rest[..n]);
                // `l + n ≤ chunk ≤ capacity`.
                let _fits = last.set_len(l + n);
            }
            rest = &rest[n..];
        }
        self.bytes += data.len() as u64;
        self.records += 1;
        Ok(at)
    }

    /// Adds a record of `len` bytes encoded in place by `fill`, with no copy,
    /// and returns its offset within the batch. `fill` receives a zeroed
    /// slice of exactly `len` bytes inside the buffer the device will read.
    /// If `fill` panics the record is not added.
    ///
    /// # Errors
    ///
    /// `NotWritten(Empty)`, `NotWritten(TooLarge)` beyond
    /// [`Self::max_in_place`], or `NotWritten(PoolExhausted)`.
    pub fn append_with(&mut self, len: usize, fill: impl FnOnce(&mut [u8])) -> Result<u64, Error> {
        let c = ctx(self.region.state());
        if len == 0 {
            return Err(not_written(NotWrittenCause::Empty, c));
        }
        if len > self.max_in_place() {
            return Err(not_written(NotWrittenCause::TooLarge, c));
        }
        if self.room() < len {
            // A record lent in place must be contiguous, so it starts a new
            // buffer. The previous buffer's trailing partial block moves with
            // it, keeping every buffer but the last a whole number of blocks.
            let block = self.store().block() as usize;
            let mut fresh = self.store().take(self.store().chunk(), c)?;
            let mut carried = 0;
            if let Some(last) = self.chunks.last_mut() {
                let l = last.len();
                carried = l % block;
                fresh.as_mut_capacity()[..carried].copy_from_slice(&last.as_slice()[l - carried..]);
                // Shrinking always fits.
                let _fits = last.set_len(l - carried);
            }
            let _fits = fresh.set_len(carried);
            if self.chunks.last().is_some_and(IoBuf::is_empty) {
                let _emptied = self.chunks.pop();
            }
            self.chunks.push(fresh);
        }
        if let Some(last) = self.chunks.last_mut() {
            let l = last.len();
            let slot = &mut last.as_mut_capacity()[l..l + len];
            // Pool memory is reused: never lend stale bytes.
            slot.fill(0);
            fill(slot);
            // `l + len ≤ chunk ≤ capacity` (checked by `room` or the carry).
            let _fits = last.set_len(l + len);
        }
        let at = self.bytes;
        self.bytes += len as u64;
        self.records += 1;
        Ok(at)
    }

    /// The largest record [`Self::append_with`] accepts: the largest pooled
    /// buffer less one block.
    #[must_use]
    pub fn max_in_place(&self) -> usize {
        self.store().chunk() - self.store().block() as usize
    }

    /// Records staged.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records
    }

    /// Whether nothing is staged.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records == 0
    }

    /// Bytes staged (before the final padding).
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Drops everything staged.
    pub fn clear(&mut self) {
        self.chunks.clear();
        self.bytes = 0;
        self.records = 0;
    }

    /// Writes the batch as one contiguous append and returns where it starts
    /// and a ticket, once every write has completed. Not yet durable: pass
    /// the ticket to [`AppendRegion::sync_through`], or use [`Self::commit`].
    /// The batch is empty afterwards, whatever the outcome.
    ///
    /// # Errors
    ///
    /// `NotWritten(Empty | RegionFull | ReadOnly | NotPositioned)` (the batch
    /// is kept), `Poisoned`, or `DurabilityUnknown` (the domain is now
    /// poisoned).
    pub fn write(&mut self) -> Result<(RegionPos, WriteTicket), Error> {
        let c = ctx(self.region.state());
        if self.records == 0 {
            return Err(not_written(NotWrittenCause::Empty, c));
        }
        let (f, _pass) = self.region.writable()?;
        let s = self.region.store();
        let state = self.region.state();
        let block = s.block() as usize;
        let mut lane = s.lane(state);
        let reserved = s.reserve_append(f, self.bytes, c)?;
        let r = reserved.range();
        if let Some(last) = self.chunks.last_mut() {
            let l = last.len();
            let padded = l.div_ceil(block) * block;
            last.as_mut_capacity()[l..padded].fill(0);
            // A staged buffer's capacity is a whole number of blocks.
            let _fits = last.set_len(padded);
        }
        let mut src = Chunks {
            bufs: self.chunks.drain(..),
            offset: state.data_offset + r.start,
        };
        let outcome = s.pump(&mut lane, &mut src);
        drop(src);
        self.bytes = 0;
        self.records = 0;
        match outcome {
            Pumped::All => {
                reserved.complete();
                let ticket = s.ticket(state, r.start, r.end);
                Ok((ticket.pos(), ticket))
            }
            Pumped::Refused(raw) | Pumped::Failed(raw) => {
                drop(reserved);
                Err(s.unknown(Op::Write, raw, ctx_range(state, r.start, r.data_end)))
            }
        }
    }

    /// Writes the batch and makes it durable with one barrier. Returns where
    /// the batch starts and the receipt.
    ///
    /// # Errors
    ///
    /// As [`Self::write`] and [`AppendRegion::sync_through`].
    pub fn commit(&mut self) -> Result<(RegionPos, DurableReceipt), Error> {
        let (pos, ticket) = self.write()?;
        let receipt = self.region.sync_through(&ticket)?;
        Ok((pos, receipt))
    }
}

/// Many page writes submitted together and made durable with one barrier.
///
/// Writes are checked as they are added, copied once into pooled buffers,
/// sorted by position at commit, and kept in flight together. Two writes in
/// one batch must not overlap.
///
/// ```
/// # use std::path::Path;
/// # use store_io_engine::{Store, StoreOptions};
/// # use store_io_sim::{SimConfig, SimPlatform};
/// # let platform = SimPlatform::new(SimConfig::volatile(1));
/// # let store = Store::create(platform, Path::new("/db"), StoreOptions::default())?;
/// let pages = store.provision_page_region("pages", 1 << 20)?;
/// let bs = pages.block_size() as usize;
/// let mut batch = pages.batch();
/// batch.write(pages.pos(8 * bs as u64)?, &vec![2; bs])?;
/// batch.write(pages.pos(0)?, &vec![1; bs])?;
/// let receipt = batch.commit()?; // both writes, one barrier
/// assert_eq!(receipt.durable_through(), 9 * bs as u64);
/// # Ok::<(), store_io_core::error::Error>(())
/// ```
pub struct PageBatch<P: Platform> {
    region: PageRegion<P>,
    /// `(region offset, staged bytes)`.
    writes: Vec<(u64, IoBuf)>,
}

impl<P: Platform> PageBatch<P>
where
    P::Queue: Send,
{
    pub(crate) fn new(region: PageRegion<P>) -> Self {
        Self {
            region,
            writes: Vec::new(),
        }
    }

    /// Adds a write of whole blocks at `pos`. Large writes are split into
    /// pooled pieces.
    ///
    /// # Errors
    ///
    /// `NotWritten` (foreign, stale, misaligned, empty, out of range, pool
    /// exhausted); the batch is left as it was.
    pub fn write(&mut self, pos: RegionPos, data: &[u8]) -> Result<(), Error> {
        let end = self.region.check(&pos, data.len())?;
        let s = self.region.store();
        let c = ctx_range(self.region.state(), pos.offset, end);
        let chunk = s.chunk();
        let before = self.writes.len();
        for (i, piece) in data.chunks(chunk).enumerate() {
            match s.take(piece.len(), c) {
                Ok(mut buf) => {
                    buf.as_mut_slice().copy_from_slice(piece);
                    self.writes.push((pos.offset + (i * chunk) as u64, buf));
                }
                Err(e) => {
                    self.writes.truncate(before);
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Adds a write of `len` bytes (whole blocks, at most one pooled buffer)
    /// encoded in place by `fill`, which receives a zeroed slice inside the
    /// buffer the device will read. If `fill` panics nothing is added.
    ///
    /// # Errors
    ///
    /// As [`Self::write`], and `NotWritten(TooLarge)` beyond one buffer.
    pub fn write_with(
        &mut self,
        pos: RegionPos,
        len: usize,
        fill: impl FnOnce(&mut [u8]),
    ) -> Result<(), Error> {
        let end = self.region.check(&pos, len)?;
        let s = self.region.store();
        let c = ctx_range(self.region.state(), pos.offset, end);
        if len > s.chunk() {
            return Err(not_written(NotWrittenCause::TooLarge, c));
        }
        let mut buf = s.take(len, c)?;
        let slot = buf.as_mut_slice();
        slot.fill(0);
        fill(slot);
        self.writes.push((pos.offset, buf));
        Ok(())
    }

    /// Writes staged.
    #[must_use]
    pub fn len(&self) -> usize {
        self.writes.len()
    }

    /// Whether nothing is staged.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }

    /// Drops everything staged.
    pub fn clear(&mut self) {
        self.writes.clear();
    }

    /// Submits every write together and makes them durable with one barrier.
    /// The receipt names the span from the first written byte to the last;
    /// it proves durable every write in the batch (and every other write to
    /// this store that completed before the barrier). The batch is kept if
    /// the commit fails with `NotWritten` or `Poisoned` before any I/O, and
    /// is empty after every other outcome.
    ///
    /// # Errors
    ///
    /// `NotWritten(Empty | Overlap | ReadOnly)` before any I/O (the batch is
    /// kept); `Io` if the platform refused the first write (nothing was
    /// written or poisoned); `Poisoned`; or `DurabilityUnknown`.
    pub fn commit(&mut self) -> Result<DurableReceipt, Error> {
        let state = self.region.state();
        let c = ctx(state);
        if self.writes.is_empty() {
            return Err(not_written(NotWrittenCause::Empty, c));
        }
        let s = self.region.store();
        let _pass = self.region.pass()?;
        s.ready()?;
        self.writes.sort_unstable_by_key(|(at, _)| *at);
        let overlap = self
            .writes
            .windows(2)
            .any(|w| w[0].0 + w[0].1.len() as u64 > w[1].0);
        if overlap {
            return Err(not_written(NotWrittenCause::Overlap, c));
        }
        let start = self.writes.first().map_or(0, |(at, _)| *at);
        let end = self.writes.last().map_or(0, |(at, b)| at + b.len() as u64);
        let c = ctx_range(state, start, end);
        let base = state.data_offset;
        let mut lane = s.lane(state);
        let mut src = Placed {
            writes: self.writes.drain(..).map(|(at, b)| (base + at, b)),
        };
        let outcome = s.pump(&mut lane, &mut src);
        drop(src);
        match outcome {
            Pumped::All => {}
            Pumped::Refused(raw) => return Err(Store::<P>::refused(raw, c)),
            Pumped::Failed(raw) => return Err(s.unknown(Op::Write, raw, c)),
        }
        let need = s.barrier(&mut lane)?;
        Ok(s.receipt(state, start, end, need, false))
    }
}

impl<P: Platform> core::fmt::Debug for AppendBatch<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AppendBatch")
            .field("region", &self.region)
            .field("records", &self.records)
            .field("bytes", &self.bytes)
            .finish()
    }
}

impl<P: Platform> core::fmt::Debug for PageBatch<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PageBatch")
            .field("region", &self.region)
            .field("writes", &self.writes.len())
            .finish()
    }
}
