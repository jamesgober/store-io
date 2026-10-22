//! The observability counters' tests.

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

/// Puts one operation in flight on `lane` (a data flush the simulator
/// holds until the lane next waits): the lane is outstanding (A1 C6: a
/// lane is outstanding while an operation is in flight on it, not while
/// it is merely held).
fn occupy(lane: &mut crate::exec::Lane<store_io_sim::SimQueue>, s: &Store<SimPlatform>) {
    lane.submit(
        store_io_platform::IoOp::FlushData {
            file: &s.inner.file,
        },
        0,
    )
    .map_err(|_| ())
    .unwrap();
}

/// Completes the oldest operation in flight on `lane`.
fn finish_one(lane: &mut crate::exec::Lane<store_io_sim::SimQueue>) {
    assert!(lane.wait(1).unwrap() >= 1);
    lane.done.drain().for_each(drop);
}

/// The busy marks of the store's lanes that are odd.
fn busy(s: &Store<SimPlatform>) -> Vec<usize> {
    s.inner
        .queues
        .marks()
        .enumerate()
        .filter(|(_, m)| m % 2 == 1)
        .map(|(i, _)| i)
        .collect()
}

/// An operation in flight ages with the clock, from the first query
/// that saw it; its completion ends it; a new operation on the same lane
/// starts again from 0 (it is not mistaken for the old one). A lane that
/// is merely held is not outstanding.
#[test]
fn test_an_operation_in_flight_ages_from_the_first_query_that_sees_it() {
    let (p, s) = store();
    let mut held = s.inner.queues.get(0);
    assert_eq!(
        s.oldest_outstanding(),
        None,
        "a held, idle lane is outstanding"
    );
    occupy(&mut held, &s);
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
    finish_one(&mut held);
    assert_eq!(s.oldest_outstanding(), None);
    occupy(&mut held, &s);
    p.with_world(|w| w.advance_clock(1_000));
    assert_eq!(s.oldest_outstanding(), Some(first));
    finish_one(&mut held);
    drop(held);
}

/// The oldest of several operations in flight wins, and a flush leader
/// blocked in a device flush is reported as one.
#[test]
fn test_the_oldest_wins_and_a_flush_in_flight_is_reported_as_a_flush() {
    let (p, s) = store();
    let mut flush = s.inner.queues.flush_lane(0).unwrap();
    occupy(&mut flush, &s);
    assert!(s.oldest_outstanding().unwrap().flush);
    p.with_world(|w| w.advance_clock(3_000));
    let mut shared = s.inner.queues.get(0);
    occupy(&mut shared, &s);
    let o = s.oldest_outstanding().unwrap();
    assert!(o.flush, "the older flush lost to a newer operation");
    assert_eq!(o.age, Duration::from_secs(3));
    finish_one(&mut flush);
    let o = s.oldest_outstanding().unwrap();
    assert!(!o.flush);
    assert_eq!(o.age, Duration::ZERO);
    finish_one(&mut shared);
    assert_eq!(s.oldest_outstanding(), None);
    drop((flush, shared));
    // With four flush slots, each slot's lane is a flush lane (D-124).
    let o = StoreOptions {
        flushes_in_flight: 4,
        ..StoreOptions::default()
    };
    let s = Store::create(p.clone(), Path::new("/db4"), o).unwrap();
    for slot in [0, 3] {
        let mut flush = s.inner.queues.flush_lane(slot).unwrap();
        occupy(&mut flush, &s);
        assert!(s.oldest_outstanding().unwrap().flush, "slot {slot}");
        finish_one(&mut flush);
    }
}

/// A lane released and taken again between two queries holds a new
/// operation: its age starts from 0, not from the earlier one.
#[test]
fn test_a_lane_retaken_between_queries_is_a_new_operation() {
    let (p, s) = store();
    let mut first = s.inner.queues.get(0);
    occupy(&mut first, &s);
    assert_eq!(s.oldest_outstanding().unwrap().age, Duration::ZERO);
    p.with_world(|w| w.advance_clock(4_000));
    finish_one(&mut first);
    drop(first);
    let mut second = s.inner.queues.get(0);
    occupy(&mut second, &s);
    assert_eq!(s.oldest_outstanding().unwrap().age, Duration::ZERO);
    finish_one(&mut second);
}

/// First seen together, a flush is the one reported.
#[test]
fn test_a_tie_reports_the_flush() {
    let (_p, s) = store();
    let mut shared = s.inner.queues.get(0);
    let mut flush = s.inner.queues.flush_lane(0).unwrap();
    occupy(&mut shared, &s);
    occupy(&mut flush, &s);
    assert!(s.oldest_outstanding().unwrap().flush);
    finish_one(&mut shared);
    finish_one(&mut flush);
}

/// A blocking call's operation is outstanding while it is being
/// submitted (A1 C6): a queue that performs it inside `submit` (an inline
/// queue, D-104) must not hide a hung device.
#[test]
fn test_a_lane_operation_is_outstanding_while_it_is_being_submitted() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    let (p, s) = store();
    let pages = s.provision_page_region("pages", 1 << 20).unwrap();
    let (seen, idle) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    {
        let (s, seen, idle) = (s.clone(), Arc::clone(&seen), Arc::clone(&idle));
        p.with_world(|w| {
            w.set_submit_probe(Some(Arc::new(move || {
                let n = if s.oldest_outstanding().is_some() {
                    &seen
                } else {
                    &idle
                };
                n.fetch_add(1, Ordering::Relaxed);
            })));
        });
    }
    pages.write(pages.pos(0).unwrap(), &[7u8; 4096]).unwrap();
    p.with_world(|w| w.set_submit_probe(None));
    assert!(
        seen.load(Ordering::Relaxed) >= 1,
        "the write was never seen in flight"
    );
    assert_eq!(
        idle.load(Ordering::Relaxed),
        0,
        "an operation was submitted while not outstanding"
    );
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
/// an operation on one is outstanding like any lane's, in its own mark,
/// never the flush lane's.
#[test]
fn test_parallel_lanes_are_made_once_and_counted_when_busy() {
    let (p, s) = store();
    let calls = std::cell::Cell::new(0);
    let n = s.inner.queues.shared_len();
    let mut held = s
        .inner
        .queues
        .get_parallel(0, make_parallel(&p, &calls, true));
    assert_eq!(calls.get(), 1);
    occupy(&mut held, &s);
    assert_eq!(
        busy(&s),
        vec![n],
        "the busy parallel lane's mark is not the first parallel one"
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
    finish_one(&mut held);
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
    let mut lane = s
        .inner
        .queues
        .get_parallel(0, make_parallel(&p, &calls, false));
    occupy(&mut lane, &s);
    assert_eq!(
        busy(&s).first(),
        Some(&0),
        "the fallback is not a shared lane"
    );
    finish_one(&mut lane);
    drop(lane);
    let mut lane = s
        .inner
        .queues
        .get_parallel(0, make_parallel(&p, &calls, true));
    assert_eq!(calls.get(), 1, "a failed start was retried");
    occupy(&mut lane, &s);
    assert_eq!(
        busy(&s).first(),
        Some(&0),
        "a later call left the shared lanes"
    );
    finish_one(&mut lane);
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
    let n = s.inner.queues.shared_len();
    let mut held: Vec<_> = (0..n)
        .map(|i| {
            s.inner
                .queues
                .get_parallel(i, make_parallel(&p, &calls, true))
        })
        .collect();
    for lane in &mut held {
        occupy(lane, &s);
    }
    assert_eq!(busy(&s), (n..2 * n).collect::<Vec<_>>());
    let mut extra = s
        .inner
        .queues
        .get_parallel(0, make_parallel(&p, &calls, true));
    occupy(&mut extra, &s);
    assert!(busy(&s).iter().any(|&i| i < n), "no shared lane was taken");
    drop((extra, held));
}

/// The completion of an operation on a parallel lane moves its own mark
/// only: never a shared lane's, never the flush lane's.
#[test]
fn test_progress_on_a_parallel_lane_moves_only_its_own_mark() {
    let (p, s) = store();
    let calls = std::cell::Cell::new(0);
    let n = s.inner.queues.shared_len();
    let mut held = s
        .inner
        .queues
        .get_parallel(0, make_parallel(&p, &calls, true));
    occupy(&mut held, &s);
    occupy(&mut held, &s);
    let before: Vec<u64> = s.inner.queues.marks().collect();
    let _n = held.wait(1).unwrap();
    held.done.drain().for_each(drop);
    let after: Vec<u64> = s.inner.queues.marks().collect();
    for (i, (b, a)) in before.iter().zip(&after).enumerate() {
        if i == n {
            assert_ne!(a, b, "its own mark did not move");
        } else {
            assert_eq!(a, b, "a completion moved lane {i}'s mark");
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
    let mut lane = lane;
    occupy(&mut lane, &s);
    assert_eq!(busy(&s).first(), Some(&0));
    finish_one(&mut lane);
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
    while let Ok(b) = s.inner.pool.take_within(chunk, chunk) {
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

/// A large append (more pieces than buffers, so buffers are refilled as
/// earlier pieces complete) with completions out of order lands every
/// byte where it belongs.
#[test]
fn test_a_large_append_out_of_order_lands_every_byte() {
    for seed in 0..6 {
        let mut cfg = SimConfig::volatile(800 + seed);
        cfg.reorder = true;
        let p = SimPlatform::new(cfg);
        let s = Store::create(p, Path::new("/db"), StoreOptions::default()).unwrap();
        let wal = s.provision_append_region("wal", 32 << 20).unwrap();
        let data: Vec<u8> = (0..(12 << 20) + 777u32)
            .map(|i| (i % 251) as u8 ^ (i >> 20) as u8)
            .collect();
        let t = wal.append(&data).unwrap();
        let mut back = vec![0u8; data.len()];
        let n = wal.read(t.pos().offset(), &mut back).unwrap();
        assert_eq!(n, data.len());
        assert!(
            back == data,
            "seed {seed}: the append read back other bytes"
        );
        assert_eq!(s.oldest_outstanding(), None);
    }
}

/// A piece of a large append failing anywhere (the first, a middle or a
/// late completion):
/// `DurabilityUnknown`, the store poisoned, every buffer back in the
/// pool, nothing left outstanding.
#[test]
fn test_a_failed_piece_of_a_large_append_poisons_and_returns_buffers() {
    for k in [1u64, 4, 9] {
        let mut cfg = SimConfig::volatile(900 + k);
        cfg.reorder = true;
        let p = SimPlatform::new(cfg);
        let s = Store::create(p.clone(), Path::new("/db"), StoreOptions::default()).unwrap();
        let wal = s.provision_append_region("wal", 32 << 20).unwrap();
        let free = |s: &Store<SimPlatform>| {
            s.inner
                .pool
                .occupancy()
                .iter()
                .map(|&(_, f, _)| f)
                .sum::<usize>()
        };
        let before = free(&s);
        p.with_world(|w| {
            let next = w.writes() + k;
            w.faults_mut().fail_write = Some(next);
        });
        let r = wal.append(&vec![9u8; 12 << 20]);
        assert!(
            matches!(
                r,
                Err(store_io_core::error::Error::DurabilityUnknown { .. })
            ),
            "piece {k}: {r:?}"
        );
        assert!(s.poisoned().is_some());
        assert_eq!(free(&s), before, "piece {k}: buffers were not returned");
        assert_eq!(s.oldest_outstanding(), None);
    }
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
