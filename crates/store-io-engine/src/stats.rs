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
    /// The oldest operation still in flight on the store's queues (shared,
    /// parallel and flush), or `None` when they are all idle. O(queues), no
    /// I/O; reads the platform's coarse clock once.
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

    /// Parallel queues for `s`, as the store makes them (D-096); counts calls.
    fn make_parallel<'a>(
        p: &'a SimPlatform,
        calls: &'a std::cell::Cell<usize>,
        ok: bool,
    ) -> impl FnOnce(usize) -> Option<Vec<store_io_sim::SimQueue>> + 'a {
        move |n| {
            calls.set(calls.get() + 1);
            ok.then(|| {
                let cfg = store_io_platform::QueueConfig {
                    parallel: true,
                    ..store_io_platform::QueueConfig::default()
                };
                (0..n)
                    .map(|_| store_io_platform::Platform::queue(p, cfg).unwrap())
                    .collect()
            })
        }
    }

    /// Parallel lanes are created once, by the first call that needs them;
    /// a held one is outstanding like any lane, in its own mark, never the
    /// flush lane's.
    #[test]
    fn test_parallel_lanes_are_made_once_and_counted_when_held() {
        let (p, s) = store();
        let calls = std::cell::Cell::new(0);
        let n = (s.inner.queues.marks().len() - 1) / 2;
        let held = s
            .inner
            .queues
            .get_parallel(0, make_parallel(&p, &calls, true));
        assert_eq!(calls.get(), 1);
        let busy: Vec<usize> = s
            .inner
            .queues
            .marks()
            .enumerate()
            .filter(|(_, m)| m % 2 == 1)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            busy,
            vec![n],
            "the held parallel lane's mark is not the first parallel one"
        );
        assert_eq!(
            s.oldest_outstanding(),
            Some(Outstanding {
                age: Duration::ZERO,
                flush: false,
            })
        );
        p.with_world(|w| w.advance_clock(2_000));
        assert_eq!(s.oldest_outstanding().unwrap().age, Duration::from_secs(2));
        drop(held);
        assert_eq!(s.oldest_outstanding(), None);
        let again = s
            .inner
            .queues
            .get_parallel(1, make_parallel(&p, &calls, true));
        assert_eq!(calls.get(), 1, "the parallel lanes were made twice");
        drop(again);
        // The simulator runs no pool.
        assert_eq!(s.submit_pool_threads(), None);
    }

    /// If the parallel queues cannot be made, the call gets a shared lane,
    /// and so does every later call, without trying again.
    #[test]
    fn test_a_failed_parallel_start_falls_back_to_a_shared_lane() {
        let (p, s) = store();
        let calls = std::cell::Cell::new(0);
        let lane = s
            .inner
            .queues
            .get_parallel(0, make_parallel(&p, &calls, false));
        let first_busy = s.inner.queues.marks().position(|m| m % 2 == 1);
        assert_eq!(first_busy, Some(0), "the fallback is not a shared lane");
        drop(lane);
        let lane = s
            .inner
            .queues
            .get_parallel(0, make_parallel(&p, &calls, true));
        assert_eq!(calls.get(), 1, "a failed start was retried");
        let first_busy = s.inner.queues.marks().position(|m| m % 2 == 1);
        assert_eq!(first_busy, Some(0), "a later call left the shared lanes");
        drop(lane);
        assert!(!s.inner.queues.has_parallel());
    }

    /// With every parallel lane busy, a call takes a shared lane instead of
    /// waiting for one: a thread holding a parallel lane never waits for
    /// itself.
    #[test]
    fn test_all_parallel_lanes_busy_falls_back_to_a_shared_lane() {
        // On a thread with a deadline: a regression that waits for a busy
        // parallel lane fails the test instead of hanging it.
        let (tx, rx) = std::sync::mpsc::channel();
        let t = std::thread::spawn(move || {
            all_busy_falls_back();
            tx.send(()).unwrap();
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(30)),
            Ok(()),
            "a call waited for a busy parallel lane"
        );
        t.join().unwrap();
    }

    fn all_busy_falls_back() {
        let (p, s) = store();
        let calls = std::cell::Cell::new(0);
        let n = (s.inner.queues.marks().len() - 1) / 2;
        let held: Vec<_> = (0..n)
            .map(|i| {
                s.inner
                    .queues
                    .get_parallel(i, make_parallel(&p, &calls, true))
            })
            .collect();
        let busy = |s: &Store<SimPlatform>| {
            s.inner
                .queues
                .marks()
                .enumerate()
                .filter(|(_, m)| m % 2 == 1)
                .map(|(i, _)| i)
                .collect::<Vec<_>>()
        };
        assert_eq!(busy(&s), (n..2 * n).collect::<Vec<_>>());
        let extra = s
            .inner
            .queues
            .get_parallel(0, make_parallel(&p, &calls, true));
        assert!(busy(&s).iter().any(|&i| i < n), "no shared lane was taken");
        drop((extra, held));
    }

    /// Progress on a held parallel lane advances its own mark only: never a
    /// shared lane's, never the flush lane's.
    #[test]
    fn test_progress_on_a_parallel_lane_moves_only_its_own_mark() {
        let (p, s) = store();
        let calls = std::cell::Cell::new(0);
        let n = (s.inner.queues.marks().len() - 1) / 2;
        let held = s
            .inner
            .queues
            .get_parallel(0, make_parallel(&p, &calls, true));
        let before: Vec<u64> = s.inner.queues.marks().collect();
        held.progress();
        let after: Vec<u64> = s.inner.queues.marks().collect();
        for (i, (b, a)) in before.iter().zip(&after).enumerate() {
            if i == n {
                assert_eq!(*a, b + 2, "its own mark did not advance");
            } else {
                assert_eq!(a, b, "progress moved lane {i}'s mark");
            }
        }
        drop(held);
    }

    /// Callers racing to the first, failing start: one of them tries it,
    /// the others queue behind it and then use the shared lanes without
    /// trying again.
    #[test]
    fn test_racing_callers_try_a_failing_start_once() {
        let (_p, s) = store();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let go = std::sync::Barrier::new(8);
        std::thread::scope(|sc| {
            for i in 0..8 {
                let (s, calls, go) = (&s, &calls, &go);
                let _worker = sc.spawn(move || {
                    let _ = go.wait();
                    let lane = s.inner.queues.get_parallel(i, |_| {
                        let _ = calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        std::thread::sleep(Duration::from_millis(50));
                        None
                    });
                    drop(lane);
                });
            }
        });
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "a failed start was tried again by a racing caller"
        );
    }

    /// A creation that returns the wrong number of queues is a failed start.
    #[test]
    fn test_a_wrong_count_of_parallel_queues_is_a_failed_start() {
        let (p, s) = store();
        let lane = s.inner.queues.get_parallel(0, |n| {
            Some(
                (0..n.saturating_sub(1))
                    .map(|_| store_io_platform::Platform::queue(&p, Default::default()).unwrap())
                    .collect(),
            )
        });
        assert_eq!(s.inner.queues.marks().position(|m| m % 2 == 1), Some(0));
        drop(lane);
        assert!(!s.inner.queues.has_parallel());
    }

    /// A scan of one buffer's range, or with only one buffer to pipeline,
    /// is a single read: it stays on a shared lane and starts nothing.
    #[test]
    fn test_a_scan_of_one_buffer_stays_on_a_shared_lane() {
        let (_p, s) = store();
        // Exactly one buffer's range: the boundary itself stays single.
        let small = s.provision_page_region("small", s.chunk() as u64).unwrap();
        let summary = small
            .scan(0, |_| core::ops::ControlFlow::Continue(()))
            .unwrap();
        assert_eq!(summary.unreadable, 0);
        assert!(
            !s.inner.queues.has_parallel(),
            "a one-buffer scan started the parallel lanes"
        );
        let big = s.provision_page_region("big", 8 << 20).unwrap();
        // Leave the pool one buffer of the scan's size: a pipeline of one.
        let chunk = s.chunk();
        let mut hold = Vec::new();
        while let Ok(b) = s.inner.pool.take(chunk) {
            hold.push(b);
        }
        drop(hold.pop());
        big.scan(0, |_| core::ops::ControlFlow::Continue(()))
            .unwrap();
        assert!(
            !s.inner.queues.has_parallel(),
            "a pipeline of one started the parallel lanes"
        );
        drop(hold);
        big.scan(0, |_| core::ops::ControlFlow::Continue(()))
            .unwrap();
        assert!(
            s.inner.queues.has_parallel(),
            "a large scan did not use a parallel lane"
        );
    }

    /// A scan's visitor that reads more than one buffer of the same store
    /// completes, with one or two queues: it never waits for the lane its
    /// own scan holds.
    #[test]
    fn test_a_large_read_inside_a_scan_visitor_completes() {
        for queues in [1, 2] {
            let p = SimPlatform::new(SimConfig::volatile(32));
            let opts = StoreOptions {
                queues,
                ..StoreOptions::default()
            };
            let s = Store::create(p, Path::new("/db"), opts).unwrap();
            let pages = s.provision_page_region("pages", 8 << 20).unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            let t = std::thread::spawn(move || {
                let mut nested = 0usize;
                let summary = pages
                    .scan(0, |item| {
                        if nested == 0 {
                            if let crate::ScanItem::Data { .. } = item {
                                let mut buf = vec![1u8; 3 << 20];
                                nested = pages.read(pages.pos(0).unwrap(), &mut buf).unwrap();
                            }
                        }
                        core::ops::ControlFlow::Continue(())
                    })
                    .unwrap();
                tx.send((summary.unreadable, nested)).unwrap();
            });
            let got = rx.recv_timeout(Duration::from_secs(30));
            assert_eq!(
                got,
                Ok((0, 3 << 20)),
                "queues: {queues}: the nested read waited"
            );
            t.join().unwrap();
        }
    }

    /// A batch of several writes on a parallel lane, completing out of order,
    /// with one write failing part-way: `DurabilityUnknown`, the store
    /// poisoned, every staged buffer back in the pool, nothing outstanding.
    #[test]
    fn test_a_failed_write_in_a_parallel_batch_poisons_and_returns_buffers() {
        let mut cfg = SimConfig::volatile(33);
        cfg.reorder = true;
        let p = SimPlatform::new(cfg);
        let s = Store::create(p.clone(), Path::new("/db"), StoreOptions::default()).unwrap();
        let pages = s.provision_page_region("pages", 8 << 20).unwrap();
        let bs = pages.block_size() as usize;
        let free = |s: &Store<SimPlatform>| {
            s.inner
                .pool
                .occupancy()
                .iter()
                .map(|&(_, f, _)| f)
                .sum::<usize>()
        };
        let before = free(&s);
        let mut batch = pages.batch();
        for i in 0..16u8 {
            batch
                .write(
                    pages.pos(u64::from(i) * 2 * bs as u64).unwrap(),
                    &vec![i; bs],
                )
                .unwrap();
        }
        p.with_world(|w| {
            let next = w.writes() + 5;
            w.faults_mut().fail_write = Some(next);
        });
        let err = batch.commit().unwrap_err();
        assert!(
            matches!(err, store_io_core::error::Error::DurabilityUnknown { .. }),
            "{err:?}"
        );
        assert!(s.poisoned().is_some());
        assert!(s.inner.queues.has_parallel());
        drop(batch);
        assert_eq!(free(&s), before, "staged buffers were not returned");
        assert_eq!(s.oldest_outstanding(), None);
    }

    /// A batch of several pages and a large read use the parallel lanes and
    /// leave every mark even.
    #[test]
    fn test_a_page_batch_and_a_large_read_leave_nothing_outstanding() {
        let (_p, s) = store();
        let pages = s.provision_page_region("pages", 8 << 20).unwrap();
        let bs = pages.block_size() as usize;
        let mut one = pages.batch();
        one.write(pages.pos(0).unwrap(), &vec![9; bs]).unwrap();
        one.commit().unwrap();
        assert!(
            !s.inner.queues.has_parallel(),
            "a one-write batch left the caller's thread"
        );
        let mut batch = pages.batch();
        for i in 0..32u8 {
            batch
                .write(
                    pages.pos(u64::from(i) * 2 * bs as u64).unwrap(),
                    &vec![i; bs],
                )
                .unwrap();
        }
        batch.commit().unwrap();
        assert!(
            s.inner.queues.has_parallel(),
            "a 32-write batch did not use a parallel lane"
        );
        let mut out = vec![0u8; 4 << 20];
        pages.read(pages.pos(0).unwrap(), &mut out).unwrap();
        for i in 0..32usize {
            assert!(
                out[i * 2 * bs..i * 2 * bs + bs]
                    .iter()
                    .all(|&b| b == i as u8)
            );
        }
        assert!(s.inner.queues.parallel_workers().is_none());
        assert_eq!(s.oldest_outstanding(), None);
        assert!(s.inner.queues.marks().all(|m| m % 2 == 0));
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
