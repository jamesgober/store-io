//! Flushes of the main handle stay off the data leaders' slots (D-124):
//! metadata writes lead their flush only on slot 0, and provisioning's full
//! flush runs holding it, so no two flushes run on one handle at once (each
//! failure report stays its own). The simulator cannot tell handles apart,
//! so these tests watch the slots and the flush count instead.

#![allow(clippy::unwrap_used, unused_results)]

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use store_io_sim::{SimConfig, SimPlatform};

use crate::{Store, StoreOptions};

const LONG: Duration = Duration::from_secs(10);

fn store(seed: u64) -> (SimPlatform, Store<SimPlatform>) {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    let o = StoreOptions {
        flushes_in_flight: 2,
        ..StoreOptions::default()
    };
    let s = Store::create(p.clone(), Path::new("/db"), o).unwrap();
    assert_eq!(s.report().flushes_in_flight, 2);
    (p, s)
}

/// Judges a store operation's outcome; on a timeout, poisons the store
/// first, so a barrier stuck in it returns and the test fails, never hangs.
fn finish(s: &Store<SimPlatform>, r: Result<bool, mpsc::RecvTimeoutError>) {
    if r.is_err() {
        s.inner.domain.poison(store_io_core::error::FirstCause {
            op: None,
            raw: None,
        });
    }
    assert_eq!(r, Ok(true));
}

fn until_waiting(s: &Store<SimPlatform>, n: u32) {
    let start = std::time::Instant::now();
    while s.inner.domain.waiting() < n {
        assert!(start.elapsed() < LONG, "never waited");
        std::thread::yield_now();
    }
}

/// A slot commit (a metadata write, flushed through the main handle) waits
/// for slot 0 while it is held, though slot 1 is free.
#[test]
fn test_a_metadata_flush_waits_for_slot_zero() {
    let (p, s) = store(500);
    let slot = s.provision_slot("m").unwrap();
    let (go, wait_go) = mpsc::channel::<()>();
    let (inside, on_inside) = mpsc::channel::<()>();
    let (done, on_done) = mpsc::channel::<bool>();
    std::thread::scope(|scope| {
        let d = &s.inner.domain;
        scope.spawn(move || {
            d.exclusive_main(|| {
                let _sent = inside.send(());
                let _released = wait_go.recv_timeout(LONG);
            });
        });
        assert_eq!(on_inside.recv_timeout(LONG), Ok(()));
        let flushes = p.with_world(|w| w.flushes());
        let slot = &slot;
        scope.spawn(move || {
            let _sent = done.send(slot.commit(b"v2").is_ok());
        });
        until_waiting(&s, 1);
        assert!(
            on_done.try_recv().is_err(),
            "committed beside the held slot 0"
        );
        assert_eq!(
            p.with_world(|w| w.flushes()),
            flushes,
            "flushed on another slot"
        );
        let _sent = go.send(());
        finish(&s, on_done.recv_timeout(LONG));
    });
}

/// Provisioning's full flush of the main handle waits while every slot is
/// held, before it flushes anything.
#[test]
fn test_provisioning_full_flush_holds_slot_zero() {
    let (p, s) = store(501);
    assert!(s.inner.domain.hold_leadership());
    let flushes = p.with_world(|w| w.flushes());
    let (done, on_done) = mpsc::channel::<bool>();
    std::thread::scope(|scope| {
        let s = &s;
        scope.spawn(move || {
            let _sent = done.send(s.provision_page_region("pages", 1 << 20).is_ok());
        });
        until_waiting(s, 1);
        let seen = p.with_world(|w| w.flushes());
        // Released before judging, so a failure ends the test, never hangs it.
        s.inner.domain.drop_leadership();
        assert_eq!(seen, flushes, "the full flush ran beside held slots");
        finish(s, on_done.recv_timeout(LONG));
    });
}
