//! End-to-end engine behaviour on the deterministic simulator: what a receipt
//! guarantees across crashes, faults and reopens.

// Test setup unwraps (a failure is the test failing) and ignores values a
// given test does not inspect.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::path::Path;

use store_io_core::class::DurabilityClass;
use store_io_core::error::{Error, NotWrittenCause};
use store_io_engine::{Store, StoreOptions};
use store_io_sim::{CrashMode, FaultPlan, FlushFailure, SimConfig, SimPlatform};

const DIR: &str = "/db";

fn opts() -> StoreOptions {
    StoreOptions {
        queues: 2,
        buffers_per_class: 8,
        append_ring: 256,
        ..StoreOptions::default()
    }
}

fn create(p: &SimPlatform) -> Store<SimPlatform> {
    Store::create(p.clone(), Path::new(DIR), opts()).unwrap()
}

fn reopen(p: &SimPlatform) -> Store<SimPlatform> {
    Store::open(p.clone(), Path::new(DIR), opts()).unwrap()
}

fn record(i: u32, len: usize) -> Vec<u8> {
    (0..len).map(|j| (i as usize * 31 + j) as u8).collect()
}

#[test]
fn test_create_then_open_round_trips_metadata() {
    let p = SimPlatform::new(SimConfig::volatile(1));
    let s = create(&p);
    assert_eq!(s.report().decision.class, DurabilityClass::FlushRequired);
    s.provision_append_region("wal", 1 << 20).unwrap();
    s.provision_page_region("pages", 1 << 20).unwrap();
    s.provision_slot("manifest").unwrap();
    let vol = s.volume();
    drop(s);
    let s = reopen(&p);
    assert_eq!(s.volume(), vol);
    assert_eq!(s.regions().len(), 3);
    assert!(s.append_region("wal").is_ok());
    assert!(s.page_region("pages").is_ok());
    assert!(s.slot("manifest").is_ok());
    assert!(matches!(
        s.append_region("nope"),
        Err(Error::NotFound { .. })
    ));
}

#[test]
fn test_create_refuses_an_existing_store_and_open_refuses_a_missing_one() {
    let p = SimPlatform::new(SimConfig::volatile(2));
    let s = create(&p);
    drop(s);
    assert!(matches!(
        Store::create(p.clone(), Path::new(DIR), opts()),
        Err(Error::AlreadyExists { .. })
    ));
    assert!(matches!(
        Store::open(p.clone(), Path::new("/missing"), opts()),
        Err(Error::NotFound { .. })
    ));
}

#[test]
fn test_second_open_is_locked() {
    let p = SimPlatform::new(SimConfig::volatile(3));
    let _s = create(&p);
    assert!(matches!(
        Store::open(p.clone(), Path::new(DIR), opts()),
        Err(Error::Locked)
    ));
}

#[test]
fn test_receipted_appends_survive_a_crash_that_loses_the_cache() {
    let p = SimPlatform::new(SimConfig::volatile(4));
    let s = create(&p);
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    let mut acked = Vec::new();
    for i in 0..20 {
        let rec = record(i, 100 + i as usize * 37);
        let (pos, rcpt) = wal.append_durable(&rec).unwrap();
        assert!(rcpt.durable_through() > pos.offset());
        acked.push((pos.offset(), rec));
    }
    // One more append that is never made durable.
    let _unsynced = wal.append(&record(99, 50)).unwrap();
    drop((wal, s));
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    let s = reopen(&p);
    let wal = s.append_region("wal").unwrap();
    for (off, rec) in &acked {
        let mut out = vec![0u8; rec.len()];
        wal.read(*off, &mut out).unwrap();
        assert_eq!(&out, rec, "acknowledged record at {off} lost or altered");
    }
}

#[test]
fn test_every_crash_subset_keeps_every_receipt() {
    // Writes after the last barrier may land in any combination; receipted
    // bytes must survive all of them.
    let p = SimPlatform::new(SimConfig::volatile(5));
    let s = create(&p);
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    let (pos, _r) = wal.append_durable(&record(1, 4096)).unwrap();
    for i in 0..4 {
        let _t = wal.append(&record(10 + i, 4096)).unwrap();
    }
    drop((wal, s));
    let base = p.with_world(|w| w.clone());
    for mask in 0..16u64 {
        let p2 = SimPlatform::new(SimConfig::volatile(5));
        p2.with_world(|w| {
            *w = base.clone();
            w.crash(&CrashMode::Subset(mask));
        });
        let s = reopen(&p2);
        let mut out = vec![0u8; 4096];
        s.append_region("wal")
            .unwrap()
            .read(pos.offset(), &mut out)
            .unwrap();
        assert_eq!(
            out,
            record(1, 4096),
            "receipted record lost under crash subset {mask:#b}"
        );
    }
}

#[test]
fn test_reopened_append_region_refuses_appends_until_resumed() {
    let p = SimPlatform::new(SimConfig::volatile(6));
    let s = create(&p);
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    wal.append_durable(&record(1, 5000)).unwrap();
    drop((wal, s));
    let s = reopen(&p);
    let wal = s.append_region("wal").unwrap();
    let e = wal.append(&record(2, 10)).unwrap_err();
    assert!(matches!(
        e,
        Error::NotWritten {
            cause: NotWrittenCause::NotPositioned,
            ..
        }
    ));
    // Recovery found valid data through byte 5000: resume after it.
    assert_eq!(wal.resume_at(5000).unwrap(), 8192);
    let (pos, _r) = wal.append_durable(&record(3, 10)).unwrap();
    assert_eq!(
        pos.offset(),
        8192,
        "never rewrites the block holding earlier data"
    );
}

#[test]
fn test_failed_flush_poisons_and_never_reports_success_again() {
    let mut cfg = SimConfig::volatile(7);
    cfg.faults = FaultPlan {
        flush_failure: FlushFailure::Drop,
        ..FaultPlan::default()
    };
    let p = SimPlatform::new(cfg);
    let s = create(&p);
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    wal.append_durable(&record(1, 100)).unwrap();
    // Arm the very next flush to fail and drop the cache (fsyncgate).
    p.with_world(|w| {
        let next = w.flushes() + 1;
        w.faults_mut().fail_flush = Some(next);
    });
    let r = wal.append_durable(&record(2, 100));
    assert!(matches!(r, Err(Error::DurabilityUnknown { .. })), "{r:?}");
    // Even though a later flush would now "succeed", the store stays poisoned.
    assert!(matches!(
        wal.append_durable(&record(3, 100)),
        Err(Error::Poisoned { .. })
    ));
    let pages = s.provision_page_region("pages", 1 << 16);
    assert!(matches!(pages, Err(Error::Poisoned { .. })));
}

#[test]
fn test_page_region_overwrites_and_rejects_bad_positions() {
    let p = SimPlatform::new(SimConfig::volatile(8));
    let s = create(&p);
    let pages = s.provision_page_region("pages", 64 * 1024).unwrap();
    let block = pages.block_size() as usize;
    let a = vec![0xA1u8; block];
    let b = vec![0xB2u8; block];
    let pos = pages.pos(block as u64).unwrap();
    pages.write_durable(pos, &a).unwrap();
    pages.write_durable(pos, &b).unwrap();
    let mut out = vec![0u8; block];
    pages.read(pos, &mut out).unwrap();
    assert_eq!(out, b);
    assert!(matches!(
        pages.pos(1),
        Err(Error::NotWritten {
            cause: NotWrittenCause::Misaligned,
            ..
        })
    ));
    assert!(matches!(
        pages.pos(1 << 20),
        Err(Error::NotWritten {
            cause: NotWrittenCause::OutOfBounds,
            ..
        })
    ));
    assert!(matches!(
        pages.write(pos, &a[..10]),
        Err(Error::NotWritten {
            cause: NotWrittenCause::Misaligned,
            ..
        })
    ));
    // A position from another region is foreign.
    let other = s.provision_page_region("other", 64 * 1024).unwrap();
    let foreign = other.pos(0).unwrap();
    assert!(matches!(
        pages.write(foreign, &a),
        Err(Error::NotWritten {
            cause: NotWrittenCause::ForeignPosition,
            ..
        })
    ));
}

#[test]
fn test_slot_is_old_or_new_after_every_crash_subset() {
    let p = SimPlatform::new(SimConfig::volatile(9));
    let s = create(&p);
    let m = s.provision_slot("manifest").unwrap();
    assert!(m.read().is_none());
    m.commit(b"version-1").unwrap();
    assert_eq!(m.read().as_deref(), Some(&b"version-1"[..]));
    drop((m, s));
    // Commit version 2 but crash before its barrier completes, in every way.
    let base = p.with_world(|w| w.clone());
    for keep in [CrashMode::LoseAll, CrashMode::KeepAll, CrashMode::Random] {
        let p2 = SimPlatform::new(SimConfig::volatile(9));
        p2.with_world(|w| *w = base.clone());
        let s = reopen(&p2);
        let m = s.slot("manifest").unwrap();
        let _ = m.commit(b"version-2-longer");
        drop((m, s));
        p2.with_world(|w| w.crash(&keep));
        let s = reopen(&p2);
        let got = s.slot("manifest").unwrap().read().unwrap();
        assert!(
            got == b"version-1" || got == b"version-2-longer",
            "mixed slot after {keep:?}: {got:?}"
        );
    }
}

#[test]
fn test_power_safe_device_needs_no_flush() {
    let p = SimPlatform::new(SimConfig::power_safe(10));
    let s = create(&p);
    assert_eq!(s.report().decision.class, DurabilityClass::PowerSafe);
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    let flushes_before = s.domain_stats().flushes;
    for i in 0..10 {
        wal.append_durable(&record(i, 512)).unwrap();
    }
    assert_eq!(
        s.domain_stats().flushes,
        flushes_before,
        "power-safe barriers must not flush"
    );
    drop((wal, s));
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    let s = reopen(&p);
    let wal = s.append_region("wal").unwrap();
    let mut out = vec![0u8; 512];
    wal.read(0, &mut out).unwrap();
    assert_eq!(out, record(0, 512));
}

#[test]
fn test_concurrent_appenders_share_flushes() {
    let p = SimPlatform::new(SimConfig::volatile(11));
    let s = Store::create(
        p.clone(),
        Path::new(DIR),
        StoreOptions {
            queues: 8,
            ..opts()
        },
    )
    .unwrap();
    let wal = s.provision_append_region("wal", 8 << 20).unwrap();
    let flushes_before = s.domain_stats().flushes;
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let wal = wal.clone();
            std::thread::spawn(move || {
                for i in 0..50 {
                    wal.append_durable(&record(t * 1000 + i, 300)).unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let flushes = s.domain_stats().flushes - flushes_before;
    assert!(
        flushes <= 400,
        "at most one flush per durable append ({flushes})"
    );
    assert_eq!(wal.durable_through(), wal.tail());
}

#[test]
fn test_read_only_open_modifies_nothing_and_cannot_write() {
    let p = SimPlatform::new(SimConfig::volatile(12));
    let s = create(&p);
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    wal.append_durable(&record(1, 1000)).unwrap();
    drop((wal, s));
    let before = p.with_world(|w| w.visible(1).map(<[u8]>::to_vec));
    let s = Store::open_readonly(p.clone(), Path::new(DIR), opts()).unwrap();
    let wal = s.append_region("wal").unwrap();
    let mut out = vec![0u8; 1000];
    wal.read(0, &mut out).unwrap();
    assert_eq!(out, record(1, 1000));
    assert!(matches!(
        wal.append(b"x"),
        Err(Error::NotWritten {
            cause: NotWrittenCause::ReadOnly,
            ..
        })
    ));
    drop((wal, s));
    let after = p.with_world(|w| w.visible(1).map(<[u8]>::to_vec));
    assert_eq!(before, after, "a read-only open changed the container");
}
