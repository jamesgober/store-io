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
use store_io_platform::{Completion, CompletionBuf, IoBuf, IoOp, Queue, RawResult, Rejected};

use crate::optable::Kind;
use crate::queue_core::Core;
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
    /// for the flush lane (D-095), each alone on its cache line and published
    /// by its lane's op table: odd while an operation is in flight on the
    /// lane (`2 * oldest + 1`, so it changes when the oldest completes),
    /// even otherwise. Only the holder writes it; a watchdog reads it
    /// without the lock. A lane not created yet keeps its mark even.
    marks: Box<[Arc<LaneMark>]>,
    waiters: AtomicU64,
    wake: Mutex<()>,
    returned: Condvar,
}

/// A [`Core`] (a queue and its op table, A1, PLANNING R3.3), as the
/// `Submitter` is one: one row per operation in flight. Every operation is
/// submitted under its row's tag (index and generation), so a completion is
/// matched to exactly the operation that made it, and the next holder of the
/// lane clears an earlier holder's abandoned operations row by row.
pub(crate) struct Lane<Q> {
    core: Core<Q>,
    /// This holder's completed operations, under the holder's own tags.
    pub(crate) done: CompletionBuf,
}

impl<Q: Queue> Lane<Q> {
    /// Submits `op` in a free row; the completion comes back in `done` with
    /// `tag`. A lane with no free row refuses it as a full queue (`raw:
    /// None`, the buffer returned).
    pub(crate) fn submit(&mut self, op: IoOp<'_, Q::File>, tag: u64) -> Result<(), Rejected> {
        let Some((index, qtag)) = self.core.table.claim() else {
            let buf = match op {
                IoOp::Write { buf, .. } | IoOp::Read { buf, .. } => Some(buf),
                IoOp::FlushData { .. } => None,
            };
            return Err(Rejected { buf, raw: None });
        };
        self.core.table.start(index, tag, Kind::Raw, None, false);
        self.core
            .queue
            .submit(op, qtag)
            .inspect_err(|_| self.core.table.abort(index))
    }

    /// Waits until at least `min` of the queue's operations complete, then
    /// moves every completion that arrived into `done` under its caller's
    /// tag (also when the wait then failed). Returns how many arrived.
    ///
    /// # Errors
    ///
    /// The raw error if waiting failed: the operations still in flight keep
    /// their rows and their buffers stay with the queue.
    pub(crate) fn wait(&mut self, min: usize) -> Result<usize, OsError> {
        let waited = self.core.queue.wait(min, &mut self.core.raw);
        let n = self.collect();
        waited.map(|_| n)
    }

    /// Operations of this lane in flight.
    pub(crate) fn in_flight(&self) -> usize {
        self.core.table.live
    }

    fn collect(&mut self) -> usize {
        let mut n = 0;
        for c in self.core.raw.drain() {
            // A completion names a row in use: the queue is this lane's alone
            // and every tag names one use of one row.
            let Some(tag) = self.core.table.take(c.tag) else {
                debug_assert!(false, "a lane completion matched no row");
                continue;
            };
            n += 1;
            // `done` holds at most one completion per row (depth) and every
            // holder drains it before waiting again: a completion that did
            // not fit would be a result lost, so this stops instead.
            assert!(
                self.done.push(Completion { tag, ..c }).is_ok(),
                "a lane's completions overflowed"
            );
        }
        n
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
/// An operation abandoned on the lane (a failed wait) stays in its row, and
/// outstanding, until the next holder clears it.
pub(crate) struct LaneGuard<'a, Q: Queue> {
    guard: MutexGuard<'a, Lane<Q>>,
    _notify: Option<Notify<'a, Q>>,
}

/// The flush lane, borrowed by the flush leader.
pub(crate) struct FlushGuard<'a, Q: Queue> {
    guard: MutexGuard<'a, Lane<Q>>,
}

impl<Q: Queue> Deref for FlushGuard<'_, Q> {
    type Target = Lane<Q>;

    fn deref(&self) -> &Lane<Q> {
        &self.guard
    }
}

impl<Q: Queue> DerefMut for FlushGuard<'_, Q> {
    fn deref_mut(&mut self) -> &mut Lane<Q> {
        &mut self.guard
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

impl<Q: Queue> Deref for LaneGuard<'_, Q> {
    type Target = Lane<Q>;

    fn deref(&self) -> &Lane<Q> {
        &self.guard
    }
}

impl<Q: Queue> DerefMut for LaneGuard<'_, Q> {
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
            FlushGuard { guard }
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
        let Some((_, guard)) = found else {
            return self.get(hint);
        };
        LaneGuard {
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
    /// then the flush lane (odd while an operation is in flight on it;
    /// always even for a lane that does not exist).
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
        if let Some((_, guard)) = self.try_any(hint) {
            return LaneGuard {
                guard,
                _notify: Some(Notify { set: self }),
            };
        }
        let _registered = self.waiters.fetch_add(1, Ordering::AcqRel);
        let mut wake = self.wake.lock().unwrap_or_else(PoisonError::into_inner);
        let (_, guard) = loop {
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
            guard,
            _notify: Some(Notify { set: self }),
        }
    }
}

/// A lane over `queue` with its completion buffer and busy mark.
fn lane<Q>(queue: Q, mark: &Arc<LaneMark>, depth: usize) -> Mutex<Lane<Q>> {
    let depth = depth.max(1);
    Mutex::new(Lane {
        // The table publishes the lane's busy mark (D-095).
        core: Core::new(queue, depth, Arc::clone(mark)),
        done: CompletionBuf::with_capacity(depth),
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
/// when its wait failed, row by row, so that every holder starts with no
/// operation in flight and no completion can ever be taken for another
/// operation's. A completion means the device is done with its buffer, so
/// dropping one is safe. One `in_flight` call when the queue is clean
/// (every time, unless a wait has failed).
///
/// # Errors
///
/// The raw error if waiting for them fails; nothing of the caller's has been
/// submitted.
pub(crate) fn clear_stale<Q: Queue>(lane: &mut Lane<Q>) -> Result<(), OsError> {
    lane.done.drain().for_each(drop);
    while lane.core.queue.in_flight() > 0 {
        let n = lane.core.queue.in_flight();
        let _added = lane.wait(n)?;
        lane.done.drain().for_each(drop);
    }
    // Rows whose queue holds nothing can never complete (a backend that lost
    // their completions): freed, so the lane is usable.
    debug_assert_eq!(lane.in_flight(), 0, "lane rows outlived their operations");
    if lane.in_flight() > 0 {
        lane.core.table.forget();
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
    slot.submit(op, TAG).map_err(|r| (r.buf, r.raw))?;
    loop {
        for c in slot.done.drain() {
            if c.tag == TAG {
                debug_assert!(
                    !is_read || c.buf.is_some() || c.result.is_err(),
                    "a read succeeded without its buffer (platform contract 1)"
                );
                return Ok(Ran::Done {
                    buf: c.buf,
                    result: c.result,
                });
            }
        }
        if let Err(raw) = slot.wait(1) {
            // Waiting failed but the operation is in flight: its buffer is
            // still owned by the kernel and must not be released (its row
            // stays, for the next holder to clear).
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
            match lane.submit(
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
        if let Err(raw) = lane.wait(1) {
            return Pumped::Failed(Some(raw));
        }
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::Path;

    use store_io_core::error::ErrorContext;
    use store_io_platform::IoOp;
    use store_io_sim::{SimConfig, SimPlatform};

    use crate::{Store, StoreOptions};

    /// A refused submission gives its row back at once, within the same
    /// hold of the lane (A1 C5): nothing is left in flight for a later
    /// holder to clear.
    #[test]
    fn test_a_refused_lane_submission_leaves_nothing_in_flight() {
        let p = SimPlatform::new(SimConfig::volatile(40));
        let s = Store::create(p.clone(), Path::new("/db"), StoreOptions::default()).unwrap();
        let mut lane = s.inner.queues.get(0);
        for _ in 0..3 {
            p.with_world(|w| {
                let next = w.write_submits() + 1;
                w.faults_mut().fail_submit = Some((next, 5));
            });
            let buf = s.take(4096, ErrorContext::default()).unwrap();
            let op = IoOp::Write {
                file: &s.inner.file,
                offset: 0,
                buf,
                dsync: false,
            };
            let refused = lane.submit(op, 7).unwrap_err();
            assert!(refused.buf.is_some() && refused.raw.is_some());
            assert_eq!(lane.in_flight(), 0, "the refused row was not given back");
        }
    }
}
