//! Crash enumeration over format 2's changes (v0.5 E7, spec §12): each
//! operation crashed before every step it makes and after it returns,
//! with every fate of the writes still cached; each outcome is the
//! operation's before or after (each reached), the media audits clean, and
//! the store opens the same way twice.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use,
    // Each enumeration reports how much it covered (E7 plan §3b).
    clippy::print_stderr
)]

mod format2_util;

use std::cell::RefCell;
use std::path::Path;

use format2_util::audit::{
    assert_closed_in, audit, audit_allowing_extra, audit_closed, audit_in, audit_space,
    audit_space_in, header_of,
};
use format2_util::craft::{region, tear_newer_copy_of, write_regions};
use format2_util::enumerate::{
    Covered, Outcomes, agree, enumerate, enumerate_settled, enumerate_with, fates,
};
use format2_util::*;
use store_io_core::error::Error;
use store_io_engine::{CONTAINER, CopyOptions, RegionOptions, Store, StoreOptions, copy_store};
use store_io_format::fill::fill_keyed_v1;
use store_io_format::meta::RegionKind;
use store_io_platform::Platform;
use store_io_sim::{SimConfig, SimPlatform};

const B: u64 = 4096;
/// Where the first region of a crafted table starts.
const BASE: u64 = 2 * B + 2 * PAGES as u64 * TABLE;
const CAP: u64 = 16 * B;

fn fresh(seed: u64) -> SimPlatform {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    drop(create(&p));
    p
}

fn created(seed: u64) -> (SimPlatform, Store<SimPlatform>) {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    let s = create(&p);
    (p, s)
}

fn not_found(e: &Error) -> bool {
    matches!(e, Error::NotFound { .. })
}

fn report(name: &str, covered: Covered) {
    eprintln!("{name}: {covered:?}");
    assert!(
        covered.points > 1 && covered.cases > covered.points,
        "{name}: too little covered"
    );
}

/// The fates judged at a crash point: every combination of a few cached
/// writes, each tear; past ten writes, the structured sample; never one
/// twice.
#[test]
fn test_the_fates_of_a_crash_point() {
    use store_io_sim::Fate::{Kept, Lost, Torn};
    let (few, sampled) = fates(&[1, 2, 1], 0);
    assert!(!sampled);
    // 8 combinations, and the 2-block write torn after 1 with the rest
    // kept and with the rest lost.
    assert_eq!(few.len(), 10);
    assert!(few.contains(&vec![Kept, Torn(1), Kept]));
    assert!(few.contains(&vec![Lost, Torn(1), Lost]));
    let (many, sampled) = fates(&[1; 12], 5);
    assert!(sampled);
    let all = |f| vec![f; 12];
    assert!(many.contains(&all(Kept)) && many.contains(&all(Lost)));
    for i in 0..12 {
        let mut alone = all(Lost);
        alone[i] = Kept;
        let mut but = all(Kept);
        but[i] = Lost;
        let mut prefix = all(Lost);
        prefix[..=i].fill(Kept);
        assert!(many.contains(&alone) && many.contains(&but) && many.contains(&prefix));
    }
    let unique: std::collections::HashSet<_> = many.iter().collect();
    assert_eq!(unique.len(), many.len(), "a fate twice");
    assert_eq!(fates(&[1; 12], 5).0, many, "seeded: the same each time");
}

/// A create (spec §12: every page durable, then the volume record), in
/// three geometries (one table page, this suite's three, and about 30, past
/// exhaustive combination). As the store left the media: nothing, or a
/// store that opens empty (the file's name is durable only with the
/// directory sync after the volume record). With the file system's
/// metadata settled (the name and length durable early): also a crashed
/// create, which the next create takes over. Once it has returned: a store.
#[test]
fn test_every_crash_of_a_create() {
    for capacity in [1, 400, 4096] {
        let o = store_io_engine::StoreOptions {
            region_capacity: capacity,
            ..opts()
        };
        let seed = 1730 + u64::from(capacity);
        let seen = Outcomes::default();
        let covered = enumerate_with(
            seed,
            true,
            || {
                (
                    SimPlatform::new(SimConfig::volatile(seed)),
                    RefCell::new(None),
                )
            },
            |p, s| {
                // Kept open: the power fails with the new store open.
                *s.borrow_mut() = Store::create(p.clone(), Path::new(DIR), o.clone()).ok();
            },
            |p, case| {
                if let Ok(s) = Store::open(p.clone(), Path::new(DIR), o.clone()) {
                    assert!(s.regions().is_empty(), "{case:?}");
                    seen.saw(case, "store");
                } else {
                    let file = p.with_world(|w| w.media_of(&DIR.into(), CONTAINER).is_some());
                    assert!(
                        !file || case.settled,
                        "a crashed create unsettled: {case:?}"
                    );
                    seen.saw(case, if file { "taken over" } else { "none" });
                    drop(
                        Store::create(p.clone(), Path::new(DIR), o.clone())
                            .unwrap_or_else(|e| panic!("{case:?}: {e:?}")),
                    );
                }
                audit_closed(p);
            },
        );
        report(&format!("create {capacity}"), covered);
        seen.expect(&["none", "store", "taken over"], "store");
    }
}

/// A crashed create on the media: the power cut before the create's `k`-th
/// step, its file's name and length settled, every cached write lost. Only
/// if that left a container no open accepts (a create to take over).
fn crashed_create(seed: u64, k: usize) -> Option<SimPlatform> {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    let steps = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = std::sync::Arc::clone(&steps);
    p.with_world(|w| {
        w.set_step_probe(Some(std::sync::Arc::new(move || {
            c.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1 == k
        })));
    });
    drop(Store::create(p.clone(), Path::new(DIR), opts()));
    p.with_world(|w| {
        w.set_step_probe(None);
        w.settle_metadata();
        w.crash(&store_io_sim::CrashMode::LoseAll);
        w.restore_power();
    });
    let file = p.with_world(|w| w.media_of(&DIR.into(), CONTAINER).is_some());
    (file && Store::open(p.clone(), Path::new(DIR), opts()).is_err()).then_some(p)
}

/// The create that takes over a crashed one (spec §12): crashed at every
/// point itself, it leaves a crashed create again (taken over by the next)
/// or the store, never a container an open half-accepts.
#[test]
fn test_every_crash_of_a_takeover() {
    let k = (1..64)
        .find(|&k| crashed_create(1743, k).is_some())
        .expect("no crash leaves a create to take over");
    let seen = Outcomes::default();
    let covered = enumerate_with(
        1743,
        true,
        || (crashed_create(1743, k).unwrap(), RefCell::new(None)),
        |p, s| {
            *s.borrow_mut() = Store::create(p.clone(), Path::new(DIR), opts()).ok();
        },
        |p, case| {
            if let Ok(s) = Store::open(p.clone(), Path::new(DIR), opts()) {
                assert!(s.regions().is_empty(), "{case:?}");
                seen.saw(case, "store");
            } else {
                let file = p.with_world(|w| w.media_of(&DIR.into(), CONTAINER).is_some());
                seen.saw(case, if file { "taken over" } else { "none" });
                drop(
                    Store::create(p.clone(), Path::new(DIR), opts())
                        .unwrap_or_else(|e| panic!("{case:?}: {e:?}")),
                );
            }
            audit_closed(p);
        },
    );
    report("takeover", covered);
    seen.expect(&["store", "taken over"], "store");
}

/// Provisioning (spec §12: before its entry is written, no region and
/// nothing of it allocated; after, the region whole) of each kind: a page
/// region, an append region, a slot, a capped page region, and one from a
/// tagged reservation. Never a third state.
#[test]
fn test_every_crash_of_each_kind_of_provisioning() {
    for (kind, name) in ["page", "append", "slot", "capped", "reserved"]
        .into_iter()
        .enumerate()
    {
        let seed = 1700 + kind as u64;
        let seen = Outcomes::default();
        let covered = enumerate_settled(
            seed,
            || created(seed),
            |s| match kind {
                0 => {
                    s.provision_page_region("r", 8 * B);
                }
                1 => {
                    s.provision_append_region("r", 8 * B);
                }
                2 => {
                    s.provision_slot("r");
                }
                3 => {
                    let o = RegionOptions::default().size(4 * B).max_size(8 * B);
                    s.provision_page_region_with("r", o);
                }
                _ => {
                    if let Ok(mut res) = s.reserve(16 * B, 7) {
                        res.provision_page_region("r", 8 * B);
                    }
                }
            },
            |p, case| {
                let s = open(p);
                let found = match kind {
                    0 | 4 => s.page_region("r").map(|r| r.len() == 8 * B),
                    1 => s.append_region("r").map(|r| r.capacity() >= 8 * B),
                    2 => s.slot("r").map(|_| true),
                    _ => s.page_region("r").map(|r| r.len() == 4 * B),
                };
                match found {
                    Ok(whole) => {
                        assert!(whole, "{name} {case:?}");
                        seen.saw(case, "whole");
                    }
                    Err(e) => {
                        assert!(not_found(&e), "{name} {case:?}: {e:?}");
                        seen.saw(case, "none");
                    }
                }
                assert!(s.unavailable_regions().is_empty(), "{name} {case:?}");
                audit_space(p, &s);
                drop(s);
                audit_closed(p);
            },
        );
        report(&format!("provisioning {name}"), covered);
        seen.expect(&["none", "whole"], "whole");
    }
}

/// Provisioning with a keyed fill into the space a released region left
/// (spec §12: the fill durable before the header; first fit reuses freed
/// space): whenever the region exists after a crash, its data reads as its
/// fill pattern, never the old region's bytes or zeros.
#[test]
fn test_every_crash_of_a_keyed_provisioning_into_freed_space() {
    const N: u64 = 8 * B;
    let keyed = || StoreOptions {
        keyed_fill: true,
        ..opts()
    };
    let seen = Outcomes::default();
    let reused = std::cell::Cell::new(None);
    let covered = enumerate_settled(
        1730,
        || {
            let p = SimPlatform::new(SimConfig::volatile(1730));
            let s = Store::create(p.clone(), Path::new(DIR), keyed()).unwrap();
            let old = s.provision_page_region("old", N).unwrap();
            old.write_durable(old.pos(0).unwrap(), &[0xAB; N as usize])
                .unwrap();
            let at = audit(&p).into_iter().find(|f| f.name == "old").unwrap();
            reused.set(Some(at.data_offset));
            old.release().unwrap();
            (p, s)
        },
        |s| {
            s.provision_page_region("r", N);
        },
        |p, case| {
            let s = open(p);
            match s.page_region("r") {
                Ok(r) => {
                    let f = audit(p).into_iter().find(|f| f.name == "r").unwrap();
                    assert_eq!(Some(f.data_offset), reused.get(), "{case:?}: not first fit");
                    let h = header_of(p, f.id, f.header_at).unwrap();
                    let mut want = vec![0u8; N as usize];
                    fill_keyed_v1(&h.header.fill_key, 0, &mut want);
                    let mut got = vec![0u8; N as usize];
                    r.read(r.pos(0).unwrap(), &mut got).unwrap();
                    let at = got.iter().zip(&want).position(|(g, w)| g != w);
                    assert_eq!(at, None, "{case:?}: the fill is not durable");
                    seen.saw(case, "whole");
                }
                Err(e) => {
                    assert!(not_found(&e), "{case:?}: {e:?}");
                    seen.saw(case, "none");
                }
            }
            assert!(s.unavailable_regions().is_empty(), "{case:?}");
            audit_space(p, &s);
            drop(s);
            audit_closed(p);
        },
    );
    report("keyed provisioning into freed space", covered);
    seen.expect(&["none", "whole"], "whole");
}

/// Two provisionings committed to the same table page, one after the other:
/// a crash in the second never loses the first (the page's older copy holds
/// it); never the second without the first.
#[test]
fn test_every_crash_of_two_commits_to_one_page() {
    let seen = Outcomes::default();
    let covered = enumerate_settled(
        1736,
        || created(1736),
        |s| {
            if s.provision_page_region("a", 4 * B).is_ok() {
                s.provision_append_region("b", 4 * B);
            }
        },
        |p, case| {
            let s = open(p);
            let a = s.page_region("a").map(|r| r.len());
            let b = s.append_region("b").map(|r| r.capacity());
            match (&a, &b) {
                (Err(ea), Err(eb)) => {
                    assert!(not_found(ea) && not_found(eb), "{case:?}");
                    seen.saw(case, "none");
                }
                (Ok(n), Err(eb)) => {
                    assert_eq!(*n, 4 * B, "{case:?}");
                    assert!(not_found(eb), "{case:?}: {eb:?}");
                    seen.saw(case, "a");
                }
                (Ok(n), Ok(c)) => {
                    assert_eq!(*n, 4 * B, "{case:?}");
                    assert!(*c >= 4 * B, "{case:?}");
                    seen.saw(case, "both");
                }
                (Err(e), Ok(_)) => panic!("{case:?}: b without a ({e:?})"),
            }
            let pages: std::collections::HashSet<u64> =
                audit(p).iter().map(|f| f.id >> 32).collect();
            assert!(pages.len() <= 1, "{case:?}: not one page: {pages:?}");
            assert!(s.unavailable_regions().is_empty(), "{case:?}");
            audit_space(p, &s);
            drop(s);
            audit_closed(p);
        },
    );
    report("two commits to one page", covered);
    seen.expect(&["a", "both", "none"], "both");
}

/// Growth within capacity (spec §12: one table write): the old size or the
/// new, the capacity as it was.
#[test]
fn test_every_crash_of_a_growth_within_capacity() {
    let seen = Outcomes::default();
    let covered = enumerate(
        1720,
        || {
            let p = fresh(1720);
            write_regions(&p, 0, &[region(0, RegionKind::Page, "p", BASE, CAP)]);
            let s = open(&p);
            (p, s)
        },
        |s| {
            s.page_region("p").unwrap().grow(CAP);
        },
        |p, case| {
            let s = open(p);
            let r = s.page_region("p").unwrap();
            match r.len() {
                n if n == CAP / 2 => seen.saw(case, "old"),
                CAP => seen.saw(case, "new"),
                n => panic!("{case:?}: {n}"),
            }
            assert_eq!(r.capacity(), CAP, "{case:?}");
            drop(r);
            audit_space(p, &s);
            drop(s);
            audit_closed(p);
        },
    );
    report("growth within capacity", covered);
    seen.expect(&["new", "old"], "new");
}

/// Growth beyond capacity (spec §12: the new extent filled, its header,
/// then the table): the old size (the header possibly one extent ahead) or
/// the new, never a region the open refuses; also with the file system's
/// metadata settled (the container's new length durable early).
#[test]
fn test_every_crash_of_a_growth_beyond_capacity() {
    let seen = Outcomes::default();
    let covered = enumerate_settled(
        1721,
        || {
            let (p, s) = created(1721);
            s.provision_page_region("p", CAP).unwrap();
            (p, s)
        },
        |s| {
            s.page_region("p").unwrap().grow(CAP + 2 * B);
        },
        |p, case| {
            let s = open(p);
            assert!(s.unavailable_regions().is_empty(), "{case:?}");
            let r = s.page_region("p").unwrap();
            match r.len() {
                CAP => seen.saw(case, "old"),
                n if n == CAP + 2 * B => seen.saw(case, "new"),
                n => panic!("{case:?}: {n}"),
            }
            drop(r);
            audit_space(p, &s);
            drop(s);
            audit_allowing_extra(p);
            assert_closed_in(p, DIR);
        },
    );
    report("growth beyond capacity", covered);
    seen.expect(&["new", "old"], "new");
}

/// "p" (`CAP`) with "q" right after it, and p's header one extent ahead:
/// a growth beyond capacity whose page write was refused before submission
/// (the `k`-th write submission of the growth). The store is open and not
/// poisoned.
fn header_ahead(seed: u64, k: u64) -> Option<(SimPlatform, Store<SimPlatform>)> {
    let (p, s) = created(seed);
    s.provision_page_region("p", CAP).unwrap();
    s.provision_page_region("q", CAP).unwrap();
    let pages = s.page_region("p").unwrap();
    p.with_world(|w| w.faults_mut().fail_submit = Some((w.write_submits() + k, 5)));
    let failed = pages.grow(CAP + 2 * B).is_err();
    p.with_world(|w| w.faults_mut().fail_submit = None);
    let f = audit_allowing_extra(&p)
        .into_iter()
        .find(|f| f.name == "p")
        .unwrap();
    let ahead = f.extra.is_some();
    drop(pages);
    (failed && ahead && s.poisoned().is_none()).then_some((p, s))
}

/// Growth beyond capacity from a header already one extent ahead (grow2:
/// the header rewritten back to its committed extents, then forward with
/// the new one, then the table): the old size or the new, never a region
/// the open refuses, "q" untouched.
#[test]
fn test_every_crash_of_a_growth_from_a_header_one_extent_ahead() {
    let k = (1..32)
        .find(|&k| header_ahead(1735, k).is_some())
        .expect("no refused commit after a header write");
    let seen = Outcomes::default();
    let covered = enumerate_settled(
        1735,
        || header_ahead(1735, k).unwrap(),
        |s| {
            s.page_region("p").unwrap().grow(CAP + 4 * B);
        },
        |p, case| {
            let s = open(p);
            assert!(s.unavailable_regions().is_empty(), "{case:?}");
            let r = s.page_region("p").unwrap();
            match r.len() {
                CAP => seen.saw(case, "old"),
                n if n == CAP + 4 * B => seen.saw(case, "new"),
                n => panic!("{case:?}: {n}"),
            }
            assert_eq!(s.page_region("q").unwrap().len(), CAP, "{case:?}");
            drop(r);
            audit_space(p, &s);
            drop(s);
            audit_allowing_extra(p);
            assert_closed_in(p, DIR);
        },
    );
    report("growth from a header one extent ahead", covered);
    seen.expect(&["new", "old"], "new");
}

/// A release (spec §12: header Released, then the table): the region whole,
/// or gone with its name free; the other region kept.
#[test]
fn test_every_crash_of_a_release() {
    let seen = Outcomes::default();
    let covered = enumerate_settled(
        1722,
        || {
            let (p, s) = created(1722);
            s.provision_slot("keep").unwrap().commit(b"kept").unwrap();
            s.provision_append_region("gone", CAP).unwrap();
            (p, s)
        },
        |s| {
            s.append_region("gone").unwrap().release();
        },
        |p, case| {
            let s = open(p);
            assert_eq!(
                s.slot("keep").unwrap().read().as_deref(),
                Some(&b"kept"[..]),
                "{case:?}"
            );
            match s.append_region("gone") {
                Ok(r) => {
                    assert_eq!(r.capacity(), CAP, "{case:?}");
                    seen.saw(case, "kept");
                }
                Err(e) => {
                    assert!(not_found(&e), "{case:?}: {e:?}");
                    // Its name is free again.
                    s.provision_append_region("gone", B).unwrap();
                    seen.saw(case, "released");
                }
            }
            assert!(s.unavailable_regions().is_empty(), "{case:?}");
            audit_space(p, &s);
            drop(s);
            audit_closed(p);
        },
    );
    report("release", covered);
    seen.expect(&["kept", "released"], "released");
}

/// A release of a region of two extents (grown beyond capacity past "q"):
/// the region whole, or gone with its name free and both extents free; "q"
/// kept.
#[test]
fn test_every_crash_of_a_release_of_two_extents() {
    const GROWN: u64 = CAP + 2 * B;
    let seen = Outcomes::default();
    let covered = enumerate_settled(
        1737,
        || {
            let (p, s) = created(1737);
            s.provision_page_region("p", CAP).unwrap();
            s.provision_page_region("q", CAP).unwrap();
            s.page_region("p").unwrap().grow(GROWN).unwrap();
            let f = audit(&p).into_iter().find(|f| f.name == "p").unwrap();
            assert_eq!(f.extents.len(), 2, "p is not in two extents");
            (p, s)
        },
        |s| {
            s.page_region("p").unwrap().release();
        },
        |p, case| {
            let s = open(p);
            assert_eq!(s.page_region("q").unwrap().len(), CAP, "{case:?}");
            match s.page_region("p") {
                Ok(r) => {
                    assert_eq!(r.len(), GROWN, "{case:?}");
                    seen.saw(case, "kept");
                }
                Err(e) => {
                    assert!(not_found(&e), "{case:?}: {e:?}");
                    s.provision_page_region("p", B).unwrap();
                    seen.saw(case, "released");
                }
            }
            assert!(s.unavailable_regions().is_empty(), "{case:?}");
            audit_space(p, &s);
            drop(s);
            audit_closed(p);
        },
    );
    report("release of two extents", covered);
    seen.expect(&["kept", "released"], "released");
}

/// A recycle: the region at its old generation or the next, never neither.
#[test]
fn test_every_crash_of_a_recycle() {
    let seen = Outcomes::default();
    let covered = enumerate(
        1723,
        || {
            let (p, s) = created(1723);
            let r = s.provision_page_region("p", 8 * B).unwrap();
            r.write_durable(r.pos(0).unwrap(), &[5; B as usize])
                .unwrap();
            drop(r);
            (p, s)
        },
        |s| {
            s.page_region("p").unwrap().recycle();
        },
        |p, case| {
            let s = open(p);
            let r = s.page_region("p").unwrap();
            match r.generation().get() {
                1 => seen.saw(case, "old"),
                2 => seen.saw(case, "new"),
                g => panic!("{case:?}: generation {g}"),
            }
            assert_eq!(r.len(), 8 * B, "{case:?}");
            drop(r);
            audit_space(p, &s);
            drop(s);
            audit_closed(p);
        },
    );
    report("recycle", covered);
    seen.expect(&["new", "old"], "new");
}

/// A slot commit: the old object or the new.
#[test]
fn test_every_crash_of_a_slot_commit() {
    let seen = Outcomes::default();
    let covered = enumerate(
        1724,
        || {
            let (p, s) = created(1724);
            s.provision_slot("s")
                .unwrap()
                .commit(b"old object")
                .unwrap();
            (p, s)
        },
        |s| {
            s.slot("s").unwrap().commit(b"the new object");
        },
        |p, case| {
            let s = open(p);
            match s.slot("s").unwrap().read().as_deref() {
                Some(b"old object") => seen.saw(case, "old"),
                Some(b"the new object") => seen.saw(case, "new"),
                got => panic!("{case:?}: {got:?}"),
            }
            audit_space(p, &s);
            drop(s);
            audit_closed(p);
        },
    );
    report("slot commit", covered);
    seen.expect(&["new", "old"], "new");
}

/// `repair_unavailable` (spec §12: the page written without the entry):
/// still unavailable, or gone with its name free; the other region kept.
#[test]
fn test_every_crash_of_a_repair() {
    let seen = Outcomes::default();
    let covered = enumerate(
        1725,
        || {
            let p = fresh(1725);
            let mut broken = region(0, RegionKind::Page, "broken", BASE, CAP);
            broken.write_header = false;
            let fine = region(1, RegionKind::Page, "fine", BASE + 2 * B + CAP, CAP);
            write_regions(&p, 0, &[broken, fine]);
            let s = open(&p);
            (p, s)
        },
        |s| {
            let id = s.unavailable_regions()[0].id;
            s.repair_unavailable(id);
        },
        |p, case| {
            let s = open(p);
            assert!(s.page_region("fine").is_ok(), "{case:?}");
            match s.unavailable_regions().len() {
                0 => {
                    s.provision_page_region("broken", B).unwrap();
                    audit_space(p, &s);
                    drop(s);
                    audit_closed(p);
                    seen.saw(case, "repaired");
                }
                1 => {
                    audit_space(p, &s);
                    seen.saw(case, "unavailable");
                }
                n => panic!("{case:?}: {n} unavailable"),
            }
        },
    );
    report("repair", covered);
    seen.expect(&["repaired", "unavailable"], "repaired");
}

/// `accept_page_rollback` (spec §12: the winner rewritten into the failed
/// slot): still stale, or confirmed and the region writable.
#[test]
fn test_every_crash_of_an_accept() {
    let seen = Outcomes::default();
    let covered = enumerate(
        1726,
        || {
            let (p, s) = created(1726);
            let r = s.provision_page_region("a", 8 * B).unwrap();
            r.write_durable(r.pos(0).unwrap(), &[3; B as usize])
                .unwrap();
            drop((r, s));
            tear_newer_copy_of(&p, 0);
            let s = open(&p);
            (p, s)
        },
        |s| {
            s.accept_page_rollback(&[0]);
        },
        |p, case| {
            let s = open(p);
            let r = s.page_region("a").unwrap();
            if s.frozen() {
                assert_eq!(s.report().metadata.stale_pages, [0], "{case:?}");
                seen.saw(case, "stale");
            } else {
                r.write_durable(r.pos(0).unwrap(), &[4; B as usize])
                    .unwrap();
                drop(r);
                audit_space(p, &s);
                drop(s);
                audit_closed(p);
                seen.saw(case, "accepted");
            }
        },
    );
    report("accept", covered);
    seen.expect(&["accepted", "stale"], "accepted");
}

/// `accept_page_rollback` of two stale pages (each one's winner rewritten
/// in turn): both still stale, the first accepted alone, or both, the
/// regions writable once nothing is stale.
#[test]
fn test_every_crash_of_an_accept_of_two_pages() {
    let seen = Outcomes::default();
    let covered = enumerate(
        1738,
        || {
            let p = fresh(1738);
            write_regions(&p, 0, &[region(0, RegionKind::Page, "a", BASE, CAP)]);
            write_regions(
                &p,
                1,
                &[region(0, RegionKind::Page, "b", BASE + 2 * B + CAP, CAP)],
            );
            tear_newer_copy_of(&p, 0);
            tear_newer_copy_of(&p, 1);
            let s = open(&p);
            assert_eq!(s.report().metadata.stale_pages, [0, 1]);
            (p, s)
        },
        |s| {
            s.accept_page_rollback(&[0, 1]);
        },
        |p, case| {
            let s = open(p);
            let stale = s.report().metadata.stale_pages.clone();
            match stale.as_slice() {
                [0, 1] => seen.saw(case, "stale"),
                [1] => seen.saw(case, "first"),
                [] => {
                    for name in ["a", "b"] {
                        let r = s.page_region(name).unwrap();
                        r.write_durable(r.pos(0).unwrap(), &[4; B as usize])
                            .unwrap_or_else(|e| panic!("{case:?} {name}: {e:?}"));
                    }
                    audit_space(p, &s);
                    drop(s);
                    audit_closed(p);
                    seen.saw(case, "accepted");
                }
                other => panic!("{case:?}: stale {other:?}"),
            }
        },
    );
    report("accept of two pages", covered);
    seen.expect(&["accepted", "first", "stale"], "accepted");
}

/// `accept_page_rollback` of a stale page holding a region whose header
/// lists an extent past its entry (a growth whose commit may be the page's
/// lost one, counted allocated while the page is stale): still stale, or
/// accepted with that extent free again and the region growing.
#[test]
fn test_every_crash_of_an_accept_with_an_extra_extent() {
    let k = (1..32)
        .find(|&k| header_ahead(1744, k).is_some())
        .expect("no refused commit after a header write");
    let seen = Outcomes::default();
    let covered = enumerate(
        1744,
        || {
            let (p, s) = header_ahead(1744, k).unwrap();
            drop(s);
            tear_newer_copy_of(&p, 0);
            let s = open(&p);
            assert_eq!(s.report().metadata.stale_pages, [0]);
            assert_eq!(s.report().metadata.extra_extents.len(), 1);
            (p, s)
        },
        |s| {
            s.accept_page_rollback(&[0]);
        },
        |p, case| {
            let s = open(p);
            let r = s.page_region("p").unwrap();
            assert_eq!(r.len(), CAP, "{case:?}");
            if s.frozen() {
                assert_eq!(s.report().metadata.stale_pages, [0], "{case:?}");
                // The extent still counted allocated (it may be the newest
                // growth).
                assert_eq!(s.report().metadata.extra_extents.len(), 1, "{case:?}");
                seen.saw(case, "stale");
            } else {
                r.grow(CAP + 4 * B)
                    .unwrap_or_else(|e| panic!("{case:?}: {e:?}"));
                drop(r);
                audit_space(p, &s);
                drop(s);
                audit_allowing_extra(p);
                assert_closed_in(p, DIR);
                seen.saw(case, "accepted");
            }
        },
    );
    report("accept with an extra extent", covered);
    seen.expect(&["accepted", "stale"], "accepted");
}

/// A repair of a region cut short (its span past the container's end, the
/// cut inside its second extent): still unavailable, or gone with its name
/// free; the other region kept.
#[test]
fn test_every_crash_of_a_repair_of_a_cut_short_region() {
    let tail = BASE + 2 * (2 * B + CAP);
    let seen = Outcomes::default();
    let covered = enumerate(
        1739,
        || {
            let p = fresh(1739);
            let mut cut = region(0, RegionKind::Page, "cut", BASE, CAP);
            cut.extents.push((tail, CAP));
            cut.entry.extent_count = 2;
            cut.header.extent_count = 2;
            cut.entry.capacity = 2 * CAP;
            cut.entry.span_end = tail + CAP;
            let fine = region(1, RegionKind::Page, "fine", BASE + 2 * B + CAP, CAP);
            write_regions(&p, 0, &[cut, fine]);
            {
                let dir = p.open_dir(Path::new(DIR), false).unwrap();
                let file = p
                    .open_file(
                        &dir,
                        &store_io_platform::FileName::new(CONTAINER).unwrap(),
                        store_io_platform::FileMode::ReadWrite,
                    )
                    .unwrap();
                p.set_len(&file, tail + CAP / 2).unwrap();
                p.flush_all(&file).unwrap();
            }
            let s = open(&p);
            assert_eq!(s.unavailable_regions().len(), 1, "cut is not unavailable");
            (p, s)
        },
        |s| {
            let id = s.unavailable_regions()[0].id;
            s.repair_unavailable(id);
        },
        |p, case| {
            let s = open(p);
            assert!(s.page_region("fine").is_ok(), "{case:?}");
            match s.unavailable_regions().len() {
                0 => {
                    s.provision_page_region("cut", B).unwrap();
                    audit_space(p, &s);
                    drop(s);
                    audit_closed(p);
                    seen.saw(case, "repaired");
                }
                1 => {
                    audit_space(p, &s);
                    seen.saw(case, "unavailable");
                }
                n => panic!("{case:?}: {n} unavailable"),
            }
        },
    );
    report("repair of a cut-short region", covered);
    seen.expect(&["repaired", "unavailable"], "repaired");
}

/// A clean close after writes (spec §12: the volume record's last write):
/// the data intact whether or not the record came to say clean.
#[test]
fn test_every_crash_of_a_clean_close() {
    let seen = Outcomes::default();
    let covered = enumerate_with(
        1727,
        false,
        || {
            let (p, s) = created(1727);
            let r = s.provision_page_region("p", 8 * B).unwrap();
            r.write_durable(r.pos(0).unwrap(), &[6; B as usize])
                .unwrap();
            drop(r);
            (p, RefCell::new(Some(s)))
        },
        |_, s| {
            // The close: the last handle dropped.
            drop(s.borrow_mut().take());
        },
        |p, case| {
            let s = open(p);
            if s.report().metadata.clean_close {
                seen.saw(case, "clean");
            } else {
                seen.saw(case, "dirty");
            }
            let r = s.page_region("p").unwrap();
            let mut back = vec![0u8; B as usize];
            r.read(r.pos(0).unwrap(), &mut back).unwrap();
            assert!(back == [6; B as usize], "{case:?}");
            drop(r);
            audit_space(p, &s);
            drop(s);
            audit_closed(p);
        },
    );
    report("clean close", covered);
    seen.expect(&["clean", "dirty"], "clean");
}

/// `copy_store`: the target holds no store, or the whole copy (also with the
/// file system's metadata settled: a name made durable early); the
/// source's bytes never written.
#[test]
fn test_every_crash_of_a_copy() {
    copy_crash(1728, "copy", create, CopyOptions::default);
}

/// As [`test_every_crash_of_a_copy`], from a format-1 source.
#[test]
fn test_every_crash_of_a_copy_from_format_1() {
    copy_crash(
        1741,
        "copy from format 1",
        |p| {
            let o = StoreOptions {
                container_format: 1,
                ..opts()
            };
            Store::create(p.clone(), Path::new(DIR), o).unwrap()
        },
        CopyOptions::default,
    );
}

/// As [`test_every_crash_of_a_copy`], from a source whose page is stale (its
/// newest commit torn), copied with `accept_stale_pages`.
#[test]
fn test_every_crash_of_a_copy_accepting_stale_pages() {
    copy_crash(1742, "copy accepting stale pages", create, || CopyOptions {
        accept_stale_pages: true,
        ..CopyOptions::default()
    });
}

/// `copy_store` to "/copy" with `options` (its store options `opts()`),
/// crashed at every point, from a source `make` creates in `DIR` and this
/// fills (a page region "p" with a block of 8s, a slot "s" holding "slot");
/// with `accept_stale_pages`, the page holding them is then made stale.
fn copy_crash(
    seed: u64,
    name: &str,
    make: fn(&SimPlatform) -> Store<SimPlatform>,
    options: fn() -> CopyOptions,
) {
    let stale = options().accept_stale_pages;
    let setup = || {
        let p = SimPlatform::new(SimConfig::volatile(seed));
        let s = make(&p);
        let r = s.provision_page_region("p", 4 * B).unwrap();
        r.write_durable(r.pos(0).unwrap(), &[8; B as usize])
            .unwrap();
        s.provision_slot("s").unwrap().commit(b"slot").unwrap();
        drop((r, s));
        if stale {
            tear_newer_copy_of(&p, 0);
            let s = Store::open_readonly(p.clone(), Path::new(DIR), opts()).unwrap();
            assert_eq!(
                s.report().metadata.stale_pages,
                [0],
                "the page is not stale"
            );
        }
        (p, ())
    };
    let copy = |p: &SimPlatform| {
        let o = CopyOptions {
            store: opts(),
            ..options()
        };
        copy_store(p.clone(), Path::new(DIR), Path::new("/copy"), o)
    };
    // The source's bytes as this run made them (a store's ids are random).
    let source = RefCell::new(Vec::new());
    let seen = Outcomes::default();
    let covered = enumerate_with(
        seed,
        true,
        setup,
        |p, ()| {
            *source.borrow_mut() = media(p);
            copy(p);
        },
        |p, case| {
            assert!(media(p) == *source.borrow(), "the source written: {case:?}");
            let placed = p.with_world(|w| w.media_of(&"/copy".into(), CONTAINER).is_some());
            agree(p, "/copy", case);
            if placed {
                let s = Store::open(p.clone(), Path::new("/copy"), opts())
                    .unwrap_or_else(|e| panic!("{case:?}: {e:?}"));
                assert_eq!(
                    s.slot("s").unwrap().read().as_deref(),
                    Some(&b"slot"[..]),
                    "{case:?}"
                );
                let r = s.page_region("p").unwrap();
                let mut back = vec![0u8; B as usize];
                r.read(r.pos(0).unwrap(), &mut back).unwrap();
                assert!(back == [8; B as usize], "{case:?}");
                drop(r);
                audit_space_in(p, "/copy", &s);
                drop(s);
                audit_in(p, "/copy");
                assert_closed_in(p, "/copy");
                seen.saw(case, "copy");
            } else {
                // What a crashed copy left (a temporary file, durable under
                // its name when the metadata settled) does not stop the
                // next copy.
                copy(p).unwrap_or_else(|e| panic!("the next copy: {e:?} {case:?}"));
                audit_in(p, "/copy");
                assert_closed_in(p, "/copy");
                seen.saw(case, "none");
            }
        },
    );
    report(name, covered);
    seen.expect(&["copy", "none"], "copy");
}

/// The space audit while a reservation is held and a region provisioned
/// from it: the reserved bytes are not free, and free plus reserved is the
/// complement of what is allocated.
#[test]
fn test_space_is_the_complement_with_a_reservation_held() {
    let (p, s) = created(1740);
    s.provision_page_region("a", 4 * B).unwrap();
    let mut res = s.reserve(16 * B, 7).unwrap();
    audit_space(&p, &s);
    res.provision_page_region("r", 8 * B).unwrap();
    assert!(s.space().reserved > 0, "the reservation still holds bytes");
    audit_space(&p, &s);
    // Outside the reservation while it is held: never in its bytes.
    s.provision_page_region("outside", 4 * B).unwrap();
    audit_space(&p, &s);
    drop(res);
    audit_space(&p, &s);
    drop(s);
    audit_closed(&p);
}
