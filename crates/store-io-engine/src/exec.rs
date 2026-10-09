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
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError, TryLockError};

use store_io_core::error::OsError;
use store_io_platform::{CompletionBuf, IoBuf, IoOp, Queue, RawResult};

use crate::sync::{AtomicU64, Ordering};

/// A fixed set of queues shared by the blocking layers.
pub(crate) struct QueueSet<Q> {
    queues: Box<[Mutex<Lane<Q>>]>,
    waiters: AtomicU64,
    wake: Mutex<()>,
    returned: Condvar,
}

/// A queue and its completion buffer.
pub(crate) struct Lane<Q> {
    pub(crate) queue: Q,
    pub(crate) done: CompletionBuf,
}

/// A borrowed queue; returning it wakes one caller waiting for a queue.
///
/// Fields drop in declaration order: the queue's lock is released first,
/// then [`Notify`] wakes a waiter, which therefore always finds it free.
pub(crate) struct LaneGuard<'a, Q> {
    guard: MutexGuard<'a, Lane<Q>>,
    _notify: Notify<'a, Q>,
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
    /// Wraps already-created queues.
    pub(crate) fn new(queues: Vec<Q>, depth: usize) -> Self {
        Self {
            queues: queues
                .into_iter()
                .map(|queue| {
                    Mutex::new(Lane {
                        queue,
                        done: CompletionBuf::with_capacity(depth.max(1)),
                    })
                })
                .collect(),
            waiters: AtomicU64::new(0),
            wake: Mutex::new(()),
            returned: Condvar::new(),
        }
    }

    fn try_any(&self, hint: usize) -> Option<MutexGuard<'_, Lane<Q>>> {
        let n = self.queues.len();
        (0..n).find_map(|i| match self.queues[(hint + i) % n].try_lock() {
            Ok(g) => Some(g),
            Err(TryLockError::Poisoned(p)) => Some(p.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        })
    }

    /// Borrows a free queue, starting the search at `hint`; when all are
    /// busy, waits for the first one returned.
    pub(crate) fn get(&self, hint: usize) -> LaneGuard<'_, Q> {
        if let Some(guard) = self.try_any(hint) {
            return LaneGuard {
                guard,
                _notify: Notify { set: self },
            };
        }
        let _registered = self.waiters.fetch_add(1, Ordering::AcqRel);
        let mut wake = self.wake.lock().unwrap_or_else(PoisonError::into_inner);
        let guard = loop {
            if let Some(guard) = self.try_any(hint) {
                break guard;
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
        for c in slot.done.drain() {
            if c.tag == TAG {
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
