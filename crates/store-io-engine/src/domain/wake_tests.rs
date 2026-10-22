//! Who a finished flush wakes (D-125): every barrier it covered, passed on
//! one to the next, and one barrier needing the next flush, which may lead
//! on the slot freed; a barrier whose bucket held another need, or that
//! leads only on slot 0, is woken from the shared condvar; poison wakes
//! every bucket. Tests build with two buckets, so needs one apart use
//! different buckets and needs two apart share one. No test joins a thread:
//! outcomes come over channels with a timeout, so a stranded barrier fails
//! its test instead of hanging the run.

use super::*;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

const LONG: Duration = Duration::from_secs(10);

fn until_waiting(d: &Domain, n: u32) {
    let start = std::time::Instant::now();
    while d.waiting() < n {
        assert!(start.elapsed() < LONG, "never registered");
        std::thread::yield_now();
    }
}

fn until_issued(d: &Domain, n: u64) {
    let start = std::time::Instant::now();
    while d.issued.load(Ordering::SeqCst) < n {
        assert!(start.elapsed() < LONG, "never issued");
        std::thread::yield_now();
    }
}

/// A leader for `need` whose flush runs until `go` is sent; its outcome
/// arrives on the returned channel (never joined: a stranded thread must
/// fail the test, not hang it).
fn held_leader(d: &Arc<Domain>, need: u64) -> (mpsc::Sender<()>, mpsc::Receiver<bool>) {
    let (go, wait_go) = mpsc::channel::<()>();
    let (tx, rx) = mpsc::channel();
    let d = Arc::clone(d);
    let _h = std::thread::spawn(move || {
        let r = d.barrier(need, |_, _| {
            let _released = wait_go.recv_timeout(LONG);
            Ok(())
        });
        let _sent = tx.send(r.is_ok());
    });
    (go, rx)
}

/// A barrier for `need` on another thread; its outcome arrives on the
/// returned channel.
fn barrier(d: &Arc<Domain>, need: u64) -> mpsc::Receiver<bool> {
    let (tx, rx) = mpsc::channel();
    let d = Arc::clone(d);
    let _h = std::thread::spawn(move || {
        let _sent = tx.send(d.barrier(need, |_, _| Ok(())).is_ok());
    });
    rx
}

/// Three barriers covered by one flush in flight sleep in one bucket; the
/// leader wakes one and each wakes the next, so all three return.
#[test]
fn test_every_barrier_a_flush_covered_is_woken() {
    let d = Arc::new(Domain::new());
    let need = d.ticket();
    let (go, leader) = held_leader(&d, need);
    until_issued(&d, 1);
    let followers: Vec<_> = (0..3).map(|_| barrier(&d, need)).collect();
    until_waiting(&d, 3);
    let _sent = go.send(());
    for f in &followers {
        assert_eq!(f.recv_timeout(LONG), Ok(true), "a covered barrier slept on");
    }
    assert_eq!(leader.recv_timeout(LONG), Ok(true));
    assert_eq!(d.waiting(), 0);
}

/// Two slots: flush 2 finishes before flush 1. When flush 1 finishes,
/// `done` moves past both, and the barriers of both needs are woken.
#[test]
fn test_a_jump_past_several_flushes_wakes_each_bucket() {
    let d = Arc::new(Domain::with_slots(2));
    let first = d.ticket();
    let (go_first, l1) = held_leader(&d, first);
    until_issued(&d, 1);
    let second = d.ticket();
    let (go_second, l2) = held_leader(&d, second);
    until_issued(&d, 2);
    let f1 = barrier(&d, first);
    let f2 = barrier(&d, second);
    until_waiting(&d, 2);
    let _sent = go_second.send(());
    // Flush 2 succeeded, but flush 1 is unfinished: nothing is covered, and
    // the second leader waits too.
    until_waiting(&d, 3);
    assert_eq!(d.done.load(Ordering::SeqCst), 0);
    let _sent = go_first.send(());
    assert_eq!(f1.recv_timeout(LONG), Ok(true), "need 1 slept on");
    assert_eq!(f2.recv_timeout(LONG), Ok(true), "need 2 slept on");
    assert_eq!(l1.recv_timeout(LONG), Ok(true));
    assert_eq!(l2.recv_timeout(LONG), Ok(true));
    assert_eq!(d.done.load(Ordering::SeqCst), 2);
}

/// One slot: a barrier needing the next flush sleeps while the slot is
/// busy, and the leader, finishing, wakes it to lead that flush.
#[test]
fn test_a_barrier_needing_the_next_flush_is_woken_to_lead_it() {
    let d = Arc::new(Domain::new());
    let first = d.ticket();
    let (go, leader) = held_leader(&d, first);
    until_issued(&d, 1);
    let next = d.ticket();
    assert_eq!(next, 2);
    let f = barrier(&d, next);
    until_waiting(&d, 1);
    let _sent = go.send(());
    assert_eq!(f.recv_timeout(LONG), Ok(true), "the next flush never led");
    assert_eq!(leader.recv_timeout(LONG), Ok(true));
    assert_eq!(d.done.load(Ordering::SeqCst), 2);
}

/// A barrier whose bucket holds another need sleeps on the shared condvar
/// and is woken when its flush finishes, not when the other need's is.
/// Two slots: flushes 1 and 2 run; a barrier needing flush 3 sleeps first
/// (bucket 1); one needing flush 1, two apart (bucket 1 too), sleeps after
/// it and must return as soon as flush 1 finishes, with flush 2 still
/// running.
#[test]
fn test_a_barrier_whose_bucket_holds_another_need_is_still_woken() {
    let d = Arc::new(Domain::with_slots(2));
    let first = d.ticket();
    let (go_first, l1) = held_leader(&d, first);
    until_issued(&d, 1);
    let (go_second, l2) = held_leader(&d, d.ticket());
    until_issued(&d, 2);
    let third = barrier(&d, d.ticket());
    until_waiting(&d, 1);
    let late = barrier(&d, first);
    until_waiting(&d, 2);
    assert_eq!(d.lock().overflow, 1, "the late barrier shares no bucket");
    let _sent = go_first.send(());
    assert_eq!(
        late.recv_timeout(LONG),
        Ok(true),
        "need 1 slept past its flush"
    );
    let _sent = go_second.send(());
    assert_eq!(third.recv_timeout(LONG), Ok(true));
    assert_eq!(l1.recv_timeout(LONG), Ok(true));
    assert_eq!(l2.recv_timeout(LONG), Ok(true));
}

/// Two slots: a barrier that can lead only on slot 0 and a data barrier,
/// both needing the next flush, sleep while both slots are busy. When the
/// flush on slot 1 finishes, the data barrier leads on it at once, though
/// slot 0 is still busy and the slot-0 barrier slept first.
#[test]
fn test_the_next_flush_is_led_by_a_barrier_that_can_use_the_freed_slot() {
    let d = Arc::new(Domain::with_slots(2));
    let (go_first, l1) = held_leader(&d, d.ticket());
    until_issued(&d, 1);
    let (go_second, l2) = held_leader(&d, d.ticket());
    until_issued(&d, 2);
    let need = d.ticket();
    let (tx, main) = mpsc::channel();
    {
        let d = Arc::clone(&d);
        let _h = std::thread::spawn(move || {
            let _sent = tx.send(d.barrier_main(need, || Ok(())).is_ok());
        });
    }
    until_waiting(&d, 1);
    let data = barrier(&d, need);
    until_waiting(&d, 2);
    let _sent = go_first.send(());
    // Flush 3 is issued on slot 1 while flush 2 still holds slot 0.
    until_issued(&d, 3);
    let _sent = go_second.send(());
    assert_eq!(data.recv_timeout(LONG), Ok(true));
    assert_eq!(main.recv_timeout(LONG), Ok(true));
    assert_eq!(l1.recv_timeout(LONG), Ok(true));
    assert_eq!(l2.recv_timeout(LONG), Ok(true));
}

/// Poison wakes the barriers asleep in a bucket, and they fail.
#[test]
fn test_poison_wakes_barriers_asleep_in_a_bucket() {
    let d = Arc::new(Domain::new());
    let need = d.ticket();
    let (go, leader) = held_leader(&d, need);
    until_issued(&d, 1);
    let followers: Vec<_> = (0..2).map(|_| barrier(&d, need)).collect();
    until_waiting(&d, 2);
    d.poison(FirstCause {
        op: None,
        raw: None,
    });
    for f in &followers {
        assert_eq!(
            f.recv_timeout(LONG),
            Ok(false),
            "a poisoned barrier slept on"
        );
    }
    let _sent = go.send(());
    assert!(leader.recv_timeout(LONG).is_ok());
}
