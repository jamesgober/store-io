//! Writes the platform refuses at submission, before they reach the device
//! (A1 C2, `settle_pumped`). A page write refused this way touched nothing:
//! `Io` with the raw code, nothing poisoned. An append refused this way has
//! already reserved its range: the range is a hole no later append may be
//! acknowledged past, so the append is `DurabilityUnknown` and the store is
//! poisoned, with the refusal's code as the first cause.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::path::Path;

use store_io_core::error::Error;
use store_io_engine::{Store, StoreOptions};
use store_io_sim::{SimConfig, SimPlatform};

const MIB: u64 = 1 << 20;
/// The simulator's EIO.
const EIO: i32 = 5;

fn store(seed: u64) -> (SimPlatform, Store<SimPlatform>) {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    let opts = StoreOptions {
        queues: 1,
        ..StoreOptions::default()
    };
    let s = Store::create(p.clone(), Path::new("/db"), opts).unwrap();
    (p, s)
}

/// Refuses the next write submission with EIO.
fn refuse_next_write(p: &SimPlatform) {
    p.with_world(|w| {
        let next = w.write_submits() + 1;
        w.faults_mut().fail_submit = Some((next, EIO));
    });
}

#[test]
fn test_an_append_refused_after_its_reservation_is_unknown_and_poisons() {
    let (p, s) = store(700);
    let wal = s.provision_append_region("wal", MIB).unwrap();
    let before = wal.append(b"before").unwrap();
    refuse_next_write(&p);
    let r = wal.append(b"refused");
    assert!(matches!(r, Err(Error::DurabilityUnknown { .. })), "{r:?}");
    let first = s.poisoned().expect("the hole poisons the store");
    assert_eq!(
        first.raw.map(|e| e.code),
        Some(EIO),
        "the first cause keeps the refusal's code"
    );
    // Nothing is acknowledged past the hole.
    assert!(matches!(wal.append(b"after"), Err(Error::Poisoned { .. })));
    assert!(wal.sync_through(&before).is_err());
}

#[test]
fn test_an_append_batch_refused_after_its_reservation_is_unknown_and_poisons() {
    let (p, s) = store(701);
    let wal = s.provision_append_region("wal", MIB).unwrap();
    let mut batch = wal.batch();
    batch.append(b"one").unwrap();
    batch.append(b"two").unwrap();
    refuse_next_write(&p);
    let r = batch.write();
    assert!(matches!(r, Err(Error::DurabilityUnknown { .. })), "{r:?}");
    let first = s.poisoned().expect("the hole poisons the store");
    assert_eq!(
        first.raw.map(|e| e.code),
        Some(EIO),
        "poisoned once, with the refusal's code"
    );
}

#[test]
fn test_a_page_write_refused_at_submission_touches_nothing() {
    let (p, s) = store(702);
    let pages = s.provision_page_region("pages", MIB).unwrap();
    let bs = pages.block_size() as usize;
    refuse_next_write(&p);
    let r = pages.write(pages.pos(0).unwrap(), &vec![1; bs]);
    assert!(
        matches!(&r, Err(Error::Io { raw, .. }) if raw.code == EIO),
        "{r:?}"
    );
    assert!(
        s.poisoned().is_none(),
        "a refusal before any I/O poisons nothing"
    );
    pages.write(pages.pos(0).unwrap(), &vec![2; bs]).unwrap();
    let mut out = vec![0; bs];
    pages.read(pages.pos(0).unwrap(), &mut out).unwrap();
    assert!(out.iter().all(|&b| b == 2));
}

#[test]
fn test_a_page_batch_refused_at_its_first_write_touches_nothing() {
    let (p, s) = store(703);
    let pages = s.provision_page_region("pages", MIB).unwrap();
    let bs = pages.block_size() as usize;
    let mut batch = pages.batch();
    batch.write(pages.pos(0).unwrap(), &vec![1; bs]).unwrap();
    batch
        .write(pages.pos(bs as u64).unwrap(), &vec![1; bs])
        .unwrap();
    refuse_next_write(&p);
    let r = batch.commit();
    assert!(matches!(r, Err(Error::Io { .. })), "{r:?}");
    assert!(
        batch.is_empty(),
        "the batch is empty after a refusal (documented)"
    );
    assert!(s.poisoned().is_none());
}

/// A lane's refused submission gives its row back (A1 C5): on a store whose
/// lanes hold one operation, a write after a refusal still finds its row.
#[test]
fn test_a_refused_lane_submission_frees_its_row() {
    let p = SimPlatform::new(SimConfig::volatile(704));
    let opts = StoreOptions {
        queues: 1,
        queue_depth: 1,
        ..StoreOptions::default()
    };
    let s = Store::create(p.clone(), Path::new("/db"), opts).unwrap();
    let pages = s.provision_page_region("pages", MIB).unwrap();
    let bs = pages.block_size() as usize;
    for round in 0..3u8 {
        refuse_next_write(&p);
        let r = pages.write(pages.pos(0).unwrap(), &vec![round; bs]);
        assert!(matches!(r, Err(Error::Io { .. })), "round {round}: {r:?}");
        pages
            .write(pages.pos(0).unwrap(), &vec![round + 1; bs])
            .unwrap_or_else(|e| panic!("round {round}: the lane's row leaked: {e:?}"));
    }
    assert!(s.poisoned().is_none());
}
