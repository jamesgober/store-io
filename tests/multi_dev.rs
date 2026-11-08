//! Stores on several real devices (R5.3, R5.14): set barriers, dependent
//! writes across devices, ditto writes and concurrent callers, on the
//! native backend. The devices are `$STORE_IO_MULTI_DEV`, a directory on
//! each, comma-separated (CI's multi-device job: four `null_blk` devices,
//! ext4); without it, two directories on this machine's one device, so the
//! scenarios run everywhere. With `$STORE_IO_EXPECT_MULTI_DEV` set, a
//! missing `$STORE_IO_MULTI_DEV` fails instead (coverage cannot vanish),
//! as do two of its directories on one file system, and every set barrier
//! must cost each store exactly one flush.
//!
//! What a real device cannot show here: that a barrier made anything
//! durable (no test can cut a device's power with its file system
//! mounted). The reopens show close and reopen agree with what the calls
//! returned; durability across crashes is the simulator's
//! (`crates/store-io-engine/tests/multi_crash.rs`).

#![cfg(any(windows, target_os = "linux", target_os = "macos"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use store_io::{Error, NotWrittenCause, Store, StoreOptions, StoreSet, Trust, WriteTicket};

const B: usize = 4096;
const MIB: u64 = 1 << 20;

/// A fresh directory on each device, removed when dropped.
struct Devices(Vec<PathBuf>);

impl Devices {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let bases: Vec<PathBuf> = match std::env::var_os("STORE_IO_MULTI_DEV") {
            Some(v) => v
                .to_string_lossy()
                .split(',')
                .filter(|d| !d.is_empty())
                .map(PathBuf::from)
                .collect(),
            None => {
                assert!(
                    std::env::var_os("STORE_IO_EXPECT_MULTI_DEV").is_none(),
                    "STORE_IO_EXPECT_MULTI_DEV is set but STORE_IO_MULTI_DEV is not"
                );
                let base = std::env::var_os("STORE_IO_TEST_DIR")
                    .or_else(|| std::env::var_os("HOME").filter(|_| cfg!(target_os = "linux")))
                    .map_or_else(std::env::temp_dir, PathBuf::from);
                vec![base.join(".store-io-md0"), base.join(".store-io-md1")]
            }
        };
        assert!(
            bases.len() >= 2,
            "STORE_IO_MULTI_DEV names fewer than two devices"
        );
        #[cfg(unix)]
        if expected() {
            use std::os::unix::fs::MetadataExt;
            let mut devs: Vec<u64> = bases
                .iter()
                .map(|b| std::fs::metadata(b).unwrap().dev())
                .collect();
            devs.sort_unstable();
            devs.dedup();
            assert_eq!(
                devs.len(),
                bases.len(),
                "two of STORE_IO_MULTI_DEV share a file system"
            );
        }
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dirs = bases
            .iter()
            .map(|b| {
                let d = b.join(format!("store-io-multi-{}-{n}-{tag}", std::process::id()));
                std::fs::create_dir_all(&d).unwrap();
                d
            })
            .collect();
        Self(dirs)
    }
}

impl Drop for Devices {
    fn drop(&mut self) {
        for d in &self.0 {
            let _cleanup = std::fs::remove_dir_all(d);
        }
    }
}

/// The multi-device job runs these: the devices are real and distinct.
fn expected() -> bool {
    std::env::var_os("STORE_IO_EXPECT_MULTI_DEV").is_some()
}

/// Default trust where the device is accepted, the labelled override
/// where it is refused (a virtual disk).
fn options_for(dir: &Path) -> StoreOptions {
    let probe = store_io::probe(dir).unwrap();
    let refused = !matches!(probe.decision.durable_open, store_io::DurableOpen::Allowed);
    StoreOptions {
        trust: Trust {
            override_refusal: refused,
            ..Trust::default()
        },
        ..StoreOptions::default()
    }
}

/// A store in each device's directory.
fn stores(d: &Devices) -> Vec<Store> {
    d.0.iter()
        .map(|dir| Store::create(dir.join("s"), options_for(dir)).unwrap())
        .collect()
}

fn reopen(d: &Devices, i: usize) -> Store {
    let dir = &d.0[i];
    Store::open_with(dir.join("s"), options_for(dir)).unwrap()
}

/// One set barrier over an append and a page write on every device: every
/// receipt covers its ticket, every store flushed at most once, and every
/// write is there after the stores close and reopen.
#[test]
fn test_a_set_barrier_over_every_device() {
    let d = Devices::new("barrier");
    let stores = stores(&d);
    let set = StoreSet::new(stores.iter().cloned()).unwrap();
    let mut tickets: Vec<WriteTicket> = Vec::new();
    for (i, s) in stores.iter().enumerate() {
        let wal = s.provision_append_region("wal", MIB).unwrap();
        let pages = s.provision_page_region("pages", MIB).unwrap();
        tickets.push(wal.append(&[i as u8 + 1; B]).unwrap());
        tickets.push(
            pages
                .write(pages.pos(0).unwrap(), &[i as u8 + 0x41; B])
                .unwrap(),
        );
    }
    let before: Vec<u64> = stores.iter().map(|s| s.domain_stats().flushes).collect();
    let refs: Vec<&WriteTicket> = tickets.iter().collect();
    let receipts = set.barrier(&refs).unwrap();
    for (r, t) in receipts.iter().zip(&tickets) {
        assert!(r.covers(t));
    }
    for (s, b) in stores.iter().zip(&before) {
        let flushes = s.domain_stats().flushes - b;
        if expected() || s.report().decision.class.needs_flush() {
            assert_eq!(flushes, 1, "a store flushed {flushes} times");
        } else {
            assert_eq!(flushes, 0, "a power-safe store flushed");
        }
    }
    drop((set, stores));
    for i in 0..d.0.len() {
        let s = reopen(&d, i);
        let wal = s.append_region("wal").unwrap();
        wal.resume_at(B as u64).unwrap();
        let mut out = vec![0u8; B];
        wal.read(0, &mut out).unwrap();
        assert_eq!(out, vec![i as u8 + 1; B]);
        let pages = s.page_region("pages").unwrap();
        pages.read(pages.pos(0).unwrap(), &mut out).unwrap();
        assert_eq!(out, vec![i as u8 + 0x41; B]);
    }
}

/// Data on the first device, a pointer to its durable prefix on the
/// second through the set's `append_after`, then the data region recycled:
/// after a reopen the pointer is there, and the receipt of the recycled
/// data is refused by a set of the reopened stores (the recycle held).
#[test]
fn test_a_pointer_on_one_device_to_data_on_another() {
    let d = Devices::new("pointer");
    let stores = stores(&d);
    let set = StoreSet::new(stores.iter().cloned()).unwrap();
    let data = stores[0].provision_append_region("data", MIB).unwrap();
    let ptr = stores[1].provision_append_region("ptr", MIB).unwrap();
    let (_at, r) = data.append_durable(&[7u8; B]).unwrap();
    let mut record = vec![0xA0u8; B];
    record[..8].copy_from_slice(&r.durable_through().to_le_bytes());
    let t = set.append_after(&ptr, &[r], &record).unwrap();
    assert!(set.barrier(&[&t]).unwrap()[0].covers(&t));
    let recycled = data.recycle().unwrap();
    match set.append_after(&ptr, &[r], &record) {
        Err(Error::NotWritten { cause, .. }) => assert_eq!(cause, NotWrittenCause::StaleGeneration),
        other => panic!("a recycled dependency was accepted: {other:?}"),
    }
    drop((recycled, data, ptr, set, stores));
    let (a, b) = (reopen(&d, 0), reopen(&d, 1));
    let ptr = b.append_region("ptr").unwrap();
    let mut out = vec![0u8; B];
    ptr.read(0, &mut out).unwrap();
    assert_eq!(out, record);
    ptr.resume_at(B as u64).unwrap();
    let data = a.append_region("data").unwrap();
    assert_ne!(
        data.generation(),
        r.generation(),
        "the recycle did not hold"
    );
    let set = StoreSet::new([a.clone(), b.clone()]).unwrap();
    match set.append_after(&ptr, &[r], &record) {
        Err(Error::NotWritten { cause, .. }) => assert_eq!(cause, NotWrittenCause::StaleGeneration),
        other => panic!("a recycled dependency was accepted after a reopen: {other:?}"),
    }
}

/// A ditto: written on every device where the devices are known to be
/// distinct, else refused before any write (memory and virtual disks never
/// are: their identities are incomplete).
#[test]
fn test_ditto_across_the_devices() {
    let d = Devices::new("ditto");
    let stores = stores(&d);
    let set = StoreSet::new(stores.iter().cloned()).unwrap();
    let logs: Vec<_> = stores
        .iter()
        .map(|s| s.provision_append_region("log", MIB).unwrap())
        .collect();
    let refs: Vec<_> = logs.iter().collect();
    let distinct = (0..set.len()).all(|i| (i + 1..set.len()).all(|j| set.distinct(i, j)));
    match set.ditto(&refs, &[9u8; B]) {
        Ok(copies) => {
            assert!(distinct, "a ditto written on devices not known distinct");
            let tickets: Vec<&WriteTicket> = copies.iter().map(|c| c.as_ref().unwrap()).collect();
            for (r, t) in set.barrier(&tickets).unwrap().iter().zip(&tickets) {
                assert!(r.covers(t));
            }
        }
        Err(Error::NotWritten { cause, .. }) => {
            assert!(!distinct, "a ditto refused on distinct devices");
            assert_eq!(cause, NotWrittenCause::SameDevice);
        }
        Err(e) => panic!("{e:?}"),
    }
}

/// Four threads, each a set barrier over every device at a time, 50 each:
/// every receipt its own ticket's.
#[test]
fn test_concurrent_callers_across_the_devices() {
    let d = Devices::new("callers");
    let stores = stores(&d);
    let set = StoreSet::new(stores.iter().cloned()).unwrap();
    std::thread::scope(|scope| {
        for t in 0..4 {
            let (set, stores) = (&set, &stores);
            scope.spawn(move || {
                let regions: Vec<_> = stores
                    .iter()
                    .map(|s| s.provision_append_region(&format!("t{t}"), MIB).unwrap())
                    .collect();
                for i in 0..50u8 {
                    let tickets: Vec<WriteTicket> =
                        regions.iter().map(|r| r.append(&[i; B]).unwrap()).collect();
                    let refs: Vec<&WriteTicket> = tickets.iter().collect();
                    for (r, t) in set.barrier(&refs).unwrap().iter().zip(&tickets) {
                        assert!(r.covers(t));
                    }
                }
            });
        }
    });
    assert_eq!(set.stats().barriers, 200);
}
