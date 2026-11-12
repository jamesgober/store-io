//! Read streams (PLANNING R5.5): a region's bytes as owned chunks, in
//! order, from a queue of the stream's own.
//!
//! A [`ReadStream`] keeps several reads in flight on a private queue
//! (`Owner::Thread`, like a `Submitter`'s), so it holds no lane a commit
//! needs and none between [`Iterator::next`] calls: the caller may use the
//! store while it iterates. Each item is a [`Chunk`] that owns a pool
//! buffer, or an exact `Corruption { MediaError, range }` for bytes the
//! device cannot read, after which the stream goes on.
//!
//! **No pass across the I/O.** The stream holds no gate pass, so a caller's
//! recycle or release never waits on it. Its region's gate is checked
//! (`Gate::live`, a read-modify-write) before each read is submitted and
//! again in `next()` once the read's completion is seen and before anything
//! of it is delivered, counted or classified. A gate still live then means
//! no retire began before the read completed, or one began and was restored
//! before anything on disk changed, so no chunk holds bytes of a newer
//! generation and no error of a released range is reported as corruption.
//! A media error is bisected on the stream's own idle queue into one
//! scratch buffer, each good result copied to its place in the piece's
//! buffer; its items are queued only after the gate holds once all its
//! reads have completed.
//!
//! **Buffers.** A stream takes `depth + lend + 1` buffers when it is made,
//! under the pool's background cap (half of each class: a stream is a bulk
//! reader, so the other half stays for commits): up to `depth` reads in
//! flight, up to `lend` chunks held by its caller, and the bisection's
//! scratch. A dropped [`Chunk`] gives its buffer back to the stream, or to
//! the pool once the stream has ended, from any thread.

use std::collections::VecDeque;
use std::marker::PhantomData;
use std::ops::Range;
use std::sync::{Arc, PoisonError};

#[cfg(loom)]
use loom::sync::{Mutex, MutexGuard};
#[cfg(not(loom))]
use std::sync::{Mutex, MutexGuard};

use store_io_core::IoClass;
use store_io_core::errno::is_media_error;
use store_io_core::error::{CorruptionKind, Error, ErrorContext, NotWrittenCause, Op, OsError};
use store_io_core::observe::Counter;
use store_io_platform::{IoBuf, IoOp, Platform, RawResult};

use crate::exec::{Lane, LaneMark, Ran, owned_lane, run};
use crate::io::{ctx_range, not_written};
use crate::lifecycle::retired_error;
use crate::pace::Pace;
use crate::qos::Charge;
use crate::store::{LiveRegion, Store};
use crate::submitter::MAX_DEPTH;
use crate::test_hook::{self, Step};

/// Reads in flight by default, at most (fewer on a pool with fewer
/// buffers free under the cap).
const DEFAULT_DEPTH: usize = 8;

/// How a [`ReadStream`] reads. `depth`: reads in flight, 1 to
/// [`MAX_DEPTH`](crate::MAX_DEPTH); `lend`: chunks its caller may hold at
/// once, at least 1. With neither set, the stream takes up to 8 of each
/// (fewer, and shallower, when fewer buffers are free under the pool's
/// background cap); with either set, the other defaults to it and the
/// stream takes exactly `depth + lend + 1` buffers or is refused. `class`:
/// [`IoClass::Background`] (the default: `background_io` requests, paced
/// while the device's commits are active) or [`IoClass::Read`] (`max_io`
/// requests, unpaced: a foreground read its caller waits on).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct StreamOptions {
    depth: Option<usize>,
    lend: Option<usize>,
    class: IoClass,
}

impl Default for StreamOptions {
    fn default() -> Self {
        Self {
            depth: None,
            lend: None,
            class: IoClass::Background,
        }
    }
}

impl StreamOptions {
    /// Reads in flight at most.
    #[must_use]
    pub fn depth(mut self, depth: usize) -> Self {
        self.depth = Some(depth);
        self
    }

    /// Chunks the caller may hold at once.
    #[must_use]
    pub fn lend(mut self, lend: usize) -> Self {
        self.lend = Some(lend);
        self
    }

    /// The class of its reads: `Background` or `Read`.
    #[must_use]
    pub fn class(mut self, class: IoClass) -> Self {
        self.class = class;
        self
    }
}

/// A stream's buffers not in flight and not lent, and how many are lent.
/// Shared with its chunks, which give their buffers back from any thread:
/// kept while the stream lives, handed back to the pool once it is closed.
/// `closed` is read and a buffer pushed under one lock, so no buffer is
/// left on a list already drained.
pub(crate) struct FreeList<B> {
    closed: bool,
    bufs: Vec<B>,
    lent: usize,
}

impl<B> FreeList<B> {
    pub(crate) fn new(bufs: Vec<B>) -> Self {
        Self {
            closed: false,
            bufs,
            lent: 0,
        }
    }

    /// A lent buffer back: kept while open; returned, to be dropped outside
    /// the lock, once closed.
    pub(crate) fn give_back(&mut self, b: B) -> Option<B> {
        self.lent = self.lent.saturating_sub(1);
        if self.closed {
            Some(b)
        } else {
            self.bufs.push(b);
            None
        }
    }

    /// Closes the list: every buffer on it, to be dropped outside the lock.
    pub(crate) fn close(&mut self) -> Vec<B> {
        self.closed = true;
        std::mem::take(&mut self.bufs)
    }
}

fn lock<B>(m: &Mutex<FreeList<B>>) -> MutexGuard<'_, FreeList<B>> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Bytes of a region, read by a [`ReadStream`]: it owns a pool buffer,
/// given back when the chunk drops (to its stream while the stream lives,
/// else to the pool). `Send`: a worker thread may hold and drop it.
pub struct Chunk {
    buf: Option<IoBuf>,
    at: usize,
    len: usize,
    offset: u64,
    home: Arc<Mutex<FreeList<IoBuf>>>,
}

impl Chunk {
    /// The region offset of `bytes()[0]`.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// The bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        match &self.buf {
            Some(b) => &b.as_slice()[self.at..self.at + self.len],
            None => &[],
        }
    }
}

impl core::fmt::Debug for Chunk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Chunk")
            .field("offset", &self.offset)
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl Drop for Chunk {
    fn drop(&mut self) {
        if let Some(b) = self.buf.take() {
            // Dropped (to the pool, its background count released) outside
            // the lock once the stream has ended.
            let back = lock(&self.home).give_back(b);
            drop(back);
        }
    }
}

/// A read in flight, and its completion once it arrives.
struct Read {
    start: u64,
    len: usize,
    done: Option<(RawResult<usize>, Option<IoBuf>)>,
    /// A `Background` stream's charge for the read: released when its
    /// completion is collected (or the stream ends).
    charge: Option<Charge>,
}

/// What a bisected piece comes to, in order: runs of its buffer and the
/// ranges that could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    /// `len` bytes of the piece's buffer from `at`, region offset `start`;
    /// `last`: the piece's buffer itself is lent with it.
    Run {
        start: u64,
        at: usize,
        len: usize,
        last: bool,
    },
    /// `[start, end)` could not be read.
    Bad { start: u64, end: u64 },
}

/// A region's bytes in order, as owned [`Chunk`]s, with an exact
/// `Corruption { MediaError, range }` item for each range the device cannot
/// read (the stream goes on after it). Made by `read_stream` on an
/// [`AppendRegion`](crate::AppendRegion) or a
/// [`PageRegion`](crate::PageRegion).
///
/// Its queue is the thread's, so a `ReadStream` is not `Send`:
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<store_io_engine::ReadStream<store_io_sim::SimPlatform>>();
/// ```
///
/// Ended by a retire of its region (seen at any check: `StaleGeneration`
/// if recycled, `NotReady` if released; the caller restarts it), by an
/// error, or at its end. A `next()` while every one of its `lend` chunks is
/// still held returns `NotWritten(TooManyInFlight)` and the stream goes on:
/// drop or give back a chunk, then call it again. Dropping the stream, or
/// [`Self::cancel`], submits nothing more and waits for the reads in
/// flight. The stream holds a clone of its store, so the store's clean
/// close waits until it is dropped. A page region's chunks are no
/// snapshot: a write made while streaming may show in them, or be torn
/// across a chunk, as in a scan. A `Background` stream between `next()`
/// calls keeps the charges of reads completed but not yet collected, which
/// count against its device group's pacing budget. Where a queue is lost
/// with reads in flight (a failed wait), their buffers never return to the
/// pool, lowering their class's background cap for the process's life.
pub struct ReadStream<P: Platform>
where
    P::Queue: Send,
{
    // Declared first, so its queue drops (after `Drop::drop` ended the
    // stream) before the store clone, as a `Submitter`'s does.
    /// `None`: an empty stream (no queue, no buffers).
    lane: Option<Lane<P::Queue>>,
    store: Store<P>,
    r: Arc<LiveRegion>,
    /// Its busy mark's id among the store's submitters.
    mark: Option<u64>,
    ring: Vec<Option<Read>>,
    submitted: u64,
    delivered: u64,
    in_flight: usize,
    /// A bisected piece's items not yet delivered, and its buffer.
    pending: VecDeque<Item>,
    piece: Option<IoBuf>,
    scratch: Option<IoBuf>,
    home: Arc<Mutex<FreeList<IoBuf>>>,
    lend: usize,
    /// The bytes asked for, and the block-aligned span read for them.
    want: Range<u64>,
    next: u64,
    end: u64,
    req: usize,
    pace: Option<Pace>,
    ended: bool,
    /// `finish` ran (once: a failed wait is never waited on again).
    finished: bool,
    wait_failed: Option<OsError>,
    /// A bisection's reads, counted once its check holds.
    bisected: (u64, u64),
    _thread: PhantomData<*const ()>,
}

impl<P: Platform> core::fmt::Debug for ReadStream<P>
where
    P::Queue: Send,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReadStream")
            .field("region", &self.r.id)
            .field("want", &self.want)
            .field("next", &self.next)
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}

/// An item that ends nothing: a chunk, or an error item the stream goes on
/// after.
type Next = Option<Result<Chunk, Error>>;

impl<P: Platform> Store<P>
where
    P::Queue: Send,
{
    /// A stream over `want` of `r` (checked by the caller: in bounds, an
    /// append region's clamped to its durable prefix, the gate live).
    pub(crate) fn read_stream_of(
        &self,
        r: &Arc<LiveRegion>,
        want: Range<u64>,
        opts: StreamOptions,
    ) -> Result<ReadStream<P>, Error> {
        let none = ErrorContext::default();
        if !matches!(opts.class, IoClass::Background | IoClass::Read) {
            return Err(not_written(NotWrittenCause::InvalidOptions, none));
        }
        let explicit = match (opts.depth, opts.lend) {
            (None, None) => None,
            (d, l) => {
                let depth = d.or(l).unwrap_or(1);
                let lend = l.or(d).unwrap_or(1);
                if depth == 0 || depth > MAX_DEPTH as usize || lend == 0 {
                    return Err(not_written(NotWrittenCause::InvalidOptions, none));
                }
                Some((depth, lend))
            }
        };
        let home = Arc::new(Mutex::new(FreeList::new(Vec::new())));
        let block = self.block();
        let mut s = ReadStream {
            lane: None,
            store: self.clone(),
            r: Arc::clone(r),
            mark: None,
            ring: Vec::new(),
            submitted: 0,
            delivered: 0,
            in_flight: 0,
            pending: VecDeque::new(),
            piece: None,
            scratch: None,
            home,
            lend: 0,
            next: want.start / block * block,
            end: want.end.div_ceil(block) * block,
            want,
            req: 0,
            pace: None,
            ended: false,
            finished: false,
            wait_failed: None,
            bisected: (0, 0),
            _thread: PhantomData,
        };
        // A `Background` stream is a call of its own (R5.4).
        let pace = match opts.class {
            IoClass::Read => None,
            _ => self.inner.domain.new_pace(),
        };
        let req = match (opts.class, &pace) {
            (IoClass::Read, _) => self.chunk(),
            (_, Some(p)) => p.piece().min(self.chunk()),
            (_, None) => (self.inner.qos.background_io() as usize).min(self.chunk()),
        };
        // From the class that holds a request, under its background cap.
        let pool = &self.inner.pool;
        let class = pool.class_len(req).unwrap_or(req);
        let cap = pool
            .background_holdings()
            .iter()
            .find(|(size, _, _)| *size == class)
            .map_or(0, |&(_, _, cap)| cap as usize);
        let c = ctx_range(r, s.want.start, s.want.end);
        let exhausted = || not_written(NotWrittenCause::PoolExhausted, c);
        // Explicit options over the cap are refused, an empty range's too.
        let explicit = match explicit {
            Some((depth, lend)) => match depth.checked_add(lend).and_then(|n| n.checked_add(1)) {
                Some(n) if n <= cap => Some((depth, lend, n)),
                _ => return Err(not_written(NotWrittenCause::InvalidOptions, none)),
            },
            None => None,
        };
        if s.want.is_empty() {
            // No queue, no buffers.
            s.ended = true;
            s.finished = true;
            return Ok(s);
        }
        let (depth, lend, mut bufs) = match explicit {
            Some((depth, lend, n)) => {
                let bufs: Vec<IoBuf> = (0..n)
                    .map_while(|_| pool.take_capped(req, class).ok())
                    .collect();
                if bufs.len() < n {
                    return Err(exhausted());
                }
                (depth, lend, bufs)
            }
            None => {
                let most = DEFAULT_DEPTH.min(cap.saturating_sub(1) / 2) * 2 + 1;
                let bufs: Vec<IoBuf> = (0..most)
                    .map_while(|_| pool.take_capped(req, class).ok())
                    .collect();
                if bufs.len() < 3 {
                    return Err(exhausted());
                }
                let depth = (bufs.len() - 1) / 2;
                (depth, bufs.len() - 1 - depth, bufs)
            }
        };
        let queue = self.thread_queue(depth as u32)?;
        let mark = Arc::new(LaneMark::new());
        s.lane = Some(owned_lane(queue, &mark, depth));
        s.mark = Some(self.register_submitter(&mark));
        s.scratch = bufs.pop();
        lock(&s.home).bufs = bufs;
        s.ring = (0..depth).map(|_| None).collect();
        s.lend = lend;
        s.req = req;
        s.pace = pace;
        Ok(s)
    }
}

impl<P: Platform> ReadStream<P>
where
    P::Queue: Send,
{
    /// Stops the stream: nothing more is submitted, the reads in flight are
    /// waited for, and every buffer not lent goes back to the pool at once
    /// (chunks still held go back as they drop).
    ///
    /// # Errors
    ///
    /// `Io` if waiting for the reads in flight failed: their buffers stay
    /// with the queue, dropped with it.
    pub fn cancel(mut self) -> Result<(), Error> {
        self.finish();
        match self.wait_failed.take() {
            Some(raw) => Err(self.read_error(raw)),
            None => Ok(()),
        }
    }

    fn ctx(&self) -> ErrorContext {
        ctx_range(&self.r, self.want.start, self.want.end)
    }

    fn read_error(&self, raw: OsError) -> Error {
        Error::Io {
            op: Op::Read,
            raw,
            ctx: self.ctx(),
        }
    }

    /// The region's gate, checked without a pass (`Gate::live`).
    fn live(&self) -> Result<(), Error> {
        self.r
            .gate
            .live()
            .map_err(|why| retired_error(why, &self.r))
    }

    /// A bisected piece's run waits to be copied out of its buffer: no read
    /// is submitted meanwhile, so a free buffer stays for it.
    fn copying(&self) -> bool {
        self.pending
            .iter()
            .any(|i| matches!(i, Item::Run { last: false, .. }))
    }

    fn depth(&self) -> usize {
        self.ring.len()
    }

    /// Submits reads while the ring has room, a buffer is free and data is
    /// left, each read cut where its extent ends.
    fn refill(&mut self) -> Result<(), Error> {
        let depth = self.depth() as u64;
        while !self.copying() && self.submitted - self.delivered < depth && self.next < self.end {
            self.live()?;
            test_hook::step(Step::StreamSubmit);
            let Some(mut buf) = lock(&self.home).bufs.pop() else {
                break;
            };
            let next = self.next;
            // Never across a run's end (one device operation each).
            let len = (self.end - next)
                .min(self.req as u64)
                .min(self.r.map.run_left(next)) as usize;
            // A background read is admitted before it is submitted; refused
            // with reads of its own in flight, the stream collects them.
            let charge = match &self.pace {
                None => None,
                Some(p) => match p.try_charge() {
                    Some(ch) => Some(ch),
                    None if self.in_flight > 0 => {
                        lock(&self.home).bufs.push(buf);
                        break;
                    }
                    None => match p.wait_charge() {
                        Some(ch) => Some(ch),
                        None => {
                            lock(&self.home).bufs.push(buf);
                            return Err(not_written(NotWrittenCause::Interrupted, self.ctx()));
                        }
                    },
                },
            };
            // `len ≤ req ≤ capacity`.
            let _fits = buf.set_len(len);
            let op = IoOp::Read {
                file: &self.store.inner.file,
                offset: self.r.map.locate(next, len as u64),
                buf,
            };
            let Some(lane) = self.lane.as_mut() else {
                return Err(not_written(NotWrittenCause::Interrupted, self.ctx()));
            };
            match lane.submit(op, self.submitted) {
                Ok(()) => {
                    let slot = (self.submitted % depth) as usize;
                    self.ring[slot] = Some(Read {
                        start: next,
                        len,
                        done: None,
                        charge,
                    });
                    self.submitted += 1;
                    self.next += len as u64;
                    self.in_flight += 1;
                }
                Err(rejected) => {
                    if let Some(b) = rejected.buf {
                        lock(&self.home).bufs.push(b);
                    }
                    match rejected.raw {
                        None if self.in_flight > 0 => break,
                        None => {
                            return Err(not_written(NotWrittenCause::QueueFull, self.ctx()));
                        }
                        Some(raw) => return Err(self.read_error(raw)),
                    }
                }
            }
        }
        Ok(())
    }

    /// Files every completion the lane holds under its read (its charge
    /// released).
    fn file(&mut self) {
        let depth = self.depth() as u64;
        let Some(lane) = self.lane.as_mut() else {
            return;
        };
        for comp in lane.done.drain() {
            self.in_flight = self.in_flight.saturating_sub(1);
            if let Some(p) = self.ring[(comp.tag % depth) as usize].as_mut() {
                p.done = Some((comp.result, comp.buf));
                p.charge = None;
            }
        }
    }

    /// Collects what has completed, without waiting.
    fn reap(&mut self) {
        if let Some(lane) = self.lane.as_mut() {
            let _arrived = lane.reap();
        }
        self.file();
    }

    /// Waits for at least one read in flight.
    fn wait_one(&mut self) -> Result<(), OsError> {
        let waited = match self.lane.as_mut() {
            Some(lane) => lane.wait(1).map(|_| ()),
            None => Ok(()),
        };
        self.file();
        waited
    }

    /// Waits for every read in flight.
    fn collect_all(&mut self) -> Result<(), OsError> {
        while self.in_flight > 0 {
            self.wait_one()?;
        }
        Ok(())
    }

    /// A chunk of `len` bytes of `buf` from `at`, region offset `offset`:
    /// one more lent.
    fn lend_chunk(&mut self, buf: IoBuf, at: usize, len: usize, offset: u64) -> Chunk {
        lock(&self.home).lent += 1;
        Chunk {
            buf: Some(buf),
            at,
            len,
            offset,
            home: Arc::clone(&self.home),
        }
    }

    /// A range that could not be read, as its item.
    fn bad(&self, start: u64, end: u64) -> Error {
        Error::Corruption {
            kind: CorruptionKind::MediaError,
            ctx: ctx_range(&self.r, start, end),
        }
    }

    /// Queues `[start, end)` as unreadable, counted once as a media error
    /// (after its check).
    fn queue_bad(&mut self, start: u64, end: u64) {
        self.store.inner.observer.add(Counter::MediaErrors, 1);
        self.pending.push_back(Item::Bad { start, end });
    }

    /// `[start, end)` clamped to the bytes asked for (empty: `None`).
    fn clamp(&self, start: u64, end: u64) -> Option<(u64, u64)> {
        let (s, e) = (start.max(self.want.start), end.min(self.want.end));
        (s < e).then_some((s, e))
    }

    /// The next item: refill, collect, deliver. `Ok(None)`: the stream's
    /// end; `Err`: an error that ends it.
    fn advance(&mut self) -> Result<Next, Error> {
        loop {
            self.refill()?;
            self.reap();
            // Decided before delivering anything (the refill above still
            // keeps reads in flight): a refused call delivers, counts and
            // classifies nothing.
            if lock(&self.home).lent >= self.lend {
                return Ok(Some(Err(not_written(
                    NotWrittenCause::TooManyInFlight,
                    self.ctx(),
                ))));
            }
            if let Some(item) = self.pending.pop_front() {
                return Ok(Some(self.deliver_item(item)));
            }
            if self.delivered == self.submitted {
                if self.next >= self.end {
                    return Ok(None);
                }
                // Nothing in flight and nothing submitted: nothing ever
                // will be (a backstop: the stream fails, never spins).
                return Err(not_written(NotWrittenCause::Interrupted, self.ctx()));
            }
            let depth = self.depth() as u64;
            let slot = (self.delivered % depth) as usize;
            while self.ring[slot].as_ref().is_some_and(|p| p.done.is_none()) {
                if let Err(raw) = self.wait_one() {
                    return Err(self.read_error(raw));
                }
            }
            let Some(Read {
                start,
                len,
                done: Some((result, buf)),
                ..
            }) = self.ring[slot].take()
            else {
                return Err(not_written(NotWrittenCause::Interrupted, self.ctx()));
            };
            self.delivered += 1;
            test_hook::step(Step::StreamCompleted);
            // Before anything of the read is delivered, counted or
            // classified (an error's too).
            if let Err(e) = self.live() {
                if let Some(b) = buf {
                    lock(&self.home).bufs.push(b);
                }
                return Err(e);
            }
            match (result, buf) {
                (Ok(n), Some(b)) => {
                    let n = n.min(len);
                    let obs = &self.store.inner.observer;
                    obs.add(Counter::Reads, 1);
                    obs.add(Counter::ReadBytes, n as u64);
                    // A short read: the rest could not be read.
                    if let Some((s, e)) = self.clamp(start + n as u64, start + len as u64) {
                        self.queue_bad(s, e);
                    }
                    match self.clamp(start, start + n as u64) {
                        Some((s, e)) => {
                            let at = (s - start) as usize;
                            let chunk = self.lend_chunk(b, at, (e - s) as usize, s);
                            return Ok(Some(Ok(chunk)));
                        }
                        None => lock(&self.home).bufs.push(b),
                    }
                }
                (Err(raw), Some(b)) if is_media_error(raw) => {
                    self.bisect(start, len as u64, b)?;
                }
                (Err(raw), b) => {
                    if let Some(b) = b {
                        lock(&self.home).bufs.push(b);
                    }
                    return Err(self.read_error(raw));
                }
                // No buffer came back (a backend breaking platform contract
                // 1): the stream stops, never reporting the range as bad.
                (Ok(_), None) => {
                    return Err(not_written(NotWrittenCause::Interrupted, self.ctx()));
                }
            }
        }
    }

    /// A bisected piece's next item.
    fn deliver_item(&mut self, item: Item) -> Result<Chunk, Error> {
        match item {
            Item::Bad { start, end } => Err(self.bad(start, end)),
            Item::Run {
                start,
                at,
                len,
                last: true,
            } => match self.piece.take() {
                Some(b) => Ok(self.lend_chunk(b, at, len, start)),
                None => Err(not_written(NotWrittenCause::Interrupted, self.ctx())),
            },
            Item::Run { start, at, len, .. } => {
                let free = lock(&self.home).bufs.pop();
                let (Some(mut buf), Some(piece)) = (free, self.piece.as_ref()) else {
                    // No free buffer while fewer than `lend` are lent cannot
                    // happen (no read is submitted while a run waits); kept
                    // for its turn.
                    self.pending.push_front(item);
                    return Err(not_written(NotWrittenCause::TooManyInFlight, self.ctx()));
                };
                let _fits = buf.set_len(len);
                buf.as_mut_capacity()[..len].copy_from_slice(&piece.as_slice()[at..at + len]);
                Ok(self.lend_chunk(buf, 0, len, start))
            }
        }
    }

    /// Bisects the piece `[start, start + len)`, whose read failed with a
    /// media error, on the stream's idle queue; its items are queued only
    /// after the gate holds once all its reads have completed.
    fn bisect(&mut self, start: u64, len: u64, mut piece: IoBuf) -> Result<(), Error> {
        // An idle queue: every read in flight collected first (delivered
        // later, in order, after this piece).
        if let Err(raw) = self.collect_all() {
            lock(&self.home).bufs.push(piece);
            return Err(self.read_error(raw));
        }
        let mut scratch = self.scratch.take();
        let mut found: Vec<(u64, u64, bool)> = Vec::new();
        let halves = self.halves(start, start, len, &mut piece, &mut scratch, &mut found);
        self.scratch = scratch;
        if let Err(e) = halves {
            self.bisected = (0, 0);
            lock(&self.home).bufs.push(piece);
            return Err(e);
        }
        test_hook::step(Step::StreamBisected);
        let (reads, bytes) = std::mem::take(&mut self.bisected);
        if let Err(e) = self.live() {
            lock(&self.home).bufs.push(piece);
            return Err(e);
        }
        let obs = &self.store.inner.observer;
        obs.add(Counter::Reads, reads);
        obs.add(Counter::ReadBytes, bytes);
        // Merged (good with good, bad with bad), clamped to the bytes asked
        // for.
        let mut merged: Vec<(u64, u64, bool)> = Vec::new();
        for (s, e, good) in found {
            match merged.last_mut() {
                Some((_, le, lg)) if *lg == good && *le == s => *le = e,
                _ => merged.push((s, e, good)),
            }
        }
        let items: Vec<Item> = merged
            .into_iter()
            .filter_map(|(s, e, good)| {
                let (s, e) = self.clamp(s, e)?;
                Some(if good {
                    Item::Run {
                        start: s,
                        at: (s - start) as usize,
                        len: (e - s) as usize,
                        last: false,
                    }
                } else {
                    Item::Bad { start: s, end: e }
                })
            })
            .collect();
        let last_run = items.iter().rposition(|i| matches!(i, Item::Run { .. }));
        for (i, item) in items.into_iter().enumerate() {
            match item {
                Item::Run { start, at, len, .. } => self.pending.push_back(Item::Run {
                    start,
                    at,
                    len,
                    last: Some(i) == last_run,
                }),
                Item::Bad { start, end } => self.queue_bad(start, end),
            }
        }
        match last_run {
            // Lent with its last run.
            Some(_) => self.piece = Some(piece),
            // No good run: its buffer back.
            None => lock(&self.home).bufs.push(piece),
        }
        Ok(())
    }

    /// Reads `[at, at + len)` again in halves into `scratch`, each good
    /// result copied to its place in `piece` (which starts at `base`),
    /// down to one sector: `found` gets `(start, end, good)` in order.
    /// `scratch` is left `None` if the queue kept it (lost).
    fn halves(
        &mut self,
        base: u64,
        at: u64,
        len: u64,
        piece: &mut IoBuf,
        scratch: &mut Option<IoBuf>,
        found: &mut Vec<(u64, u64, bool)>,
    ) -> Result<(), Error> {
        let sector = self.store.sector();
        let block = self.store.block();
        let Some(mut buf) = scratch.take() else {
            return Err(not_written(NotWrittenCause::Interrupted, self.ctx()));
        };
        // `len ≤ req ≤ capacity`.
        let _fits = buf.set_len(len as usize);
        let admitted = match &self.pace {
            Some(p) => Some(p.admit()?),
            None => None,
        };
        let op = IoOp::Read {
            file: &self.store.inner.file,
            offset: self.r.map.locate(at, len),
            buf,
        };
        let Some(lane) = self.lane.as_mut() else {
            return Err(not_written(NotWrittenCause::Interrupted, self.ctx()));
        };
        let done = run(lane, op);
        // Completed (or abandoned): its charge goes with it.
        drop(admitted);
        let (result, back) = match done {
            Ok(Ran::Done { buf, result }) => (result, buf),
            Ok(Ran::Abandoned(raw)) => return Err(self.read_error(raw)),
            Err((buf, Some(raw))) => {
                *scratch = buf;
                return Err(self.read_error(raw));
            }
            Err((buf, None)) => {
                *scratch = buf;
                return Err(not_written(NotWrittenCause::QueueFull, self.ctx()));
            }
        };
        let Some(b) = back else {
            return Err(match result {
                Err(raw) => self.read_error(raw),
                Ok(_) => not_written(NotWrittenCause::Interrupted, self.ctx()),
            });
        };
        match result {
            Ok(n) => {
                let n = (n as u64).min(len);
                let place = (at - base) as usize;
                piece.as_mut_capacity()[place..place + n as usize]
                    .copy_from_slice(&b.as_slice()[..n as usize]);
                *scratch = Some(b);
                // Counted once the bisection's check holds.
                self.bisected.0 += 1;
                self.bisected.1 += n;
                if n > 0 {
                    found.push((at, at + n, true));
                }
                if n < len {
                    found.push((at + n, at + len, false));
                }
                Ok(())
            }
            Err(raw) if is_media_error(raw) => {
                *scratch = Some(b);
                if len <= sector {
                    found.push((at, at + len, false));
                    return Ok(());
                }
                // Whole blocks while the piece spans several; sectors only
                // inside one block (as the scan's bisection).
                let unit = if len > block { block } else { sector };
                let half = (len / unit).div_ceil(2) * unit;
                self.halves(base, at, half, piece, scratch, found)?;
                self.halves(base, at + half, len - half, piece, scratch, found)
            }
            // Below the store block the device refused the smaller read
            // (alignment): it lies in a block that failed on the media.
            Err(_) if len < block => {
                *scratch = Some(b);
                found.push((at, at + len, false));
                Ok(())
            }
            Err(raw) => {
                *scratch = Some(b);
                Err(self.read_error(raw))
            }
        }
    }

    /// Ends the stream: the reads in flight waited for (a failed wait kept
    /// for `cancel`; their buffers stay with the queue), every buffer not
    /// lent back to the pool, the list closed so chunks still held go to
    /// the pool as they drop.
    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.ended = true;
        if self.wait_failed.is_none() {
            if let Err(raw) = self.collect_all() {
                self.wait_failed = Some(raw);
            }
        }
        // Their charges and buffers go with them.
        self.ring.iter_mut().for_each(|slot| *slot = None);
        self.pending.clear();
        self.piece = None;
        self.scratch = None;
        let bufs = lock(&self.home).close();
        drop(bufs);
    }
}

impl<P: Platform> Iterator for ReadStream<P>
where
    P::Queue: Send,
{
    type Item = Result<Chunk, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.ended {
            return None;
        }
        match self.advance() {
            Ok(Some(item)) => Some(item),
            Ok(None) => {
                self.finish();
                None
            }
            Err(e) => {
                self.finish();
                Some(Err(e))
            }
        }
    }
}

impl<P: Platform> Drop for ReadStream<P>
where
    P::Queue: Send,
{
    fn drop(&mut self) {
        self.finish();
        if let Some(id) = self.mark.take() {
            self.store.unregister_submitter(id);
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn test_a_chunk_is_send() {
        fn send<T: Send>() {}
        send::<Chunk>();
    }

    #[test]
    fn test_a_closed_list_hands_buffers_back() {
        let mut l = FreeList::new(vec![1u32, 2]);
        l.lent = 1;
        assert_eq!(l.give_back(3), None);
        assert_eq!(l.close(), vec![1, 2, 3]);
        l.lent = 1;
        assert_eq!(l.give_back(4), Some(4));
        assert_eq!(l.lent, 0);
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;
    use loom::thread;

    /// A chunk's buffer given back while the stream closes its list: every
    /// buffer ends either on the drained list or handed back, never on a
    /// list already drained.
    #[test]
    fn loom_a_give_back_against_a_close_strands_nothing() {
        loom::model(|| {
            let home = Arc::new(Mutex::new(FreeList::new(vec![1u32])));
            lock(&home).lent = 1;
            let h = Arc::clone(&home);
            let t = thread::spawn(move || lock(&h).give_back(2));
            let drained = lock(&home).close();
            let back = t.join().unwrap_or(None);
            // Nothing left on the closed list (it would be stranded), and
            // both buffers accounted for.
            assert!(
                lock(&home).bufs.is_empty(),
                "a buffer pushed after the close"
            );
            assert_eq!(drained.len() + usize::from(back.is_some()), 2);
        });
    }
}
