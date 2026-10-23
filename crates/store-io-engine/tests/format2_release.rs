//! Releasing in format 2 (spec §12): the page written without the entry
//! (the commit), the header Released, the data deallocated, free space the
//! complement again; and crash consistency, each outcome judged by the raw
//! audit (`format2_util::audit`) and the next open.

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

use format2_util::audit::audit;
use format2_util::craft::{region, tear_newer_copy_of, write_regions};
use format2_util::*;
use store_io_core::error::{Error, NotWrittenCause};
use store_io_engine::Store;
use store_io_format::meta::{RegionKind, RegionState};
use store_io_format::meta2::decode_region_header;
use store_io_format::slot::{Expect, FORMAT_2, SlotIndex, SlotStatus, validate};
use store_io_sim::{CrashMode, SimConfig, SimPlatform};

const B: u64 = 4096;
const BASE: u64 = 2 * B + 2 * PAGES as u64 * TABLE;
const CAP: u64 = 64 * 1024;

fn fresh(seed: u64) -> SimPlatform {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    drop(create(&p));
    p
}

fn names(p: &SimPlatform) -> Vec<String> {
    audit(p).into_iter().map(|f| f.name).collect()
}

/// The state the newer valid copy of the header pair at `at` says.
fn header_state(p: &SimPlatform, id: u64, at: [u64; 2]) -> Option<RegionState> {
    let m = media(p);
    let mut best: Option<(u64, RegionState)> = None;
    for (i, &off) in at.iter().enumerate() {
        let slot = &m[off as usize..(off + B) as usize];
        let e = Expect {
            volume_uuid: m[32..48].try_into().unwrap(),
            object_id: (1 << 62) | id,
            slot_offset: off,
            slot_index: if i == 0 { SlotIndex::A } else { SlotIndex::B },
            log2_slot_size: 12,
            format_major: FORMAT_2,
            numbered: false,
        };
        if let SlotStatus::Valid(info) = validate(slot, &e) {
            let v = decode_region_header(info.payload(slot), 12, u64::MAX).unwrap();
            if best.is_none_or(|b| info.generation > b.0) {
                best = Some((info.generation, v.header.state));
            }
        }
    }
    best.map(|b| b.1)
}

#[test]
fn test_a_released_region_is_gone_its_header_released_its_name_free() {
    let p = fresh(601);
    let s = open(&p);
    s.provision_append_region("keep", CAP).unwrap();
    let gone = s.provision_append_region("gone", CAP).unwrap();
    let space = s.space();
    gone.release().unwrap();
    let after = s.space();
    assert_eq!(after.released, space.released + 2 * B + CAP);
    assert_eq!(after.regions, space.regions - (2 * B + CAP));
    drop(s);
    assert_eq!(names(&p), ["keep"]);
    let at = BASE + 2 * B + CAP;
    assert_eq!(
        header_state(&p, 1, [at, at + B]),
        Some(RegionState::Released)
    );
    let s = open(&p);
    assert!(s.append_region("gone").is_err());
    assert!(s.report().unavailable.is_empty());
    assert!(s.provision_append_region("gone", CAP).is_ok());
}

#[test]
fn test_a_smaller_region_fills_the_freed_hole_with_a_new_id() {
    let p = fresh(602);
    let s = open(&p);
    s.provision_page_region("a", CAP).unwrap();
    let b = s.provision_page_region("b", CAP).unwrap();
    s.provision_page_region("c", CAP).unwrap();
    b.release().unwrap();
    s.provision_page_region("d", CAP / 2).unwrap();
    drop(s);
    let found = audit(&p);
    let d = found.iter().find(|f| f.name == "d").unwrap();
    assert_eq!(d.data_offset, BASE + (2 * B + CAP) + 2 * B, "in b's hole");
    // `next_seq` never decreases: b's id 1 is not handed out again.
    assert_eq!(d.id, 3);
}

#[test]
fn test_a_frozen_store_releases_nothing() {
    let p = fresh(603);
    write_regions(&p, 0, &[region(0, RegionKind::Append, "log", BASE, CAP)]);
    tear_newer_copy_of(&p, 0);
    let s = open(&p);
    let e = s.append_region("log").unwrap().release().unwrap_err();
    assert!(
        matches!(
            e,
            Error::NotWritten {
                cause: NotWrittenCause::Frozen | NotWrittenCause::ReadOnly,
                ..
            }
        ),
        "{e:?}"
    );
}

/// Power fails at each write submission of a release: the next open finds
/// the region whole (it was not committed) or not at all, and the media
/// pass the audit.
#[test]
fn test_a_power_failure_at_any_point_of_a_release() {
    let mut k = 1u64;
    loop {
        let mut finished = false;
        for seed in 0..4 {
            let p = fresh(700 + seed);
            let s = open(&p);
            s.provision_slot("keep").unwrap();
            let r = s.provision_append_region("gone", CAP).unwrap();
            let n = Arc::new(AtomicU64::new(0));
            p.with_world(|w| {
                let n = Arc::clone(&n);
                w.set_submit_probe(Some(Arc::new(move || {
                    if n.fetch_add(1, Ordering::SeqCst) + 1 == k {
                        panic!("power fails");
                    }
                })));
            });
            let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| r.release()));
            finished |= matches!(out, Ok(Ok(())));
            std::mem::forget(s);
            p.with_world(|w| {
                w.set_submit_probe(None);
                w.crash(&CrashMode::Random);
            });
            let step = format!("write {k} seed {seed}");
            let found = names(&p);
            assert!(found.contains(&"keep".to_owned()), "{step}: {found:?}");
            let s = Store::open(p.clone(), Path::new(DIR), opts())
                .unwrap_or_else(|e| panic!("{step}: {e:?}"));
            assert!(s.report().unavailable.is_empty(), "{step}");
            assert_eq!(
                s.append_region("gone").is_ok(),
                found.contains(&"gone".to_owned()),
                "{step}"
            );
            if !found.contains(&"gone".to_owned()) {
                // Its space and name are free again.
                assert!(s.provision_append_region("again", CAP).is_ok(), "{step}");
                assert!(s.provision_append_region("gone", CAP).is_ok(), "{step}");
                drop(s);
                audit(&p);
            }
        }
        if finished {
            break;
        }
        k += 1;
        assert!(k < 32, "a release never finished");
    }
    // The page write and the header write at least.
    assert!(k > 2, "only {k} submissions: the probe missed the release");
}

/// Each write and each flush of a release fails in turn: after a power
/// failure the media pass the audit, and the region is whole or gone.
#[test]
fn test_each_write_and_flush_of_a_release_failing_in_turn() {
    let steps = (1..=3u64)
        .map(|k| (Some(k), None))
        .chain((1..=3).map(|k| (None, Some(k))));
    for (write, flush) in steps {
        for seed in 0..3 {
            let p = fresh(800 + seed);
            let s = open(&p);
            let r = s.provision_page_region("gone", 2 * B).unwrap();
            p.with_world(|w| {
                let (wr, fl) = (w.writes(), w.flushes());
                let f = w.faults_mut();
                f.fail_write = write.map(|k| wr + k);
                f.fail_flush = flush.map(|k| fl + k);
            });
            let out = r.release();
            std::mem::forget(s);
            p.with_world(|w| {
                *w.faults_mut() = Default::default();
                w.crash(&CrashMode::Random);
            });
            let step = format!("{write:?} {flush:?} seed {seed}: {out:?}");
            let found = names(&p);
            if out.is_ok() {
                assert!(
                    !found.contains(&"gone".to_owned()),
                    "{step}: released, then back"
                );
            }
            let s = Store::open(p.clone(), Path::new(DIR), opts())
                .unwrap_or_else(|e| panic!("{step}: {e:?}"));
            assert!(s.report().unavailable.is_empty(), "{step}");
            assert_eq!(
                s.page_region("gone").is_ok(),
                found.contains(&"gone".to_owned()),
                "{step}"
            );
        }
    }
}

/// Another page lost freezes allocation: a region whose own page is
/// confirmed (so writable) still may not be released.
#[test]
fn test_a_store_frozen_by_another_page_refuses_a_release() {
    let p = fresh(604);
    write_regions(&p, 0, &[region(0, RegionKind::Append, "log", BASE, CAP)]);
    format2_util::craft::lose_page(&p, 1);
    let s = open(&p);
    assert_eq!(s.report().metadata.lost_pages, vec![1]);
    let log = s.append_region("log").unwrap();
    assert_eq!(
        log.resume_at(0).unwrap(),
        0,
        "the region itself is writable"
    );
    let e = log.release().unwrap_err();
    assert!(
        matches!(
            e,
            Error::NotWritten {
                cause: NotWrittenCause::Frozen,
                ..
            }
        ),
        "{e:?}"
    );
}
