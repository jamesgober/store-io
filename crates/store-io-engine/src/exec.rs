//! Running operations on platform queues from the blocking layers.
//!
//! The simple and batch layers block their caller until the I/O is done. Each
//! call borrows one queue from a small per-store set with a non-blocking
//! `try_lock` (one atomic compare-and-swap when uncontended; a queue is held
//! only for the duration of one call's I/O, never while waiting for someone
//! else's flush). Only when every queue is busy does a caller wait, and then
//! for whichever queue is returned first: the guard's drop wakes one waiter.
//!
//! The wake-up is lost-wake-free without sequentially consistent loads: a
//! returning thread unlocks its queue (a read-modify-write of the mutex) and
//! then reads the waiter count with a read-modify-write; a waiter increments
//! the count and then retries every queue with `try_lock` (read-modify-writes)
//! while holding the wake mutex. Either the waiter's retry finds the returned
//! queue, or the returner sees the waiter and signals under the same mutex.

use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, TryLockError};
use store_io_core::observe::{Counter, Observer};

use store_io_core::error::OsError;
use store_io_platform::{CompletionBuf, IoBuf, IoOp, Queue, RawResult};

use crate::sync::{AtomicBool, AtomicU64, Ordering};

/// A fixed set of queues shared by the blocking layers, plus an optional
/// flush lane and, created on first use, as many parallel lanes.
///
/// The flush lane (D-070) is used only by the thread holding a flush
/// domain's leadership. Leadership is exclusive, so the lane is never
/// contended, and a leader never waits for one of the shared queues, which
/// followers may be holding while they wait for that very leader.
///
/// Parallel lanes (D-096) hold queues asked to run their operations
/// concurrently (`QueueConfig::parallel`): a call that submits several
/// independent operations before it waits (a page batch of several writes,
/// a read or scan spanning more than one buffer) takes one. Everything else
/// keeps the shared lanes, so a single write or a read of one buffer never
/// leaves the caller's thread. They are created by the
/// first such call (on the Linux synchronous tier that starts the platform's
/// worker pool); if they cannot be, that call and every later one use the
/// shared lanes.
pub(crate) struct QueueSet<Q> {
    queues: Box<[Mutex<Lane<Q>>]>,
    flush: Option<Mutex<Lane<Q>>>,
    parallel: OnceLock<Box<[Mutex<Lane<Q>>]>>,
    /// Serialises creating the parallel lanes.
    parallel_init: Mutex<()>,
    /// The parallel queues' worker threads (`Queue::workers`), once created.
    parallel_workers: OnceLock<Option<usize>>,
    /// Creating the parallel queues failed: every later call uses a shared
    /// lane at once, without retrying (no lock convoy, no failed spawn per
    /// call).
    parallel_off: AtomicBool,
    depth: usize,
    /// One busy mark per shared lane, then one per parallel lane, then one
    /// for the flush lane (D-095), each alone on its cache line and shared
    /// with its lane: odd while the lane is held, advanced by 2 at each
    /// completion. Only the holder writes it; a watchdog reads it without
    /// the lock. A lane not created yet keeps its mark even.
    marks: Box<[Arc<LaneMark>]>,
    waiters: AtomicU64,
    wake: Mutex<()>,
    returned: Condvar,
}

/// A queue and its completion buffer.
pub(crate) struct Lane<Q> {
    pub(crate) queue: Q,
    pub(crate) done: CompletionBuf,
    mark: Arc<LaneMark>,
}

impl<Q> Lane<Q> {
    /// An operation on this lane completed: whatever is outstanding now is
    /// a newer operation, so `oldest_outstanding` ages it from now, not
    /// from when the lane was taken (D-095). Called by the holder only.
    #[inline]
    pub(crate) fn progress(&self) {
        self.mark.advance();
    }
}

impl LaneMark {
    /// See [`Lane::progress`].
    #[inline]
    fn advance(&self) {
        let v = self.0.load(Ordering::Relaxed);
        self.0.store(v.wrapping_add(2), Ordering::Relaxed);
    }
}

/// A lane's busy mark, alone on its cache line so that lanes used by
/// different threads never share one. A `Submitter` keeps one too (D-098).
#[repr(align(128))]
pub(crate) struct LaneMark(AtomicU64);

impl LaneMark {
    pub(crate) fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// Sets the mark (its only writer: the owner).
    #[inline]
    pub(crate) fn set(&self, v: u64) {
        self.0.store(v, Ordering::Relaxed);
    }

    pub(crate) fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// A borrowed queue; returning a shared one wakes one caller waiting for a
/// shared queue (nothing waits for a parallel lane: a call that finds them
/// all busy takes a shared one).
///
/// Fields drop in declaration order: the queue's lock is released first,
/// then [`Notify`] wakes a waiter, which therefore always finds it free.
pub(crate) struct LaneGuard<'a, Q> {
    // Declared first, so the mark is back to even before the lock is
    // released: only the holder may write it.
    _mark: Mark<'a>,
    guard: MutexGuard<'a, Lane<Q>>,
    _notify: Option<Notify<'a, Q>>,
}

/// The flush lane, borrowed by the flush leader.
pub(crate) struct FlushGuard<'a, Q> {
    // As in `LaneGuard`: the mark goes back to even before the unlock.
    _mark: Mark<'a>,
    guard: MutexGuard<'a, Lane<Q>>,
}

impl<Q> Deref for FlushGuard<'_, Q> {
    type Target = Lane<Q>;

    fn deref(&self) -> &Lane<Q> {
        &self.guard
    }
}

impl<Q> DerefMut for FlushGuard<'_, Q> {
    fn deref_mut(&mut self) -> &mut Lane<Q> {
        &mut self.guard
    }
}

/// A lane's busy mark while it is held: odd from `take` until dropped.
struct Mark<'a>(&'a AtomicU64);

impl<'a> Mark<'a> {
    /// Called with the lane's lock held: no other thread writes the mark.
    fn take(mark: &'a AtomicU64) -> Self {
        let v = mark.load(Ordering::Relaxed);
        mark.store(v.wrapping_add(1), Ordering::Relaxed);
        Self(mark)
    }
}

impl Drop for Mark<'_> {
    fn drop(&mut self) {
        let v = self.0.load(Ordering::Relaxed);
        self.0.store(v.wrapping_add(1), Ordering::Relaxed);
    }
}

/// Wakes one waiting caller, if any, when dropped.
struct Notify<'a, Q> {
    set: &'a QueueSet<Q>,
}

impl<Q> Drop for Notify<'_, Q> {
    fn drop(&mut self) {
        if self.set.waiters.fetch_add(0, Ordering::AcqRel) > 0 {
            let _wake = self.set.wake.lock().unwrap_or_else(PoisonError::into_inner);
            self.set.returned.notify_one();
        }
    }
}

impl<Q> Deref for LaneGuard<'_, Q> {
    type Target = Lane<Q>;

    fn deref(&self) -> &Lane<Q> {
        &self.guard
    }
}

impl<Q> DerefMut for LaneGuard<'_, Q> {
    fn deref_mut(&mut self) -> &mut Lane<Q> {
        &mut self.guard
    }
}

impl<Q: Queue> QueueSet<Q> {
    /// Wraps already-created queues; no flush lane.
    pub(crate) fn new(queues: Vec<Q>, depth: usize) -> Self {
        Self::build(queues, None, depth)
    }

    /// Wraps already-created queues and a dedicated flush lane.
    pub(crate) fn with_flush_lane(queues: Vec<Q>, flush: Q, depth: usize) -> Self {
        Self::build(queues, Some(flush), depth)
    }

    fn build(queues: Vec<Q>, flush: Option<Q>, depth: usize) -> Self {
        // One mark per shared lane, one per parallel lane, then the flush
        // lane's (which stays even when there is no flush lane).
        let n = queues.len();
        let marks: Box<[Arc<LaneMark>]> = (0..=2 * n)
            .map(|_| Arc::new(LaneMark(AtomicU64::new(0))))
            .collect();
        Self {
            queues: queues
                .into_iter()
                .zip(marks.iter())
                .map(|(q, m)| lane(q, m, depth))
                .collect(),
            flush: flush.map(|q| lane(q, &marks[2 * n], depth)),
            parallel: OnceLock::new(),
            parallel_init: Mutex::new(()),
            parallel_workers: OnceLock::new(),
            parallel_off: AtomicBool::new(false),
            depth,
            marks,
            waiters: AtomicU64::new(0),
            wake: Mutex::new(()),
            returned: Condvar::new(),
        }
    }

    /// The flush lane, for the thread holding flush leadership only; `None`
    /// when the set has none.
    pub(crate) fn flush_lane(&self) -> Option<FlushGuard<'_, Q>> {
        self.flush.as_ref().map(|m| {
            let guard = m.lock().unwrap_or_else(PoisonError::into_inner);
            FlushGuard {
                _mark: Mark::take(&self.marks[2 * self.queues.len()].0),
                guard,
            }
        })
    }

    /// A parallel lane (see [`QueueSet`]), starting the search at `hint`.
    /// When every parallel lane is busy, the call takes a shared lane
    /// instead (waiting, as any shared call does, for whichever is returned
    /// first): never worse than a set without parallel lanes, and a thread
    /// that already holds a parallel lane (a scan's visitor reading) can
    /// never wait for itself. The first call creates them with
    /// `make(count)`; if that fails, or returns another count, this and every
    /// later call uses the shared lanes.
    pub(crate) fn get_parallel(
        &self,
        hint: usize,
        make: impl FnOnce(usize) -> Option<Vec<Q>>,
    ) -> LaneGuard<'_, Q> {
        let n = self.queues.len();
        if self.parallel_off.load(Ordering::Relaxed) {
            return self.get(hint);
        }
        let parallel = self.parallel.get().map(|b| &**b);
        let Some(lanes) = parallel.or_else(|| self.create_parallel(make)) else {
            return self.get(hint);
        };
        let found = (0..n).find_map(|i| {
            let at = (hint + i) % n;
            match lanes[at].try_lock() {
                Ok(g) => Some((at, g)),
                Err(TryLockError::Poisoned(p)) => Some((at, p.into_inner())),
                Err(TryLockError::WouldBlock) => None,
            }
        });
        let Some((at, guard)) = found else {
            return self.get(hint);
        };
        LaneGuard {
            // The lane's own `Lane::mark` (used by `progress`) is this same
            // `marks[n + at]`, bound in `create_parallel`: the two must agree.
            _mark: Mark::take(&self.marks[n + at].0),
            guard,
            _notify: None,
        }
    }

    fn create_parallel(
        &self,
        make: impl FnOnce(usize) -> Option<Vec<Q>>,
    ) -> Option<&[Mutex<Lane<Q>>]> {
        let _one = self
            .parallel_init
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(lanes) = self.parallel.get() {
            return Some(&**lanes);
        }
        // A caller that queued here behind a failed start does not try again.
        if self.parallel_off.load(Ordering::Relaxed) {
            return None;
        }
        let n = self.queues.len();
        let Some(queues) = make(n).filter(|qs| qs.len() == n && n > 0) else {
            self.parallel_off.store(true, Ordering::Relaxed);
            return None;
        };
        let _workers = self
            .parallel_workers
            .set(queues.first().and_then(Queue::workers));
        let lanes: Box<[Mutex<Lane<Q>>]> = queues
            .into_iter()
            .zip(&self.marks[n..2 * n])
            .map(|(q, m)| lane(q, m, self.depth))
            .collect();
        let _set = self.parallel.set(lanes);
        self.parallel.get().map(|b| &**b)
    }

    /// Whether the parallel lanes exist.
    #[cfg(test)]
    pub(crate) fn has_parallel(&self) -> bool {
        self.parallel.get().is_some()
    }

    /// Worker threads running the parallel lanes' operations, once they
    /// exist and the backend runs them on a pool (`Queue::workers`).
    pub(crate) fn parallel_workers(&self) -> Option<usize> {
        self.parallel_workers.get().copied().flatten()
    }

    /// Every lane's busy mark: the shared lanes in order, the parallel lanes,
    /// then the flush lane (odd while held; always even for a lane that does
    /// not exist).
    pub(crate) fn marks(&self) -> impl ExactSizeIterator<Item = u64> + '_ {
        self.marks.iter().map(|m| m.0.load(Ordering::Relaxed))
    }

    fn try_any(&self, hint: usize) -> Option<(usize, MutexGuard<'_, Lane<Q>>)> {
        let n = self.queues.len();
        (0..n).find_map(|i| {
            let at = (hint + i) % n;
            match self.queues[at].try_lock() {
                Ok(g) => Some((at, g)),
                Err(TryLockError::Poisoned(p)) => Some((at, p.into_inner())),
                Err(TryLockError::WouldBlock) => None,
            }
        })
    }

    /// Borrows a free queue, starting the search at `hint`; when all are
    /// busy, waits for the first one returned.
    pub(crate) fn get(&self, hint: usize) -> LaneGuard<'_, Q> {
        if let Some((at, guard)) = self.try_any(hint) {
            return LaneGuard {
                _mark: Mark::take(&self.marks[at].0),
                guard,
                _notify: Some(Notify { set: self }),
            };
        }
        let _registered = self.waiters.fetch_add(1, Ordering::AcqRel);
        let mut wake = self.wake.lock().unwrap_or_else(PoisonError::into_inner);
        let (at, guard) = loop {
            if let Some(found) = self.try_any(hint) {
                break found;
            }
            wake = self
                .returned
                .wait(wake)
                .unwrap_or_else(PoisonError::into_inner);
        };
        drop(wake);
        let _unregistered = self.waiters.fetch_sub(1, Ordering::AcqRel);
        LaneGuard {
            _mark: Mark::take(&self.marks[at].0),
            guard,
            _notify: Some(Notify { set: self }),
        }
    }
}

/// A lane over `queue` with its completion buffer and busy mark.
fn lane<Q>(queue: Q, mark: &Arc<LaneMark>, depth: usize) -> Mutex<Lane<Q>> {
    Mutex::new(Lane {
        queue,
        done: CompletionBuf::with_capacity(depth.max(1)),
        mark: Arc::clone(mark),
    })
}

/// What became of an operation that reached the queue.
pub(crate) enum Ran {
    /// It completed: its buffer (if any) and the raw result.
    Done {
        buf: Option<IoBuf>,
        result: RawResult<usize>,
    },
    /// Waiting failed with it in flight (or the queue could not even be
    /// cleared for it), so there is no result. Its buffer stays with the
    /// queue until it completes, and the next holder of the lane discards
    /// that completion ([`clear_stale`]). For a write, durability is unknown;
    /// for a read, it is a failure of the queue, never a verdict on the
    /// media.
    Abandoned(OsError),
}

/// Collects and discards what an earlier holder of this lane left in flight
/// when its wait failed, so that every holder starts on an empty queue and
/// no completion can ever be taken for another operation's. A completion
/// means the device is done with its buffer, so dropping one is safe. One
/// `in_flight` call when the queue is clean (every time, unless a wait has
/// failed).
///
/// # Errors
///
/// The raw error if waiting for them fails; nothing of the caller's has been
/// submitted.
pub(crate) fn clear_stale<Q: Queue>(lane: &mut Lane<Q>) -> Result<(), OsError> {
    lane.done.drain().for_each(drop);
    while lane.queue.in_flight() > 0 {
        let n = lane.queue.in_flight();
        let _added = lane.queue.wait(n, &mut lane.done)?;
        lane.done.drain().for_each(drop);
        // Clearing is progress, not the holder's own operation aging.
        lane.progress();
    }
    Ok(())
}

/// Submits one operation and waits for its completion.
///
/// # Errors
///
/// `Err((buf, raw))` when the operation was refused before reaching the
/// device (media untouched); the buffer is returned. If clearing an earlier
/// holder's abandoned operations fails first, the result is
/// [`Ran::Abandoned`].
pub(crate) fn run<Q: Queue>(
    slot: &mut Lane<Q>,
    op: IoOp<'_, Q::File>,
) -> Result<Ran, (Option<IoBuf>, Option<OsError>)> {
    const TAG: u64 = 0x5110;
    let is_read = matches!(op, IoOp::Read { .. });
    if let Err(raw) = clear_stale(slot) {
        // The queue cannot be waited on: nothing of this operation was
        // submitted, but the queue is broken, so it is abandoned, which
        // every caller treats as fail-stop (never as a refusal whose code
        // could read as "out of space").
        drop(op);
        return Ok(Ran::Abandoned(raw));
    }
    slot.queue.submit(op, TAG).map_err(|r| (r.buf, r.raw))?;
    loop {
        // A borrow of the mark beside the drain of the completions.
        let mark = &slot.mark;
        for c in slot.done.drain() {
            if c.tag == TAG {
                debug_assert!(
                    !is_read || c.buf.is_some() || c.result.is_err(),
                    "a read succeeded without its buffer (platform contract 1)"
                );
                mark.advance();
                return Ok(Ran::Done {
                    buf: c.buf,
                    result: c.result,
                });
            }
        }
        if let Err(raw) = slot.queue.wait(1, &mut slot.done) {
            // Waiting failed but the operation is in flight: its buffer is
            // still owned by the kernel and must not be released.
            return Ok(Ran::Abandoned(raw));
        }
    }
}

/// A producer of writes for [`pump`].
pub(crate) trait Source {
    /// The next write as `(container offset, buffer)`, [`Next::Wait`] when a
    /// buffer must come back first, or [`Next::Done`].
    fn next(&mut self) -> Next;
    /// A buffer whose write completed in full; the source may reuse it.
    fn recycle(&mut self, buf: IoBuf);
    /// Whether this is one write larger than a buffer, submitted as several
    /// device writes (observability only; a batch of several is not).
    fn is_split(&self) -> bool {
        false
    }
}

/// What a [`Source`] has to offer.
pub(crate) enum Next {
    /// Write this buffer at this container offset.
    Write(u64, IoBuf),
    /// Nothing until a buffer is recycled.
    Wait,
    /// Every write has been produced.
    Done,
}

/// Outcome of [`pump`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pumped {
    /// Every write transferred its full length.
    All,
    /// The first write was refused before reaching the device and nothing
    /// was submitted: the media is untouched.
    Refused(Option<OsError>),
    /// A write failed, transferred short, or was refused after others had
    /// been submitted; or waiting failed. Durability is unknown.
    Failed(Option<OsError>),
}

/// Submits every write `src` produces on one queue, keeping as many in
/// flight as the queue accepts, and waits for all of them.
///
/// After the first failure nothing new is submitted, but every write already
/// in flight is still waited for: its buffer belongs to the kernel until its
/// completion arrives. Only a failed wait returns early, leaving those
/// buffers with the queue, which never releases a buffer the kernel owns.
pub(crate) fn pump<Q: Queue>(
    lane: &mut Lane<Q>,
    file: &Q::File,
    dsync: bool,
    src: &mut impl Source,
    obs: &Observer,
) -> Pumped {
    const TAG: u64 = 0x5111;
    // The caller's lane is already cleared (`Store::lane`).
    let mut in_flight = 0usize;
    let mut submitted = 0usize;
    let mut failure: Option<Option<OsError>> = None;
    let mut held: Option<(u64, IoBuf)> = None;
    let mut producing = true;
    loop {
        while failure.is_none() && producing {
            let (offset, buf) = match held.take() {
                Some(w) => w,
                None => match src.next() {
                    Next::Write(offset, buf) => (offset, buf),
                    Next::Wait => break,
                    Next::Done => {
                        producing = false;
                        break;
                    }
                },
            };
            let bytes = buf.len() as u64;
            match lane.queue.submit(
                IoOp::Write {
                    file,
                    offset,
                    buf,
                    dsync,
                },
                TAG,
            ) {
                Ok(()) => {
                    in_flight += 1;
                    submitted += 1;
                    obs.add(Counter::Writes, 1);
                    obs.add(Counter::WriteBytes, bytes);
                }
                // Full: keep the write and retry once a completion frees room.
                Err(r) if r.raw.is_none() && in_flight > 0 => {
                    match r.buf {
                        Some(buf) => held = Some((offset, buf)),
                        // A refusal that did not return the buffer (a backend
                        // breaking platform contract 1): the write is lost,
                        // so the call fails rather than being acknowledged
                        // short.
                        None => failure = Some(None),
                    }
                    break;
                }
                Err(r) => {
                    if submitted == 0 {
                        return Pumped::Refused(r.raw);
                    }
                    failure = Some(r.raw);
                }
            }
        }
        if in_flight == 0 {
            if producing && failure.is_none() {
                // The source waits for a buffer while none is in flight: it
                // can never be satisfied. Fail rather than report success.
                if submitted == 0 {
                    return Pumped::Refused(None);
                }
                failure = Some(None);
            }
            break;
        }
        if let Err(raw) = lane.queue.wait(1, &mut lane.done) {
            return Pumped::Failed(Some(raw));
        }
        lane.progress();
        for c in lane.done.drain() {
            in_flight = in_flight.saturating_sub(1);
            match (c.result, c.buf) {
                (Ok(n), Some(buf)) if n == buf.len() => src.recycle(buf),
                (result, _) => {
                    if failure.is_none() {
                        failure = Some(result.err());
                    }
                }
            }
        }
    }
    match failure {
        None => Pumped::All,
        Some(raw) => Pumped::Failed(raw),
    }
}
