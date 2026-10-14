//! Forced data sync per region (D-102, G7): on a power-safe store a forced
//! region's barriers issue a device flush and its receipts say so; other
//! regions' do not; the setting survives recycle, not reopen.

#![allow(clippy::unwrap_used, unused_results, unused_must_use)]

use std::path::Path;

use store_io_core::error::Error;
use store_io_engine::completion::Done;
use store_io_engine::{Store, StoreOptions, SubmitterConfig, WriteMode};
use store_io_sim::{SimConfig, SimPlatform};

const MIB: u64 = 1 << 20;
const DIR: &str = "/db";

fn opts() -> StoreOptions {
    StoreOptions {
        queues: 1,
        ..StoreOptions::default()
    }
}

fn flushes(p: &SimPlatform) -> u64 {
    p.with_world(|w| w.flushes())
}

fn power_safe(seed: u64) -> (SimPlatform, Store<SimPlatform>) {
    let p = SimPlatform::new(SimConfig::power_safe(seed));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    (p, s)
}

/// Power-safe: an ordinary region's syncs flush nothing and are not
/// `data_synced`; a forced region's flush, and say so (append, page,
/// already-durable prefix).
#[test]
fn test_a_forced_region_flushes_on_a_power_safe_store() {
    let (p, s) = power_safe(1400);
    let wal = s.provision_append_region("wal", MIB).unwrap();
    let pages = s.provision_page_region("pages", MIB).unwrap();
    let f0 = flushes(&p);
    let t = wal.append(b"plain").unwrap();
    let r = wal.sync_through(&t).unwrap();
    assert!(!r.data_synced());
    assert_eq!(flushes(&p), f0, "an ordinary power-safe sync flushed");
    // Forced: the already-durable prefix is flushed too.
    wal.set_force_data_sync(true);
    assert!(wal.force_data_sync());
    let r = wal.sync_through(&t).unwrap();
    assert!(r.data_synced() && r.covers(&t));
    assert_eq!(flushes(&p), f0 + 1, "a forced sync did not flush");
    let (_, r) = wal.append_durable(b"forced").unwrap();
    assert!(r.data_synced());
    assert_eq!(flushes(&p), f0 + 2);
    // Page regions.
    let r = pages
        .write_durable(pages.pos(0).unwrap(), &vec![1; 4096])
        .unwrap();
    assert!(!r.data_synced());
    assert_eq!(flushes(&p), f0 + 2);
    pages.set_force_data_sync(true);
    let r = pages
        .write_durable(pages.pos(0).unwrap(), &vec![2; 4096])
        .unwrap();
    assert!(r.data_synced());
    assert_eq!(flushes(&p), f0 + 3);
}

/// A flush-required store's receipts are always `data_synced`.
#[test]
fn test_flush_required_receipts_are_always_data_synced() {
    let p = SimPlatform::new(SimConfig::volatile(1401));
    let s = Store::create(p, Path::new(DIR), opts()).unwrap();
    let wal = s.provision_append_region("wal", MIB).unwrap();
    let (_, r) = wal.append_durable(b"x").unwrap();
    assert!(r.data_synced());
}

/// A forced flush that fails poisons, as any flush does.
#[test]
fn test_a_failed_forced_flush_poisons() {
    let (p, s) = power_safe(1402);
    let wal = s.provision_append_region("wal", MIB).unwrap();
    wal.set_force_data_sync(true);
    p.with_world(|w| {
        let next = w.flushes() + 1;
        w.faults_mut().fail_flush = Some(next);
    });
    let r = wal.append_durable(b"x");
    assert!(matches!(r, Err(Error::DurabilityUnknown { .. })), "{r:?}");
    assert!(s.poisoned().is_some());
}

/// Submitter durable writes and barriers on a forced region go through a
/// flush; on an ordinary power-safe region they do not.
#[test]
fn test_submitter_durable_writes_on_a_forced_region_flush() {
    let (p, s) = power_safe(1403);
    let plain = s.provision_append_region("plain", MIB).unwrap();
    let forced = s.provision_append_region("forced", MIB).unwrap();
    forced.set_force_data_sync(true);
    let pages = s.provision_page_region("pages", MIB).unwrap();
    let mut sub = s.submitter(SubmitterConfig::default()).unwrap();
    let f0 = flushes(&p);
    let buf = sub.take_buf(100).unwrap();
    sub.append(&plain, buf, WriteMode::Durable, 1).unwrap();
    let c = sub.wait(1).unwrap().next().unwrap();
    let Ok(Done::Written(w)) = c.outcome else {
        panic!("{:?}", c.outcome)
    };
    assert!(!w.receipt.unwrap().data_synced());
    assert_eq!(flushes(&p), f0);
    let buf = sub.take_buf(100).unwrap();
    sub.append(&forced, buf, WriteMode::Durable, 2).unwrap();
    let c = sub.wait(1).unwrap().next().unwrap();
    let Ok(Done::Written(w)) = c.outcome else {
        panic!("{:?}", c.outcome)
    };
    let r = w.receipt.unwrap();
    assert!(r.data_synced() && r.covers(&w.ticket));
    assert_eq!(flushes(&p), f0 + 1, "a forced durable write did not flush");
    // An ordinary page region's durable write: a receipt for the write
    // alone, durable at completion, not flush-backed.
    let buf = sub.take_buf(4096).unwrap();
    sub.write(&pages, pages.pos(0).unwrap(), buf, WriteMode::Durable, 4)
        .unwrap();
    let c = sub.wait(1).unwrap().next().unwrap();
    let Ok(Done::Written(w)) = c.outcome else {
        panic!("{:?}", c.outcome)
    };
    let r = w.receipt.unwrap();
    assert!(!r.is_prefix() && !r.data_synced() && r.covers(&w.ticket));
    assert_eq!(flushes(&p), f0 + 1);
    // A barrier for a forced region's plain write flushes too.
    let t = forced.append(b"plain write").unwrap();
    sub.barrier(&t, 3).unwrap();
    let c = sub.wait(1).unwrap().next().unwrap();
    assert!(matches!(&c.outcome, Ok(Done::Synced(r)) if r.data_synced() && r.covers(&t)));
    assert_eq!(flushes(&p), f0 + 2);
}

/// The setting is kept by recycle and cleared by reopen.
#[test]
fn test_forcing_survives_recycle_not_reopen() {
    let (p, s) = power_safe(1404);
    let wal = s.provision_append_region("wal", MIB).unwrap();
    wal.set_force_data_sync(true);
    let next = wal.recycle().unwrap();
    assert!(next.force_data_sync());
    drop((next, wal, s));
    let s = Store::open(p, Path::new(DIR), opts()).unwrap();
    assert!(!s.append_region("wal").unwrap().force_data_sync());
}

/// Submitter barriers: an unforced power-safe one is not flush-backed;
/// `barrier_all` flushes once any region forces data sync.
#[test]
fn test_submitter_barriers_and_forcing() {
    let (p, s) = power_safe(1405);
    let wal = s.provision_append_region("wal", MIB).unwrap();
    let mut sub = s.submitter(SubmitterConfig::default()).unwrap();
    let t = wal.append(b"x").unwrap();
    let f0 = flushes(&p);
    sub.barrier(&t, 1).unwrap();
    sub.barrier_all(2).unwrap();
    let done: Vec<_> = sub.wait(2).unwrap().collect();
    assert_eq!(done.len(), 2);
    for c in &done {
        match (&c.outcome, c.tag) {
            (Ok(Done::Synced(r)), 1) => assert!(!r.data_synced() && r.covers(&t)),
            (Ok(Done::Flushed), 2) => {}
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(flushes(&p), f0, "an unforced power-safe barrier flushed");
    wal.set_force_data_sync(true);
    sub.barrier_all(3).unwrap();
    let c = sub.wait(1).unwrap().next().unwrap();
    assert!(matches!(c.outcome, Ok(Done::Flushed)));
    assert_eq!(flushes(&p), f0 + 1, "barrier_all ignored a forced region");
}

/// Batches on a forced region flush; turning forcing off stops it.
#[test]
fn test_forced_batches_flush_and_forcing_can_be_turned_off() {
    let (p, s) = power_safe(1406);
    let wal = s.provision_append_region("wal", MIB).unwrap();
    let pages = s.provision_page_region("pages", MIB).unwrap();
    wal.set_force_data_sync(true);
    pages.set_force_data_sync(true);
    let f0 = flushes(&p);
    let mut b = wal.batch();
    b.append(b"one").unwrap();
    b.append(b"two").unwrap();
    let (_, r) = b.commit().unwrap();
    assert!(r.data_synced());
    let mut b = pages.batch();
    b.write(pages.pos(0).unwrap(), &vec![1; 4096]).unwrap();
    b.write(pages.pos(8192).unwrap(), &vec![2; 4096]).unwrap();
    assert!(b.commit().unwrap().data_synced());
    assert_eq!(flushes(&p), f0 + 2);
    wal.set_force_data_sync(false);
    assert!(!wal.force_data_sync());
    let (_, r) = wal.append_durable(b"plain again").unwrap();
    assert!(!r.data_synced());
    assert_eq!(flushes(&p), f0 + 2, "an unforced region flushed");
}

/// The already-durable shortcut: on a flush-required store it is
/// flush-backed; on a forced power-safe region it counts only flushed bytes
/// (no new flush for a prefix already flushed).
#[test]
fn test_the_already_durable_shortcut() {
    let p = SimPlatform::new(SimConfig::volatile(1407));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let wal = s.provision_append_region("wal", MIB).unwrap();
    let t = wal.append(b"x").unwrap();
    wal.sync_through(&t).unwrap();
    let f0 = flushes(&p);
    let r = wal.sync_through(&t).unwrap();
    assert!(r.data_synced());
    assert_eq!(flushes(&p), f0);
    let (p, s) = power_safe(1408);
    let wal = s.provision_append_region("wal", MIB).unwrap();
    wal.set_force_data_sync(true);
    let t = wal.append(b"x").unwrap();
    wal.sync_through(&t).unwrap();
    let f0 = flushes(&p);
    let r = wal.sync_through(&t).unwrap();
    assert!(r.data_synced() && r.covers(&t));
    assert_eq!(
        flushes(&p),
        f0,
        "an already flushed prefix was flushed again"
    );
}

/// Slots: a forced slot's commit flushes on a power-safe store and says so.
#[test]
fn test_a_forced_slot_flushes() {
    let (p, s) = power_safe(1409);
    let slot = s.provision_slot("manifest").unwrap();
    let f0 = flushes(&p);
    assert!(!slot.commit(b"v1").unwrap().data_synced());
    assert_eq!(flushes(&p), f0);
    slot.set_force_data_sync(true);
    assert!(slot.commit(b"v2").unwrap().data_synced());
    assert_eq!(flushes(&p), f0 + 1);
}

/// A setting made through an old generation's handle after a recycle holds
/// for the new generation (one setting per region).
#[test]
fn test_one_setting_per_region_across_generations() {
    let (_p, s) = power_safe(1410);
    let wal = s.provision_append_region("wal", MIB).unwrap();
    let next = wal.recycle().unwrap();
    wal.set_force_data_sync(true);
    assert!(next.force_data_sync());
}

/// A submitter's flush-backed prefix receipt on a forced region publishes
/// the flushed prefix: a blocking sync of the same write needs no new flush.
#[test]
fn test_a_submitter_prefix_receipt_publishes_the_flushed_prefix() {
    let (p, s) = power_safe(1411);
    let wal = s.provision_append_region("wal", MIB).unwrap();
    wal.set_force_data_sync(true);
    let mut sub = s.submitter(SubmitterConfig::default()).unwrap();
    let buf = sub.take_buf(100).unwrap();
    sub.append(&wal, buf, WriteMode::Durable, 1).unwrap();
    let c = sub.wait(1).unwrap().next().unwrap();
    let Ok(Done::Written(w)) = c.outcome else {
        panic!("{:?}", c.outcome)
    };
    assert!(w.receipt.unwrap().is_prefix());
    let f0 = flushes(&p);
    let r = wal.sync_through(&w.ticket).unwrap();
    assert!(r.data_synced());
    assert_eq!(flushes(&p), f0, "the flushed prefix was not published");
}
