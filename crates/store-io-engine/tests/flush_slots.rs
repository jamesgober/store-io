//! Flushes in flight (D-124): a store may run up to four flushes at once,
//! each on a handle and queue of its own. Receipts keep their meaning: an
//! append that returned one survives a power failure, however the flushes
//! overlapped or reordered, and a failed flush poisons the store. (The
//! simulator's flush persists the whole file, so these tests check the
//! wiring, the handles and the end-to-end outcome; the rule that a later
//! flush finishing first satisfies nothing is the domain's own tests'.)

#![allow(clippy::unwrap_used, unused_results, unused_must_use)]

use std::path::Path;
use std::sync::{Arc, Mutex};

use store_io_core::error::Error;
use store_io_engine::{Store, StoreOptions};
use store_io_sim::{CrashMode, SimConfig, SimPlatform};

const DIR: &str = "/db";
const WRITERS: u32 = 8;
const EACH: u32 = 40;

fn opts(slots: u8) -> StoreOptions {
    StoreOptions {
        queues: 2,
        buffers_per_class: 16,
        flushes_in_flight: slots,
        ..StoreOptions::default()
    }
}

fn record(w: u32, i: u32) -> Vec<u8> {
    (0..300 + (i as usize % 7) * 50)
        .map(|j| (w as usize * 131 + i as usize * 17 + j) as u8)
        .collect()
}

/// `WRITERS` threads each append `EACH` records durably; returns what came
/// back Ok, with its position.
fn append_concurrently(s: &Store<SimPlatform>) -> Vec<(u64, Vec<u8>)> {
    let acked = Arc::new(Mutex::new(Vec::new()));
    std::thread::scope(|scope| {
        for w in 0..WRITERS {
            let (s, acked) = (s, Arc::clone(&acked));
            scope.spawn(move || {
                let wal = s.append_region("wal").unwrap();
                for i in 0..EACH {
                    let data = record(w, i);
                    match wal.append_durable(&data) {
                        Ok((pos, receipt)) => {
                            assert!(receipt.durable_through() >= pos.offset() + data.len() as u64);
                            acked.lock().unwrap().push((pos.offset(), data));
                        }
                        Err(Error::Poisoned { .. } | Error::DurabilityUnknown { .. }) => return,
                        Err(e) => panic!("writer {w}: {e:?}"),
                    }
                }
            });
        }
    });
    Arc::try_unwrap(acked).unwrap().into_inner().unwrap()
}

/// Every acknowledged append reads back after a power failure.
fn check_after_power_failure(p: &SimPlatform, slots: u8, acked: &[(u64, Vec<u8>)]) {
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    let s = Store::open(p.clone(), Path::new(DIR), opts(slots)).unwrap();
    let wal = s.append_region("wal").unwrap();
    for (at, data) in acked {
        let mut out = vec![0u8; data.len()];
        let n = wal.read(*at, &mut out).unwrap();
        assert_eq!(n, data.len(), "acknowledged append at {at} lost");
        assert!(out == *data, "acknowledged append at {at} changed");
    }
}

#[test]
fn test_overlapping_flushes_keep_every_receipt_across_a_power_failure() {
    for slots in [1u8, 4] {
        let p = SimPlatform::new(SimConfig::volatile(400 + u64::from(slots)));
        let s = Store::create(p.clone(), Path::new(DIR), opts(slots)).unwrap();
        assert_eq!(s.report().flushes_in_flight, slots);
        s.provision_append_region("wal", 8 << 20).unwrap();
        // Every submission takes a while, as a device's does, and some take
        // longer than others: flushes overlap, every slot's handle and lane
        // is used, and later flushes finish before earlier ones.
        let n = Arc::new(std::sync::atomic::AtomicU64::new(0));
        p.with_world(|w| {
            w.set_submit_probe(Some(Arc::new(move || {
                let k = n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                std::thread::sleep(std::time::Duration::from_micros(100 + (k % 3) * 300));
            })));
        });
        let acked = append_concurrently(&s);
        p.with_world(|w| w.set_submit_probe(None));
        assert_eq!(acked.len(), (WRITERS * EACH) as usize);
        let ds = s.domain_stats();
        assert!(ds.peak_in_flight <= u64::from(slots));
        if slots > 1 {
            assert!(ds.peak_in_flight > 1, "{slots} slots never overlapped");
            assert!(
                ds.out_of_order > 0,
                "no flush finished before an earlier one"
            );
        } else {
            assert_eq!(ds.out_of_order, 0);
        }
        drop(s);
        check_after_power_failure(&p, slots, &acked);
    }
}

#[test]
fn test_a_failed_flush_among_overlapping_ones_poisons_and_keeps_receipts() {
    for slots in [1u8, 4] {
        for nth in [3u64, 10, 25] {
            let p = SimPlatform::new(SimConfig::volatile(410 + nth));
            let s = Store::create(p.clone(), Path::new(DIR), opts(slots)).unwrap();
            s.provision_append_region("wal", 8 << 20).unwrap();
            p.with_world(|w| {
                let n = w.flushes() + nth;
                w.faults_mut().fail_flush = Some(n);
            });
            let acked = append_concurrently(&s);
            assert!(s.poisoned().is_some(), "{slots} slots, flush {nth}");
            assert!(acked.len() < (WRITERS * EACH) as usize);
            std::mem::forget(s);
            p.with_world(|w| *w.faults_mut() = Default::default());
            check_after_power_failure(&p, slots, &acked);
        }
    }
}

#[test]
fn test_metadata_writes_beside_overlapping_flushes() {
    // Provisioning (metadata, flushed through the main handle only) while
    // writers flush on every slot.
    let p = SimPlatform::new(SimConfig::volatile(420));
    let s = Store::create(p.clone(), Path::new(DIR), opts(4)).unwrap();
    s.provision_append_region("wal", 8 << 20).unwrap();
    let acked = std::thread::scope(|scope| {
        let meta = scope.spawn(|| {
            for i in 0..12 {
                let slot = s.provision_slot(&format!("m{i}")).unwrap();
                slot.commit(format!("v{i}").as_bytes()).unwrap();
            }
        });
        let acked = append_concurrently(&s);
        meta.join().unwrap();
        acked
    });
    drop(s);
    check_after_power_failure(&p, 4, &acked);
    let s = Store::open(p.clone(), Path::new(DIR), opts(4)).unwrap();
    for i in 0..12 {
        let slot = s.slot(&format!("m{i}")).unwrap();
        assert_eq!(slot.read(), Some(format!("v{i}").into_bytes()));
    }
}

#[test]
fn test_the_number_of_flushes_run_is_reported_and_bounded() {
    let p = SimPlatform::new(SimConfig::volatile(430));
    // Asked for more than the most: four.
    let s = Store::create(p.clone(), Path::new(DIR), opts(9)).unwrap();
    assert_eq!(s.report().flushes_in_flight, 4);
    drop(s);
    // Zero means one.
    let s = Store::open(p.clone(), Path::new(DIR), opts(0)).unwrap();
    assert_eq!(s.report().flushes_in_flight, 1);
    drop(s);
    // A read-only store flushes nothing.
    let s = Store::open_readonly(p.clone(), Path::new(DIR), opts(4)).unwrap();
    assert_eq!(s.report().flushes_in_flight, 1);
}
