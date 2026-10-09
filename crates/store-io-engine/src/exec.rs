//! Running operations on platform queues from the blocking layers.
//!
//! The simple and batch layers block their caller until the I/O is done. Each
//! call borrows one queue from a small per-store set with a non-blocking
//! `try_lock` (one atomic compare-and-swap when uncontended; a queue is held
//! only for the duration of one call's I/O). Only when every queue is busy
//! does a caller wait for one, which happens only when more threads than
//! queues are inside I/O at the same moment.

use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};

use store_io_core::error::OsError;
use store_io_platform::{CompletionBuf, IoBuf, IoOp, Queue, RawResult};

/// A fixed set of queues shared by the blocking layers.
pub(crate) struct QueueSet<Q> {
    queues: Box<[Mutex<Lane<Q>>]>,
}

/// A queue and its completion buffer.
pub(crate) struct Lane<Q> {
    pub(crate) queue: Q,
    pub(crate) done: CompletionBuf,
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
        }
    }

    /// Borrows a free queue, waiting only if all are busy.
    pub(crate) fn get(&self, hint: usize) -> MutexGuard<'_, Lane<Q>> {
        let n = self.queues.len();
        for i in 0..n {
            match self.queues[(hint + i) % n].try_lock() {
                Ok(g) => return g,
                Err(TryLockError::Poisoned(p)) => return p.into_inner(),
                Err(TryLockError::WouldBlock) => {}
            }
        }
        self.queues[hint % n]
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
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
