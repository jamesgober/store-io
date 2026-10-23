//! The flush domain's tests (D-047, D-124).

use super::*;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc;
use std::time::Duration;

use store_io_core::error::OsErrorSource;

const EIO: OsError = OsError {
    code: 5,
    source: OsErrorSource::Sim,
};

const LONG: Duration = Duration::from_secs(10);

/// Waits until `n` followers are parked: a follower registers under
/// the slot lock and releases it only by waiting, so seeing the count
/// under the lock proves they are waiting.
fn until_waiting(d: &Domain, n: u32) {
    let start = std::time::Instant::now();
    while d.waiting() < n {
        assert!(start.elapsed() < LONG, "never registered");
        std::thread::yield_now();
    }
}

/// A follower waiting behind a leader is woken when leadership is given
/// back (the leader notifies only when a follower is registered), and a
/// lone barrier registers nobody.
#[test]
fn test_a_waiting_follower_is_woken_when_the_leader_returns() {
    let d = Arc::new(Domain::new());
    assert!(d.hold_leadership());
    let need = d.ticket();
    let (tx, rx) = mpsc::channel();
    let follower = {
        let d = Arc::clone(&d);
        std::thread::spawn(move || {
            let r = d.barrier(need, |_, _| Ok(()));
            let _sent = tx.send(r.is_ok());
        })
    };
    until_waiting(&d, 1);
    d.drop_leadership();
    assert_eq!(
        rx.recv_timeout(LONG),
        Ok(true),
        "the follower was never woken"
    );
    assert!(follower.join().is_ok(), "the follower panicked");
    assert_eq!(d.waiting(), 0);
    // A lone barrier leads and registers nobody.
    let need = d.ticket();
    assert!(d.barrier(need, |_, _| Ok(())).is_ok());
    assert_eq!(d.waiting(), 0);
}

/// A flush that panics poisons the domain and gives its slot back:
/// later barriers fail at once instead of waiting for a leader that is
/// gone.
#[test]
fn test_a_panicking_flush_poisons_and_releases_leadership() {
    let d = Domain::new();
    let need = d.ticket();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _r = d.barrier(need, |_, _| panic!("the flush panicked"));
    }));
    assert!(r.is_err());
    assert!(
        d.poisoned().is_some(),
        "a flush of unknown outcome did not poison"
    );
    let need = d.ticket();
    assert!(matches!(
        d.barrier(need, |_, _| Ok(())),
        Err(Error::Poisoned { .. })
    ));
}

#[test]
fn test_lone_barrier_issues_exactly_one_flush() {
    let d = Domain::new();
    let mut calls = 0;
    let need = d.ticket();
    assert!(
        d.barrier(need, |_, _| {
            calls += 1;
            Ok(())
        })
        .is_ok()
    );
    assert_eq!(calls, 1);
    // The same need is now satisfied without another flush.
    assert!(d.barrier(need, |_, _| Err(Some(EIO))).is_ok());
    assert_eq!(
        d.stats(),
        DomainStats {
            flushes: 1,
            joined: 1,
            led: 1,
            already_durable: 0,
            peak_in_flight: 1,
            out_of_order: 0,
        }
    );
}

#[test]
fn test_ticket_taken_after_a_flush_needs_the_next_flush() {
    let d = Domain::new();
    assert!(d.barrier(d.ticket(), |_, _| Ok(())).is_ok());
    let mut calls = 0;
    assert!(
        d.barrier(d.ticket(), |_, _| {
            calls += 1;
            Ok(())
        })
        .is_ok()
    );
    assert_eq!(calls, 1, "a write completing after flush 1 needs flush 2");
}

#[test]
fn test_failed_flush_poisons_and_is_never_retried() {
    let d = Domain::new();
    let r = d.barrier(d.ticket(), |_, _| Err(Some(EIO)));
    assert!(matches!(
        r,
        Err(Error::DurabilityUnknown {
            op: Op::FlushData,
            ..
        })
    ));
    let mut calls = 0;
    let r = d.barrier(d.ticket(), |_, _| {
        calls += 1;
        Ok(())
    });
    assert!(matches!(r, Err(Error::Poisoned { .. })));
    assert_eq!(calls, 0, "a poisoned domain must never flush again");
}

#[test]
fn test_satisfied_barrier_keeps_its_result_after_later_poison() {
    let d = Domain::new();
    let need = d.ticket();
    assert!(d.barrier(need, |_, _| Ok(())).is_ok());
    let _ = d.barrier(d.ticket(), |_, _| Err(Some(EIO)));
    assert!(
        d.barrier(need, |_, _| Ok(())).is_ok(),
        "already-durable data stays durable"
    );
}

#[test]
fn test_explicit_poison_wakes_and_fails_followers() {
    let d = Arc::new(Domain::new());
    d.poison(FirstCause {
        op: None,
        raw: None,
    });
    assert!(matches!(
        d.barrier(d.ticket(), |_, _| Ok(())),
        Err(Error::Poisoned { .. })
    ));
}

/// Two slots: flush 2 finishes first, but a barrier needing it is not
/// satisfied until flush 1 (issued earlier, still running) has
/// succeeded too.
#[test]
fn test_a_later_flush_finishing_first_waits_for_the_earlier_one() {
    let d = Arc::new(Domain::with_slots(2));
    let (go, wait_go) = mpsc::channel::<()>();
    let (started, on_start) = mpsc::channel::<usize>();
    let first = {
        let d = Arc::clone(&d);
        std::thread::spawn(move || {
            let need = d.ticket();
            d.barrier(need, |slot, _| {
                let _sent = started.send(slot);
                let _released = wait_go.recv_timeout(3 * LONG); // outlasts the test's waits
                Ok(())
            })
        })
    };
    assert_eq!(
        on_start.recv_timeout(LONG),
        Ok(1),
        "flush 1 on the highest slot"
    );
    // A write completed now needs flush 2, which leads on slot 0.
    let need = d.ticket();
    assert_eq!(need, 2);
    let (tx, rx) = mpsc::channel();
    let second = {
        let d = Arc::clone(&d);
        std::thread::spawn(move || {
            let mut slot_used = None;
            let r = d.barrier(need, |slot, _| {
                slot_used = Some(slot);
                Ok(())
            });
            let _sent = tx.send((r.is_ok(), slot_used));
        })
    };
    until_waiting(&d, 1);
    assert_eq!(d.done.load(Ordering::SeqCst), 0, "flush 1 is unfinished");
    assert!(rx.try_recv().is_err(), "satisfied before flush 1 finished");
    let _sent = go.send(());
    assert_eq!(rx.recv_timeout(LONG), Ok((true, Some(0))));
    assert!(matches!(first.join(), Ok(Ok(()))));
    assert!(second.join().is_ok());
    assert_eq!(d.done.load(Ordering::SeqCst), 2);
}

/// Two slots: flush 2 succeeds, then flush 1 (issued earlier) fails: the
/// barrier waiting on flush 2 is poisoned, never satisfied.
#[test]
fn test_an_earlier_flush_failing_poisons_a_later_success() {
    let d = Arc::new(Domain::with_slots(2));
    let (go, wait_go) = mpsc::channel::<()>();
    let (started, on_start) = mpsc::channel::<()>();
    let first = {
        let d = Arc::clone(&d);
        std::thread::spawn(move || {
            let need = d.ticket();
            d.barrier(need, |_, _| {
                let _sent = started.send(());
                let _released = wait_go.recv_timeout(3 * LONG); // outlasts the test's waits
                Err(Some(EIO))
            })
        })
    };
    assert_eq!(on_start.recv_timeout(LONG), Ok(()));
    let need = d.ticket();
    let second = {
        let d = Arc::clone(&d);
        std::thread::spawn(move || d.barrier(need, |_, _| Ok(())))
    };
    until_waiting(&d, 1);
    let _sent = go.send(());
    assert!(matches!(
        first.join(),
        Ok(Err(Error::DurabilityUnknown { .. }))
    ));
    assert!(matches!(second.join(), Ok(Err(Error::Poisoned { .. }))));
    assert_eq!(d.done.load(Ordering::SeqCst), 0);
}

/// A barrier that can flush only through the main handle waits for slot
/// 0, even with another slot free.
#[test]
fn test_a_main_handle_barrier_leads_only_on_slot_zero() {
    let d = Arc::new(Domain::with_slots(2));
    let (go, wait_go) = mpsc::channel::<()>();
    let (inside, on_inside) = mpsc::channel::<()>();
    // Slot 0 held (a full flush of the main handle); slot 1 free.
    let full = {
        let d = Arc::clone(&d);
        std::thread::spawn(move || {
            d.exclusive_main(|| {
                let _sent = inside.send(());
                let _released = wait_go.recv_timeout(3 * LONG); // outlasts the test's waits
            });
        })
    };
    assert_eq!(on_inside.recv_timeout(LONG), Ok(()));
    let need = d.ticket();
    let (tx, rx) = mpsc::channel();
    let main = {
        let d = Arc::clone(&d);
        std::thread::spawn(move || {
            let r = d.barrier_main(need, || Ok(()));
            let _sent = tx.send(r.is_ok());
        })
    };
    until_waiting(&d, 1);
    assert_eq!(d.stats().flushes, 0, "led on slot 1 while slot 0 was held");
    let _sent = go.send(());
    assert_eq!(rx.recv_timeout(LONG), Ok(true));
    assert!(full.join().is_ok());
    assert!(main.join().is_ok());
    assert_eq!(d.stats().flushes, 1);
}

/// Model: a "write" completes at a moment; a flush covers every write
/// completed before the flush started. Each thread checks that a flush
/// it waited for started after its write completed, and that every
/// flush up to it finished first, with one slot and with four.
#[test]
fn test_concurrent_barriers_share_flushes_and_never_cover_late_writes() {
    for slots in [1, 4] {
        let d = Arc::new(Domain::with_slots(slots));
        let clock = Arc::new(AtomicUsize::new(0));
        // (flush number, started at, finished at)
        let flushes = Arc::new(Mutex::new(Vec::<(u64, usize, usize)>::new()));
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let (d, clock, log) = (Arc::clone(&d), Arc::clone(&clock), Arc::clone(&flushes));
                std::thread::spawn(move || {
                    for _ in 0..200 {
                        let completed_at = clock.fetch_add(1, Ordering::SeqCst);
                        let need = d.ticket();
                        let r = d.barrier(need, |_, k| {
                            let started = clock.fetch_add(1, Ordering::SeqCst);
                            // A flush takes time, as a real one does, so
                            // the barriers overlap even on a loaded
                            // machine.
                            std::thread::sleep(Duration::from_micros(100));
                            let finished = clock.fetch_add(1, Ordering::SeqCst);
                            log.lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .push((k, started, finished));
                            Ok(())
                        });
                        assert!(r.is_ok());
                        let returned = clock.fetch_add(1, Ordering::SeqCst);
                        let log = log
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let Some(&(_, started, _)) = log.iter().find(|f| f.0 == need) else {
                            panic!("flush {need} never ran");
                        };
                        assert!(
                            started > completed_at,
                            "covering flush started before the write completed"
                        );
                        for n in 1..=need {
                            let f = log.iter().find(|f| f.0 == n);
                            assert!(
                                f.is_some_and(|f| f.2 < returned),
                                "flush {n} unfinished when barrier {need} returned"
                            );
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            assert!(h.join().is_ok());
        }
        let s = d.stats();
        assert_eq!(s.led as usize + s.joined as usize, 16 * 200);
        assert!(s.peak_in_flight <= slots as u64);
        assert!(
            slots == 1 || s.peak_in_flight > 1,
            "four slots never overlapped"
        );
        assert!(
            s.flushes < 16 * 200,
            "concurrent barriers must share flushes ({} flushes, {slots} slots)",
            s.flushes
        );
    }
}

/// A domain poisoned while a flush runs (a failed write elsewhere) keeps
/// that flush's success out: its barrier is poisoned, and `done` stays.
#[test]
fn test_a_flush_finishing_after_poison_satisfies_nothing() {
    let d = Domain::new();
    let need = d.ticket();
    let r = d.barrier(need, |_, _| {
        d.poison(FirstCause {
            op: Some(Op::Write),
            raw: None,
        });
        Ok(())
    });
    assert!(matches!(r, Err(Error::Poisoned { .. })), "{r:?}");
    assert_eq!(d.done.load(Ordering::SeqCst), 0);
}

/// Behind one unfinished flush, at most `WINDOW` flushes are issued: the
/// successes kept above `done` fit their mask, so no later flush can be
/// mistaken for the unfinished one. When it finishes, every barrier is
/// satisfied.
#[test]
fn test_flushes_behind_an_unfinished_one_stay_within_the_window() {
    let d = Arc::new(Domain::with_slots(2));
    let (go, wait_go) = mpsc::channel::<()>();
    let (started, on_start) = mpsc::channel::<()>();
    let first = {
        let d = Arc::clone(&d);
        std::thread::spawn(move || {
            let need = d.ticket();
            d.barrier(need, |_, _| {
                let _sent = started.send(());
                let _released = wait_go.recv_timeout(3 * LONG); // outlasts the test's waits
                Ok(())
            })
        })
    };
    assert_eq!(on_start.recv_timeout(LONG), Ok(()));
    let mut later = Vec::new();
    for i in 1..=70u64 {
        let d2 = Arc::clone(&d);
        later.push(std::thread::spawn(move || {
            let need = d2.ticket();
            (need, d2.barrier(need, |_, _| Ok(())))
        }));
        // Until it is parked: after its own flush, or behind the window.
        until_waiting(&d, i as u32);
    }
    // `issued - done` never reached past the window: flush 1 and 31 more.
    assert_eq!(d.stats().flushes, WINDOW, "issued past the window");
    // The next flush's barrier waits behind the window as the designate,
    // and is woken to lead it when flush 1 finishes.
    assert_eq!(d.lock().designate, Some(WINDOW + 1));
    let _sent = go.send(());
    assert!(matches!(first.join(), Ok(Ok(()))));
    for h in later {
        let (need, r) = h.join().unwrap_or((
            0,
            Err(Error::Poisoned {
                first: FirstCause {
                    op: None,
                    raw: None,
                },
            }),
        ));
        assert!(r.is_ok(), "barrier {need}: {r:?}");
        assert!(d.done.load(Ordering::SeqCst) >= need);
    }
}

/// A full flush of the main handle waits for slot 0 and holds it: a barrier
/// that needs a flush leads on another slot meanwhile, and slot 0 is free
/// again afterwards, even when the flush panics.
#[test]
fn test_exclusive_main_holds_slot_zero() {
    let d = Arc::new(Domain::with_slots(2));
    let (go, wait_go) = mpsc::channel::<()>();
    let (inside, on_inside) = mpsc::channel::<()>();
    let full = {
        let d = Arc::clone(&d);
        std::thread::spawn(move || {
            d.exclusive_main(|| {
                let _sent = inside.send(());
                let _released = wait_go.recv_timeout(3 * LONG); // outlasts the test's waits
            });
        })
    };
    assert_eq!(on_inside.recv_timeout(LONG), Ok(()));
    assert_eq!(d.lock().busy & 1, 1, "slot 0 not held");
    let mut slot_used = None;
    assert!(
        d.barrier(d.ticket(), |slot, _| {
            slot_used = Some(slot);
            Ok(())
        })
        .is_ok()
    );
    assert_eq!(slot_used, Some(1), "led on slot 0 while it was held");
    let _sent = go.send(());
    assert!(full.join().is_ok());
    // Checked without waiting, so slot 0 left held fails the test at once.
    assert_eq!(d.lock().busy & 1, 0, "slot 0 stayed held");
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        d.exclusive_main(|| panic!("the full flush panicked"));
    }));
    assert!(r.is_err());
    assert_eq!(d.lock().busy & 1, 0, "slot 0 stayed held after a panic");
    // On a thread, so slot 0 left held fails the test instead of hanging it.
    let (tx, rx) = mpsc::channel();
    let after = {
        let d = Arc::clone(&d);
        std::thread::spawn(move || {
            let _sent = tx.send(d.barrier_main(d.ticket(), || Ok(())).is_ok());
        })
    };
    assert_eq!(rx.recv_timeout(LONG), Ok(true), "slot 0 stayed held");
    assert!(after.join().is_ok());
}

/// A full flush of the main handle waits while a data leader flushes on
/// slot 0, and runs once it has finished.
#[test]
fn test_exclusive_main_waits_for_a_data_flush_on_slot_zero() {
    let d = Arc::new(Domain::with_slots(2));
    // Two data leaders, flushing at once: the first on slot 1, the second
    // on slot 0.
    let mut leaders = Vec::new();
    let mut gos = Vec::new();
    for expect in [1usize, 0] {
        let (go, wait_go) = mpsc::channel::<()>();
        let (started, on_start) = mpsc::channel::<usize>();
        let d2 = Arc::clone(&d);
        leaders.push(std::thread::spawn(move || {
            let need = d2.ticket();
            d2.barrier(need, |slot, _| {
                let _sent = started.send(slot);
                let _released = wait_go.recv_timeout(3 * LONG); // outlasts the test's waits
                Ok(())
            })
        }));
        assert_eq!(on_start.recv_timeout(LONG), Ok(expect));
        gos.push(go);
    }
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (full_done, on_full) = mpsc::channel();
    let full = {
        let (d, ran) = (Arc::clone(&d), Arc::clone(&ran));
        std::thread::spawn(move || {
            d.exclusive_main(|| ran.store(true, Ordering::SeqCst));
            let _sent = full_done.send(());
        })
    };
    until_waiting(&d, 1);
    assert!(!ran.load(Ordering::SeqCst), "ran beside the slot-0 flush");
    for go in gos {
        let _sent = go.send(());
    }
    for l in leaders {
        assert!(matches!(l.join(), Ok(Ok(()))));
    }
    let r = on_full.recv_timeout(LONG);
    if r.is_err() {
        // Wake it before failing, so the test never hangs.
        d.notify();
    }
    assert_eq!(r, Ok(()), "the full flush was never woken");
    assert!(full.join().is_ok());
    assert!(ran.load(Ordering::SeqCst));
}

/// On one slot, a full flush waiting for slot 0 has it before a leader that
/// arrives while it waits: with slot 0 free but the full flush not yet
/// woken, the new barrier leaves the slot to it instead of leading.
#[test]
fn test_a_waiting_full_flush_goes_before_new_leaders() {
    let d = Arc::new(Domain::new());
    let order = Arc::new(Mutex::new(Vec::<&str>::new()));
    let push = |order: &Mutex<Vec<&'static str>>, what| {
        order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(what);
    };
    assert!(d.hold_leadership());
    let (full_done, on_full) = mpsc::channel();
    let (go, wait_go) = mpsc::channel::<()>();
    let full = {
        let (d, order) = (Arc::clone(&d), Arc::clone(&order));
        std::thread::spawn(move || {
            d.exclusive_main(|| {
                push(&order, "full");
                let _released = wait_go.recv_timeout(3 * LONG); // outlasts the test's waits
            });
            let _sent = full_done.send(());
        })
    };
    until_waiting(&d, 1);
    // Slot 0 free again, nobody woken yet: the state the rule is for.
    d.lock().busy = 0;
    let (data_done, on_data) = mpsc::channel();
    let data = {
        let (d, order) = (Arc::clone(&d), Arc::clone(&order));
        std::thread::spawn(move || {
            let r = d.barrier(d.ticket(), |_, _| {
                push(&order, "data");
                Ok(())
            });
            let _sent = data_done.send(r.is_ok());
        })
    };
    // (A sleeper leaves only when a waker moved its word, so the full flush
    // cannot run before the data barrier parks: D-126.)
    until_waiting(&d, 2);
    assert_eq!(
        d.stats().flushes,
        0,
        "led on the slot a full flush waits for"
    );
    d.notify();
    // The full flush now holds slot 0; the data barrier parks behind it, so
    // releasing it always has exactly one waiter to wake.
    until_waiting(&d, 1);
    let _sent = go.send(());
    let (f, r) = (on_full.recv_timeout(LONG), on_data.recv_timeout(LONG));
    if f.is_err() || r.is_err() {
        // Free whatever is stuck before failing, so the test never hangs.
        d.poison(FirstCause {
            op: None,
            raw: None,
        });
        d.drop_leadership();
    }
    assert_eq!((f, r), (Ok(()), Ok(true)));
    assert!(full.join().is_ok());
    assert!(data.join().is_ok());
    let order = order
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(order, ["full", "data"]);
}
