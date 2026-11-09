//! The engine-layer [`Submitter`] (D-065, D-080, D-098): many operations in
//! flight from one thread, without blocking, with the same integrity rules
//! as the blocking calls.
//!
//! Every operation is admitted like a blocking call (the region's gate, the
//! store's state, the same validation), then held in a fixed table until its
//! completion is processed by [`Submitter::reap`] or [`Submitter::wait`]:
//!
//! - its region's **owned pass**, so recycle and release wait for it;
//! - for an append, its **frontier reservation**, completed only when the
//!   write transferred every byte (else the store is poisoned: the reserved
//!   range is a hole no later append may be acknowledged past);
//! - its tag and submission sequence.
//!
//! A [`WriteTicket`](crate::WriteTicket) is minted only from a full
//! completion (D-080). Queue tags are table indices plus a generation, so a
//! completion matches exactly one operation. Nothing allocates after
//! creation. Durable writes and barriers ([`durable`]) wait for a flush
//! without blocking: the store's flush thread answers them (D-099).

mod durable;

use std::marker::PhantomData;
use std::sync::Arc;
use store_io_core::IoClass;

use store_io_core::error::{Error, ErrorContext, FirstCause, NotWrittenCause, Op, OsError};
use store_io_core::observe::Counter;
use store_io_platform::{IoBuf, IoOp, Owner, Platform, Queue, QueueConfig};

use crate::completion::{Completions, Rejected, SubmitterConfig, WriteMode};
use crate::exec::LaneMark;
use crate::flusher::{Inbox, SubmitterWaker, WakerOf};
use crate::gate::{Gate, Gated, OwnedPass};
use crate::io::{ctx, ctx_range, not_written};
use crate::lifecycle::retired_error;
use crate::optable::{Finish, Kind, Parked, Split, Table};
use crate::queue_core::Core;
use crate::receipt::RegionPos;
use crate::region::{AppendRegion, PageRegion};
use crate::store::{LiveRegion, Store};

/// The largest `SubmitterConfig::depth`.
pub const MAX_DEPTH: u32 = 4096;

impl Gated for LiveRegion {
    fn gate(&self) -> &Gate {
        &self.gate
    }
}

/// Many reads and writes in flight from one thread (D-065, D-098).
///
/// Submitting never blocks: a full submitter is refused with the buffer
/// given back ([`Rejected`], `NotWritten(QueueFull)`). Completions are
/// handed back by [`Self::reap`] (never blocks) and [`Self::wait`]
/// (blocks), with a [`WriteTicket`](crate::WriteTicket) for every write that
/// completed in full. Buffers come from [`Self::take_buf`] (the store's
/// pool, shared with its blocking calls: buffers held in flight or kept by
/// the caller are not available to them) and come back in the completion.
///
/// The rules the blocking calls keep hold per operation:
///
/// - **An append's position is final at submission** (returned at once),
///   but it counts as written only once its completion is processed by
///   this submitter's `reap` or `wait`. Until then, on that region, from
///   any thread: `sync_through` of any later append waits for it, and an
///   append starting more than `StoreOptions::append_window` bytes past it
///   waits (blocking calls) or is refused `NotWritten(TooManyInFlight)`
///   (submitters). Reap your own appends before syncing or appending far
///   past them, or the thread waits for itself.
/// - **Recycle and release wait** for every operation of the submitter on
///   that region until its completion is processed: a thread must reap its
///   own submitter's operations on a region before it recycles or releases
///   that region.
/// - **A write that fails or transfers short after it was issued** is
///   `DurabilityUnknown`, and the store is poisoned.
///
/// Dropping the submitter waits, untimed, for every operation in flight
/// before it releases anything the device may still touch. A device that
/// never completes an operation blocks the drop: the fail-stop choice
/// (see `Store::oldest_outstanding` for a watchdog).
///
/// A submitter belongs to the thread that created it:
///
/// ```compile_fail,E0277
/// fn need_send<T: Send>() {}
/// need_send::<store_io_engine::Submitter<store_io_sim::SimPlatform>>();
/// ```
pub struct Submitter<P: Platform>
where
    P::Queue: Send,
{
    // Declared first, so its queue drops (after `Drop::drop` drained it)
    // before the table releases the passes and reservations.
    core: Core<P::Queue>,
    /// Where the flush thread answers this submitter's flush requests.
    inbox: Arc<Inbox<<P::Queue as Queue>::Waker>>,
    id: u64,
    pub(crate) store: Store<P>,
    /// The class of its I/O (`SubmitterConfig::class`).
    class: IoClass,
    /// A background submitter's pacing record (one issuer: R5.4).
    issuer: Option<Arc<crate::qos::Issuer>>,
    /// The largest piece of one of its operations (`background_io` for a
    /// background submitter; else unbounded).
    piece_max: usize,
    _thread: PhantomData<*const ()>,
}

impl<P: Platform> core::fmt::Debug for Submitter<P>
where
    P::Queue: Send,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Submitter")
            .field("depth", &self.core.table.depth())
            .field("in_flight", &self.core.table.live)
            .field("ready", &self.core.table.ready.len())
            .finish()
    }
}

/// The request [`Submitter::issue`] turns into an [`IoOp`].
enum Req {
    Write { offset: u64, buf: IoBuf },
    Read { offset: u64, buf: IoBuf },
}

/// The first device operation of `[start, start + buf.len())`: all of it,
/// or (format 2, across a run's end, spec §11) the piece up to the run's
/// end, `buf` shortened to it, the rest following under the same row
/// ([`Split`]). Returns its file offset.
fn first_piece(
    state: &LiveRegion,
    start: u64,
    buf: &mut IoBuf,
    piece_max: usize,
) -> (u64, Option<Split>) {
    let total = buf.len();
    let piece = (state.map.run_left(start) as usize)
        .min(total)
        .min(piece_max);
    let split = (piece < total).then(|| {
        let fits = buf.set_len(piece);
        debug_assert!(fits, "a piece longer than its buffer");
        Split {
            total,
            done: 0,
            piece_max,
        }
    });
    (state.map.locate(start, piece as u64), split)
}

fn rejected(error: Error, buf: IoBuf) -> Rejected {
    Rejected {
        error,
        buf: Some(buf),
    }
}

/// `?` for admission: refuses with the buffer given back.
macro_rules! admit {
    ($e:expr, $buf:ident) => {
        match $e {
            Ok(v) => v,
            Err(error) => return Err(rejected(error, $buf)),
        }
    };
}

impl<P: Platform> Store<P>
where
    P::Queue: Send,
{
    /// A [`Submitter`] for this thread, with its own queue of
    /// `cfg.depth` operations (1 to [`MAX_DEPTH`]).
    ///
    /// # Errors
    ///
    /// `Io` if the platform could not create the queue.
    pub fn submitter(&self, cfg: SubmitterConfig) -> Result<Submitter<P>, Error> {
        let depth = cfg.depth.clamp(1, MAX_DEPTH);
        let qcfg = QueueConfig {
            depth,
            parallel: true,
            pool_threads: self.inner.opts.t3_pool_threads,
            owner: Owner::Thread,
        };
        let inner = &*self.inner;
        // The file outlives the queue: a registering backend keeps its own
        // reference to it (`Platform::queue_registered`).
        let queue = if inner.opts.register_file {
            inner.platform.queue_registered(qcfg, &inner.file)
        } else {
            inner.platform.queue(qcfg)
        };
        let queue = queue.map_err(|raw| Error::Io {
            op: Op::Queue,
            raw,
            ctx: ErrorContext::default(),
        })?;
        let n = depth as usize;
        let mark = Arc::new(LaneMark::new());
        let inbox = Arc::new(Inbox::new(n, queue.waker()));
        let id = self.register_submitter(&mark);
        let background = cfg.class.is_background();
        Ok(Submitter {
            core: Core::new(queue, n, mark),
            inbox,
            id,
            store: self.clone(),
            class: cfg.class,
            issuer: background.then(|| crate::qos::Issuer::new(&inner.qos)),
            piece_max: if background {
                inner.qos.background_io() as usize
            } else {
                usize::MAX
            },
            _thread: PhantomData,
        })
    }
}

impl<P: Platform> Submitter<P>
where
    P::Queue: Send,
{
    /// A buffer of `len` bytes from the store's pool, for one operation.
    ///
    /// # Errors
    ///
    /// `NotWritten(TooLarge)` beyond [`Self::max_buf`];
    /// `NotWritten(PoolExhausted)` if none is free (for a background
    /// submitter: if its class's background half is taken).
    pub fn take_buf(&self, len: usize) -> Result<IoBuf, Error> {
        if len > self.max_buf() {
            return Err(not_written(
                NotWrittenCause::TooLarge,
                ErrorContext::default(),
            ));
        }
        if self.issuer.is_some() {
            // Background work never holds more than half of a class (R5.4).
            return self
                .store
                .inner
                .pool
                .take_capped(len, usize::MAX)
                .map_err(|_| not_written(NotWrittenCause::PoolExhausted, ErrorContext::default()));
        }
        self.store.take_any(len, ErrorContext::default())
    }

    /// The largest buffer, and so the largest single operation, in bytes (a
    /// whole number of blocks): the pool's largest class, `max_write`
    /// rounded up to a power of two (or `max_io` without large buffers). One
    /// OS operation (D-101); the kernel may still split it into several
    /// device requests.
    #[must_use]
    pub fn max_buf(&self) -> usize {
        self.store.max_op()
    }

    /// Operations submitted whose completions have not been handed to
    /// `reap` or `wait` yet: I/O in flight, a background submitter's
    /// operations whose next piece pacing holds back, and durable writes and
    /// barriers waiting for their flush.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        // Never more than are really pending (an operation lost to a panic
        // in processing is no longer waited for; the store is poisoned then).
        // An operation has one piece at a time: in the queue or parked.
        self.core
            .table
            .live
            .min(self.core.queue.in_flight() + self.core.table.parked.len() + self.core.table.asked)
    }

    /// Ends this submitter's current or next [`Self::wait`] from another
    /// thread (D-083), whether it waits for I/O or for a flush.
    #[must_use]
    pub fn waker(&self) -> SubmitterWaker<WakerOf<P>> {
        SubmitterWaker::new(&self.inbox)
    }

    /// The checks every operation makes first: this store's region, room in
    /// the table, and the region's gate.
    fn admit(
        &self,
        store: &Store<P>,
        state: &Arc<LiveRegion>,
        buf: &IoBuf,
    ) -> Result<OwnedPass<LiveRegion>, Error> {
        let c = ctx(state);
        if !Arc::ptr_eq(&store.inner, &self.store.inner) {
            return Err(not_written(NotWrittenCause::ForeignPosition, c));
        }
        // Only this store's buffers have its alignment and whole blocks of
        // capacity; any other could fail at the device after an append's
        // space is reserved.
        if !self.store.inner.pool.owns(buf) {
            return Err(not_written(NotWrittenCause::ForeignBuffer, c));
        }
        if self.core.table.full() {
            return Err(not_written(NotWrittenCause::QueueFull, c));
        }
        OwnedPass::enter(state).map_err(|why| retired_error(why, state))
    }

    /// Appends the buffer's bytes (any length of at least one byte) and
    /// returns their position at once; the completion carries the ticket
    /// (and, with [`WriteMode::Durable`], the receipt).
    /// The append starts at a block boundary and the buffer's tail is
    /// zero-padded in place to the next one (no copy): the buffer comes back
    /// with its length set to the padded size.
    ///
    /// # Errors
    ///
    /// [`Rejected`], with the buffer: `NotWritten` (`QueueFull`: the
    /// submitter is full; `TooManyInFlight`: the region's append ring is
    /// full, see [`Submitter`]; empty, region full, read-only, not
    /// positioned, another store's region or buffer), `Poisoned`, or
    /// `DurabilityUnknown` if the OS refused the write after its position
    /// was reserved (the store is then poisoned, as for a blocking append).
    pub fn append(
        &mut self,
        region: &AppendRegion<P>,
        buf: IoBuf,
        mode: WriteMode,
        tag: u64,
    ) -> Result<RegionPos, Rejected> {
        self.append_with(region, buf, mode, tag, || Ok(()), |()| {})
    }

    /// [`Self::append`], running `before_reserve` after the append's own
    /// checks and before its reservation, and handing what it returned to
    /// `on_reserved` after the reservation and before the write is issued (a
    /// dependent write's gate passes, `after.rs`). Its error, or a refused
    /// reservation, is [`Rejected`] with the buffer and nothing reserved.
    pub(crate) fn append_with<T>(
        &mut self,
        region: &AppendRegion<P>,
        mut buf: IoBuf,
        mode: WriteMode,
        tag: u64,
        before_reserve: impl FnOnce() -> Result<T, Error>,
        on_reserved: impl FnOnce(T),
    ) -> Result<RegionPos, Rejected> {
        let state = region.state();
        let c = ctx(state);
        let pass = admit!(self.admit(region.store(), state, &buf), buf);
        admit!(self.store.ready_for(state), buf);
        let Some(f) = state.frontier.as_ref() else {
            return Err(rejected(not_written(NotWrittenCause::NotReady, c), buf));
        };
        if !state.positioned.load(std::sync::atomic::Ordering::Acquire) {
            return Err(rejected(
                not_written(NotWrittenCause::NotPositioned, c),
                buf,
            ));
        }
        let len = buf.len();
        if len == 0 {
            return Err(rejected(not_written(NotWrittenCause::Empty, c), buf));
        }
        // Checked before the reservation, so the padding below cannot fail
        // (the pool's capacities are whole blocks anyway).
        let block = self.store.block() as usize;
        if len.div_ceil(block) * block > buf.capacity() {
            return Err(rejected(not_written(NotWrittenCause::TooLarge, c), buf));
        }
        let passes = admit!(before_reserve(), buf);
        // A background write's first piece is admitted by pacing before any
        // space is reserved: refused, it is `Paced` with nothing reserved
        // (R5.4); a refused reservation drops the charge.
        let charge = admit!(self.admit_first(c), buf);
        let domain = &self.store.inner.domain;
        let reserved = f
            .reserve(len as u64)
            .map_err(|why| match domain.poisoned() {
                Some(first) => Error::Poisoned { first },
                None => not_written(why, c),
            });
        let r = admit!(reserved, buf);
        // Held until the row holds it: a panic before then (in `locate`, or
        // a backend's `submit`) poisons and wakes every waiter at once,
        // never a hole that a later barrier waits on until this submitter
        // drops. The store is the region's (`admit` checked it is this one).
        let held = region.store().hold_reservation(r);
        let padded = (r.end - r.start) as usize;
        buf.as_mut_capacity()[len..padded].fill(0);
        let _fits = buf.set_len(padded);
        let pos = RegionPos {
            volume: self.store.inner.volume,
            region: state.id,
            generation: state.generation,
            offset: r.start,
        };
        let (offset, split) = first_piece(state, r.start, &mut buf, self.piece_max);
        let req = Req::Write { offset, buf };
        let durable = mode == WriteMode::Durable;
        // Never held through the issue: on a queue that performs at once the
        // write runs inside it (R5.3).
        on_reserved(passes);
        let issued = self.issue(req, tag, Kind::Append(r), pass, durable, split, charge);
        let _r = held.disarm();
        match issued {
            Ok(()) => Ok(pos),
            // Past the reservation: a hole, as for a blocking append.
            Err((raw, buf)) => Err(Rejected {
                error: self
                    .store
                    .unknown(Op::Write, raw, ctx_range(state, r.start, r.data_end)),
                buf,
            }),
        }
    }

    /// Writes the buffer (whole blocks) at `pos`; the completion carries the
    /// ticket (and, with [`WriteMode::Durable`], the receipt).
    ///
    /// # Errors
    ///
    /// [`Rejected`], with the buffer: `NotWritten` (foreign, stale,
    /// misaligned, out of range, submitter full, read-only, another store's
    /// buffer), `Poisoned`, or `Io` if the OS refused it (nothing reached
    /// the device).
    pub fn write(
        &mut self,
        region: &PageRegion<P>,
        pos: RegionPos,
        buf: IoBuf,
        mode: WriteMode,
        tag: u64,
    ) -> Result<(), Rejected> {
        self.write_with(region, pos, buf, mode, tag, || Ok(()))
    }

    /// [`Self::write`], running `before_issue` after the write's own checks
    /// and before its buffer is cut to the first run (so a refusal gives
    /// back the whole buffer); what it returns is dropped before the write
    /// is issued. Its error is [`Rejected`] with the buffer.
    pub(crate) fn write_with<T>(
        &mut self,
        region: &PageRegion<P>,
        pos: RegionPos,
        mut buf: IoBuf,
        mode: WriteMode,
        tag: u64,
        before_issue: impl FnOnce() -> Result<T, Error>,
    ) -> Result<(), Rejected> {
        let state = region.state();
        let pass = admit!(self.admit(region.store(), state, &buf), buf);
        admit!(self.store.ready_for(state), buf);
        let end = admit!(region.check(&pos, buf.len()), buf);
        let c = ctx_range(state, pos.offset, end);
        let passes = admit!(before_issue(), buf);
        // A background write is admitted by pacing before it is issued.
        let charge = admit!(self.admit_first(c), buf);
        let (offset, split) = first_piece(state, pos.offset, &mut buf, self.piece_max);
        let req = Req::Write { offset, buf };
        let kind = Kind::Page {
            start: pos.offset,
            end,
        };
        drop(passes);
        self.issue(
            req,
            tag,
            kind,
            pass,
            mode == WriteMode::Durable,
            split,
            charge,
        )
        .map_err(|(raw, buf)| Rejected {
            error: Store::<P>::refused(raw, c),
            buf,
        })
    }

    /// Reads into the buffer (whole blocks) from the block-aligned `offset`
    /// of an append region; the completion carries the bytes. Reads work on
    /// read-only and poisoned stores. A background submitter's reads are
    /// paced as its writes are.
    ///
    /// # Errors
    ///
    /// [`Rejected`], with the buffer: `NotWritten` (empty, misaligned, out
    /// of range, submitter full, another store's region or buffer; `Paced`
    /// for a background submitter while commits are active: reap and
    /// retry), or `Io` if the OS refused it.
    pub fn read_append(
        &mut self,
        region: &AppendRegion<P>,
        offset: u64,
        buf: IoBuf,
        tag: u64,
    ) -> Result<(), Rejected> {
        self.read(region.store(), region.state(), offset, buf, tag)
    }

    /// Reads into the buffer (whole blocks) at `pos` of a page region; the
    /// completion carries the bytes.
    ///
    /// # Errors
    ///
    /// As [`Self::read_append`], and `NotWritten(ForeignPosition |
    /// StaleGeneration)` for a position of another region or generation.
    pub fn read_page(
        &mut self,
        region: &PageRegion<P>,
        pos: RegionPos,
        buf: IoBuf,
        tag: u64,
    ) -> Result<(), Rejected> {
        let state = region.state();
        let c = ctx(state);
        if pos.volume != self.store.inner.volume || pos.region != state.id {
            return Err(rejected(
                not_written(NotWrittenCause::ForeignPosition, c),
                buf,
            ));
        }
        if pos.generation != state.generation {
            return Err(rejected(
                not_written(NotWrittenCause::StaleGeneration, c),
                buf,
            ));
        }
        self.read(region.store(), state, pos.offset, buf, tag)
    }

    fn read(
        &mut self,
        store: &Store<P>,
        state: &Arc<LiveRegion>,
        offset: u64,
        mut buf: IoBuf,
        tag: u64,
    ) -> Result<(), Rejected> {
        let pass = admit!(self.admit(store, state, &buf), buf);
        let len = buf.len() as u64;
        let block = self.store.block();
        let c = ctx_range(state, offset, offset.saturating_add(len));
        if len == 0 {
            return Err(rejected(not_written(NotWrittenCause::Empty, c), buf));
        }
        if offset % block != 0 || len % block != 0 {
            return Err(rejected(not_written(NotWrittenCause::Misaligned, c), buf));
        }
        if offset.checked_add(len).is_none_or(|e| e > state.map.len()) {
            return Err(rejected(not_written(NotWrittenCause::OutOfBounds, c), buf));
        }
        // A background read is admitted by pacing as a write is (R5.4: the
        // class is the submitter's).
        let charge = admit!(self.admit_first(c), buf);
        let (at, split) = first_piece(state, offset, &mut buf, self.piece_max);
        let req = Req::Read { offset: at, buf };
        let kind = Kind::Read {
            start: offset,
            end: offset + len,
        };
        self.issue(req, tag, kind, pass, false, split, charge)
            .map_err(|(raw, buf)| Rejected {
                error: match raw {
                    Some(raw) => Error::Io {
                        op: Op::Read,
                        raw,
                        ctx: c,
                    },
                    None => not_written(NotWrittenCause::QueueFull, c),
                },
                buf,
            })
    }

    /// Submits an admitted operation into a free row. On a refusal the row
    /// stays free and the pass is released.
    // An operation's request, tag, kind, pass, durability, split and pacing
    // charge are all needed, and none derives from another.
    #[allow(clippy::too_many_arguments)]
    fn issue(
        &mut self,
        req: Req,
        tag: u64,
        kind: Kind,
        pass: OwnedPass<LiveRegion>,
        durable: bool,
        split: Option<Split>,
        charge: Option<crate::qos::Charge>,
    ) -> Result<(), (Option<OsError>, Option<IoBuf>)> {
        // A refusal gives the caller's buffer back whole (a split's first
        // piece is its front: nothing was moved).
        let whole = |mut buf: IoBuf| {
            if let Some(s) = split {
                let fits = buf.set_len(s.total);
                debug_assert!(fits, "a split buffer shorter than its operation");
            }
            buf
        };
        // `admit` checked there is room: a free row always exists here.
        let Some((index, qtag)) = self.core.table.claim() else {
            let buf = match req {
                Req::Write { buf, .. } | Req::Read { buf, .. } => buf,
            };
            return Err((None, Some(whole(buf))));
        };
        let inner = &*self.store.inner;
        let file = &inner.file;
        let (op, write) = match req {
            Req::Write { offset, buf } => {
                let bytes = buf.len() as u64;
                if inner.policy.dsync && !self.class.is_background() {
                    inner.domain.note_commit();
                }
                let op = IoOp::Write {
                    file,
                    offset,
                    buf,
                    dsync: inner.policy.dsync,
                };
                (op, Some(bytes))
            }
            Req::Read { offset, buf } => (IoOp::Read { file, offset, buf }, None),
        };
        // Recorded (and the busy mark published) before the submission: a
        // queue may do the I/O inside `submit`.
        self.core.table.start(index, tag, kind, Some(pass), durable);
        if let Some(s) = split {
            self.core.table.set_split(index, s);
        }
        // Held by the row: released when the piece completes, or with the
        // row (a refused submission below aborts it).
        self.core.table.set_charge(qtag, charge);
        if let Err(r) = self.core.queue.submit(op, qtag) {
            self.core.table.abort(index);
            if r.raw.is_none() {
                inner.observer.add(Counter::Refused, 1);
            }
            return Err((r.raw, r.buf.map(whole)));
        }
        if let Some(bytes) = write {
            inner.observer.add(Counter::Writes, 1);
            inner.observer.add(Counter::WriteBytes, bytes);
        }
        Ok(())
    }

    /// Hands back every completion that has arrived, without blocking.
    #[must_use = "completions not taken stay with the submitter and count against its depth"]
    pub fn reap(&mut self) -> Completions<'_> {
        let _added = self.core.queue.reap(&mut self.core.raw);
        self.process();
        Completions {
            ready: &mut self.core.table.ready,
        }
    }

    /// Blocks until at least `min` completions are ready to hand back (no
    /// more than are in flight or ready), or the submitter's waker fires,
    /// then hands back every one that has arrived.
    ///
    /// # Errors
    ///
    /// `Io` if waiting itself failed: the operations stay in flight and a
    /// later `reap` or `wait` collects them (a failed wait is never a
    /// verdict on the media, D-097).
    #[must_use = "completions not taken stay with the submitter and count against its depth"]
    pub fn wait(&mut self, min: usize) -> Result<Completions<'_>, Error> {
        let want = min.min(self.core.table.live + self.core.table.ready.len());
        if self.core.table.ready.len() >= want {
            let _added = self.core.queue.reap(&mut self.core.raw);
            self.process();
        }
        while self.core.table.ready.len() < want {
            // Only the caller's waker ends a wait early: a wake left in the
            // queue by the flush thread just loops.
            if self.inbox.take_woken() {
                break;
            }
            let in_queue = self.core.queue.in_flight();
            if in_queue > 0 {
                let need = (want - self.core.table.ready.len()).min(in_queue);
                let waited = if self.inbox.block_in_queue() {
                    let waited = self.core.queue.wait(need, &mut self.core.raw);
                    self.inbox.unblock();
                    waited
                } else {
                    // A flush outcome or a wake arrived: no blocking.
                    Ok(self.core.queue.reap(&mut self.core.raw))
                };
                // Whatever arrived is processed, even if the wait then failed.
                self.process();
                if let Err(raw) = waited {
                    return Err(Error::Io {
                        op: if self.core.table.writes_live() {
                            Op::Write
                        } else {
                            Op::Read
                        },
                        raw,
                        ctx: ErrorContext::default(),
                    });
                }
            } else if self.core.table.asked > 0 {
                self.inbox.wait(true);
                self.process();
            } else if !self.core.table.parked.is_empty() {
                // Only parked pieces: nothing of this submitter in flight, so
                // the oldest goes out (R5.4: a wait never ends early on pieces
                // pacing held back).
                self.unpark_idle();
            } else {
                break;
            }
        }
        Ok(Completions {
            ready: &mut self.core.table.ready,
        })
    }

    /// Processes every raw completion collected, then resubmits parked
    /// pieces while pacing admits them, then asks for the flushes the pass
    /// wants and takes the answers.
    fn process(&mut self) {
        self.process_with(true);
    }

    /// [`Self::process`]; with `issue` false nothing more is submitted, no
    /// parked piece and no next piece (parked instead: a drop whose wait
    /// failed, fail-stop; its live writes poison the store).
    fn process_with(&mut self, issue: bool) {
        let class = self.barrier_class();
        let Self {
            core,
            inbox,
            store,
            issuer,
            ..
        } = self;
        let _unwind = PoisonOnUnwind::new(store);
        let mut pieces = Pieces {
            table: &mut core.table,
            queue: &mut core.queue,
            store,
            issuer: issuer.as_ref(),
            issue,
        };
        for c in core.raw.drain() {
            // Only a write needing a flush asks (see `Table::finish`).
            let finished = pieces.table.finish(store, c.tag, c.buf, c.result);
            pieces.follow(finished);
        }
        if issue {
            pieces.unpark(false);
        }
        pieces.ask_and_take(inbox, class);
    }

    /// Resubmits parked pieces with nothing in flight (a wait, a drop): the
    /// oldest always goes out.
    fn unpark_idle(&mut self) {
        let class = self.barrier_class();
        let Self {
            core,
            inbox,
            store,
            issuer,
            ..
        } = self;
        let _unwind = PoisonOnUnwind::new(store);
        let mut pieces = Pieces {
            table: &mut core.table,
            queue: &mut core.queue,
            store,
            issuer: issuer.as_ref(),
            issue: true,
        };
        pieces.unpark(true);
        pieces.ask_and_take(inbox, class);
    }

    /// The class its barriers run as: a background submitter's never mark
    /// commits active; a read-class submitter's are commit-class (its writes
    /// are).
    pub(crate) fn barrier_class(&self) -> IoClass {
        if self.class.is_background() {
            IoClass::Background
        } else {
            IoClass::Commit
        }
    }

    /// Admits a background operation's first piece (R5.4): `None` for a
    /// submitter that is not background.
    fn admit_first(&self, c: ErrorContext) -> Result<Option<crate::qos::Charge>, Error> {
        match &self.issuer {
            None => Ok(None),
            Some(i) => match i.admit(self.store.inner.platform.monotonic_ns()) {
                Some(charge) => Ok(Some(charge)),
                None => Err(not_written(NotWrittenCause::Paced, c)),
            },
        }
    }
}

/// What processing a pass's pieces needs of a submitter, borrowed field by
/// field (so the raw completions can be drained beside it).
struct Pieces<'a, P: Platform>
where
    P::Queue: Send,
{
    table: &'a mut Table,
    queue: &'a mut P::Queue,
    store: &'a Store<P>,
    issuer: Option<&'a Arc<crate::qos::Issuer>>,
    /// Pieces may be submitted (false: every next piece is parked).
    issue: bool,
}

impl<P: Platform> Pieces<'_, P>
where
    P::Queue: Send,
{
    /// Follows a completion through every next piece it leads to; a flush
    /// it wants is asked for at the end of the pass.
    fn follow(&mut self, mut finished: Finish) {
        while let Finish::Next {
            qtag,
            offset,
            buf,
            write,
        } = finished
        {
            finished = self.next_piece(qtag, offset, buf, write);
        }
        if let Finish::Flush(qtag, need) = finished {
            self.table.asks.push((qtag, need));
        }
    }

    /// Pacing's admission of one more piece: `Some(None)` for a submitter
    /// that is not background, `None` when refused.
    fn admit(&self) -> Option<Option<crate::qos::Charge>> {
        match self.issuer {
            None => Some(None),
            Some(i) => i.admit(self.store.inner.platform.monotonic_ns()).map(Some),
        }
    }

    /// The next piece of an operation across pieces: never a write piece to
    /// a poisoned store (fail-stop: the operation ends `Poisoned`); a
    /// background piece admitted by pacing, or parked, never dropped; behind
    /// any piece parked before it (oldest first: the pass's `unpark`
    /// submits them in order); parked, never submitted, once a drop's wait
    /// has failed.
    fn next_piece(&mut self, qtag: u64, offset: u64, buf: IoBuf, write: bool) -> Finish {
        if write {
            if let Some(first) = self.store.inner.domain.poisoned() {
                return self
                    .table
                    .refused_poisoned(self.store, qtag, Some(buf), first);
            }
        }
        let admitted = if self.issue && self.table.parked.is_empty() {
            self.admit()
        } else {
            None
        };
        let Some(charge) = admitted else {
            self.table.parked.push_back(Parked {
                qtag,
                offset,
                buf,
                write,
            });
            return Finish::Done;
        };
        self.submit_piece(qtag, offset, buf, write, charge)
    }

    /// Submits a piece, its charge held by its row.
    fn submit_piece(
        &mut self,
        qtag: u64,
        offset: u64,
        buf: IoBuf,
        write: bool,
        charge: Option<crate::qos::Charge>,
    ) -> Finish {
        let inner = &*self.store.inner;
        let file = &inner.file;
        let bytes = buf.len() as u64;
        self.table.set_charge(qtag, charge);
        let op = if write {
            IoOp::Write {
                file,
                offset,
                buf,
                dsync: inner.policy.dsync,
            }
        } else {
            IoOp::Read { file, offset, buf }
        };
        match self.queue.submit(op, qtag) {
            Ok(()) => {
                if write {
                    inner.observer.add(Counter::Writes, 1);
                    inner.observer.add(Counter::WriteBytes, bytes);
                }
                Finish::Done
            }
            Err(r) => {
                if r.raw.is_none() {
                    inner.observer.add(Counter::Refused, 1);
                }
                self.table.refused_next(self.store, qtag, r.buf, r.raw)
            }
        }
    }

    /// Resubmits parked pieces, oldest first, while pacing admits them. With
    /// `force`, the oldest is submitted (uncharged) even if refused when
    /// nothing at all is in flight: pacing admits a call with nothing in
    /// flight anyway, so this changes nothing in a correct run, and a drain
    /// or a wait with only parked pieces can never spin.
    fn unpark(&mut self, force: bool) {
        while let Some(p) = self.table.parked.pop_front() {
            let Parked {
                qtag,
                offset,
                buf,
                write,
            } = p;
            if write {
                if let Some(first) = self.store.inner.domain.poisoned() {
                    let _done = self
                        .table
                        .refused_poisoned(self.store, qtag, Some(buf), first);
                    continue;
                }
            }
            let charge = match self.admit() {
                Some(charge) => charge,
                None if force && self.queue.in_flight() == 0 => {
                    // Pacing admits an issuer with nothing in flight, and a
                    // piece's charge goes at its completion: so only a
                    // charge that outlived its piece is refused here.
                    // (After a panic in processing, a completion dropped
                    // unprocessed keeps its row's charge: the store is
                    // poisoned then, and no write goes out.)
                    debug_assert!(
                        self.issuer.is_none_or(|i| i.in_flight() == 0)
                            || self.store.inner.domain.poisoned().is_some(),
                        "a charge outlived its piece"
                    );
                    None
                }
                None => {
                    self.table.parked.push_front(Parked {
                        qtag,
                        offset,
                        buf,
                        write,
                    });
                    return;
                }
            };
            let finished = self.submit_piece(qtag, offset, buf, write, charge);
            self.follow(finished);
        }
    }

    /// Asks the flush thread for what the pass wants, and takes its answers.
    fn ask_and_take(&mut self, inbox: &Arc<Inbox<<P::Queue as Queue>::Waker>>, class: IoClass) {
        // Asked once every completion of the pass is finished, the largest
        // need first, so one flush can cover them all. Taken one at a time:
        // after a panic the rest stay to be asked for, in the same buffer.
        while let Some((qtag, need)) = self.table.asks.pop() {
            durable::ask(self.table, self.store, inbox, qtag, need, true, class);
        }
        // Flush outcomes too, under the same guard.
        while self.table.asked > 0 {
            let Some((qtag, outcome)) = inbox.pop() else {
                break;
            };
            self.table.asked -= 1;
            self.table.flushed(self.store, qtag, outcome);
        }
    }
}

/// Poisons the store if a panic unwinds through the processing of
/// completions: an operation taken from its row but not settled may be an
/// append whose reserved range is then never completed.
struct PoisonOnUnwind<'a, P: Platform>
where
    P::Queue: Send,
{
    store: &'a Store<P>,
    /// Already unwinding when created (a drop during another panic): not
    /// this processing's panic.
    was: bool,
}

impl<'a, P: Platform> PoisonOnUnwind<'a, P>
where
    P::Queue: Send,
{
    fn new(store: &'a Store<P>) -> Self {
        Self {
            store,
            was: std::thread::panicking(),
        }
    }
}

impl<P: Platform> Drop for PoisonOnUnwind<'_, P>
where
    P::Queue: Send,
{
    fn drop(&mut self) {
        if !self.was && std::thread::panicking() {
            self.store.inner.domain.poison(FirstCause {
                op: Some(Op::Write),
                raw: None,
            });
            self.store.wake_frontiers();
        }
    }
}

impl<P: Platform> Drop for Submitter<P>
where
    P::Queue: Send,
{
    /// Waits for every operation in flight, untimed (TYP-05), so nothing the
    /// device may still touch is released. If waiting fails, the queue's own
    /// drop keeps what the kernel owns, nothing more is submitted (no
    /// parked piece, no next piece), and the store is poisoned when a write
    /// was in flight or unfinished: its range may never be written, and
    /// nothing of its generation may be recycled while it could still land.
    /// The same if a panic interrupted the processing of a completion (an
    /// operation left unaccounted may have been an append).
    fn drop(&mut self) {
        let mut failed = None;
        // First every I/O (a durable write's completion may ask for a
        // flush), parked pieces submitted as pacing admits them (a parked
        // piece is a live write: never dropped while the waits succeed; with
        // nothing in flight the oldest is always admitted)...
        while self.core.table.live > 0
            && (self.core.queue.in_flight() > 0 || !self.core.table.parked.is_empty())
        {
            if self.core.queue.in_flight() == 0 {
                self.unpark_idle();
                continue;
            }
            match self
                .core
                .queue
                .wait(self.core.queue.in_flight(), &mut self.core.raw)
            {
                Ok(_) => self.process(),
                Err(raw) => {
                    failed = Some(raw);
                    break;
                }
            }
            self.core.table.ready.clear();
        }
        // ... with whatever the queue collected before a failed wait (and
        // then nothing more submitted: the store is poisoned below)...
        let issue = failed.is_none();
        let _added = self.core.queue.reap(&mut self.core.raw);
        self.process_with(issue);
        // ... then every flush asked for: the flush thread answers every
        // request it accepted.
        while self.core.table.asked > 0 {
            self.inbox.wait(false);
            self.process_with(issue);
        }
        if self.core.table.live > 0
            && (self.core.table.writes_live() || !self.core.table.consistent())
        {
            self.store.inner.domain.poison(FirstCause {
                op: Some(Op::Write),
                raw: failed,
            });
            self.store.wake_frontiers();
        }
        self.store.unregister_submitter(self.id);
    }
}
