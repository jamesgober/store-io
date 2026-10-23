//! Format-2 growth within capacity (spec §12, v0.5 E4 C8), on regions an
//! independent writer crafted with a capacity past their data size: the new
//! range filled with the region's pattern (keyed by region offset, across
//! runs), flushed and verified, then one page write commits the new size,
//! then it is published. A crash at any point leaves the old size or the
//! new; nothing before the commit is visible.

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

use format2_util::craft::{Region, lose_page, region, tear_newer_copy_of, write_regions};
use format2_util::*;
use store_io_core::error::{Capability, Error, NotWrittenCause};
use store_io_engine::{Store, StoreOptions};
use store_io_format::fill::fill_keyed_v1;
use store_io_format::meta::{FillPattern, RegionKind};
use store_io_sim::{CrashMode, FaultPlan, SimConfig, SimPlatform};

const B: u64 = 4096;
/// The table's end for `opts()`: the volume pair, then three pages.
const BASE: u64 = 2 * B + 2 * PAGES as u64 * TABLE;
/// A region's capacity: 16 blocks (its data, as crafted: 8).
const CAP: u64 = 16 * B;
const KEY: [u8; 16] = *b"store-io growth!";

fn fresh(seed: u64) -> SimPlatform {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    drop(create(&p));
    p
}

fn is_cause(e: &Error, want: NotWrittenCause) -> bool {
    matches!(e, Error::NotWritten { cause, .. } if *cause == want)
}

/// A page region "p" of `CAP` bytes, half of them data, keyed fill.
fn page_region() -> Region {
    let mut r = region(0, RegionKind::Page, "p", BASE, CAP);
    r.header.fill = FillPattern::KeyedV1;
    r.header.fill_key = KEY;
    r
}

/// An append region "a" of `CAP` bytes, half of them data.
fn append_region() -> Region {
    region(1, RegionKind::Append, "a", BASE + 2 * B + CAP, CAP)
}

fn crafted(seed: u64) -> SimPlatform {
    let p = fresh(seed);
    write_regions(&p, 0, &[page_region(), append_region()]);
    p
}

#[test]
fn test_a_page_region_grows_within_its_capacity() {
    let p = crafted(601);
    let s = open(&p);
    let pages = s.page_region("p").unwrap();
    assert_eq!((pages.len(), pages.capacity()), (CAP / 2, CAP));
    let before = pages.pos(0).unwrap();
    // The range past the data is out of bounds until the growth.
    let e = pages.pos(CAP / 2).map(|_| ()).unwrap_err();
    assert!(is_cause(&e, NotWrittenCause::OutOfBounds), "{e:?}");
    // A size at most the current one changes nothing, and writes nothing.
    let writes = p.with_world(|w| w.writes());
    assert_eq!(pages.grow(B).unwrap(), CAP / 2);
    assert_eq!(p.with_world(|w| w.writes()), writes);
    // Rounded up to the block.
    assert_eq!(pages.grow(CAP / 2 + 1).unwrap(), CAP / 2 + B);
    assert_eq!(pages.grow(CAP - B).unwrap(), CAP - B);
    assert_eq!(pages.len(), CAP - B);
    // The new range is writable; what was there before is unchanged.
    pages
        .write_durable(pages.pos(CAP / 2).unwrap(), &[9u8; 4096])
        .unwrap();
    pages.write_durable(before, &[1u8; 4096]).unwrap();
    // (Past the capacity is growth beyond it: tests/format2_grow_beyond.rs.)
    drop((pages, s));
    let s = open(&p);
    let pages = s.page_region("p").unwrap();
    assert_eq!(pages.len(), CAP - B, "the new size is on the media");
    let mut out = vec![0u8; 4096];
    pages.read(pages.pos(CAP / 2).unwrap(), &mut out).unwrap();
    assert_eq!(out, [9u8; 4096]);
}

/// The grown range holds the region's pattern, keyed by region offset.
#[test]
fn test_the_grown_range_holds_the_regions_pattern() {
    let p = crafted(602);
    let s = open(&p);
    let pages = s.page_region("p").unwrap();
    pages.grow(CAP).unwrap();
    let mut out = vec![0u8; (CAP / 2) as usize];
    pages.read(pages.pos(CAP / 2).unwrap(), &mut out).unwrap();
    let mut want = vec![0u8; (CAP / 2) as usize];
    fill_keyed_v1(&KEY, CAP / 2, &mut want);
    assert_eq!(out, want);
}

/// An append region's limit moves with its size: an append that did not
/// fit does after the growth.
#[test]
fn test_an_append_region_grows_its_limit() {
    let p = crafted(603);
    let s = open(&p);
    let a = s.append_region("a").unwrap();
    a.resume_at(CAP / 2).unwrap();
    let e = a.append(b"x").unwrap_err();
    assert!(is_cause(&e, NotWrittenCause::RegionFull), "{e:?}");
    assert_eq!(a.grow(CAP).unwrap(), CAP);
    let (pos, _r) = a.append_durable(b"after the growth").unwrap();
    assert_eq!(pos.offset(), CAP / 2);
    drop((a, s));
    let s = open(&p);
    let a = s.append_region("a").unwrap();
    assert_eq!(a.len(), CAP);
    let mut out = vec![0u8; 4096];
    a.read(CAP / 2, &mut out).unwrap();
    assert_eq!(&out[..16], b"after the growth");
}

/// Growth across runs: the grown range lies in two extents, each filled
/// with the pattern at its own region offsets.
#[test]
fn test_growth_across_runs() {
    let p = fresh(604);
    let mut r = page_region();
    // Two extents in reverse file order: [0, CAP/2) high, [CAP/2, CAP) low.
    let high = BASE + 2 * B + CAP;
    r.extents = vec![(high, CAP / 2), (BASE + 2 * B, CAP / 2)];
    r.header.extent_count = 2;
    r.entry.extent_count = 2;
    r.entry.data_size = CAP / 4;
    r.entry.span_end = high + CAP / 2;
    write_regions(&p, 0, &[r]);
    let s = open(&p);
    let pages = s.page_region("p").unwrap();
    assert_eq!(pages.grow(CAP).unwrap(), CAP);
    let mut out = vec![0u8; (3 * CAP / 4) as usize];
    pages.read(pages.pos(CAP / 4).unwrap(), &mut out).unwrap();
    let mut want = vec![0u8; out.len()];
    fill_keyed_v1(&KEY, CAP / 4, &mut want);
    assert_eq!(out, want);
    // On the media, the second run's bytes at its own file offsets.
    let m = media(&p);
    let mut second = vec![0u8; (CAP / 2) as usize];
    fill_keyed_v1(&KEY, CAP / 2, &mut second);
    let at = (BASE + 2 * B) as usize;
    assert_eq!(&m[at..at + second.len()], &second[..]);
}

/// Power fails at each write submission of a growth, keeping, dropping or
/// tearing what was cached: the next open finds the old size or the new,
/// the region whole either way.
#[test]
fn test_a_power_failure_at_any_point_of_a_growth() {
    let mut k = 1u64;
    loop {
        let mut finished = false;
        for seed in 0..4 {
            let p = crafted(610 + seed);
            let s = open(&p);
            let pages = s.page_region("p").unwrap();
            let n = Arc::new(AtomicU64::new(0));
            p.with_world(|w| {
                let n = Arc::clone(&n);
                w.set_submit_probe(Some(Arc::new(move || {
                    if n.fetch_add(1, Ordering::SeqCst) + 1 == k {
                        panic!("power fails");
                    }
                })));
            });
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pages.grow(CAP)));
            finished |= matches!(r, Ok(Ok(_)));
            std::mem::forget((pages, s));
            p.with_world(|w| {
                w.set_submit_probe(None);
                w.crash(&CrashMode::Random);
            });
            let step = format!("write {k} seed {seed}");
            let s = Store::open(p.clone(), Path::new(DIR), opts())
                .unwrap_or_else(|e| panic!("{step}: {e:?}"));
            assert!(
                s.report().unavailable.is_empty(),
                "{step}: {:?}",
                s.report().unavailable
            );
            let pages = s.page_region("p").unwrap();
            let len = pages.len();
            assert!(len == CAP / 2 || len == CAP, "{step}: {len}");
            if len == CAP {
                // Committed: the grown range was filled and flushed first.
                let mut out = vec![0u8; (CAP / 2) as usize];
                pages.read(pages.pos(CAP / 2).unwrap(), &mut out).unwrap();
                let mut want = vec![0u8; out.len()];
                fill_keyed_v1(&KEY, CAP / 2, &mut want);
                assert_eq!(out, want, "{step}");
            } else {
                // The old size: a growth again works.
                assert_eq!(pages.grow(CAP).unwrap(), CAP, "{step}");
            }
        }
        if finished {
            break;
        }
        k += 1;
        assert!(k < 64, "a growth never finished");
    }
    assert!(k > 1, "only {k} submissions: the probe missed the growth");
}

/// Each write and each flush of a growth fails in turn: the error is
/// returned, and after a power failure the store opens with the old size
/// or the new.
#[test]
fn test_each_write_and_flush_failing_in_turn() {
    for flush in [false, true] {
        let mut k = 1u64;
        loop {
            let p = crafted(620);
            let s = open(&p);
            let pages = s.page_region("p").unwrap();
            p.with_world(|w| {
                if flush {
                    w.faults_mut().fail_flush = Some(w.flushes() + k);
                } else {
                    w.faults_mut().fail_write = Some(w.writes() + k);
                }
            });
            let r = pages.grow(CAP);
            let done = r.is_ok();
            if let Err(e) = &r {
                assert!(
                    matches!(e, Error::DurabilityUnknown { .. }),
                    "flush {flush} k {k}: {e:?}"
                );
                assert!(s.poisoned().is_some());
                // Published only after the commit returned.
                assert_eq!(pages.len(), CAP / 2, "flush {flush} k {k}");
            }
            drop((pages, s));
            p.with_world(|w| {
                *w.faults_mut() = FaultPlan::default();
                w.crash(&CrashMode::LoseAll);
            });
            let s = open(&p);
            let len = s.page_region("p").unwrap().len();
            assert!(len == CAP / 2 || len == CAP, "flush {flush} k {k}: {len}");
            if done {
                break;
            }
            k += 1;
            assert!(k < 64, "a growth never finished");
        }
    }
}

/// A fill that does not verify (pages in the OS cache, say) refuses the
/// growth before its commit: nothing is poisoned, the old size stands.
#[test]
fn test_a_fill_that_does_not_verify_refuses_the_growth() {
    let p = crafted(630);
    let s = open(&p);
    let pages = s.page_region("p").unwrap();
    p.with_world(|w| w.faults_mut().cached_pages = true);
    let e = pages.grow(CAP).unwrap_err();
    assert!(is_cause(&e, NotWrittenCause::NotReady), "{e:?}");
    assert!(s.poisoned().is_none());
    assert_eq!(pages.len(), CAP / 2);
}

/// Allocation frozen (a page lost): growth is refused, also within the
/// capacity (stricter than the spec, which freezes only growth beyond it: a
/// page write while a page is lost could take a sequence that page used);
/// a region on a stale page is read-only.
#[test]
fn test_frozen_and_stale_pages() {
    let p = crafted(640);
    write_regions(
        &p,
        1,
        &[region(
            0,
            RegionKind::Page,
            "on1",
            BASE + 4 * B + 2 * CAP,
            CAP,
        )],
    );
    lose_page(&p, 1);
    let s = open(&p);
    assert!(s.report().metadata.frozen);
    let e = s.page_region("p").unwrap().grow(CAP).unwrap_err();
    assert!(is_cause(&e, NotWrittenCause::Frozen), "{e:?}");
    assert_eq!(s.page_region("p").unwrap().len(), CAP / 2);
    drop(s);
    let p = crafted(641);
    tear_newer_copy_of(&p, 0);
    let s = open(&p);
    let e = s.page_region("p").unwrap().grow(CAP).unwrap_err();
    assert!(is_cause(&e, NotWrittenCause::ReadOnly), "{e:?}");
}

/// Format 1 has no growth.
#[test]
fn test_format_1_refuses_growth() {
    let p = SimPlatform::new(SimConfig::volatile(650));
    let s = Store::create(p, Path::new(DIR), StoreOptions::default()).unwrap();
    let pages = s.provision_page_region("p", CAP).unwrap();
    let e = pages.grow(2 * CAP).unwrap_err();
    assert!(
        matches!(
            e,
            Error::Unsupported {
                what: Capability::FormatVersion
            }
        ),
        "{e:?}"
    );
}

/// The space accounting follows a growth (the data bytes, not the
/// occupied ones, which the capacity already counted), across a reopen,
/// and a release of the grown region gives it all back.
#[test]
fn test_space_follows_a_growth_and_its_release() {
    let p = crafted(660);
    let s = open(&p);
    let tag = |s: &Store<_>| {
        let t = s.space().tags.into_iter().find(|t| t.tag == 0).unwrap();
        (t.logical, t.physical)
    };
    let (logical, physical) = tag(&s);
    s.page_region("p").unwrap().grow(CAP).unwrap();
    assert_eq!(tag(&s), (logical + CAP / 2, physical));
    drop(s);
    let s = open(&p);
    assert_eq!(
        tag(&s),
        (logical + CAP / 2, physical),
        "rebuilt at the open"
    );
    let s2 = s.clone();
    s2.page_region("p").unwrap().release().unwrap();
    assert_eq!(tag(&s), (logical - CAP / 2, physical - CAP - 2 * B));
    // And in a growth made in this open, released in the same one.
    drop((s, s2));
    let p = crafted(661);
    let s = open(&p);
    let (logical, physical) = tag(&s);
    let regions = s.space().regions;
    s.page_region("p").unwrap().grow(CAP).unwrap();
    s.page_region("p").unwrap().release().unwrap();
    assert_eq!(tag(&s), (logical - CAP / 2, physical - CAP - 2 * B));
    assert_eq!(
        s.space().regions,
        regions - CAP - 2 * B,
        "the region's bytes back"
    );
}

/// A ticket taken before a growth is still served after it.
#[test]
fn test_a_ticket_from_before_a_growth() {
    let p = crafted(670);
    let s = open(&p);
    let pages = s.page_region("p").unwrap();
    let t = pages.write(pages.pos(0).unwrap(), &[4u8; 4096]).unwrap();
    pages.grow(CAP).unwrap();
    let receipt = pages.sync_through(&t).unwrap();
    assert!(receipt.covers(&t));
}

/// Appends and reads run while the region grows: those within the old size
/// all land, those past it land once the new size is published, and
/// nothing panics.
#[test]
fn test_appends_during_a_growth() {
    let p = crafted(671);
    let s = open(&p);
    let a = s.append_region("a").unwrap();
    a.resume_at(0).unwrap();
    let appender = {
        let a = a.clone();
        std::thread::spawn(move || {
            let mut landed = 0u64;
            let mut full = 0u64;
            for _ in 0..200 {
                match a.append(&[7u8; 4096]) {
                    Ok(_) => landed += 1,
                    Err(e) if is_cause(&e, NotWrittenCause::RegionFull) => full += 1,
                    Err(e) => panic!("{e:?}"),
                }
                if landed * B >= CAP {
                    break;
                }
            }
            (landed, full)
        })
    };
    assert_eq!(a.grow(CAP).unwrap(), CAP);
    let (landed, _full) = appender.join().unwrap();
    assert!(landed * B <= CAP);
    // Every landed append is readable, whichever side of the growth.
    let mut out = vec![0u8; (landed * B) as usize];
    a.read(0, &mut out).unwrap();
    assert!(out.iter().all(|&b| b == 7));
}
