//! The I/O paths shared by the region handles and batches: staging caller
//! bytes into pooled aligned buffers, writes of any length, the reservation
//! guard, barriers, tickets, receipts and reads.

use store_io_core::align::align_up;
use store_io_core::class::Missing;
use store_io_core::error::{
    ByteRange, CorruptionKind, Error, ErrorContext, FirstCause, NotWrittenCause, Op, OsError,
};
use store_io_core::observe::{Counter, Stage};
use store_io_platform::{IoBuf, IoOp, Platform, QueueConfig};

use std::ops::ControlFlow;

use crate::exec::{Lane, LaneGuard, Next, Pumped, Ran, Source, clear_stale, pump, run};
use crate::frontier::{AppendFrontier, Reservation};
use crate::mapped::Mapped;
use crate::receipt::{DurableReceipt, WriteTicket};
use crate::scan::ScanItem;
use crate::settle::settle_read;
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
    /// Region offset of `data[0]` (placed by [`crate::mapped::Mapped`]).
    base: u64,
    block: usize,
    chunk: usize,
    first: Option<IoBuf>,
    /// Free buffers for later pieces (a write of several only): at most
    /// [`SPARES`] taken at the start, the first buffer joining them once its
    /// piece is written; only buffers this stream handed out come back.
    spare: Vec<IoBuf>,
}

impl Stream<'_> {
    /// Sets where the data goes (after the reservation): a region offset.
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
    fn is_split(&self) -> bool {
        self.data.len() > self.chunk
    }

    fn next(&mut self) -> Next {
        if let Some(buf) = self.first.take() {
            return Next::Write(self.base, buf);
        }
        if self.staged >= self.data.len() {
            return Next::Done;
        }
        let Some(mut buf) = self.spare.pop() else {
            return Next::Wait;
        };
        let at = self.fill(&mut buf);
        Next::Write(self.base + at as u64, buf)
    }

    fn recycle(&mut self, buf: IoBuf) {
        if self.staged < self.data.len() {
            self.spare.push(buf);
        }
    }
}

/// Pooled buffers written in order from a region offset (a batch).
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

/// Buffers written at their own region offsets (a page batch).
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

/// An append reservation that poisons the device domain unless it is handed
/// on ([`Self::disarm`]) to the settle step ([`crate::settle`], which
/// completes it or poisons) or to a `Submitter` row (whose completion does,
/// the submission's refusal poisoning through `Store::unknown`): a reserved
/// range left unwritten is a hole no later append may be acknowledged past.
/// Dropping it before then (an error, or a panic while it is held) poisons
/// and wakes every waiter.
pub(crate) struct Reserved<'a, P: Platform>
where
    P::Queue: Send,
{
    store: &'a Store<P>,
    r: Option<Reservation>,
}

impl<P: Platform> Reserved<'_, P>
where
    P::Queue: Send,
{
    pub(crate) fn range(&self) -> Reservation {
        // Always `Some` until `disarm` consumes the guard.
        self.r.unwrap_or(Reservation {
            start: 0,
            data_end: 0,
            end: 0,
        })
    }

    /// Hands the reservation on: to the settle step ([`crate::settle`]),
    /// which completes it or poisons, called immediately before settling
    /// with nothing that can fail in between; or, in a `Submitter`'s
    /// append, once its row holds it (or its refusal has poisoned).
    pub(crate) fn disarm(mut self) -> Reservation {
        let r = self.range();
        self.r = None;
        r
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
    /// [`Self::ready`] for a write to `r`, which also refuses a read-only
    /// region (naming it).
    pub(crate) fn ready_for(&self, r: &LiveRegion) -> Result<(), Error> {
        self.ready()?;
        if r.is_read_only() {
            return Err(not_written(NotWrittenCause::ReadOnly, ctx(r)));
        }
        Ok(())
    }

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

    /// The smallest read the bisection of a failed read goes down to: the
    /// file system's direct-I/O offset alignment (or the device's logical
    /// block, if larger), but only where the probe reported that alignment.
    /// Without it the store block: a layered device (dm-crypt with 4 KiB
    /// sectors over a 512e disk, a kernel without `STATX_DIOALIGN`) may refuse
    /// smaller direct reads. Always a power of two dividing the block.
    pub(crate) fn sector(&self) -> u64 {
        let ev = &self.inner.report.evidence;
        if ev.missing.contains(Missing::DioAlignment) || ev.fs.dio_offset_align == 0 {
            return self.block();
        }
        let need = u64::from(ev.device.logical_block.max(ev.fs.dio_offset_align));
        need.next_power_of_two().min(self.block())
    }

    /// The blocking layers' largest single write or read: the largest
    /// ordinary pool buffer (`max_io`), in whole blocks. The larger buffers
    /// are for submitter operations (D-101).
    pub(crate) fn chunk(&self) -> usize {
        self.inner.chunk
    }

    /// A submitter's largest single operation: the pool's largest buffer
    /// (`max_write`), in whole blocks.
    pub(crate) fn max_op(&self) -> usize {
        let block = self.block() as usize;
        (self.inner.pool.max_len() / block * block).max(block)
    }

    /// A free queue for a call on `r`, cleared of anything an earlier holder
    /// abandoned ([`clear_stale`]) before the call reserves space or submits
    /// anything.
    ///
    /// # Errors
    ///
    /// `Io` for `op` if clearing failed: nothing of the call happened.
    pub(crate) fn lane(&self, r: &LiveRegion, op: Op) -> Result<LaneGuard<'_, P::Queue>, Error> {
        self.cleared(self.inner.queues.get(queue_hint(r.id)), r, op)
    }

    /// A parallel lane for a call on `r` that submits several independent
    /// operations before it waits (D-096); a shared lane if the parallel
    /// queues cannot be created. Cleared as [`Self::lane`].
    ///
    /// # Errors
    ///
    /// As [`Self::lane`].
    pub(crate) fn par_lane(
        &self,
        r: &LiveRegion,
        op: Op,
    ) -> Result<LaneGuard<'_, P::Queue>, Error> {
        let inner = &*self.inner;
        let lane = inner.queues.get_parallel(queue_hint(r.id), |n| {
            let cfg = QueueConfig {
                depth: inner.opts.queue_depth.max(1),
                parallel: true,
                pool_threads: inner.opts.t3_pool_threads,
                ..QueueConfig::default()
            };
            (0..n).map(|_| inner.platform.queue(cfg).ok()).collect()
        });
        self.cleared(lane, r, op)
    }

    fn cleared<'a>(
        &self,
        mut lane: LaneGuard<'a, P::Queue>,
        r: &LiveRegion,
        op: Op,
    ) -> Result<LaneGuard<'a, P::Queue>, Error> {
        clear_stale(&mut lane).map_err(|raw| Error::Io {
            op,
            raw,
            ctx: ctx(r),
        })?;
        Ok(lane)
    }

    /// The largest ordinary pool buffer, as the pool was built: `max_io`
    /// (rounded and clamped), or the smallest class if that is larger.
    pub(crate) fn ordinary(&self) -> usize {
        self.inner.ordinary
    }

    /// A pooled buffer of at least `len` bytes, with its length set to `len`,
    /// from the ordinary classes (never a submitter's large buffers).
    pub(crate) fn take(&self, len: usize, c: ErrorContext) -> Result<IoBuf, Error> {
        self.inner
            .pool
            .take_within(len, self.ordinary())
            .map_err(|_| not_written(NotWrittenCause::PoolExhausted, c))
    }

    /// As [`Self::take`], from any class (a submitter's buffer).
    pub(crate) fn take_any(&self, len: usize, c: ErrorContext) -> Result<IoBuf, Error> {
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
        let mut spare = Vec::new();
        if data.len() > chunk {
            // Extra buffers are an optimisation (writes in flight together);
            // one buffer is enough, so a short pool never fails the write.
            let extra = data.len().div_ceil(chunk).saturating_sub(1).min(SPARES);
            spare.reserve_exact(extra + 1);
            spare.extend((0..extra).filter_map(|_| self.inner.pool.take_within(chunk, chunk).ok()));
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

    /// Stages `data` (non-empty, whole blocks after padding) and writes it at
    /// region offset `offset` of `r`, reporting the outcome as the pump does
    /// (G1 P4b). Data of one chunk lying in one run is one write: on an idle
    /// lane of a queue that performs operations at once it is performed here,
    /// with no stream, no mapping and no pump, settled exactly as the pump
    /// settles a performed write (the buffer back to the pool, a short
    /// transfer or an error a failure, a refusal before submission
    /// `Refused`); on any other lane the same staged buffer goes to the
    /// pump. Anything larger or crossing a run takes the pump as before.
    pub(crate) fn write_staged(
        &self,
        r: &LiveRegion,
        offset: u64,
        data: &[u8],
        c: ErrorContext,
    ) -> Result<Pumped, Error> {
        let (block, chunk) = (self.block() as usize, self.chunk());
        let padded = data.len().div_ceil(block) * block;
        if data.len() > chunk || r.map.run_left(offset) < padded as u64 {
            let mut stream = self.stage(data, c)?;
            stream.at(offset);
            let mut lane = self.lane(r, Op::Write)?;
            return Ok(self.pump(&mut lane, &mut Mapped::new(&r.map, &mut stream)));
        }
        // As `stage` fills its first buffer, and in the same order: the
        // buffer, then the lane.
        let mut buf = self.take(padded, c)?;
        let dst = buf.as_mut_capacity();
        dst[..data.len()].copy_from_slice(data);
        dst[data.len()..padded].fill(0);
        // `padded <= chunk <= capacity`: the length always fits.
        let _fits = buf.set_len(padded);
        let mut lane = self.lane(r, Op::Write)?;
        let obs = &self.inner.observer;
        let op = IoOp::Write {
            file: &self.inner.file,
            offset: r.map.locate(offset, padded as u64),
            buf,
            dsync: self.inner.policy.dsync,
        };
        // One span for the write, however it goes (as `Self::pump`'s).
        let _timed = obs.span(Stage::Write);
        match lane.perform(op) {
            Ok(Ok((buf, result))) => {
                obs.add(Counter::Writes, 1);
                obs.add(Counter::WriteBytes, padded as u64);
                Ok(match (result, buf) {
                    (Ok(n), Some(buf)) if n == buf.len() => Pumped::All,
                    (result, _) => Pumped::Failed(result.err()),
                })
            }
            // Not performed here: the pump, with the buffer already staged,
            // inside this span (`Self::pump`'s accounting: one chunk in one
            // run is never split).
            Ok(Err(IoOp::Write { buf, .. })) => {
                let mut stream = Stream {
                    data,
                    staged: data.len(),
                    base: 0,
                    block,
                    chunk,
                    first: Some(buf),
                    spare: Vec::new(),
                };
                stream.at(offset);
                let outcome = pump(
                    &mut lane,
                    &self.inner.file,
                    self.inner.policy.dsync,
                    &mut Mapped::new(&r.map, &mut stream),
                    obs,
                );
                if matches!(outcome, Pumped::Refused(_)) {
                    obs.add(Counter::Refused, 1);
                }
                Ok(outcome)
            }
            // The operation came back as something else (a backend breaking
            // its contract): what it did is unknown, so it fails.
            Ok(Err(_)) => Ok(Pumped::Failed(None)),
            Err(rejected) => {
                obs.add(Counter::Refused, 1);
                Ok(Pumped::Refused(rejected.raw))
            }
        }
    }

    /// Submits a staged write on `lane`.
    pub(crate) fn pump(&self, lane: &mut Lane<P::Queue>, src: &mut impl Source) -> Pumped {
        let obs = &self.inner.observer;
        let _timed = obs.span(Stage::Write);
        let outcome = pump(lane, &self.inner.file, self.inner.policy.dsync, src, obs);
        if matches!(outcome, Pumped::Refused(_)) {
            obs.add(Counter::Refused, 1);
        } else if src.is_split() {
            obs.add(Counter::Splits, 1);
        }
        outcome
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

    /// `r`, reserved on an append frontier, under the guard that poisons
    /// unless it is disarmed (a `Submitter`'s append, until its row holds
    /// the reservation).
    pub(crate) fn hold_reservation(&self, r: Reservation) -> Reserved<'_, P> {
        Reserved {
            store: self,
            r: Some(r),
        }
    }

    /// A lane for an append on `r` (cleared first, as [`Self::lane`]) and
    /// its reservation of `len` bytes on `f`, waiting (never failing) while
    /// too many appends are in flight ahead of it, as
    /// [`AppendFrontier::reserve_wait`] does, but with no lane held: the
    /// appends ahead may be a `Submitter`'s, which complete only when its
    /// owner reaps them, and the owner may first need a shared lane.
    ///
    /// # Errors
    ///
    /// `Io` if a lane could not be cleared (nothing reserved), `Poisoned`, or
    /// `NotWritten` as [`AppendFrontier::reserve`].
    pub(crate) fn lane_and_reservation<'a>(
        &'a self,
        r: &LiveRegion,
        f: &'a AppendFrontier,
        len: u64,
        c: ErrorContext,
    ) -> Result<(LaneGuard<'a, P::Queue>, Reserved<'a, P>), Error> {
        let domain = &self.inner.domain;
        let refused = |cause| match domain.poisoned() {
            Some(first) => Error::Poisoned { first },
            None => not_written(cause, c),
        };
        loop {
            let lane = self.lane(r, Op::Write)?;
            // Never reserve on a poisoned store (`reserve_wait`'s rule).
            if domain.poisoned().is_some() {
                return Err(refused(NotWrittenCause::TooManyInFlight));
            }
            // The prefix a refusal is judged against, read before the
            // reservation (`reserve_wait`'s reasoning).
            let seen = f.completed();
            match f.reserve(len) {
                Ok(res) => {
                    return Ok((
                        lane,
                        Reserved {
                            store: self,
                            r: Some(res),
                        },
                    ));
                }
                Err(NotWrittenCause::TooManyInFlight) => {
                    drop(lane);
                    if !f.wait_completed(seen + 1, || domain.poisoned().is_some()) {
                        return Err(refused(NotWrittenCause::TooManyInFlight));
                    }
                }
                Err(cause) => return Err(refused(cause)),
            }
        }
    }

    /// The domain barrier. Returns the flush it waited for: on a
    /// flush-required device every write completed before the call is durable
    /// once it returns; on a power-safe device writes were durable at
    /// completion and no flush is issued (`u64::MAX`). Only the thread that
    /// leads the flush uses a queue: the store's flush lane, which no one
    /// else uses, so a leader never waits for a queue that a follower holds
    /// (D-070). Followers wait holding nothing.
    pub(crate) fn barrier(&self) -> Result<u64, Error> {
        if !self.inner.policy.flush {
            self.inner.observer.add(Counter::Barriers, 1);
            return match self.inner.domain.poisoned() {
                Some(first) => Err(Error::Poisoned { first }),
                None => Ok(u64::MAX),
            };
        }
        let need = self.inner.domain.ticket();
        self.barrier_to(need)?;
        Ok(need)
    }

    /// The barrier for a sync on `r`: a data flush when the store needs one
    /// or the region forces it (D-102). Returns the flush waited for and
    /// whether a flush backs it.
    pub(crate) fn barrier_for(&self, r: &LiveRegion) -> Result<(u64, bool), Error> {
        if !self.inner.policy.flush && !r.forced() {
            return self.barrier().map(|need| (need, false));
        }
        let need = self.inner.domain.ticket();
        self.barrier_to(need)?;
        Ok((need, true))
    }

    /// The domain barrier for a ticket taken earlier (after the writes it
    /// covers completed): a flush `>= need` (flush-required stores only).
    pub(crate) fn barrier_to(&self, need: u64) -> Result<(), Error> {
        let obs = &self.inner.observer;
        obs.add(Counter::Barriers, 1);
        let _timed = obs.span(Stage::Barrier);
        let inner = &self.inner;
        inner
            .domain
            .barrier(need, |slot, _k| {
                let _flush = obs.span(Stage::Flush);
                obs.add(Counter::Flushes, 1);
                // Each slot its own handle and lane (D-124); the domain
                // never hands out a slot without both.
                let file = match slot {
                    0 => &inner.file,
                    s => &inner.flush_files[s - 1],
                };
                match inner.queues.flush_lane(slot) {
                    Some(mut lane) => flush_data(&mut lane, file),
                    None => flush_data(&mut inner.queues.get(0), file),
                }
            })
            .inspect_err(|_| self.wake_frontiers())
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
        synced: bool,
    ) -> DurableReceipt {
        DurableReceipt {
            volume: self.inner.volume,
            region: r.id,
            generation: r.generation,
            class: self.inner.policy.class,
            label: self.inner.policy.label,
            start,
            through,
            // Never untorn: a write may be several device writes (pieces of
            // one larger than a buffer, or across runs, spec §11), each
            // durable or not on its own.
            untorn: false,
            need,
            prefix,
            synced,
            epoch: self.inner.epoch,
        }
    }

    /// A receipt for exactly a ticket's range, backed by flush `need`
    /// (`u64::MAX` on a power-safe store, where writes are durable at
    /// completion).
    pub(crate) fn receipt_for(&self, t: &WriteTicket, need: u64) -> DurableReceipt {
        DurableReceipt {
            volume: t.volume,
            region: t.region,
            generation: t.generation,
            class: self.inner.policy.class,
            label: self.inner.policy.label,
            start: t.start,
            through: t.end,
            // As in `receipt`: never untorn.
            untorn: false,
            need,
            prefix: false,
            // A real flush number: not the power-safe "durable at
            // completion" mark, nor 0 (no barrier yet).
            synced: need != u64::MAX && need != 0,
            epoch: t.epoch,
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
        if offset.checked_add(len).is_none_or(|e| e > r.map.len()) {
            return Err(not_written(NotWrittenCause::OutOfBounds, c));
        }
        let _timed = self.inner.observer.span(Stage::Read);
        let chunk = self.chunk() as u64;
        if len > chunk {
            return self.read_pipelined(r, offset, len, out, c);
        }
        let mut lane = self.lane(r, Op::Read)?;
        let mut done = 0usize;
        while (done as u64) < len {
            // One run at a time: a piece never crosses a run's end.
            let at = offset + done as u64;
            let piece = chunk.min(len - done as u64).min(r.map.run_left(at));
            let piece_len =
                usize::try_from(piece).map_err(|_| not_written(NotWrittenCause::TooLarge, c))?;
            let buf = self.take(piece_len, c)?;
            let ran = run(
                &mut lane,
                IoOp::Read {
                    file: &self.inner.file,
                    offset: r.map.locate(at, piece),
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
            let (got, result) = match ran {
                Ran::Done { buf, result } => (buf, result),
                // The wait failed with the read in flight: a failure of the
                // queue, no verdict on the media.
                Ran::Abandoned(raw) => {
                    return Err(Error::Io {
                        op: Op::Read,
                        raw,
                        ctx: c,
                    });
                }
            };
            // The verdict, as for a submitter's read (`settle_read`): the
            // bytes read, a media error only when the device says so (else
            // the raw code: accuracy), or no bytes back.
            let start = offset + done as u64;
            let end = start + piece;
            let n = match settle_read(self, r, start, end, got.as_ref(), result) {
                Ok(n) => n,
                Err(Error::Corruption {
                    kind: CorruptionKind::MediaError,
                    ..
                }) => {
                    // Name the exact unreadable range, found by the scan's
                    // bisection, not just the whole read. The queue and the
                    // failed read's buffer are returned first: the scan takes
                    // its own. If the rescan itself fails (a short pool, a
                    // full queue), the media error stands for the whole
                    // piece rather than being replaced by that failure.
                    drop(got);
                    drop(lane);
                    let (bad_start, bad_end) =
                        self.first_unreadable(r, start, end).unwrap_or((start, end));
                    return Err(Error::Corruption {
                        kind: CorruptionKind::MediaError,
                        ctx: ctx_range(r, bad_start, bad_end),
                    });
                }
                Err(e) => return Err(e),
            };
            // `settle_read` returns the length only when the buffer came
            // back (else `Interrupted`): this cannot fail.
            let Some(buf) = got else {
                return Err(not_written(NotWrittenCause::Interrupted, c));
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

    /// The first unreadable range in `[start, end)` of `r`, found by the
    /// scan's bisection; the whole range if the device reads it all now (a
    /// transient failure is still reported, never hidden).
    fn first_unreadable(&self, r: &LiveRegion, start: u64, end: u64) -> Result<(u64, u64), Error> {
        let mut first = None;
        let _summary = self.scan_region(r, start, end, false, |item| match item {
            ScanItem::Unreadable { start, end } => {
                first = Some((start, end));
                ControlFlow::Break(())
            }
            ScanItem::Data { .. } => ControlFlow::Continue(()),
        })?;
        Ok(first.unwrap_or((start, end)))
    }

    /// A read larger than one pooled buffer: the scan's pipeline (several
    /// large reads in flight, delivered in order) copying into `out`. A range
    /// the device cannot read is a media error, as for a single read.
    fn read_pipelined(
        &self,
        r: &LiveRegion,
        offset: u64,
        len: u64,
        out: &mut [u8],
        c: ErrorContext,
    ) -> Result<usize, Error> {
        let mut copied = 0usize;
        let mut unreadable = None;
        let summary = self.scan_region(r, offset, offset + len, false, |item| match item {
            ScanItem::Data { offset: at, bytes } => {
                let start = (at - offset) as usize;
                if start < out.len() {
                    let n = bytes.len().min(out.len() - start);
                    out[start..start + n].copy_from_slice(&bytes[..n]);
                    copied = copied.max(start + n);
                }
                ControlFlow::Continue(())
            }
            ScanItem::Unreadable { start, end } => {
                unreadable = Some((start, end));
                ControlFlow::Break(())
            }
        })?;
        if unreadable.is_some() || summary.unreadable > 0 {
            // Name the exact range, as a single read does.
            let ctx = match unreadable {
                Some((start, end)) => ctx_range(r, start, end),
                None => c,
            };
            self.inner.observer.add(Counter::MediaErrors, 1);
            return Err(Error::Corruption {
                kind: CorruptionKind::MediaError,
                ctx,
            });
        }
        Ok(copied)
    }
}

/// Where a region's search for a free queue starts: its id folded to a
/// `usize` (format 2 ids carry the page in their high bits).
#[allow(clippy::cast_possible_truncation)]
fn queue_hint(id: store_io_core::id::RegionId) -> usize {
    let id = id.get();
    (id ^ (id >> 32)) as usize
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
        let (lane, reserved) = s
            .lane_and_reservation(
                wal.state(),
                f,
                100,
                store_io_core::error::ErrorContext::default(),
            )
            .unwrap();
        drop(lane);
        assert!(s.inner.domain.poisoned().is_none());
        drop(reserved);
        assert!(s.inner.domain.poisoned().is_some());
        assert!(wal.append(b"x").is_err());
    }

    #[test]
    fn test_a_flush_leader_never_waits_for_a_shared_queue() {
        // D-070, deterministically: the store's only shared queue is held
        // (as a slot commit following a flush holds it), and a barrier that
        // must lead a flush still completes, through the flush lane. Without
        // the flush lane the leader waits for that queue forever.
        let p = SimPlatform::new(SimConfig::volatile(2));
        let opts = StoreOptions {
            queues: 1,
            ..StoreOptions::default()
        };
        let s = Store::create(p, Path::new("/db"), opts).unwrap();
        let wal = s.provision_append_region("wal", 1 << 20).unwrap();
        let ticket = wal.append(b"needs a flush").unwrap();
        let before = s.domain_stats();
        let held = s.inner.queues.get(0);
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let r = wal.sync_through(&ticket);
            tx.send(r.is_ok()).unwrap();
        });
        let finished = rx.recv_timeout(std::time::Duration::from_secs(30));
        drop(held);
        waiter.join().unwrap();
        assert_eq!(
            finished,
            Ok(true),
            "the flush leader waited for the shared queue the test holds"
        );
        // The barrier really led a flush (not a shortcut that needs none).
        let after = s.domain_stats();
        assert_eq!(after.led - before.led, 1, "no flush was led");
        assert!(after.flushes > before.flushes, "no flush was issued");
    }
}
