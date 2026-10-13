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
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, TryLockError};
use store_io_core::observe::{Counter, Observer};

use store_io_core::error::OsError;
use store_io_platform::{CompletionBuf, IoBuf, IoOp, Queue, RawResult};

use crate::sync::{AtomicU64, Ordering};

/// A fixed set of queues shared by the blocking layers, plus an optional
/// flush lane.
///
/// The flush lane (D-070) is used only by the thread holding a flush
/// domain's leadership. Leadership is exclusive, so the lane is never
/// contended, and a leader never waits for one of the shared queues, which
/// followers may be holding while they wait for that very leader.
pub(crate) struct QueueSet<Q> {
    queues: Box<[Mutex<Lane<Q>>]>,
    flush: Option<Mutex<Lane<Q>>>,
    /// One busy mark per shared lane, then one for the flush lane (D-095),
    /// each alone on its cache line and shared with its lane: odd while the
    /// lane is held, advanced by 2 at each completion. Only the holder
    /// writes it; a watchdog reads it without the lock.
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
/// different threads never share one.
#[repr(align(128))]
pub(crate) struct LaneMark(AtomicU64);

/// A borrowed queue; returning it wakes one caller waiting for a queue.
///
/// Fields drop in declaration order: the queue's lock is released first,
/// then [`Notify`] wakes a waiter, which therefore always finds it free.
pub(crate) struct LaneGuard<'a, Q> {
    // Declared first, so the mark is back to even before the lock is
    // released: only the holder may write it.
    _mark: Mark<'a>,
    guard: MutexGuard<'a, Lane<Q>>,
    _notify: Notify<'a, Q>,
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
        // One mark per shared lane, then the flush lane's (which stays even
        // when there is no flush lane).
        let marks: Box<[Arc<LaneMark>]> = (0..=queues.len())
            .map(|_| Arc::new(LaneMark(AtomicU64::new(0))))
            .collect();
        let lane = |(queue, mark): (Q, &Arc<LaneMark>)| {
            Mutex::new(Lane {
                queue,
                done: CompletionBuf::with_capacity(depth.max(1)),
                mark: Arc::clone(mark),
            })
        };
        let n = queues.len();
        Self {
            queues: queues.into_iter().zip(marks.iter()).map(lane).collect(),
            flush: flush.map(|q| lane((q, &marks[n]))),
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
                _mark: Mark::take(&self.marks[self.queues.len()].0),
                guard,
            }
        })
    }

    /// Every lane's busy mark: the shared lanes in order, then the flush
    /// lane (odd while held; always even when the set has none).
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
                _notify: Notify { set: self },
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
            _notify: Notify { set: self },
        }
    }
}

/// Outcome of one operation: the buffer (if any) and the raw result.
pub(crate) struct Done {
    pub(crate) buf: Option<IoBuf>,
    pub(crate) result: RawResult<usize>,
}

/// Submits one operation and waits for its completion.
///
/// # Errors
///
/// `Err((buf, raw))` when the operation was refused before reaching the
/// device (media untouched); the buffer is returned.
pub(crate) fn run<Q: Queue>(
    slot: &mut Lane<Q>,
    op: IoOp<'_, Q::File>,
) -> Result<Done, (Option<IoBuf>, Option<OsError>)> {
    const TAG: u64 = 0x5110;
    slot.queue.submit(op, TAG).map_err(|r| (r.buf, r.raw))?;
    loop {
        // A borrow of the mark beside the drain of the completions.
        let mark = &slot.mark;
        for c in slot.done.drain() {
            if c.tag == TAG {
                mark.advance();
                return Ok(Done {
                    buf: c.buf,
                    result: c.result,
                });
            }
        }
        if let Err(raw) = slot.queue.wait(1, &mut slot.done) {
            // Waiting failed but the operation is in flight: its buffer is
            // still owned by the kernel and must not be released. The only
            // safe outcome is durability unknown; the caller poisons.
            return Ok(Done {
                buf: None,
                result: Err(raw),
            });
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
                    if let Some(buf) = r.buf {
                        held = Some((offset, buf));
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
