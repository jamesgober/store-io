//! Who a finished flush wakes (D-125, D-126): every barrier it covered, all
//! at once, and the designate, which may lead the next flush on the slot
//! freed; a barrier whose bucket held another need, or that leads only on
//! slot 0, is woken from the shared word; poison wakes every bucket. Tests build with two buckets, so needs one apart use
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
            // Longer than any wait of the test: a flush released by its
            // own timeout must never rescue a stranded barrier in time
            // (it made a mutant survive at random).
            let _released = wait_go.recv_timeout(3 * LONG);
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
/// leader wakes them all with one wake, so all three return.
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

/// A barrier whose bucket holds another need sleeps on the shared word
/// and is woken when its flush finishes. Three slots: flushes 1 to 3 run; a
/// barrier needing flush 3 sleeps first (bucket 1); one needing flush 1,
/// two apart (bucket 1 too), sleeps after it and must return as soon as
/// flush 1 finishes, with flushes 2 and 3 still running.
#[test]
fn test_a_barrier_whose_bucket_holds_another_need_is_still_woken() {
    let d = Arc::new(Domain::with_slots(3));
    let first = d.ticket();
    let (go_first, l1) = held_leader(&d, first);
    until_issued(&d, 1);
    let (go_second, l2) = held_leader(&d, d.ticket());
    until_issued(&d, 2);
    let third_need = d.ticket();
    let (go_third, l3) = held_leader(&d, third_need);
    until_issued(&d, 3);
    let third = barrier(&d, third_need);
    until_waiting(&d, 1);
    let late = barrier(&d, first);
    until_waiting(&d, 2);
    assert_eq!(d.lock().shared, 1, "the late barrier shares no bucket");
    let _sent = go_first.send(());
    assert_eq!(
        late.recv_timeout(LONG),
        Ok(true),
        "need 1 slept past its flush"
    );
    let _sent = go_second.send(());
    let _sent = go_third.send(());
    assert_eq!(third.recv_timeout(LONG), Ok(true));
    assert_eq!(l1.recv_timeout(LONG), Ok(true));
    assert_eq!(l2.recv_timeout(LONG), Ok(true));
    assert_eq!(l3.recv_timeout(LONG), Ok(true));
}

/// One slot: of several barriers needing the next flush, the first sleeps
/// as the designate and the rest in that flush's bucket; when the slot is
/// freed only the designate is woken, leads, and its flush wakes the rest.
#[test]
fn test_the_designate_leads_the_next_flush_for_its_bucket() {
    let d = Arc::new(Domain::new());
    let (go, leader) = held_leader(&d, d.ticket());
    until_issued(&d, 1);
    let next = d.ticket();
    let first = barrier(&d, next);
    until_waiting(&d, 1);
    assert_eq!(d.lock().designate, Some(next));
    let rest: Vec<_> = (0..3).map(|_| barrier(&d, next)).collect();
    until_waiting(&d, 4);
    let b = (next % BUCKETS as u64) as usize;
    assert_eq!(
        d.lock().buckets[b].waiting,
        3,
        "the rest sleep in the bucket"
    );
    let _sent = go.send(());
    assert_eq!(first.recv_timeout(LONG), Ok(true));
    for r in &rest {
        assert_eq!(r.recv_timeout(LONG), Ok(true), "a covered barrier slept on");
    }
    assert_eq!(leader.recv_timeout(LONG), Ok(true));
    assert_eq!(d.stats().flushes, 2, "the next flush was led once");
    assert_eq!(d.waiting(), 0);
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

/// A wake of a bucket's word that deregistered nobody (as a spurious
/// return of the OS's wait would be) leaves its barrier asleep: it does
/// not leave its bed, so it never registers twice.
#[test]
fn test_a_wake_that_deregisters_nobody_leaves_the_barrier_asleep() {
    let d = Arc::new(Domain::new());
    let need = d.ticket();
    let (go, leader) = held_leader(&d, need);
    until_issued(&d, 1);
    let f = barrier(&d, need);
    until_waiting(&d, 1);
    // Long enough to be inside the OS's wait.
    std::thread::sleep(Duration::from_millis(20));
    d.bucket_words[(need % BUCKETS as u64) as usize].wake_all();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(d.waiting(), 1, "a barrier left its bed unregistered");
    assert_eq!(f.try_recv(), Err(mpsc::TryRecvError::Empty));
    let _sent = go.send(());
    assert_eq!(f.recv_timeout(LONG), Ok(true));
    assert_eq!(leader.recv_timeout(LONG), Ok(true));
}

/// Three slots, flushes finishing 3, 2, then 1: `done` jumps from 0 to 3,
/// more needs than there are buckets (2 in tests), and every covered need
/// is woken, the one whose bucket held another need included.
#[test]
fn test_a_jump_past_more_needs_than_buckets_wakes_every_need() {
    let d = Arc::new(Domain::with_slots(3));
    let needs: Vec<u64> = (0..3).map(|i| i + d.ticket()).collect();
    let mut leaders = Vec::new();
    for (i, &need) in needs.iter().enumerate() {
        leaders.push(held_leader(&d, need));
        until_issued(&d, i as u64 + 1);
    }
    let followers: Vec<_> = needs.iter().map(|&n| barrier(&d, n)).collect();
    until_waiting(&d, 3);
    // Flush 3, then 2: each leader, its flush done but `done` still 0,
    // parks too; then flush 1 moves `done` from 0 to 3 at once.
    for (parked, (go, _)) in (4..).zip(leaders.iter().rev().take(2)) {
        let _sent = go.send(());
        until_waiting(&d, parked);
    }
    assert_eq!(d.done.load(Ordering::SeqCst), 0);
    let _sent = leaders[0].0.send(());
    for f in &followers {
        assert_eq!(f.recv_timeout(LONG), Ok(true), "a covered need slept on");
    }
    for (_, l) in &leaders {
        assert_eq!(l.recv_timeout(LONG), Ok(true));
    }
    assert_eq!(d.done.load(Ordering::SeqCst), 3);
    assert_eq!(d.waiting(), 0);
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
