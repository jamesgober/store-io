//! `AppendRegion::resume_at` moves the append position forward without
//! claiming durability it does not have: bytes appended in this open and not
//! yet flushed stay not durable, so a later sync flushes them and they
//! survive a power failure; a poisoned store refuses it.

#![allow(clippy::unwrap_used, unused_results, unused_must_use)]

use std::path::Path;

use store_io_core::error::Error;
use store_io_engine::{Store, StoreOptions};
use store_io_sim::{CrashMode, SimConfig, SimPlatform};

const DIR: &str = "/db";

fn opts() -> StoreOptions {
    StoreOptions {
        queues: 2,
        buffers_per_class: 8,
        append_window: 256 << 12,
        ..StoreOptions::default()
    }
}

fn record(i: u32, len: usize) -> Vec<u8> {
    (0..len).map(|j| (i as usize * 31 + j + 1) as u8).collect()
}

/// After an append that is not yet durable, `resume_at` past it leaves the
/// durable point alone: the sync still flushes, and the append survives.
#[test]
fn test_resume_after_an_unflushed_append_keeps_it_not_durable() {
    let p = SimPlatform::new(SimConfig::volatile(9001));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    let data = record(1, 100);
    let t = wal.append(&data).unwrap();
    assert_eq!(wal.durable_through(), 0);
    let at = wal.resume_at(wal.tail() + 4096).unwrap();
    assert!(at > wal.durable_through());
    assert_eq!(
        wal.durable_through(),
        0,
        "resume_at claimed the append durable"
    );
    let f0 = p.with_world(|w| w.flushes());
    wal.sync_through(&t).unwrap();
    assert_eq!(
        p.with_world(|w| w.flushes()) - f0,
        1,
        "the sync did not flush"
    );
    drop((wal, s));
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    let s = Store::open(p.clone(), Path::new(DIR), opts()).unwrap();
    let wal = s.append_region("wal").unwrap();
    let mut out = vec![0u8; data.len()];
    wal.read(0, &mut out).unwrap();
    assert!(out == data, "an acknowledged append was lost");
}

/// With nothing waiting for a flush, `resume_at` still counts the skipped
/// range as durable (the open flushed it).
#[test]
fn test_resume_with_everything_durable_moves_the_durable_point() {
    let p = SimPlatform::new(SimConfig::volatile(9003));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    wal.append_durable(&record(2, 100)).unwrap();
    let at = wal.resume_at(wal.tail() + 8192).unwrap();
    assert_eq!(wal.durable_through(), at);
}

/// A poisoned store refuses `resume_at`, so it can never raise the durable
/// point after a failure.
#[test]
fn test_resume_on_a_poisoned_store_is_refused() {
    let p = SimPlatform::new(SimConfig::volatile(9002));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    let t = wal.append(&record(3, 100)).unwrap();
    s.poison();
    let r = wal.resume_at(wal.tail());
    assert!(matches!(r, Err(Error::Poisoned { .. })), "{r:?}");
    assert!(wal.sync_through(&t).is_err());
}

/// Power-safe, forced region (D-102): after an append not yet flushed,
/// `resume_at` leaves the flushed prefix alone too, so the forced sync
/// still flushes before its receipt says `data_synced`.
#[test]
fn test_resume_keeps_a_forced_region_unflushed_append_unflushed() {
    let p = SimPlatform::new(SimConfig::power_safe(9004));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    wal.set_force_data_sync(true);
    let t = wal.append(&record(4, 100)).unwrap();
    wal.resume_at(wal.tail() + 4096).unwrap();
    let f0 = p.with_world(|w| w.flushes());
    let r = wal.sync_through(&t).unwrap();
    assert!(r.data_synced() && r.covers(&t));
    assert_eq!(
        p.with_world(|w| w.flushes()) - f0,
        1,
        "a forced sync was satisfied without a flush"
    );
}
