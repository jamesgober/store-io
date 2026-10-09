//! Recycle and release on the deterministic simulator: generations, the
//! exclusion of in-flight writes, crash behaviour and space reuse.

// Test setup unwraps (a failure is the test failing) and ignores values a
// given test does not inspect.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use store_io_core::error::{Error, NotWrittenCause};
use store_io_engine::{CONTAINER, ScanItem, Store, StoreOptions};
use store_io_sim::{CrashMode, SimConfig, SimPlatform};

const DIR: &str = "/db";

fn opts() -> StoreOptions {
    StoreOptions {
        queues: 2,
        buffers_per_class: 8,
        max_io: 1 << 17,
        ..StoreOptions::default()
    }
}

fn create(p: &SimPlatform) -> Store<SimPlatform> {
    Store::create(p.clone(), Path::new(DIR), opts()).unwrap()
}

fn reopen(p: &SimPlatform) -> Store<SimPlatform> {
    Store::open(p.clone(), Path::new(DIR), opts()).unwrap()
}

fn not_written(e: &Result<impl std::fmt::Debug, Error>, want: NotWrittenCause) -> bool {
    matches!(e, Err(Error::NotWritten { cause, .. }) if *cause == want)
}

fn container_len(p: &SimPlatform) -> usize {
    p.with_world(|w| {
        w.media_of(&PathBuf::from(DIR), CONTAINER)
            .map_or(0, <[u8]>::len)
    })
}

#[test]
fn test_recycle_starts_a_new_generation_with_one_header_write() {
    let p = SimPlatform::new(SimConfig::volatile(41));
    let s = create(&p);
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    let old_ticket = wal.append(&[1; 5000]).unwrap();
    wal.sync_through(&old_ticket).unwrap();
    let g1 = wal.generation();
    let (w0, f0) = p.with_world(|w| (w.writes(), w.flushes()));
    let wal2 = wal.recycle().unwrap();
    let (w1, f1) = p.with_world(|w| (w.writes(), w.flushes()));
    assert_eq!((w1 - w0, f1 - f0), (1, 1), "one header write, one barrier");
    assert_eq!(wal2.generation().get(), g1.get() + 1);
    assert_eq!(wal2.tail(), 0);
    // Everything of the old generation is refused.
    assert!(not_written(
        &wal.append(b"x"),
        NotWrittenCause::StaleGeneration
    ));
    assert!(not_written(
        &wal.sync_through(&old_ticket),
        NotWrittenCause::StaleGeneration
    ));
    assert!(not_written(
        &wal.read(0, &mut [0; 10]),
        NotWrittenCause::StaleGeneration
    ));
    assert!(not_written(
        &wal.recycle(),
        NotWrittenCause::StaleGeneration
    ));
    assert!(not_written(
        &wal2.sync_through(&old_ticket),
        NotWrittenCause::StaleGeneration
    ));
    // The new generation appends from 0; lookups find it.
    let t = wal2.append(&[2; 100]).unwrap();
    assert_eq!(t.pos().offset(), 0);
    assert_eq!(
        s.append_region("wal").unwrap().generation(),
        wal2.generation()
    );
    wal2.sync_through(&t).unwrap();
    // After a crash the region reopens in the new generation, with the old
    // bytes still on disk past the new data (recycle wrote nothing else).
    drop((wal, wal2, s));
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    let s = reopen(&p);
    let wal = s.append_region("wal").unwrap();
    assert_eq!(wal.generation().get(), g1.get() + 1);
    let mut out = vec![0u8; 8192];
    wal.read(0, &mut out).unwrap();
    assert!(out[..100].iter().all(|&b| b == 2));
    // The old record's second block: its bytes, then its zero padding.
    assert!(
        out[4096..5000].iter().all(|&b| b == 1),
        "old generation bytes untouched"
    );
    assert!(out[5000..].iter().all(|&b| b == 0));
}

#[test]
fn test_a_crash_during_recycle_leaves_the_old_or_the_new_generation() {
    let p = SimPlatform::new(SimConfig::volatile(42));
    let s = create(&p);
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    wal.append_durable(&[1; 100]).unwrap();
    let g1 = wal.generation().get();
    // Fail the recycle's barrier: the header write may or may not survive.
    p.with_world(|w| {
        let next = w.flushes() + 1;
        w.faults_mut().fail_flush = Some(next);
    });
    assert!(matches!(
        wal.recycle(),
        Err(Error::DurabilityUnknown { .. })
    ));
    drop((wal, s));
    let base = p.with_world(|w| w.clone());
    for mask in 0..2u64 {
        let p2 = SimPlatform::new(SimConfig::volatile(42));
        p2.with_world(|w| {
            *w = base.clone();
            w.faults_mut().fail_flush = None;
            w.crash(&CrashMode::Subset(mask));
        });
        let s = reopen(&p2);
        let g = s.append_region("wal").unwrap().generation().get();
        assert!(g == g1 || g == g1 + 1, "generation {g} after crash {mask}");
    }
}

#[test]
fn test_recycle_waits_out_writers_and_no_old_write_lands_after_it() {
    let p = SimPlatform::new(SimConfig::volatile(43));
    let s = create(&p);
    let wal = s.provision_append_region("wal", 64 << 20).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let writers: Vec<_> = (0..4)
        .map(|_| {
            let wal = wal.clone();
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut ok = 0u32;
                while !stop.load(Ordering::SeqCst) {
                    match wal.append(&[0xAB; 3000]) {
                        Ok(_) => ok += 1,
                        Err(Error::NotWritten {
                            cause: NotWrittenCause::StaleGeneration,
                            ..
                        }) => break,
                        Err(e) => panic!("{e:?}"),
                    }
                }
                ok
            })
        })
        .collect();
    while wal.tail() < 64 * 4096 {
        std::thread::yield_now();
    }
    let wal2 = wal.recycle().unwrap();
    let writes_after = p.with_world(|w| w.writes());
    stop.store(true, Ordering::SeqCst);
    for h in writers {
        h.join().unwrap();
    }
    assert_eq!(
        p.with_world(|w| w.writes()),
        writes_after,
        "an old-generation write landed after the recycle"
    );
    // The new generation starts clean at 0.
    let t = wal2.append(&[0xCD; 10]).unwrap();
    assert_eq!(t.pos().offset(), 0);
}

#[test]
fn test_page_region_recycle_refuses_old_positions() {
    let p = SimPlatform::new(SimConfig::volatile(44));
    let s = create(&p);
    let pages = s.provision_page_region("pages", 1 << 20).unwrap();
    let pos = pages.pos(4096).unwrap();
    pages.write_durable(pos, &[1; 4096]).unwrap();
    let pages2 = pages.recycle().unwrap();
    assert!(not_written(
        &pages2.write(pos, &[2; 4096]),
        NotWrittenCause::StaleGeneration
    ));
    assert!(not_written(
        &pages.write(pos, &[2; 4096]),
        NotWrittenCause::StaleGeneration
    ));
    let pos2 = pages2.pos(4096).unwrap();
    pages2.write_durable(pos2, &[3; 4096]).unwrap();
    let mut out = [0u8; 4096];
    pages2.read(pos2, &mut out).unwrap();
    assert!(out.iter().all(|&b| b == 3));
}

#[test]
fn test_release_frees_the_region_and_its_space_is_reused_clean() {
    let p = SimPlatform::new(SimConfig::volatile(45));
    let s = create(&p);
    let a = s.provision_append_region("a", 1 << 20).unwrap();
    let b = s.provision_page_region("b", 1 << 20).unwrap();
    a.append_durable(&[0x77; 1 << 19]).unwrap();
    let held = a.clone();
    let len = container_len(&p);
    a.release().unwrap();
    assert!(not_written(&held.append(b"x"), NotWrittenCause::NotReady));
    assert!(not_written(
        &held.read(0, &mut [0; 4]),
        NotWrittenCause::NotReady
    ));
    assert!(not_written(
        &held.scan(0, |_| ControlFlow::Continue(())),
        NotWrittenCause::NotReady
    ));
    assert!(matches!(s.append_region("a"), Err(Error::NotFound { .. })));
    assert_eq!(s.regions().len(), 1);
    // Other regions are untouched.
    b.write_durable(b.pos(0).unwrap(), &[1; 4096]).unwrap();
    // The name and the space are free again: same size, same place, refilled.
    let c = s.provision_append_region("a", 1 << 20).unwrap();
    assert_eq!(container_len(&p), len, "released space was reused");
    let mut stale = false;
    c.scan(0, |item| {
        if let ScanItem::Data { bytes, .. } = item {
            stale |= bytes.contains(&0x77);
        }
        ControlFlow::Continue(())
    })
    .unwrap();
    assert!(!stale, "released data came back");
    drop((b, c, held, s));
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    let s = reopen(&p);
    assert_eq!(s.regions().len(), 2);
}

#[test]
fn test_a_release_interrupted_after_the_table_flip_still_frees_the_space() {
    let p = SimPlatform::new(SimConfig::volatile(46));
    let s = create(&p);
    let a = s.provision_append_region("a", 1 << 20).unwrap();
    s.provision_append_region("keep", 1 << 20).unwrap();
    let len = container_len(&p);
    // The table flip's barrier succeeds; the header's barrier fails.
    p.with_world(|w| {
        let next = w.flushes() + 2;
        w.faults_mut().fail_flush = Some(next);
    });
    assert!(matches!(a.release(), Err(Error::DurabilityUnknown { .. })));
    drop(s);
    p.with_world(|w| {
        w.faults_mut().fail_flush = None;
        w.crash(&CrashMode::LoseAll);
    });
    let s = reopen(&p);
    assert!(matches!(s.append_region("a"), Err(Error::NotFound { .. })));
    s.provision_append_region("again", 1 << 20).unwrap();
    assert_eq!(container_len(&p), len, "the released space was not leaked");
}

#[test]
fn test_slots_cannot_be_recycled_or_released_and_reads_survive_release() {
    let p = SimPlatform::new(SimConfig::volatile(47));
    let s = create(&p);
    s.provision_slot("manifest").unwrap();
    let pages = s.provision_page_region("pages", 1 << 20).unwrap();
    let ro = pages.clone();
    pages.release().unwrap();
    assert!(not_written(&ro.recycle(), NotWrittenCause::NotReady));
    assert!(not_written(
        &ro.clone().release(),
        NotWrittenCause::NotReady
    ));
}
