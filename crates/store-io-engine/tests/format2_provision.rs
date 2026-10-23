//! Provisioning in format 2 (spec §10, §12): placement, the refusals, and
//! crash consistency, each outcome judged by an independent audit of the
//! raw media (`format2_util::audit`) and by the next open.

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
use store_io_core::error::{CorruptionKind, Error, NotWrittenCause, ReserveTag};
use store_io_engine::{Store, StoreOptions};
use store_io_format::meta::{RegionKind, RegionState};
use store_io_sim::{CrashMode, SimConfig, SimPlatform};

const B: u64 = 4096;
const BASE: u64 = 2 * B + 2 * PAGES as u64 * TABLE;
const CAP: u64 = 64 * 1024;
const ONE: u64 = 2 * B + CAP;

fn fresh(seed: u64) -> SimPlatform {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    drop(create(&p));
    p
}

fn names(p: &SimPlatform) -> Vec<String> {
    audit(p).into_iter().map(|f| f.name).collect()
}

#[test]
fn test_each_kind_is_provisioned_audited_and_reopened() {
    let p = fresh(301);
    let s = open(&p);
    let log = s.provision_append_region("log", CAP).unwrap();
    s.provision_page_region("pages", 3 * B).unwrap();
    s.provision_slot("slot").unwrap();
    let data = vec![5u8; B as usize];
    log.append_durable(&data).unwrap();
    drop(log);
    drop(s);
    let found = audit(&p);
    let kinds: Vec<_> = found
        .iter()
        .map(|f| (f.name.as_str(), f.kind, f.data_size))
        .collect();
    assert_eq!(
        kinds,
        [
            ("log", RegionKind::Append, CAP),
            ("pages", RegionKind::Page, 3 * B),
            ("slot", RegionKind::Slot, 0),
        ]
    );
    // Ids `p << 32 | seq`, in order on page 0.
    assert_eq!(found.iter().map(|f| f.id).collect::<Vec<_>>(), [0, 1, 2]);
    let s = open(&p);
    assert!(s.report().unavailable.is_empty());
    let mut out = vec![0u8; B as usize];
    s.append_region("log").unwrap().read(0, &mut out).unwrap();
    assert_eq!(out, data);
    assert!(s.slot("slot").unwrap().read().is_none());
}

#[test]
fn test_a_full_page_moves_on_to_the_next() {
    let p = fresh(302);
    let s = open(&p);
    let per_page = store_io_format::meta2::entries_per_page(LOG2_TABLE) as usize;
    for i in 0..=per_page {
        s.provision_slot(&format!("s{i}")).unwrap();
    }
    drop(s);
    let found = audit(&p);
    assert_eq!(found.len(), per_page + 1);
    assert_eq!(
        found.last().unwrap().id,
        1 << 32,
        "the next page's first id"
    );
}

#[test]
fn test_a_taken_name_is_refused() {
    let p = fresh(303);
    // Unavailable (its header says released) and of an unknown kind.
    let mut gone = region(0, RegionKind::Append, "gone", BASE, CAP);
    gone.header.state = RegionState::Released;
    let mut newer = region(1, RegionKind::Append, "newer", BASE + ONE, CAP);
    newer.kind_raw = Some(9);
    write_regions(&p, 0, &[gone, newer]);
    let s = open(&p);
    s.provision_slot("live").unwrap();
    for name in ["live", "gone", "newer"] {
        assert!(
            matches!(s.provision_slot(name), Err(Error::AlreadyExists { .. })),
            "{name}"
        );
    }
}

#[test]
fn test_a_frozen_store_provisions_and_reserves_nothing() {
    let p = fresh(304);
    write_regions(&p, 0, &[region(0, RegionKind::Slot, "slot", BASE, 0)]);
    tear_newer_copy_of(&p, 0);
    let before = media(&p);
    let s = open(&p);
    assert!(s.report().metadata.frozen);
    let frozen = |e: Error| {
        matches!(
            e,
            Error::NotWritten {
                cause: NotWrittenCause::Frozen,
                ..
            }
        )
    };
    assert!(frozen(s.provision_slot("x").map(|_| ()).unwrap_err()));
    assert!(frozen(s.reserve(B, 0).map(|_| ()).unwrap_err()));
    drop(s);
    // Only the open's own writes (the volume record) changed the media.
    let after = media(&p);
    assert_eq!(after.len(), before.len());
}

#[test]
fn test_first_fit_reuses_a_hole() {
    let p = fresh(305);
    write_regions(
        &p,
        0,
        &[
            region(0, RegionKind::Append, "a", BASE, CAP),
            region(1, RegionKind::Append, "b", BASE + 2 * ONE, CAP),
        ],
    );
    let s = open(&p);
    s.provision_page_region("hole", CAP).unwrap();
    drop(s);
    let hole = audit(&p).into_iter().find(|f| f.name == "hole").unwrap();
    assert_eq!(hole.data_offset, BASE + ONE + 2 * B);
}

#[test]
fn test_a_large_regions_data_is_aligned() {
    let p = fresh(306);
    let s = open(&p);
    s.provision_slot("first").unwrap();
    s.provision_page_region("big", 2 << 20).unwrap();
    drop(s);
    let big = audit(&p).into_iter().find(|f| f.name == "big").unwrap();
    assert_eq!(big.data_offset % (1 << 20), 0);
}

#[test]
fn test_a_full_table_is_refused_before_any_io() {
    let p = SimPlatform::new(SimConfig::volatile(307));
    let o = StoreOptions {
        region_capacity: 1,
        ..opts()
    };
    drop(Store::create(p.clone(), Path::new(DIR), o.clone()).unwrap());
    let s = Store::open(p.clone(), Path::new(DIR), o).unwrap();
    let per_page = store_io_format::meta2::entries_per_page(LOG2_TABLE) as usize;
    for i in 0..per_page {
        s.provision_slot(&format!("s{i}")).unwrap();
    }
    let writes = p.with_world(|w| w.writes());
    assert!(matches!(
        s.provision_slot("one-more"),
        Err(Error::NoSpace {
            tag: ReserveTag::Table,
            ..
        })
    ));
    assert_eq!(p.with_world(|w| w.writes()), writes);
}

/// An entry past the file's end: growing the container would make zeros
/// of its lost tail, so growth is refused.
#[test]
fn test_growth_past_a_truncated_entry_is_refused() {
    let p = fresh(308);
    let mut short = region(0, RegionKind::Append, "short", BASE + 4 * ONE, CAP);
    short.extend = false;
    write_regions(&p, 0, &[short]);
    let s = open(&p);
    let truncated = |e: Error| {
        matches!(
            e,
            Error::Corruption {
                kind: CorruptionKind::Truncated,
                ..
            }
        )
    };
    assert!(truncated(s.reserve(CAP, 0).map(|_| ()).unwrap_err()));
    assert!(truncated(
        s.provision_page_region("grows", 16 * CAP)
            .map(|_| ())
            .unwrap_err()
    ));
}

/// Power fails at each write submission of a provisioning, keeping,
/// dropping or tearing what was cached: the next open sees the region
/// whole or not at all, and the media pass the audit.
#[test]
fn test_a_power_failure_at_any_point_of_a_provisioning() {
    let mut k = 1u64;
    loop {
        let mut finished = false;
        for seed in 0..4 {
            let p = fresh(400 + seed);
            let s = open(&p);
            s.provision_slot("before").unwrap();
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
                s.provision_append_region("new", CAP).map(|_| ())
            }));
            finished |= matches!(r, Ok(Ok(())));
            std::mem::forget(s);
            p.with_world(|w| {
                w.set_submit_probe(None);
                w.crash(&CrashMode::Random);
            });
            let step = format!("write {k} seed {seed}");
            let found = names(&p);
            assert!(found.contains(&"before".to_owned()), "{step}: {found:?}");
            let s = Store::open(p.clone(), Path::new(DIR), opts())
                .unwrap_or_else(|e| panic!("{step}: {e:?}"));
            assert!(
                s.report().unavailable.is_empty(),
                "{step}: {:?}",
                s.report().unavailable
            );
            if found.contains(&"new".to_owned()) {
                assert!(s.append_region("new").is_ok(), "{step}");
            } else {
                // The name is free again: provisioning it works.
                assert!(s.provision_append_region("new", CAP).is_ok(), "{step}");
                drop(s);
                audit(&p);
            }
        }
        if finished {
            break;
        }
        k += 1;
        assert!(k < 64, "a provisioning never finished");
    }
    // The fill, the header and the page write at least: each was a point
    // where power failed.
    assert!(
        k > 3,
        "only {k} submissions: the probe missed the provisioning"
    );
}

/// Each write and each flush of a provisioning fails in turn: the error
/// is returned, and after a power failure the media pass the audit and the
/// store opens without the region or with all of it.
#[test]
fn test_each_write_and_flush_failing_in_turn() {
    let steps = (1..=6u64)
        .map(|k| (Some(k), None))
        .chain((1..=4).map(|k| (None, Some(k))));
    for (write, flush) in steps {
        for seed in 0..3 {
            let p = fresh(500 + seed);
            let s = open(&p);
            p.with_world(|w| {
                let (wr, fl) = (w.writes(), w.flushes());
                let f = w.faults_mut();
                f.fail_write = write.map(|k| wr + k);
                f.fail_flush = flush.map(|k| fl + k);
            });
            let r = s.provision_page_region("new", 2 * B).map(|_| ());
            std::mem::forget(s);
            p.with_world(|w| {
                *w.faults_mut() = Default::default();
                w.crash(&CrashMode::Random);
            });
            let step = format!("{write:?} {flush:?} seed {seed}: {r:?}");
            let found = names(&p);
            let s = Store::open(p.clone(), Path::new(DIR), opts())
                .unwrap_or_else(|e| panic!("{step}: {e:?}"));
            assert!(s.report().unavailable.is_empty(), "{step}");
            if r.is_ok() {
                assert!(
                    found.contains(&"new".to_owned()),
                    "{step}: receipted, then lost"
                );
            }
            assert_eq!(
                s.page_region("new").is_ok(),
                found.contains(&"new".to_owned()),
                "{step}"
            );
        }
    }
}

/// The same with an entry of a kind this version does not know (listed
/// only by place, so no region carries the reason): growth is refused.
#[test]
fn test_growth_past_a_truncated_entry_of_an_unknown_kind_is_refused() {
    let p = fresh(309);
    let mut short = region(0, RegionKind::Append, "short", BASE + 4 * ONE, CAP);
    short.extend = false;
    short.kind_raw = Some(9);
    write_regions(&p, 0, &[short]);
    let s = open(&p);
    assert!(s.report().unavailable.is_empty());
    assert!(matches!(
        s.reserve(CAP, 0).map(|_| ()),
        Err(Error::Corruption {
            kind: CorruptionKind::Truncated,
            ..
        })
    ));
}

/// A reservation grows the container past everything allocated, so what
/// it promises is there.
#[test]
fn test_a_reservation_grows_past_everything_allocated() {
    let p = fresh(310);
    write_regions(&p, 0, &[region(0, RegionKind::Append, "a", BASE, CAP)]);
    let s = open(&p);
    assert_eq!(s.space().container, BASE + ONE);
    let r = s.reserve(4 * CAP, 0).unwrap();
    assert!(s.space().container >= BASE + ONE + 4 * CAP);
    drop(r);
}
