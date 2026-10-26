//! The oldest operation in flight (D-084, D-095), for a watchdog the caller
//! owns: store-io keeps no timer.
//!
//! The I/O path pays nothing but a lane's busy mark (odd while an operation
//! is in flight on the lane, naming the oldest; see `QueueSet`). The clock
//! is read here, by the query: it remembers when it first saw each busy
//! mark, so an operation still the lane's oldest at a later query has been
//! outstanding at least that long. The age is a
//! lower bound, within one query interval of the truth, and 0 the first time
//! an operation is seen; a watchdog polls and acts when the age passes its
//! deadline (for example by calling `Store::poison`).

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use store_io_platform::Platform;

use crate::exec::LaneMark;
use crate::store::Store;

/// The oldest operation in flight on the store's queues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outstanding {
    /// How long it has been outstanding, at least: measured from the first
    /// query that saw it (0 on that query).
    pub age: Duration,
    /// It is a device flush led by a barrier (rather than a write or read).
    pub flush: bool,
}

/// What earlier queries saw: per lane, the busy mark and the clock reading
/// when that mark was first seen; per busy submitter, its id too.
#[derive(Debug, Default)]
pub(crate) struct Seen {
    lanes: Vec<(u64, u64)>,
    submitters: Vec<(u64, u64, u64)>,
}

/// The busy marks of the store's live `Submitter`s (D-098), by id. Locked
/// only when one is created or dropped, and by queries (after `Seen`).
#[derive(Default)]
pub(crate) struct Submitters {
    next: u64,
    marks: Vec<(u64, Arc<LaneMark>)>,
}

fn lock_submitters(s: &Mutex<Submitters>) -> std::sync::MutexGuard<'_, Submitters> {
    s.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A lane mark not yet given a first-seen time.
const NOT_SEEN: u64 = u64::MAX;

/// Locks the table; it holds no invariant a panic could break.
pub(crate) fn lock_seen(s: &Mutex<Seen>) -> std::sync::MutexGuard<'_, Seen> {
    s.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<P: Platform> Store<P>
where
    P::Queue: Send,
{
    /// The oldest operation still in flight on the store's queues (shared,
    /// parallel and flush) and its submitters, or `None` when they are all
    /// idle. O(queues + submitters), no I/O; reads the platform's coarse
    /// clock once. A submitter's operation counts until its completion is
    /// processed by `reap` or `wait`.
    ///
    /// The age counts from the first call that saw the operation, so a
    /// watchdog calling this every second learns of a hung device within a
    /// second of its deadline.
    #[must_use]
    pub fn oldest_outstanding(&self) -> Option<Outstanding> {
        let inner = &self.inner;
        let mut seen = lock_seen(&inner.seen);
        let lanes = inner.queues.marks().len();
        if seen.lanes.len() != lanes {
            seen.lanes = vec![(0, 0); lanes];
        }
        // Marks first, then the clock: an operation that starts after the
        // marks were read is not dated before it began (the age stays a
        // lower bound).
        for (mark, entry) in inner.queues.marks().zip(seen.lanes.iter_mut()) {
            // Idle, or a different operation than last time: not yet seen.
            if mark % 2 == 0 || entry.0 != mark {
                *entry = (mark, NOT_SEEN);
            }
        }
        let busy: Vec<(u64, u64)> = lock_submitters(&inner.submitters)
            .marks
            .iter()
            .map(|(id, m)| (*id, m.get()))
            .filter(|(_, mark)| mark % 2 == 1)
            .collect();
        let now = inner.platform.monotonic_ms();
        // The flush lanes follow the shared, parallel and metadata lanes
        // (D-124, D-135).
        let first_flush = inner.queues.first_flush_mark();
        let mut oldest: Option<(u64, bool)> = None;
        for (i, entry) in seen.lanes.iter_mut().enumerate() {
            if entry.0 % 2 == 0 {
                continue;
            }
            if entry.1 == NOT_SEEN {
                // A new oldest operation on this lane: first seen now.
                entry.1 = now;
            }
            let flush = i >= first_flush;
            // The oldest wins; on a tie the flush (a leader blocked in a
            // device flush is the more telling report).
            if oldest.is_none_or(|(first, was_flush)| {
                entry.1 < first || (entry.1 == first && flush && !was_flush)
            }) {
                oldest = Some((entry.1, flush));
            }
        }
        // A submitter's mark names its oldest operation in flight: the same
        // mark as last time is the same operation, still outstanding.
        let before = std::mem::take(&mut seen.submitters);
        for (id, mark) in busy {
            let first = before
                .iter()
                .find(|e| e.0 == id && e.1 == mark)
                .map_or(now, |e| e.2);
            seen.submitters.push((id, mark, first));
            if oldest.is_none_or(|(at, _)| first < at) {
                oldest = Some((first, false));
            }
        }
        oldest.map(|(first, flush)| Outstanding {
            age: Duration::from_millis(now.saturating_sub(first)),
            flush,
        })
    }
}

impl<P: Platform> Store<P>
where
    P::Queue: Send,
{
    /// Registers a submitter's busy mark; returns its id.
    pub(crate) fn register_submitter(&self, mark: &Arc<LaneMark>) -> u64 {
        let mut s = lock_submitters(&self.inner.submitters);
        let id = s.next;
        s.next += 1;
        s.marks.push((id, Arc::clone(mark)));
        id
    }

    pub(crate) fn unregister_submitter(&self, id: u64) {
        lock_submitters(&self.inner.submitters)
            .marks
            .retain(|(i, _)| *i != id);
    }
}

#[cfg(test)]
mod tests;
