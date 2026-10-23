//! Recycle and slot commit in format 2 (spec §8, §12): one header write
//! each under the region lock; recycle moves the region generation on and
//! keeps exactly the entry's extents, a slot commit carries the object
//! after the header's fixed part. Crash outcomes are judged by the raw
//! media and the next open.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

mod format2_util;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use format2_util::audit::{audit, header_of};
use format2_util::craft::{region, tear_newer_copy_of, write_regions};
use format2_util::*;
use store_io_core::error::{Error, NotWrittenCause};
use store_io_engine::Store;
use store_io_format::meta::RegionKind;
use store_io_sim::{CrashMode, SimConfig, SimPlatform};

const B: u64 = 4096;
const BASE: u64 = 2 * B + 2 * PAGES as u64 * TABLE;
const CAP: u64 = 64 * 1024;

fn fresh(seed: u64) -> SimPlatform {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    drop(create(&p));
    p
}

fn cause(e: &Error) -> Option<NotWrittenCause> {
    match e {
        Error::NotWritten { cause, .. } => Some(*cause),
        _ => None,
    }
}

#[test]
fn test_recycle_moves_the_region_generation_on() {
    let p = fresh(1001);
    let s = open(&p);
    let log = s.provision_append_region("log", CAP).unwrap();
    log.append_durable(&[1u8; B as usize]).unwrap();
    let next = log.recycle().unwrap();
    // The old handle belongs to the old generation.
    assert_eq!(
        cause(&log.append_durable(&[2u8; B as usize]).unwrap_err()),
        Some(NotWrittenCause::StaleGeneration)
    );
    // The new one starts again at 0.
    let (pos, _) = next.append_durable(&[3u8; B as usize]).unwrap();
    assert_eq!(pos.offset(), 0);
    drop((log, next));
    drop(s);
    let h = header_of(&p, 0, [BASE, BASE + B]).unwrap();
    assert_eq!(h.header.region_generation, 2);
    assert_eq!(h.extents.len(), 1);
    let s = open(&p);
    assert!(s.report().unavailable.is_empty());
    assert_eq!(s.append_region("log").unwrap().generation().get(), 2);
    audit(&p);
}

/// Recycle writes exactly the entry's extents (an extra one, left by a
/// growth cut short, is dropped) and keeps the may-ignore flags, fill and
/// key.
#[test]
fn test_recycle_keeps_the_entry_s_extents_and_the_flags() {
    let p = fresh(1002);
    let mut r = region(0, RegionKind::Append, "log", BASE, CAP);
    r.header.compat_flags = 0x40;
    r.header.extent_count = 2;
    r.extents.push((BASE + 4 * CAP, 4 * B));
    write_regions(&p, 0, &[r]);
    let s = open(&p);
    assert_eq!(s.report().metadata.extra_extents.len(), 1);
    s.append_region("log").unwrap().recycle().unwrap();
    drop(s);
    let h = header_of(&p, 0, [BASE, BASE + B]).unwrap();
    assert_eq!(h.extents, [(BASE + 2 * B, CAP)]);
    assert_eq!(h.header.compat_flags, 0x40);
    assert_eq!(h.header.region_generation, 2);
    let s = open(&p);
    assert!(s.report().metadata.extra_extents.is_empty());
}

/// Power fails at a recycle's header write: the next open finds the region
/// at its old generation or its new one, never unavailable.
#[test]
fn test_a_power_failure_inside_a_recycle() {
    let mut seen = [false; 2];
    for seed in 0..8 {
        let p = fresh(1100 + seed);
        let s = open(&p);
        let log = s.provision_append_region("log", CAP).unwrap();
        let q = p.clone();
        p.with_world(|w| {
            w.set_submit_probe(Some(Arc::new(move || {
                // After the header write's submission: at its flush.
                if q.with_world(|w| w.cached_writes()) > 0 {
                    panic!("power fails");
                }
            })));
        });
        let r =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| log.recycle().map(|_| ())));
        std::mem::forget((s, log));
        p.with_world(|w| {
            w.set_submit_probe(None);
            w.crash(&CrashMode::Random);
        });
        let step = format!("seed {seed}: {:?}", r.map(|x| x.is_ok()));
        let s = Store::open(p.clone(), Path::new(DIR), opts())
            .unwrap_or_else(|e| panic!("{step}: {e:?}"));
        assert!(s.report().unavailable.is_empty(), "{step}");
        let g = s.append_region("log").unwrap().generation().get();
        assert!(g == 1 || g == 2, "{step}: generation {g}");
        seen[(g - 1) as usize] = true;
    }
    assert_eq!(
        seen,
        [true, true],
        "the seeds never kept (or never lost) the write"
    );
}

#[test]
fn test_a_slot_commit_is_read_back_and_kept() {
    let p = fresh(1003);
    let s = open(&p);
    let slot = s.provision_slot("slot").unwrap();
    assert_eq!(slot.read(), None);
    slot.commit(b"abc").unwrap();
    assert_eq!(slot.read().as_deref(), Some(&b"abc"[..]));
    slot.commit(b"de").unwrap();
    drop(slot);
    drop(s);
    let s = open(&p);
    let slot = s.slot("slot").unwrap();
    assert_eq!(slot.read().as_deref(), Some(&b"de"[..]));
    drop(s);
    let h = header_of(&p, 0, [BASE, BASE + B]).unwrap();
    assert_eq!((h.header.region_generation, h.slot_generation), (1, 3));
    assert_eq!(h.object, b"de");
    audit(&p);
}

#[test]
fn test_a_slot_takes_exactly_its_capacity() {
    let p = fresh(1004);
    let s = open(&p);
    let slot = s.provision_slot("slot").unwrap();
    let cap = slot.capacity();
    assert_eq!(cap, B as usize - 192);
    let full = vec![7u8; cap];
    slot.commit(&full).unwrap();
    assert_eq!(slot.read().unwrap(), full);
    assert_eq!(
        cause(&slot.commit(&vec![7u8; cap + 1]).unwrap_err()),
        Some(NotWrittenCause::TooLarge)
    );
    drop(slot);
    drop(s);
    assert_eq!(open(&p).slot("slot").unwrap().read().unwrap(), full);
}

/// A slot commit keeps the header's may-ignore flags and region generation.
#[test]
fn test_a_slot_commit_keeps_the_fixed_part() {
    let p = fresh(1005);
    let mut r = region(0, RegionKind::Slot, "slot", BASE, 0);
    r.header.compat_flags = 0x40;
    r.header.region_generation = 5;
    write_regions(&p, 0, &[r]);
    let s = open(&p);
    s.slot("slot").unwrap().commit(b"x").unwrap();
    drop(s);
    let h = header_of(&p, 0, [BASE, BASE + B]).unwrap();
    assert_eq!(
        (h.header.compat_flags, h.header.region_generation),
        (0x40, 5)
    );
    assert_eq!(h.object, b"x");
}

/// Power fails at any point of a slot commit, keeping, dropping or tearing
/// what was cached: the next open reads the old object or the new one.
#[test]
fn test_every_crash_of_a_slot_commit_reads_old_or_new() {
    let mut seen = [false; 2];
    for k in 1..=3u64 {
        for seed in 0..8 {
            let p = fresh(1200 + seed);
            let s = open(&p);
            let slot = s.provision_slot("slot").unwrap();
            slot.commit(b"old").unwrap();
            let n = Arc::new(AtomicU64::new(0));
            p.with_world(|w| {
                let n = Arc::clone(&n);
                w.set_submit_probe(Some(Arc::new(move || {
                    if n.fetch_add(1, Ordering::SeqCst) + 1 == k {
                        panic!("power fails");
                    }
                })));
            });
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                slot.commit(b"new").map(|_| ())
            }));
            std::mem::forget((s, slot));
            p.with_world(|w| {
                w.set_submit_probe(None);
                w.crash(&CrashMode::Random);
            });
            let step = format!("submission {k} seed {seed}: {:?}", r.map(|x| x.is_ok()));
            let s = Store::open(p.clone(), Path::new(DIR), opts())
                .unwrap_or_else(|e| panic!("{step}: {e:?}"));
            let got = s.slot("slot").unwrap().read();
            assert!(
                got.as_deref() == Some(&b"old"[..]) || got.as_deref() == Some(&b"new"[..]),
                "{step}: {got:?}"
            );
            seen[usize::from(got.as_deref() == Some(&b"new"[..]))] = true;
        }
    }
    assert_eq!(seen, [true, true], "never old, or never new");
}

/// On a stale page a region is read-only: no recycle, no slot commit.
#[test]
fn test_a_stale_page_refuses_recycle_and_slot_commit() {
    let p = fresh(1006);
    write_regions(
        &p,
        0,
        &[
            region(0, RegionKind::Append, "log", BASE, CAP),
            region(1, RegionKind::Slot, "slot", BASE + 2 * B + CAP, 0),
        ],
    );
    tear_newer_copy_of(&p, 0);
    let s = open(&p);
    let ro = Some(NotWrittenCause::ReadOnly);
    assert_eq!(
        cause(&s.append_region("log").unwrap().recycle().err().unwrap()),
        ro
    );
    assert_eq!(
        cause(&s.slot("slot").unwrap().commit(b"x").unwrap_err()),
        ro
    );
}
