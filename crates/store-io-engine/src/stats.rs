//! The oldest operation in flight (D-084, D-095), for a watchdog the caller
//! owns: store-io keeps no timer.
//!
//! The I/O path pays nothing but a lane's busy mark (odd while the lane is
//! held; see `QueueSet`). The clock is read here, by the query: it remembers
//! when it first saw each busy mark, so an operation still holding its lane
//! at a later query has been outstanding at least that long. The age is a
//! lower bound, within one query interval of the truth, and 0 the first time
//! an operation is seen; a watchdog polls and acts when the age passes its
//! deadline (for example by calling `Store::poison`).

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use store_io_platform::Platform;

use crate::store::Store;

/// The oldest operation in flight on the store's shared queues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outstanding {
    /// How long it has been outstanding, at least: measured from the first
    /// query that saw it (0 on that query).
    pub age: Duration,
    /// It is a device flush led by a barrier (rather than a write or read).
    pub flush: bool,
}

/// What earlier queries saw: per lane, the busy mark and the clock reading
/// when that mark was first seen.
#[derive(Debug, Default)]
pub(crate) struct Seen {
    lanes: Vec<(u64, u64)>,
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
    /// The oldest operation still in flight on the store's shared queues,
    /// or `None` when they are all idle. O(queues), no I/O; reads the
    /// platform's coarse clock once.
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
        let now = inner.platform.monotonic_ms();
        let flush_at = lanes.saturating_sub(1);
        let mut oldest: Option<(u64, bool)> = None;
        for (i, entry) in seen.lanes.iter_mut().enumerate() {
            if entry.0 % 2 == 0 {
                continue;
            }
            if entry.1 == NOT_SEEN {
                // A new operation (or progress on this lane): first seen now.
                entry.1 = now;
            }
            let flush = i == flush_at;
            // The oldest wins; on a tie the flush (a leader blocked in a
            // device flush is the more telling report).
            if oldest.is_none_or(|(first, was_flush)| {
                entry.1 < first || (entry.1 == first && flush && !was_flush)
            }) {
                oldest = Some((entry.1, flush));
            }
        }
        oldest.map(|(first, flush)| Outstanding {
            age: Duration::from_millis(now.saturating_sub(first)),
            flush,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, unused_results, unused_must_use)]

    use std::path::Path;
    use std::time::Duration;

    use store_io_sim::{SimConfig, SimPlatform};

    use super::Outstanding;
    use crate::store::{Store, StoreOptions};

    fn store() -> (SimPlatform, Store<SimPlatform>) {
        let p = SimPlatform::new(SimConfig::volatile(30));
        let s = Store::create(p.clone(), Path::new("/db"), StoreOptions::default()).unwrap();
        (p, s)
    }

    #[test]
    fn test_an_idle_store_has_nothing_outstanding() {
        let (_p, s) = store();
        assert_eq!(s.oldest_outstanding(), None);
    }

    /// The age of a held lane grows with the clock, from the first query
    /// that saw it; releasing ends it; a new operation on the same lane
    /// starts again from 0 (it is not mistaken for the old one).
    #[test]
    fn test_a_held_queue_ages_from_the_first_query_that_sees_it() {
        let (p, s) = store();
        let held = s.inner.queues.get(0);
        let first = Outstanding {
            age: Duration::ZERO,
            flush: false,
        };
        assert_eq!(s.oldest_outstanding(), Some(first));
        p.with_world(|w| w.advance_clock(5_000));
        assert_eq!(
            s.oldest_outstanding(),
            Some(Outstanding {
                age: Duration::from_secs(5),
                flush: false,
            })
        );
        drop(held);
        assert_eq!(s.oldest_outstanding(), None);
        let again = s.inner.queues.get(0);
        p.with_world(|w| w.advance_clock(1_000));
        assert_eq!(s.oldest_outstanding(), Some(first));
        drop(again);
    }

    /// The oldest of several held queues wins, and a flush leader blocked
    /// in a device flush is reported as one.
    #[test]
    fn test_the_oldest_wins_and_a_held_flush_is_reported_as_a_flush() {
        let (p, s) = store();
        let flush = s.inner.queues.flush_lane().unwrap();
        assert!(s.oldest_outstanding().unwrap().flush);
        p.with_world(|w| w.advance_clock(3_000));
        let shared = s.inner.queues.get(0);
        let o = s.oldest_outstanding().unwrap();
        assert!(o.flush, "the older flush lost to a newer queue");
        assert_eq!(o.age, Duration::from_secs(3));
        drop(flush);
        let o = s.oldest_outstanding().unwrap();
        assert!(!o.flush);
        assert_eq!(o.age, Duration::ZERO);
        drop(shared);
        assert_eq!(s.oldest_outstanding(), None);
    }

    /// A lane released and taken again between two queries holds a new
    /// operation: its age starts from 0, not from the earlier one.
    #[test]
    fn test_a_lane_retaken_between_queries_is_a_new_operation() {
        let (p, s) = store();
        let first = s.inner.queues.get(0);
        assert_eq!(s.oldest_outstanding().unwrap().age, Duration::ZERO);
        p.with_world(|w| w.advance_clock(4_000));
        drop(first);
        let second = s.inner.queues.get(0);
        assert_eq!(s.oldest_outstanding().unwrap().age, Duration::ZERO);
        drop(second);
    }

    /// Progress on a held lane (a completion) restarts its age: a long
    /// provisioning that keeps completing writes never looks hung.
    #[test]
    fn test_progress_on_a_held_lane_restarts_its_age() {
        let (p, s) = store();
        let held = s.inner.queues.get(0);
        assert_eq!(s.oldest_outstanding().unwrap().age, Duration::ZERO);
        p.with_world(|w| w.advance_clock(5_000));
        assert_eq!(s.oldest_outstanding().unwrap().age, Duration::from_secs(5));
        held.progress();
        assert_eq!(s.oldest_outstanding().unwrap().age, Duration::ZERO);
        p.with_world(|w| w.advance_clock(2_000));
        assert_eq!(s.oldest_outstanding().unwrap().age, Duration::from_secs(2));
        drop(held);
    }

    /// First seen together, a flush is the one reported.
    #[test]
    fn test_a_tie_reports_the_flush() {
        let (_p, s) = store();
        let shared = s.inner.queues.get(0);
        let flush = s.inner.queues.flush_lane().unwrap();
        assert!(s.oldest_outstanding().unwrap().flush);
        drop((shared, flush));
    }

    /// Ordinary work leaves every mark even (idle) when it is done.
    #[test]
    fn test_real_operations_leave_nothing_outstanding() {
        let (_p, s) = store();
        let wal = s.provision_append_region("wal", 1 << 20).unwrap();
        for i in 0..20u8 {
            wal.append_durable(&[i; 64]).unwrap();
        }
        let mut out = vec![0u8; 4096];
        wal.read(0, &mut out).unwrap();
        assert_eq!(s.oldest_outstanding(), None);
        assert!(s.inner.queues.marks().all(|m| m % 2 == 0));
    }
}
