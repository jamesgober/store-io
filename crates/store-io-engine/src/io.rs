//! The I/O paths shared by the region handles and batches: staging caller
//! bytes into pooled aligned buffers, writes of any length, the reservation
//! guard, barriers, tickets, receipts and reads.

use std::sync::MutexGuard;

use store_io_core::align::align_up;
use store_io_core::error::{
    ByteRange, CorruptionKind, Error, ErrorContext, FirstCause, NotWrittenCause, Op, OsError,
};
use store_io_platform::{IoBuf, IoOp, Platform};

use crate::exec::{Lane, Next, Pumped, Source, pump, run};
use crate::frontier::{AppendFrontier, Reservation};
use crate::receipt::{DurableReceipt, WriteTicket};
use crate::store::{LiveRegion, Store, flush_data};

/// Spare buffers a [`Stream`] keeps in flight for one large write.
const SPARES: usize = 8;

pub(crate) fn ctx(r: &LiveRegion) -> ErrorContext {
    ErrorContext {
        region: Some(r.id),
        range: None,
    }
}

pub(crate) fn ctx_range(r: &LiveRegion, start: u64, end: u64) -> ErrorContext {
    ErrorContext {
        region: Some(r.id),
        range: Some(ByteRange { start, end }),
    }
}

pub(crate) fn not_written(cause: NotWrittenCause, ctx: ErrorContext) -> Error {
    Error::NotWritten { cause, ctx }
}

/// Caller bytes staged for one write of any length.
///
/// The first chunk is copied into its buffer when the stream is built, before
/// any space is reserved, so a lone write reserves and submits back to back.
/// Further chunks are copied as buffers come back from completed writes, so a
/// write of any size needs at most [`SPARES`] buffers.
pub(crate) struct Stream<'a> {
    data: &'a [u8],
    /// Next caller byte not yet staged.
    staged: usize,
    /// Container offset of `data[0]`.
    base: u64,
    block: usize,
    chunk: usize,
    first: Option<IoBuf>,
    spare: [Option<IoBuf>; SPARES],
}

impl Stream<'_> {
    /// Sets where the data goes (after the reservation).
    pub(crate) fn at(&mut self, base: u64) {
        self.base = base;
    }

    /// Copies the next chunk of caller bytes into `buf`, zero-padding the
    /// final partial block. Returns the chunk's offset in `data`.
    fn fill(&mut self, buf: &mut IoBuf) -> usize {
        let at = self.staged;
        let n = self.chunk.min(self.data.len() - at);
        let padded = n.div_ceil(self.block) * self.block;
        let dst = buf.as_mut_capacity();
        dst[..n].copy_from_slice(&self.data[at..at + n]);
        dst[n..padded].fill(0);
        // `padded ≤ chunk ≤ capacity`: the length always fits.
        let _fits = buf.set_len(padded);
        self.staged = at + n;
        at
    }
}

impl Source for Stream<'_> {
    fn next(&mut self) -> Next {
        if let Some(buf) = self.first.take() {
            return Next::Write(self.base, buf);
        }
        if self.staged >= self.data.len() {
            return Next::Done;
        }
        let Some(mut buf) = self.spare.iter_mut().find_map(Option::take) else {
            return Next::Wait;
        };
        let at = self.fill(&mut buf);
        Next::Write(self.base + at as u64, buf)
    }

    fn recycle(&mut self, buf: IoBuf) {
        if self.staged < self.data.len() {
            if let Some(slot) = self.spare.iter_mut().find(|s| s.is_none()) {
                *slot = Some(buf);
            }
        }
    }
}

/// Pooled buffers written in order from a container offset (a batch).
pub(crate) struct Chunks<I> {
    pub(crate) bufs: I,
    pub(crate) offset: u64,
}

impl<I: Iterator<Item = IoBuf>> Source for Chunks<I> {
    fn next(&mut self) -> Next {
        match self.bufs.next() {
            Some(buf) => {
                let at = self.offset;
                self.offset += buf.len() as u64;
                Next::Write(at, buf)
            }
            None => Next::Done,
        }
    }

    fn recycle(&mut self, _buf: IoBuf) {}
}

/// Buffers written at their own container offsets (a page batch).
pub(crate) struct Placed<I> {
    pub(crate) writes: I,
}

impl<I: Iterator<Item = (u64, IoBuf)>> Source for Placed<I> {
    fn next(&mut self) -> Next {
        match self.writes.next() {
            Some((at, buf)) => Next::Write(at, buf),
            None => Next::Done,
        }
    }

    fn recycle(&mut self, _buf: IoBuf) {}
}

/// An append reservation that poisons the device domain unless its write is
/// completed: a reserved range left unwritten is a hole no later append may
/// be acknowledged past. Dropping it uncompleted (an error, or a panic while
/// it is held) poisons and wakes every waiter.
pub(crate) struct Reserved<'a, P: Platform>
where
    P::Queue: Send,
{
    store: &'a Store<P>,
    frontier: &'a AppendFrontier,
    r: Option<Reservation>,
}

impl<P: Platform> Reserved<'_, P>
where
    P::Queue: Send,
{
    pub(crate) fn range(&self) -> Reservation {
        // Always `Some` until `complete` consumes the guard.
        self.r.unwrap_or(Reservation {
            start: 0,
            data_end: 0,
            end: 0,
        })
    }

    /// Records the completed write.
    pub(crate) fn complete(mut self) {
        if let Some(r) = self.r.take() {
            self.frontier.complete(r);
        }
    }
}

impl<P: Platform> Drop for Reserved<'_, P>
where
    P::Queue: Send,
{
    fn drop(&mut self) {
        if self.r.take().is_some() {
            self.store.inner.domain.poison(FirstCause {
                op: Some(Op::Write),
                raw: None,
            });
            self.store.wake_frontiers();
        }
    }
}

impl<P: Platform> Store<P>
where
    P::Queue: Send,
{
    /// Writes are allowed: not read-only and not poisoned.
    pub(crate) fn ready(&self) -> Result<(), Error> {
        if self.inner.read_only {
            return Err(not_written(
                NotWrittenCause::ReadOnly,
                ErrorContext::default(),
            ));
        }
        if let Some(first) = self.inner.domain.poisoned() {
            return Err(Error::Poisoned { first });
        }
        Ok(())
    }

    pub(crate) fn block(&self) -> u64 {
        self.inner.layout.block()
    }

    /// The largest single write: the pool's largest buffer in whole blocks.
    pub(crate) fn chunk(&self) -> usize {
        let block = self.block() as usize;
        (self.inner.pool.max_len() / block * block).max(block)
    }

    /// A free queue for a call on `r`.
    pub(crate) fn lane(&self, r: &LiveRegion) -> MutexGuard<'_, Lane<P::Queue>> {
        self.inner.queues.get(r.id.get() as usize)
    }

    /// A pooled buffer of at least `len` bytes, with its length set to `len`.
    pub(crate) fn take(&self, len: usize, c: ErrorContext) -> Result<IoBuf, Error> {
        self.inner
            .pool
            .take(len)
            .map_err(|_| not_written(NotWrittenCause::PoolExhausted, c))
    }

    /// Stages `data` (non-empty) for one write of whole blocks. Fails only
    /// before any I/O: with `PoolExhausted` if not even one buffer is free.
    pub(crate) fn stage<'a>(&self, data: &'a [u8], c: ErrorContext) -> Result<Stream<'a>, Error> {
        let block = self.block() as usize;
        let chunk = self.chunk();
        let first_len = data.len().min(chunk).div_ceil(block) * block;
        let mut first = self.take(first_len, c)?;
        // A multi-chunk write's first buffer is a whole chunk (`first_len`),
        // so it can be reused for later chunks.
        let mut spare: [Option<IoBuf>; SPARES] = std::array::from_fn(|_| None);
        if data.len() > chunk {
            // Extra buffers are an optimisation (writes in flight together);
            // one buffer is enough, so a short pool never fails the write.
            let extra = data.len().div_ceil(chunk).saturating_sub(1).min(SPARES);
            for slot in spare.iter_mut().take(extra) {
                *slot = self.inner.pool.take(chunk).ok();
            }
        }
        let mut stream = Stream {
            data,
            staged: 0,
            base: 0,
            block,
            chunk,
            first: None,
            spare,
        };
        let _at = stream.fill(&mut first);
        stream.first = Some(first);
        Ok(stream)
    }

    /// Submits a staged write on `lane`.
    pub(crate) fn pump(&self, lane: &mut Lane<P::Queue>, src: &mut impl Source) -> Pumped {
        pump(lane, &self.inner.file, self.inner.policy.dsync, src)
    }

    /// Poisons the domain after a failure past submission and returns the
    /// matching error.
    pub(crate) fn unknown(&self, op: Op, raw: Option<OsError>, c: ErrorContext) -> Error {
        self.inner.domain.poison(FirstCause { op: Some(op), raw });
        self.wake_frontiers();
        Error::DurabilityUnknown { op, raw, ctx: c }
    }

    /// The error for a write refused before it reached the device: the media
    /// is untouched and nothing is poisoned.
    pub(crate) fn refused(raw: Option<OsError>, c: ErrorContext) -> Error {
        match raw {
            Some(raw) => Error::Io {
                op: Op::Write,
                raw,
                ctx: c,
            },
            None => not_written(NotWrittenCause::QueueFull, c),
        }
    }

    pub(crate) fn wake_frontiers(&self) {
        for r in self.regions_snapshot() {
            if let Some(f) = &r.frontier {
                f.wake_all();
            }
        }
    }

    /// Reserves `len` bytes on an append frontier, waiting (never failing)
    /// while too many appends are in flight ahead of it.
    pub(crate) fn reserve<'a>(
        &'a self,
        f: &'a AppendFrontier,
        len: u64,
        c: ErrorContext,
    ) -> Result<Reserved<'a, P>, Error> {
        let domain = &self.inner.domain;
        match f.reserve_wait(len, || domain.poisoned().is_some()) {
            Ok(r) => Ok(Reserved {
                store: self,
                frontier: f,
                r: Some(r),
            }),
            Err(cause) => match domain.poisoned() {
                Some(first) => Err(Error::Poisoned { first }),
                None => Err(not_written(cause, c)),
            },
        }
    }

    /// The domain barrier. Returns the flush it waited for: on a
    /// flush-required device every write completed before the call is durable
    /// once it returns; on a power-safe device writes were durable at
    /// completion and no flush is issued (`u64::MAX`).
    pub(crate) fn barrier(&self, lane: &mut Lane<P::Queue>) -> Result<u64, Error> {
        if !self.inner.policy.flush {
            return match self.inner.domain.poisoned() {
                Some(first) => Err(Error::Poisoned { first }),
                None => Ok(u64::MAX),
            };
        }
        let need = self.inner.domain.ticket();
        let file = &self.inner.file;
        self.inner
            .domain
            .barrier(need, || flush_data(lane, file))
            .inspect_err(|_| self.wake_frontiers())?;
        Ok(need)
    }

    /// A ticket for a write of `r` that has just completed.
    pub(crate) fn ticket(&self, r: &LiveRegion, start: u64, end: u64) -> WriteTicket {
        WriteTicket {
            volume: self.inner.volume,
            region: r.id,
            generation: r.generation,
            start,
            end,
            need: self.inner.domain.ticket(),
            epoch: self.inner.epoch,
        }
    }

    /// A receipt for `r`, backed by flush `need` and, for append regions, a
    /// durable prefix ending at `through`.
    pub(crate) fn receipt(
        &self,
        r: &LiveRegion,
        start: u64,
        through: u64,
        need: u64,
        prefix: bool,
    ) -> DurableReceipt {
        DurableReceipt {
            volume: self.inner.volume,
            region: r.id,
            generation: r.generation,
            class: self.inner.policy.class,
            label: self.inner.policy.label,
            start,
            through,
            untorn: false,
            need,
            prefix,
            epoch: self.inner.epoch,
        }
    }

    /// Reads raw bytes of `r` at a block-aligned `offset` into `out`.
    pub(crate) fn read_at(
        &self,
        r: &LiveRegion,
        offset: u64,
        out: &mut [u8],
    ) -> Result<usize, Error> {
        let block = self.block();
        let c = ctx_range(r, offset, offset.saturating_add(out.len() as u64));
        if offset % block != 0 {
            return Err(not_written(NotWrittenCause::Misaligned, c));
        }
        if out.is_empty() {
            return Ok(0);
        }
        let len = align_up(out.len() as u64, block)
            .ok_or_else(|| not_written(NotWrittenCause::TooLarge, c))?;
        if offset.checked_add(len).is_none_or(|e| e > r.data_size) {
            return Err(not_written(NotWrittenCause::OutOfBounds, c));
        }
        let chunk = self.chunk() as u64;
        let mut lane = self.lane(r);
        let mut done = 0usize;
        while (done as u64) < len {
            let piece = chunk.min(len - done as u64);
            let piece_len =
                usize::try_from(piece).map_err(|_| not_written(NotWrittenCause::TooLarge, c))?;
            let buf = self.take(piece_len, c)?;
            let completed = run(
                &mut lane,
                IoOp::Read {
                    file: &self.inner.file,
                    offset: r.data_offset + offset + done as u64,
                    buf,
                },
            )
            .map_err(|(_b, raw)| match raw {
                Some(raw) => Error::Io {
                    op: Op::Read,
                    raw,
                    ctx: c,
                },
                None => not_written(NotWrittenCause::QueueFull, c),
            })?;
            let n = completed.result.map_err(|_raw| Error::Corruption {
                kind: CorruptionKind::MediaError,
                ctx: c,
            })?;
            let Some(buf) = completed.buf else {
                return Ok(done.min(out.len()));
            };
            let want = (out.len() - done.min(out.len())).min(n);
            out[done..done + want].copy_from_slice(&buf.as_slice()[..want]);
            done += want;
            if n < piece_len || done >= out.len() {
                break;
            }
        }
        Ok(done)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::Path;

    use store_io_sim::{SimConfig, SimPlatform};

    use crate::{Store, StoreOptions};

    #[test]
    fn test_an_abandoned_reservation_poisons_the_domain() {
        // A reservation dropped without completing (an error path or a
        // panic between reserving and writing) leaves a hole: the guard must
        // poison, so no later append is acknowledged past it.
        let p = SimPlatform::new(SimConfig::volatile(1));
        let s = Store::create(p, Path::new("/db"), StoreOptions::default()).unwrap();
        let wal = s.provision_append_region("wal", 1 << 20).unwrap();
        let (f, _pass) = wal.writable().unwrap();
        let reserved = s
            .reserve(f, 100, store_io_core::error::ErrorContext::default())
            .unwrap();
        assert!(s.inner.domain.poisoned().is_none());
        drop(reserved);
        assert!(s.inner.domain.poisoned().is_some());
        assert!(wal.append(b"x").is_err());
    }
}
