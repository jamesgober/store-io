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

use std::cell::RefCell;
use std::collections::VecDeque;
use std::marker::PhantomData;
use std::ops::Range;
use std::rc::{Rc, Weak};
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
use crate::pace::{Held, Pace, Registered};
use crate::qos::Collect;
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
    range: usize,
    home: Arc<Mutex<FreeList<IoBuf>>>,
}

impl Chunk {
    /// The region offset of `bytes()[0]`.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Which of the stream's ranges it is of: its index in
    /// [`Store::prefetch`](crate::Store::prefetch)'s `ranges` (0 for a
    /// region's own stream), so overlapping ranges read twice are told
    /// apart.
    #[must_use]
    pub fn range(&self) -> usize {
        self.range
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
            .field("range", &self.range)
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
    /// Its part (an index into the stream's parts).
    part: usize,
    start: u64,
    len: usize,
    done: Option<(RawResult<usize>, Option<IoBuf>)>,
    /// A `Background` stream's charge for the read: released when its
    /// completion is collected (or the stream ends).
    charge: Option<Held>,
}

/// What a bisected piece comes to, in order: runs of its buffer and the
/// ranges that could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    /// `len` bytes of the piece's buffer from `at`, region offset `start`;
    /// `last`: the piece's buffer itself is lent with it.
    Run {
        part: usize,
        start: u64,
        at: usize,
        len: usize,
        last: bool,
    },
    /// `[start, end)` could not be read.
    Bad { part: usize, start: u64, end: u64 },
}

/// One range of a stream: its region (owned, no pass), the bytes asked for,
/// and its index in the caller's ranges.
struct Part {
    r: Arc<LiveRegion>,
    want: Range<u64>,
    index: usize,
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
    /// Its whole state, shared (weakly) with its registration as a
    /// collector of its call's record: a `Background` stream's reads hold
    /// the call's charges between `next()` calls, which another part of
    /// the call on this thread collects instead of waiting on them.
    core: Rc<RefCell<Core<P>>>,
    _part: Option<Registered>,
}

/// A read stream's state, in one cell: a collector borrows it whole, so a
/// collection leaves its lane, its ring and its count of reads in flight
/// consistent.
struct Core<P: Platform>
where
    P::Queue: Send,
{
    // Declared first, so its queue drops (after `Drop::drop` ended the
    // stream) before the store clone, as a `Submitter`'s does.
    /// `None`: an empty stream (no queue, no buffers).
    lane: Option<Lane<P::Queue>>,
    store: Store<P>,
    /// Its ranges, in order (none empty), and the one being read.
    parts: Vec<Part>,
    cur: usize,
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
    /// The current part's next read, and the block-aligned end of its span.
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
    /// A collection for another part failed (sticky): its error and the
    /// stream offset of the oldest read lost.
    collect_failed: Option<(OsError, u64)>,
    _thread: PhantomData<*const ()>,
}

impl<P: Platform> core::fmt::Debug for ReadStream<P>
where
    P::Queue: Send,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut d = f.debug_struct("ReadStream");
        if let Ok(c) = self.core.try_borrow() {
            let _d = d
                .field("parts", &c.parts.len())
                .field("cur", &c.cur)
                .field("next", &c.next)
                .field("ended", &c.ended);
        }
        d.finish_non_exhaustive()
    }
}

/// An item that ends nothing: a chunk, or an error item the stream goes on
/// after.
type Next = Option<Result<Chunk, Error>>;

/// One range for [`Store::prefetch`]: a region's bytes, checked when made
/// by [`AppendRegion::stream_range`](crate::AppendRegion::stream_range) or
/// [`PageRegion::stream_range`](crate::PageRegion::stream_range) (in
/// bounds, an append region's clamped to its durable prefix then).
pub struct StreamRange<'a, P: Platform> {
    store: &'a Store<P>,
    r: Arc<LiveRegion>,
    want: Range<u64>,
}

impl<'a, P: Platform> StreamRange<'a, P> {
    pub(crate) fn new(store: &'a Store<P>, r: &Arc<LiveRegion>, want: Range<u64>) -> Self {
        Self {
            store,
            r: Arc::clone(r),
            want,
        }
    }
}

impl<P: Platform> core::fmt::Debug for StreamRange<'_, P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StreamRange")
            .field("region", &self.r.id)
            .field("want", &self.want)
            .finish_non_exhaustive()
    }
}

impl<P: Platform> Store<P>
where
    P::Queue: Send,
{
    /// The bytes of several ranges, of this store's regions, as one
    /// [`ReadStream`] on one queue: each range in the order given, each in
    /// offset order ([`Chunk::range`] says which; overlapping ranges are
    /// read twice; no range, an empty stream). An append region's range was
    /// clamped to its durable prefix when it was made. Each region's gate is
    /// checked again here and as the stream reads it: a retire of a region
    /// ends the stream before any more of that region's reads are
    /// delivered. An error tied to no read (`TooManyInFlight`, a failed
    /// wait, `Interrupted`) names the range being read ahead.
    ///
    /// # Errors
    ///
    /// `NotWritten`: `ForeignPosition` (a range of another store, or of
    /// another open of this one), `StaleGeneration` or `NotReady` (a region
    /// recycled or released), `PoolExhausted`, `InvalidOptions`; `Io` if
    /// its queue could not be made.
    pub fn prefetch(
        &self,
        ranges: &[StreamRange<'_, P>],
        opts: StreamOptions,
    ) -> Result<ReadStream<P>, Error> {
        for sr in ranges {
            if !Arc::ptr_eq(&sr.store.inner, &self.inner) {
                return Err(not_written(
                    NotWrittenCause::ForeignPosition,
                    ctx_range(&sr.r, sr.want.start, sr.want.end),
                ));
            }
            sr.r.gate.live().map_err(|why| retired_error(why, &sr.r))?;
        }
        let parts = ranges
            .iter()
            .enumerate()
            .map(|(i, sr)| (Arc::clone(&sr.r), sr.want.clone(), i))
            .collect();
        self.read_stream_of(parts, opts)
    }
}

impl<P: Platform> Store<P>
where
    P::Queue: Send,
{
    /// A stream over `parts`, each a region, the bytes asked of it and its
    /// index in the caller's ranges (checked by the caller: in bounds, an
    /// append region's clamped to its durable prefix, the gate live).
    pub(crate) fn read_stream_of(
        &self,
        parts: Vec<(Arc<LiveRegion>, Range<u64>, usize)>,
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
        // The error context of a refusal at the call: the first range's.
        let c = parts
            .first()
            .map_or(none, |(r, w, _)| ctx_range(r, w.start, w.end));
        let parts: Vec<Part> = parts
            .into_iter()
            .filter(|(_, want, _)| !want.is_empty())
            .map(|(r, want, index)| Part { r, want, index })
            .collect();
        let (next, end) = parts.first().map_or((0, 0), |p| {
            (
                p.want.start / block * block,
                p.want.end.div_ceil(block) * block,
            )
        });
        let mut s = Core {
            lane: None,
            store: self.clone(),
            parts,
            cur: 0,
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
            next,
            end,
            req: 0,
            pace: None,
            ended: false,
            finished: false,
            wait_failed: None,
            bisected: (0, 0),
            collect_failed: None,
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
        let exhausted = || not_written(NotWrittenCause::PoolExhausted, c);
        // Explicit options over the cap are refused, an empty range's too.
        let explicit = match explicit {
            Some((depth, lend)) => match depth.checked_add(lend).and_then(|n| n.checked_add(1)) {
                Some(n) if n <= cap => Some((depth, lend, n)),
                _ => return Err(not_written(NotWrittenCause::InvalidOptions, none)),
            },
            None => None,
        };
        if s.parts.is_empty() {
            // No queue, no buffers.
            s.ended = true;
            s.finished = true;
            return Ok(ReadStream::new(s));
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
        Ok(ReadStream::new(s))
    }
}

impl<P: Platform> ReadStream<P>
where
    P::Queue: Send,
{
    /// The handle over `core`, registered as a collector of its call's
    /// record when it is paced.
    fn new(core: Core<P>) -> Self {
        let pace = core.pace.clone();
        let core = Rc::new(RefCell::new(core));
        let _part = pace.map(|p| {
            let weak: Weak<RefCell<Core<P>>> = Rc::downgrade(&core);
            p.register(weak)
        });
        Self { core, _part }
    }

    /// Stops the stream: nothing more is submitted, the reads in flight are
    /// waited for, and every buffer not lent goes back to the pool at once
    /// (chunks still held go back as they drop).
    ///
    /// # Errors
    ///
    /// `Io` if waiting for the reads in flight failed: their buffers stay
    /// with the queue, dropped with it.
    pub fn cancel(self) -> Result<(), Error> {
        let mut c = self.core.borrow_mut();
        c.finish();
        match c.wait_failed.take() {
            Some(raw) => Err(c.read_error(raw)),
            None => Ok(()),
        }
    }

    /// The stream's pacing (a `Background` stream's call).
    #[cfg(all(test, not(loom)))]
    pub(crate) fn pace(&self) -> Option<Pace> {
        self.core.borrow().pace.clone()
    }
}

impl<P: Platform> Collect for RefCell<Core<P>>
where
    P::Queue: Send,
{
    fn collect_one(&self) -> Result<bool, OsError> {
        // Borrowed: this stream is the part asking, which collected its
        // own reads first.
        match self.try_borrow_mut() {
            Ok(mut c) => c.collect_for_another(false),
            Err(_) => Ok(false),
        }
    }

    fn collect_all(&self) -> Result<(), OsError> {
        match self.try_borrow_mut() {
            Ok(mut c) => c.collect_for_another(true).map(|_| ()),
            Err(_) => Ok(()),
        }
    }

    fn failed(&self) -> Option<(OsError, u64)> {
        self.try_borrow().ok()?.collect_failed
    }
}

impl<P: Platform> Core<P>
where
    P::Queue: Send,
{
    /// Collects reads in flight for another part of the call (one at
    /// least, or all): whether there were any. A failed wait is recorded
    /// (sticky: the next `next()` ends the stream with it) and every read's
    /// charge dropped (the reads stay with the queue, which is never waited
    /// on again).
    fn collect_for_another(&mut self, all: bool) -> Result<bool, OsError> {
        // Ended: it holds nothing (a failure already reported is its own).
        if self.finished {
            return Ok(false);
        }
        if let Some((raw, _)) = self.collect_failed {
            return Err(raw);
        }
        if self.in_flight == 0 {
            return Ok(false);
        }
        let waited = if all {
            self.collect_all()
        } else {
            self.wait_one()
        };
        match waited {
            Ok(()) => Ok(true),
            Err(raw) => {
                let lost = self
                    .ring
                    .iter()
                    .flatten()
                    .filter(|r| r.done.is_none())
                    .map(|r| r.start)
                    .min()
                    .unwrap_or(self.next);
                self.collect_failed = Some((raw, lost));
                self.wait_failed = Some(raw);
                for r in self.ring.iter_mut().flatten() {
                    r.charge = None;
                }
                Err(raw)
            }
        }
    }

    /// Part `i`'s range as an error's context.
    fn ctx_of(&self, i: usize) -> ErrorContext {
        match self.parts.get(i) {
            Some(p) => ctx_range(&p.r, p.want.start, p.want.end),
            None => ErrorContext::default(),
        }
    }

    /// The current part's.
    fn ctx(&self) -> ErrorContext {
        self.ctx_of(self.cur)
    }

    fn read_error_of(&self, i: usize, raw: OsError) -> Error {
        Error::Io {
            op: Op::Read,
            raw,
            ctx: self.ctx_of(i),
        }
    }

    fn read_error(&self, raw: OsError) -> Error {
        self.read_error_of(self.cur, raw)
    }

    /// Part `i`'s region's gate, checked without a pass (`Gate::live`).
    fn live_of(&self, i: usize) -> Result<(), Error> {
        match self.parts.get(i) {
            Some(p) => p.r.gate.live().map_err(|why| retired_error(why, &p.r)),
            None => Err(not_written(NotWrittenCause::Interrupted, self.ctx())),
        }
    }

    /// Whether data is left to read: moves to the next part once the
    /// current one is all submitted.
    fn more(&mut self) -> bool {
        while self.next >= self.end {
            if self.cur + 1 >= self.parts.len() {
                return false;
            }
            self.cur += 1;
            let block = self.store.block();
            let p = &self.parts[self.cur];
            self.next = p.want.start / block * block;
            self.end = p.want.end.div_ceil(block) * block;
        }
        true
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
        while !self.copying() && self.submitted - self.delivered < depth && self.more() {
            let part = self.cur;
            self.live_of(part)?;
            test_hook::step(Step::StreamSubmit);
            let Some(mut buf) = lock(&self.home).bufs.pop() else {
                break;
            };
            let next = self.next;
            // Never across a run's end (one device operation each).
            let len = (self.end - next)
                .min(self.req as u64)
                .min(self.parts[part].r.map.run_left(next)) as usize;
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
                offset: self.parts[part].r.map.locate(next, len as u64),
                buf,
            };
            let Some(lane) = self.lane.as_mut() else {
                return Err(not_written(NotWrittenCause::Interrupted, self.ctx()));
            };
            match lane.submit(op, self.submitted) {
                Ok(()) => {
                    let slot = (self.submitted % depth) as usize;
                    self.ring[slot] = Some(Read {
                        part,
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

    /// A chunk of part `part`, `len` bytes of `buf` from `at`, region
    /// offset `offset`: one more lent.
    fn lend_chunk(&mut self, part: usize, buf: IoBuf, at: usize, len: usize, offset: u64) -> Chunk {
        lock(&self.home).lent += 1;
        Chunk {
            buf: Some(buf),
            at,
            len,
            offset,
            range: self.parts.get(part).map_or(0, |p| p.index),
            home: Arc::clone(&self.home),
        }
    }

    /// A range of part `part` that could not be read, as its item.
    fn bad(&self, part: usize, start: u64, end: u64) -> Error {
        Error::Corruption {
            kind: CorruptionKind::MediaError,
            ctx: match self.parts.get(part) {
                Some(p) => ctx_range(&p.r, start, end),
                None => ErrorContext::default(),
            },
        }
    }

    /// Queues `[start, end)` of part `part` as unreadable, counted once as
    /// a media error (after its check).
    fn queue_bad(&mut self, part: usize, start: u64, end: u64) {
        self.store.inner.observer.add(Counter::MediaErrors, 1);
        self.pending.push_back(Item::Bad { part, start, end });
    }

    /// `[start, end)` clamped to part `part`'s bytes asked for (empty:
    /// `None`).
    fn clamp(&self, part: usize, start: u64, end: u64) -> Option<(u64, u64)> {
        let want = &self.parts.get(part)?.want;
        let (s, e) = (start.max(want.start), end.min(want.end));
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
                if !self.more() {
                    return Ok(None);
                }
                // Nothing in flight and nothing submitted: nothing ever
                // will be (a backstop: the stream fails, never spins).
                return Err(not_written(NotWrittenCause::Interrupted, self.ctx()));
            }
            let depth = self.depth() as u64;
            let slot = (self.delivered % depth) as usize;
            let waiting = self.ring[slot].as_ref().map_or(self.cur, |p| p.part);
            while self.ring[slot].as_ref().is_some_and(|p| p.done.is_none()) {
                if let Err(raw) = self.wait_one() {
                    return Err(self.read_error_of(waiting, raw));
                }
            }
            let Some(Read {
                part,
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
            // classified (an error's too): its own part's gate.
            if let Err(e) = self.live_of(part) {
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
                    if let Some((s, e)) = self.clamp(part, start + n as u64, start + len as u64) {
                        self.queue_bad(part, s, e);
                    }
                    match self.clamp(part, start, start + n as u64) {
                        Some((s, e)) => {
                            let at = (s - start) as usize;
                            let chunk = self.lend_chunk(part, b, at, (e - s) as usize, s);
                            return Ok(Some(Ok(chunk)));
                        }
                        None => lock(&self.home).bufs.push(b),
                    }
                }
                (Err(raw), Some(b)) if is_media_error(raw) => {
                    self.bisect(part, start, len as u64, b)?;
                }
                (Err(raw), b) => {
                    if let Some(b) = b {
                        lock(&self.home).bufs.push(b);
                    }
                    return Err(self.read_error_of(part, raw));
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
            Item::Bad { part, start, end } => Err(self.bad(part, start, end)),
            Item::Run {
                part,
                start,
                at,
                len,
                last: true,
            } => match self.piece.take() {
                Some(b) => Ok(self.lend_chunk(part, b, at, len, start)),
                None => Err(not_written(NotWrittenCause::Interrupted, self.ctx())),
            },
            Item::Run {
                part,
                start,
                at,
                len,
                ..
            } => {
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
                Ok(self.lend_chunk(part, buf, 0, len, start))
            }
        }
    }

    /// Bisects the piece `[start, start + len)`, whose read failed with a
    /// media error, on the stream's idle queue; its items are queued only
    /// after the gate holds once all its reads have completed.
    fn bisect(&mut self, part: usize, start: u64, len: u64, mut piece: IoBuf) -> Result<(), Error> {
        // An idle queue: every read in flight collected first (delivered
        // later, in order, after this piece).
        if let Err(raw) = self.collect_all() {
            lock(&self.home).bufs.push(piece);
            return Err(self.read_error_of(part, raw));
        }
        let mut scratch = self.scratch.take();
        let mut found: Vec<(u64, u64, bool)> = Vec::new();
        let halves = self.halves(
            part,
            start,
            start,
            len,
            &mut piece,
            &mut scratch,
            &mut found,
        );
        self.scratch = scratch;
        if let Err(e) = halves {
            self.bisected = (0, 0);
            lock(&self.home).bufs.push(piece);
            return Err(e);
        }
        test_hook::step(Step::StreamBisected);
        let (reads, bytes) = std::mem::take(&mut self.bisected);
        if let Err(e) = self.live_of(part) {
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
                let (s, e) = self.clamp(part, s, e)?;
                Some(if good {
                    Item::Run {
                        part,
                        start: s,
                        at: (s - start) as usize,
                        len: (e - s) as usize,
                        last: false,
                    }
                } else {
                    Item::Bad {
                        part,
                        start: s,
                        end: e,
                    }
                })
            })
            .collect();
        let last_run = items.iter().rposition(|i| matches!(i, Item::Run { .. }));
        for (i, item) in items.into_iter().enumerate() {
            match item {
                Item::Run {
                    part,
                    start,
                    at,
                    len,
                    ..
                } => self.pending.push_back(Item::Run {
                    part,
                    start,
                    at,
                    len,
                    last: Some(i) == last_run,
                }),
                Item::Bad { part, start, end } => self.queue_bad(part, start, end),
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
    #[allow(clippy::too_many_arguments)]
    fn halves(
        &mut self,
        part: usize,
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
            return Err(not_written(NotWrittenCause::Interrupted, self.ctx_of(part)));
        };
        // `len ≤ req ≤ capacity`.
        let _fits = buf.set_len(len as usize);
        let admitted = match &self.pace {
            Some(p) => Some(p.admit()?),
            None => None,
        };
        let op = IoOp::Read {
            file: &self.store.inner.file,
            offset: self.parts[part].r.map.locate(at, len),
            buf,
        };
        let Some(lane) = self.lane.as_mut() else {
            return Err(not_written(NotWrittenCause::Interrupted, self.ctx_of(part)));
        };
        let done = run(lane, op);
        // Completed (or abandoned): its charge goes with it.
        drop(admitted);
        let (result, back) = match done {
            Ok(Ran::Done { buf, result }) => (result, buf),
            Ok(Ran::Abandoned(raw)) => return Err(self.read_error_of(part, raw)),
            Err((buf, Some(raw))) => {
                *scratch = buf;
                return Err(self.read_error_of(part, raw));
            }
            Err((buf, None)) => {
                *scratch = buf;
                return Err(not_written(NotWrittenCause::QueueFull, self.ctx_of(part)));
            }
        };
        let Some(b) = back else {
            return Err(match result {
                Err(raw) => self.read_error_of(part, raw),
                Ok(_) => not_written(NotWrittenCause::Interrupted, self.ctx_of(part)),
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
                self.halves(part, base, at, half, piece, scratch, found)?;
                self.halves(part, base, at + half, len - half, piece, scratch, found)
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
                Err(self.read_error_of(part, raw))
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
        self.core.borrow_mut().next_item()
    }
}

impl<P: Platform> Core<P>
where
    P::Queue: Send,
{
    /// `Iterator::next`, under the cell's one borrow.
    fn next_item(&mut self) -> Option<Result<Chunk, Error>> {
        if self.ended {
            return None;
        }
        // A collection for another part failed: the reads it lost end the
        // stream.
        if let Some((raw, _)) = self.collect_failed {
            self.finish();
            return Some(Err(self.read_error(raw)));
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

impl<P: Platform> Drop for Core<P>
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

/// A stream as one part of a call (V6): another part on its thread
/// collects its reads, and its own admission skips it while it asks.
#[cfg(all(test, not(loom)))]
mod collect_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, unused_results)]

    use std::cell::RefCell;
    use std::path::Path;
    use std::rc::Rc;

    use store_io_sim::{SimConfig, SimPlatform};

    use super::*;
    use crate::{AppendRegion, StoreOptions};

    const LEN: usize = 4 << 20;

    /// A store whose commits are quiet and whose calls keep at most four
    /// pieces in flight unpaced, and a region of `LEN` stamped bytes,
    /// durable.
    fn setup(
        seed: u64,
    ) -> (
        SimPlatform,
        Store<SimPlatform>,
        AppendRegion<SimPlatform>,
        Vec<u8>,
    ) {
        let p = SimPlatform::new(SimConfig::volatile(seed));
        let opts = StoreOptions {
            background_unpaced: 4,
            ..StoreOptions::default()
        };
        let s = Store::create(p.clone(), Path::new("/s"), opts).unwrap();
        let log = s.provision_append_region("log", LEN as u64).unwrap();
        let data: Vec<u8> = (0..LEN).map(|i| (i % 251) as u8).collect();
        let t = log.append(&data).unwrap();
        let _receipt = log.sync_through(&t).unwrap();
        p.with_world(|w| w.advance_clock_ns(10_000_000_000));
        assert!(!s.qos().active);
        (p, s, log, data)
    }

    /// The stream's next chunk, appended to `out` (checked in order).
    fn take(stream: &mut ReadStream<SimPlatform>, out: &mut Vec<u8>) {
        let c = stream.next().unwrap().unwrap();
        assert_eq!(c.offset() as usize, out.len());
        out.extend_from_slice(c.bytes());
    }

    /// Two chunks read, the second's refill left in flight: the stream
    /// then holds the call's four unpaced pieces.
    fn holding_the_cap(p: &SimPlatform, stream: &mut ReadStream<SimPlatform>, out: &mut Vec<u8>) {
        take(stream, out);
        p.with_world(|w| w.hold_reaps(1));
        take(stream, out);
        assert_eq!(stream.pace().unwrap().held_here(), 4, "the call's cap");
    }

    /// Another part of a stream's call, refused because the stream's reads
    /// hold the call's cap, collects them on this thread and is admitted;
    /// the stream then delivers every byte in order.
    #[test]
    fn test_another_part_collects_a_stream_s_reads() {
        let (p, _s, log, data) = setup(70);
        let mut stream = log.read_stream(0..LEN as u64).unwrap();
        let mut out = Vec::new();
        holding_the_cap(&p, &mut stream, &mut out);
        let pace = stream.pace().unwrap();
        let piece = pace.admit().unwrap();
        assert_eq!(pace.held_here(), 1, "the stream's reads collected");
        drop(piece);
        while out.len() < LEN {
            take(&mut stream, &mut out);
        }
        assert!(stream.next().is_none());
        assert!(out == data);
    }

    /// Paced, another part refused collects all of the stream's reads.
    #[test]
    fn test_a_paced_part_collects_all_of_a_stream_s_reads() {
        let (p, s, log, data) = setup(73);
        let mut stream = log.read_stream(0..LEN as u64).unwrap();
        let mut out = Vec::new();
        holding_the_cap(&p, &mut stream, &mut out);
        let hold = s.commit_hold();
        let pace = stream.pace().unwrap();
        let piece = pace.admit().unwrap();
        assert_eq!(pace.held_here(), 1);
        drop(piece);
        drop(hold);
        p.with_world(|w| w.advance_clock_ns(10_000_000_000));
        while out.len() < LEN {
            take(&mut stream, &mut out);
        }
        assert!(out == data);
    }

    /// A stream with nothing in flight is never waited on for another part.
    #[test]
    fn test_a_stream_with_nothing_in_flight_is_not_waited_on() {
        let (p, _s, log, _data) = setup(74);
        let mut stream = log.read_stream(0..LEN as u64).unwrap();
        let mut out = Vec::new();
        take(&mut stream, &mut out);
        let pace = stream.pace().unwrap();
        assert_eq!(pace.held_here(), 0, "the sim's wait completed every read");
        let held = (0..4).map(|_| pace.try_charge().unwrap()).collect();
        let other = Rc::new(Other(RefCell::new(held)));
        let weak: Weak<Other> = Rc::downgrade(&other);
        let _reg = pace.register(weak);
        let waits = p.with_world(|w| w.waits());
        let _piece = pace.admit().unwrap();
        assert_eq!(p.with_world(|w| w.waits()), waits);
        assert_eq!(other.0.borrow().len(), 3);
    }

    /// Another part of a call holding its pieces.
    struct Other(RefCell<Vec<Held>>);

    impl Collect for Other {
        fn collect_one(&self) -> Result<bool, OsError> {
            Ok(self.0.borrow_mut().pop().is_some())
        }

        fn collect_all(&self) -> Result<(), OsError> {
            self.0.borrow_mut().clear();
            Ok(())
        }

        fn failed(&self) -> Option<(OsError, u64)> {
            None
        }
    }

    /// A stream refused because another part on its thread holds the
    /// call's cap collects that part's pieces, skipping its own collector
    /// (its cell is borrowed: it is the part asking), and reads on.
    #[test]
    fn test_a_stream_asking_skips_itself_and_collects_the_other_part() {
        let (_p, _s, log, data) = setup(71);
        let mut stream = log.read_stream(0..LEN as u64).unwrap();
        let pace = stream.pace().unwrap();
        let held = (0..4).map(|_| pace.try_charge().unwrap()).collect();
        let other = Rc::new(Other(RefCell::new(held)));
        let weak: Weak<Other> = Rc::downgrade(&other);
        let _reg = pace.register(weak);
        let mut out = Vec::new();
        while out.len() < LEN {
            take(&mut stream, &mut out);
        }
        assert!(stream.next().is_none());
        assert!(out == data);
        assert!(
            other.0.borrow().len() < 4,
            "the other part's pieces collected"
        );
    }

    /// Paced, a stream asking while another part holds the call's pieces
    /// collects them all, skipping its own collector, and reads on.
    #[test]
    fn test_a_paced_stream_asking_skips_itself_and_collects_the_other_part() {
        let (_p, s, log, data) = setup(75);
        let mut stream = log.read_stream(0..LEN as u64).unwrap();
        let pace = stream.pace().unwrap();
        let held = (0..2).map(|_| pace.try_charge().unwrap()).collect();
        let other = Rc::new(Other(RefCell::new(held)));
        let weak: Weak<Other> = Rc::downgrade(&other);
        let _reg = pace.register(weak);
        let _hold = s.commit_hold();
        let mut out = Vec::new();
        while out.len() < LEN {
            take(&mut stream, &mut out);
        }
        assert!(out == data);
        assert!(other.0.borrow().is_empty());
    }

    /// A collection for another part whose wait fails: the admission ends,
    /// the stream's charges are dropped (the reads stay with its queue,
    /// never waited on again), the failure is recorded, and the stream's
    /// next call ends it with the error.
    #[test]
    fn test_a_failed_collection_ends_the_stream_with_its_error() {
        let (p, _s, log, _data) = setup(72);
        let mut stream = log.read_stream(0..LEN as u64).unwrap();
        let mut out = Vec::new();
        holding_the_cap(&p, &mut stream, &mut out);
        let pace = stream.pace().unwrap();
        p.with_world(|w| {
            let next = w.waits() + 1;
            w.faults_mut().fail_wait = Some((next, 5));
        });
        assert!(pace.admit().is_err());
        assert_eq!(pace.held_here(), 0, "its charges dropped");
        // Two chunks delivered, two more read: the oldest lost is the fifth.
        let lost = stream.core.failed().map(|(_, at)| at);
        assert_eq!(lost, Some(4 * (out.len() as u64 / 2)));
        let waits = p.with_world(|w| w.waits());
        // Sticky: another admission fails without waiting again.
        let again = pace.try_charge().unwrap();
        let more: Vec<_> = (0..3).map(|_| pace.try_charge().unwrap()).collect();
        assert!(pace.admit().is_err());
        assert_eq!(p.with_world(|w| w.waits()), waits);
        drop((again, more));
        assert!(matches!(stream.next(), Some(Err(Error::Io { .. }))));
        assert!(stream.next().is_none());
        // Ended, it holds nothing: never a failure for the call's others.
        let held = (0..4).map(|_| pace.try_charge().unwrap()).collect();
        let other = Rc::new(Other(RefCell::new(held)));
        let weak: Weak<Other> = Rc::downgrade(&other);
        let _reg = pace.register(weak);
        let _piece = pace.admit().unwrap();
        drop(stream);
        assert_eq!(p.with_world(|w| w.waits()), waits, "never waited on again");
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
