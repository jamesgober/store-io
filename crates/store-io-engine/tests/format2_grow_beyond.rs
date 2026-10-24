//! Format-2 growth beyond capacity (spec §12, v0.5 E4 C9): one new extent,
//! after the region's last one when that space is free (one run), else
//! elsewhere (a new run); filled, flushed and verified; the header listing
//! it written; then one page write commits the new size, capacity, extent
//! count, span and `footprint_seq`. A crash leaves the old size (the
//! header possibly one extent ahead, that extent free) or the new one;
//! every refusal leaves the space exactly as it was.

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

use format2_util::audit::{Found, audit, audit_allowing_extra, header_of};
use format2_util::craft::{lose_page, region, write_regions};
use format2_util::*;
use store_io_core::error::{Error, NotWrittenCause, Op, ReserveTag};
use store_io_core::evidence::PlatformKind;
use store_io_engine::{PageRegion, Store, StoreOptions};
use store_io_format::fill::fill_keyed_v1;
use store_io_format::meta::{FillPattern, RegionKind};
use store_io_format::meta2::{FLAG_CAPPED, max_extents};
use store_io_sim::{CrashMode, FaultPlan, SimConfig, SimPlatform};

const B: u64 = 4096;
/// A provisioned region's size (and capacity).
const CAP: u64 = 16 * B;
/// The extent a small region's growth adds: 1 MiB.
const EXT: u64 = 1 << 20;
/// The size a growth asks for: two blocks past the capacity.
const NEW: u64 = CAP + 2 * B;

fn kopts() -> StoreOptions {
    StoreOptions {
        keyed_fill: true,
        ..opts()
    }
}

fn fresh_with(cfg: SimConfig, o: &StoreOptions) -> SimPlatform {
    let p = SimPlatform::new(cfg);
    drop(Store::create(p.clone(), Path::new(DIR), o.clone()).unwrap());
    p
}

fn fresh(seed: u64) -> SimPlatform {
    fresh_with(SimConfig::volatile(seed), &kopts())
}

fn store(p: &SimPlatform) -> Store<SimPlatform> {
    Store::open(p.clone(), Path::new(DIR), kopts()).unwrap()
}

fn is_cause(e: &Error, want: NotWrittenCause) -> bool {
    matches!(e, Error::NotWritten { cause, .. } if *cause == want)
}

fn is_no_space(e: &Error, want: ReserveTag) -> bool {
    matches!(e, Error::NoSpace { tag, .. } if *tag == want)
}

/// The audit's record of region `name` (the header may be one ahead).
fn found(p: &SimPlatform, name: &str) -> Found {
    audit_allowing_extra(p)
        .into_iter()
        .find(|f| f.name == name)
        .unwrap()
}

/// Region `f`'s keyed pattern for region offsets `[from, from + len)`.
fn pattern(p: &SimPlatform, f: &Found, from: u64, len: u64) -> Vec<u8> {
    let h = header_of(p, f.id, f.header_at).unwrap();
    assert_eq!(h.header.fill, FillPattern::KeyedV1);
    let mut out = vec![0u8; len as usize];
    fill_keyed_v1(&h.header.fill_key, from, &mut out);
    out
}

fn media_at(p: &SimPlatform, at: u64, len: u64) -> Vec<u8> {
    media(p)[at as usize..(at + len) as usize].to_vec()
}

/// Device writes a durable write of two blocks at region offset `at` takes.
fn writes_of(p: &SimPlatform, pages: &PageRegion<SimPlatform>, at: u64, byte: u8) -> u64 {
    let before = p.with_world(|w| w.writes());
    pages
        .write_durable(pages.pos(at).unwrap(), &[byte; 2 * B as usize])
        .unwrap();
    p.with_world(|w| w.writes()) - before
}

/// "p" (`CAP`), and "q" after it when `blocked`: p's growth cannot follow
/// its last extent.
fn two(p: &SimPlatform, blocked: bool) -> Store<SimPlatform> {
    let s = store(p);
    s.provision_page_region("p", CAP).unwrap();
    if blocked {
        s.provision_page_region("q", CAP).unwrap();
    }
    s
}

/// Growth after the last extent, at the tail: one run in memory (a write
/// across the old end is one device write), two extents on the media, the
/// new range filled with the region's pattern, the slack allocated and not
/// written; all of it as the next open finds it.
#[test]
fn test_growth_after_the_last_extent_is_one_run() {
    let p = fresh(701);
    let s = two(&p, false);
    let pages = s.page_region("p").unwrap();
    let allocations = p.with_world(|w| w.range_allocations());
    assert_eq!(pages.grow(NEW).unwrap(), NEW);
    assert_eq!((pages.len(), pages.capacity()), (NEW, CAP + EXT));
    assert_eq!(p.with_world(|w| w.range_allocations()), allocations + 1);
    let f = found(&p, "p");
    let d = f.data_offset;
    assert_eq!(f.extents, [(d, CAP), (d + CAP, EXT)]);
    assert_eq!((f.data_size, f.capacity, f.extra), (NEW, CAP + EXT, None));
    assert_eq!(f.footprint_seq, f.page_meta_seq, "the growth's own write");
    // The new range holds the pattern; the slack is not written.
    let mut out = vec![0u8; 2 * B as usize];
    pages.read(pages.pos(CAP).unwrap(), &mut out).unwrap();
    assert_eq!(out, pattern(&p, &f, CAP, 2 * B));
    assert_ne!(media_at(&p, d + NEW, B), pattern(&p, &f, NEW, B));
    let inside = writes_of(&p, &pages, 0, 1);
    assert_eq!(writes_of(&p, &pages, CAP - B, 2), inside, "one run");
    audit(&p);
    drop((pages, s));
    let s = store(&p);
    let pages = s.page_region("p").unwrap();
    assert_eq!((pages.len(), pages.capacity()), (NEW, CAP + EXT));
    assert!(s.report().metadata.extra_extents.is_empty());
    pages.read(pages.pos(CAP - B).unwrap(), &mut out).unwrap();
    assert_eq!(out, [2u8; 2 * B as usize]);
    assert_eq!(writes_of(&p, &pages, CAP - B, 3), inside, "one run at open");
}

/// Growth where the space after the last extent is taken: a new run past
/// everything allocated, aligned as a large data area; a write across the
/// old end is two device writes, each run's bytes at its own offsets.
#[test]
fn test_growth_elsewhere_is_a_new_run() {
    let p = fresh(702);
    let s = two(&p, true);
    let pages = s.page_region("p").unwrap();
    let q = found(&p, "q");
    pages.grow(NEW).unwrap();
    let f = found(&p, "p");
    assert_eq!(f.extents.len(), 2);
    let (at, len) = f.extents[1];
    assert_eq!(len, EXT);
    assert!(at >= q.data_offset + CAP, "past q");
    assert_eq!(at % EXT, 0, "aligned");
    let inside = writes_of(&p, &pages, 0, 1);
    assert_eq!(writes_of(&p, &pages, CAP - B, 5), inside + 1, "two runs");
    assert_eq!(media_at(&p, f.data_offset + CAP - B, B), [5u8; B as usize]);
    assert_eq!(media_at(&p, at, B), [5u8; B as usize]);
    audit(&p);
    drop((pages, s));
    let s = store(&p);
    let pages = s.page_region("p").unwrap();
    let mut out = vec![0u8; 2 * B as usize];
    pages.read(pages.pos(CAP - B).unwrap(), &mut out).unwrap();
    assert_eq!(out, [5u8; 2 * B as usize]);
    // A later growth past the new capacity follows the new last extent.
    assert_eq!(pages.grow(CAP + EXT + B).unwrap(), CAP + EXT + B);
    let f = found(&p, "p");
    assert_eq!(f.extents.len(), 3);
    assert_eq!(f.extents[2].0, at + EXT);
    audit(&p);
}

/// The new extent is allocated at once: a region placed after the growth,
/// in the same open, goes elsewhere.
#[test]
fn test_the_new_extent_is_allocated_at_once() {
    let p = fresh(706);
    let s = two(&p, true);
    s.page_region("p").unwrap().grow(NEW).unwrap();
    let (at, _) = found(&p, "p").extents[1];
    // Larger than the gap before the extent.
    s.provision_page_region("r", EXT).unwrap();
    assert!(found(&p, "r").header_at[0] >= at + EXT);
    audit(&p);
}

/// The table's end for `opts()`: the volume pair, then the pages.
const BASE: u64 = 2 * B + 2 * PAGES as u64 * TABLE;

/// Free space right after the region's last extent (a released region's,
/// too small for an aligned extent) takes the new extent there: one run,
/// though not at the tail.
#[test]
fn test_growth_into_free_space_after_the_region() {
    let p = fresh(704);
    let s = two(&p, false);
    // Unaligned (below 1 MiB), after p; q after it.
    s.provision_page_region("gap", EXT - B).unwrap();
    s.provision_page_region("q", CAP).unwrap();
    s.page_region("gap").unwrap().release().unwrap();
    let pages = s.page_region("p").unwrap();
    pages.grow(NEW).unwrap();
    let f = found(&p, "p");
    assert_eq!(f.extents[1].0, f.data_offset + CAP, "after the last extent");
    let inside = writes_of(&p, &pages, 0, 1);
    assert_eq!(writes_of(&p, &pages, CAP - B, 2), inside, "one run");
    audit(&p);
}

/// A hole before the region (a released region's) takes the new extent by
/// first fit: the span starts there now.
#[test]
fn test_growth_into_a_hole_before_the_region() {
    let p = fresh(705);
    let s = store(&p);
    // A pad up to 2 blocks before 1 MiB, so the hole's aligned data area
    // follows it with nothing between; p and q after the hole.
    s.provision_page_region("pad", EXT - 4 * B - BASE).unwrap();
    s.provision_page_region("hole", 2 * EXT).unwrap();
    s.provision_page_region("p", CAP).unwrap();
    s.provision_page_region("q", CAP).unwrap();
    let hole = found(&p, "hole");
    s.page_region("hole").unwrap().release().unwrap();
    let pages = s.page_region("p").unwrap();
    pages.grow(NEW).unwrap();
    let f = found(&p, "p");
    let (at, _) = f.extents[1];
    assert!(
        at >= hole.header_at[0] && at < f.header_at[0],
        "in the hole"
    );
    // (The audit checks the span is tight: it starts at the extent.)
    audit(&p);
    drop((pages, s));
    let s = store(&p);
    assert_eq!(s.page_region("p").unwrap().len(), NEW);
}

/// Appends run on past the old capacity, across the new run.
#[test]
fn test_appends_across_the_new_run() {
    let p = fresh(703);
    let s = store(&p);
    let a = s.provision_append_region("a", CAP).unwrap();
    s.provision_page_region("q", CAP).unwrap();
    for i in 0..15u8 {
        a.append(&[i; B as usize]).unwrap();
    }
    a.grow(NEW).unwrap();
    let t = a.append(&[0xAB; 2 * B as usize]).unwrap();
    a.sync_through(&t).unwrap();
    let mut out = vec![0u8; 2 * B as usize];
    a.read(CAP - B, &mut out).unwrap();
    assert_eq!(out, [0xAB; 2 * B as usize]);
    let f = found(&p, "a");
    assert_eq!(media_at(&p, f.extents[1].0, B), [0xAB; B as usize]);
}

/// Power fails at each write submission of a growth, keeping, dropping or
/// tearing what was cached: the next open finds the old size with nothing
/// changed, the old size with the header one extent ahead (that extent
/// reported and free), or the new size committed by the growth's own page
/// write. Each is seen, and a growth again then works.
#[test]
fn test_a_power_failure_at_any_point_of_a_growth() {
    let mut seen = [false; 3];
    let mut k = 1u64;
    loop {
        let mut finished = false;
        for seed in 0..4 {
            let p = fresh(710 + seed);
            let s = two(&p, true);
            let pages = s.page_region("p").unwrap();
            let regions = s.space().regions;
            let n = Arc::new(AtomicU64::new(0));
            p.with_world(|w| {
                let n = Arc::clone(&n);
                w.set_submit_probe(Some(Arc::new(move || {
                    if n.fetch_add(1, Ordering::SeqCst) + 1 == k {
                        panic!("power fails");
                    }
                })));
            });
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pages.grow(NEW)));
            finished |= matches!(r, Ok(Ok(_)));
            std::mem::forget((pages, s));
            p.with_world(|w| {
                w.set_submit_probe(None);
                w.crash(&CrashMode::Random);
            });
            let step = format!("write {k} seed {seed}");
            let s = Store::open(p.clone(), Path::new(DIR), kopts())
                .unwrap_or_else(|e| panic!("{step}: {e:?}"));
            assert!(s.report().unavailable.is_empty(), "{step}");
            let pages = s.page_region("p").unwrap();
            let f = found(&p, "p");
            let extra = s.report().metadata.extra_extents.clone();
            match pages.len() {
                CAP => {
                    assert_eq!((f.capacity, f.extents.len()), (CAP, 1), "{step}");
                    assert_eq!(s.space().regions, regions, "{step}: space");
                    if extra.is_empty() {
                        assert_eq!(f.extra, None, "{step}");
                        seen[0] = true;
                    } else {
                        assert_eq!(extra.len(), 1, "{step}");
                        assert_eq!(extra[0].0.get(), f.id, "{step}");
                        assert_eq!(Some(extra[0].1), f.extra, "{step}");
                        seen[1] = true;
                    }
                    // A growth again works and commits.
                    assert_eq!(pages.grow(NEW).unwrap(), NEW, "{step}");
                    let f = found(&p, "p");
                    assert_eq!(f.footprint_seq, f.page_meta_seq, "{step}");
                }
                NEW => {
                    assert!(extra.is_empty(), "{step}");
                    assert_eq!((f.capacity, f.extents.len()), (CAP + EXT, 2), "{step}");
                    assert_eq!(f.footprint_seq, f.page_meta_seq, "{step}");
                    let mut out = vec![0u8; 2 * B as usize];
                    pages.read(pages.pos(CAP).unwrap(), &mut out).unwrap();
                    assert_eq!(out, pattern(&p, &f, CAP, 2 * B), "{step}");
                    seen[2] = true;
                }
                len => panic!("{step}: {len}"),
            }
            audit(&p);
        }
        if finished {
            break;
        }
        k += 1;
        assert!(k < 64, "a growth never finished");
    }
    assert_eq!(seen, [true; 3], "old, header ahead, grown");
}

/// A store left with "p"'s header one extent ahead (a growth whose page
/// write was refused before submission), open, not poisoned.
fn ahead(seed: u64) -> (SimPlatform, Store<SimPlatform>) {
    let mut k = 1u64;
    loop {
        let p = fresh(seed);
        let s = two(&p, true);
        let pages = s.page_region("p").unwrap();
        p.with_world(|w| w.faults_mut().fail_submit = Some((w.write_submits() + k, 5)));
        let failed = pages.grow(NEW).is_err();
        p.with_world(|w| w.faults_mut().fail_submit = None);
        let f = found(&p, "p");
        let listed = header_of(&p, f.id, f.header_at).unwrap().extents.len();
        if failed && listed == 2 && pages.len() == CAP && s.poisoned().is_none() {
            drop(pages);
            return (p, s);
        }
        k += 1;
        assert!(k < 32, "no refused commit after a header write");
    }
}

/// The extent a header one extent ahead lists is free while the region's
/// page is Confirmed, and counted allocated while it is stale (it may be
/// the newest growth); the region is then read-only. Accepting the page
/// (not another) frees it.
#[test]
fn test_an_extra_extent_on_a_stale_page_stays_allocated() {
    let (p, s) = ahead(785);
    drop(s);
    let s = store(&p);
    let f = found(&p, "p");
    let extra = header_of(&p, f.id, f.header_at).unwrap().extents[1];
    assert_eq!(s.report().metadata.extra_extents.len(), 1);
    assert_eq!(s.report().metadata.extra_extents[0].1, extra);
    let free = s.space().released;
    drop(s);
    format2_util::craft::tear_newer_copy_of(&p, 0);
    format2_util::craft::tear_newer_copy_of(&p, 1);
    let s = store(&p);
    assert!(s.report().metadata.frozen);
    assert_eq!(s.report().metadata.extra_extents[0].1, extra);
    assert_eq!(s.space().released, free - extra.1, "the extent allocated");
    let pages = s.page_region("p").unwrap();
    let e = pages.grow(NEW).unwrap_err();
    assert!(is_cause(&e, NotWrittenCause::ReadOnly), "{e:?}");
    // Another page accepted: the extent stays allocated. Its own page
    // accepted: the extent is free again, the region grows.
    s.accept_page_rollback(&[1]).unwrap();
    assert_eq!(s.space().released, free - extra.1, "still allocated");
    s.accept_page_rollback(&[0]).unwrap();
    assert_eq!(s.space().released, free, "the extent freed");
    pages.grow(NEW).unwrap();
    drop((pages, s));
    audit(&p);
}

/// A growth whose commit was refused leaves the header one extent ahead,
/// that extent free; another region takes it, and the region grows again
/// by the same length elsewhere. The header copy the second growth leaves
/// behind lists only the committed extents, so with the newer copy damaged
/// the open refuses the region instead of taking the other region's bytes
/// as its own.
#[test]
fn test_a_header_left_ahead_never_survives_the_next_growth() {
    let (p, s) = ahead(780);
    let f = found(&p, "p");
    let ahead = header_of(&p, f.id, f.header_at).unwrap().extents[1];
    // r takes space the header lists (the extent is free).
    let r = s.provision_page_region("r", EXT - 2 * B).unwrap();
    let at = found(&p, "r").header_at[0];
    assert!(
        at < ahead.0 + ahead.1 && ahead.0 < at + EXT,
        "r over the extent"
    );
    r.write_durable(r.pos(0).unwrap(), &[0x77; B as usize])
        .unwrap();
    let pages = s.page_region("p").unwrap();
    assert_eq!(pages.grow(NEW).unwrap(), NEW);
    let f = found(&p, "p");
    assert_eq!(f.extents.len(), 2);
    assert_ne!(f.extents[1], ahead);
    // The older copy lists the committed extent only.
    let [a, b] = f.header_at;
    let (ha, hb) = (
        header_of(&p, f.id, [a, a]).unwrap(),
        header_of(&p, f.id, [b, b]).unwrap(),
    );
    let (newer, older) = if ha.slot_generation > hb.slot_generation {
        ((a, ha), (b, hb))
    } else {
        ((b, hb), (a, ha))
    };
    assert_eq!(
        older.1.extents.len(),
        1,
        "the older copy: {:?}",
        older.1.extents
    );
    assert_eq!(newer.1.extents, f.extents);
    drop((pages, r, s));
    tear(&p, newer.0);
    let s = store(&p);
    assert!(
        s.page_region("p").is_err(),
        "a region whose header lost an extent"
    );
    let r = s.page_region("r").unwrap();
    let mut out = vec![0u8; B as usize];
    r.read(r.pos(0).unwrap(), &mut out).unwrap();
    assert_eq!(out, [0x77; B as usize], "r keeps its bytes");
}

/// Each write and each flush of a growth fails in turn: the error is
/// returned with the store poisoned and nothing published, and after a
/// power failure the store opens with the old size or the new.
#[test]
fn test_each_write_and_flush_failing_in_turn() {
    for flush in [false, true] {
        let mut k = 1u64;
        loop {
            let p = fresh(720);
            let s = two(&p, true);
            let pages = s.page_region("p").unwrap();
            p.with_world(|w| {
                if flush {
                    w.faults_mut().fail_flush = Some(w.flushes() + k);
                } else {
                    w.faults_mut().fail_write = Some(w.writes() + k);
                }
            });
            let r = pages.grow(NEW);
            let done = r.is_ok();
            if let Err(e) = &r {
                assert!(
                    matches!(e, Error::DurabilityUnknown { .. }),
                    "flush {flush} k {k}: {e:?}"
                );
                assert!(s.poisoned().is_some());
                assert_eq!((pages.len(), pages.capacity()), (CAP, CAP));
            }
            drop((pages, s));
            p.with_world(|w| {
                *w.faults_mut() = FaultPlan::default();
                w.crash(&CrashMode::LoseAll);
            });
            let s = store(&p);
            let len = s.page_region("p").unwrap().len();
            assert!(len == CAP || len == NEW, "flush {flush} k {k}: {len}");
            audit_allowing_extra(&p);
            if done {
                break;
            }
            k += 1;
            assert!(k < 64, "a growth never finished");
        }
    }
}

/// Every refusal of a growth leaves the region, the space report and the
/// tags exactly as they were, the store unpoisoned, and a growth after the
/// cause is gone works.
#[test]
fn test_a_refused_growth_changes_nothing() {
    type Arm = fn(&SimPlatform, &Store<SimPlatform>);
    type Undo = fn(&SimPlatform, &Store<SimPlatform>);
    type Case = (&'static str, Arm, Undo, fn(&Error) -> bool);
    let cases: [Case; 5] = [
        (
            "the device is full",
            |p, _| p.with_world(|w| w.faults_mut().capacity = Some(media_len(w))),
            |p, _| p.with_world(|w| w.faults_mut().capacity = None),
            |e| is_no_space(e, ReserveTag::Growth),
        ),
        (
            "the slack finds no space",
            |p, _| {
                p.with_world(|w| {
                    w.faults_mut().fail_allocate_range = Some((w.range_allocations() + 1, 28));
                })
            },
            |p, _| p.with_world(|w| w.faults_mut().fail_allocate_range = None),
            |e| is_no_space(e, ReserveTag::Growth),
        ),
        (
            "the slack's allocation fails",
            |p, _| {
                p.with_world(|w| {
                    w.faults_mut().fail_allocate_range = Some((w.range_allocations() + 1, 5));
                })
            },
            |p, _| p.with_world(|w| w.faults_mut().fail_allocate_range = None),
            |e| {
                matches!(
                    e,
                    Error::Io {
                        op: Op::Allocate,
                        ..
                    }
                )
            },
        ),
        (
            "the fill does not verify",
            |p, _| p.with_world(|w| w.faults_mut().cached_pages = true),
            |p, _| p.with_world(|w| w.faults_mut().cached_pages = false),
            |e| is_cause(e, NotWrittenCause::NotReady),
        ),
        (
            "the tag's cap",
            |_, s| {
                let used = s.space().tags.iter().find(|t| t.tag == 0).unwrap().physical;
                s.set_cap(0, Some(used + EXT - B));
            },
            |_, s| s.set_cap(0, None),
            |e| is_no_space(e, ReserveTag::Caller(0)),
        ),
    ];
    for (what, arm, undo, want) in cases {
        let p = fresh(730);
        let s = two(&p, true);
        let pages = s.page_region("p").unwrap();
        arm(&p, &s);
        let space = s.space();
        let e = pages.grow(NEW).unwrap_err();
        assert!(want(&e), "{what}: {e:?}");
        assert!(s.poisoned().is_none(), "{what}");
        assert_eq!((pages.len(), pages.capacity()), (CAP, CAP), "{what}");
        let mut now = s.space();
        // The container may have grown: all of that is free; nothing in it
        // changed hands.
        let grown = now.container - space.container;
        assert_eq!(now.released, space.released + grown, "{what}: free");
        now.container = space.container;
        now.released = space.released;
        assert_eq!(now, space, "{what}");
        undo(&p, &s);
        assert_eq!(pages.grow(NEW).unwrap(), NEW, "{what}");
        audit(&p);
    }
    // A cap the extent exactly fits allows it.
    let p = fresh(731);
    let s = two(&p, true);
    let used = s.space().tags.iter().find(|t| t.tag == 0).unwrap().physical;
    s.set_cap(0, Some(used + EXT));
    assert_eq!(s.page_region("p").unwrap().grow(NEW).unwrap(), NEW);
}

fn media_len(w: &store_io_sim::World) -> u64 {
    w.media_of(&std::path::PathBuf::from(DIR), store_io_engine::CONTAINER)
        .unwrap()
        .len() as u64
}

/// Refused before any I/O: a capped region (`RegionLimit`), a header that
/// lists as many extents as it can hold (`Extents`; one fewer still
/// grows), and a frozen store (`Frozen`).
#[test]
fn test_capped_full_and_frozen_regions_refuse_before_any_write() {
    let base = 2 * B + 2 * u64::from(PAGES) * TABLE;
    let max = max_extents(12) as u64;
    let crafted = |seed, extents: u64, capped: bool| {
        let p = fresh(seed);
        let mut r = region(0, RegionKind::Page, "p", base, extents * B);
        if capped {
            r.entry.flags = FLAG_CAPPED;
            r.header.flags = FLAG_CAPPED;
        }
        // Every extent one block, in file order apart from each other (so
        // they are separate runs too).
        r.extents = (0..extents)
            .map(|i| (base + 2 * B + 2 * i * B, B))
            .collect();
        r.entry.extent_count = extents as u16;
        r.header.extent_count = extents as u16;
        r.entry.data_size = extents * B;
        r.entry.span_end = base + 2 * B + 2 * extents * B - B;
        write_regions(&p, 0, &[r]);
        p
    };
    for (what, p, want) in [
        (
            "capped",
            crafted(740, 1, true),
            Some(ReserveTag::RegionLimit),
        ),
        ("full", crafted(741, max, false), Some(ReserveTag::Extents)),
        ("one short", crafted(742, max - 1, false), None),
    ] {
        let s = store(&p);
        let pages = s.page_region("p").unwrap();
        let len = pages.len();
        let writes = p.with_world(|w| w.writes());
        match want {
            Some(tag) => {
                let e = pages.grow(len + B).unwrap_err();
                assert!(is_no_space(&e, tag), "{what}: {e:?}");
                assert_eq!(p.with_world(|w| w.writes()), writes, "{what}");
                assert_eq!(pages.len(), len, "{what}");
            }
            None => {
                assert_eq!(pages.grow(len + B).unwrap(), len + B, "{what}");
                assert_eq!(found(&p, "p").extents.len() as u64, max, "{what}");
            }
        }
    }
    // Frozen: page 1 lost.
    let p = fresh(743);
    drop(two(&p, false));
    lose_page(&p, 1);
    let s = store(&p);
    assert!(s.report().metadata.frozen);
    let pages = s.page_region("p").unwrap();
    let writes = p.with_world(|w| w.writes());
    let e = pages.grow(NEW).unwrap_err();
    assert!(is_cause(&e, NotWrittenCause::Frozen), "{e:?}");
    assert_eq!(p.with_world(|w| w.writes()), writes);
}

/// A grown region's release gives back every extent: the space report
/// counts none of it, each extent is deallocated, and the next region
/// placed by first fit lands in the freed space.
#[test]
fn test_release_of_a_grown_region_frees_every_extent() {
    let p = fresh(750);
    let s = two(&p, true);
    let regions = s.space().regions;
    let physical = s.space().tags[0].physical;
    let pages = s.page_region("p").unwrap();
    pages.grow(NEW).unwrap();
    let f = found(&p, "p");
    assert_eq!(s.space().regions, regions + EXT);
    assert_eq!(s.space().tags[0].physical, physical + EXT, "the tag's too");
    let releases = p.with_world(|w| w.releases());
    pages.release().unwrap();
    assert_eq!(p.with_world(|w| w.releases()), releases + 2, "both extents");
    assert_eq!(s.space().regions, regions - CAP - 2 * B);
    // The freed extent is free space: a region that needs it is placed
    // there (with the gap before it), the container unchanged.
    let container = s.space().container;
    s.provision_page_region("r", EXT - 2 * B).unwrap();
    let r = found(&p, "r");
    assert!(r.header_at[0] < f.extents[1].0 && r.header_at[0] + EXT > f.extents[1].0);
    assert_eq!(s.space().container, container);
    audit(&p);
    drop(s);
    let s = store(&p);
    assert_eq!(s.space().regions, regions - CAP - 2 * B + EXT);
}

/// A region grown after its last extent is one run of two extents: its
/// release frees each extent (as the open records them), so the space is
/// reused without growing the container.
#[test]
fn test_release_of_a_region_grown_in_place_frees_both_extents() {
    let p = fresh(751);
    let s = two(&p, false);
    let pages = s.page_region("p").unwrap();
    pages.grow(NEW).unwrap();
    assert_eq!(found(&p, "p").extents.len(), 2);
    pages.release().unwrap();
    let container = s.space().container;
    // Below 1 MiB (unaligned): it fits only in p's freed extents.
    s.provision_page_region("r", EXT - 4 * B).unwrap();
    assert_eq!(s.space().container, container, "the freed space reused");
    audit(&p);
}

/// Recycling a grown region: its region generation moves on by one (2),
/// while its header slot's generation counts every header write (3: the
/// provisioning, the growth, the recycle); its extents are kept.
#[test]
fn test_recycle_after_a_growth() {
    let p = fresh(760);
    let s = two(&p, true);
    let pages = s.page_region("p").unwrap();
    pages.grow(NEW).unwrap();
    let next = pages.recycle().unwrap();
    drop(pages);
    let pages = next;
    assert_eq!(pages.generation().get(), 2);
    let f = found(&p, "p");
    let h = header_of(&p, f.id, f.header_at).unwrap();
    assert_eq!((h.header.region_generation, h.slot_generation), (2, 3));
    assert_eq!(h.extents, f.extents);
    assert_eq!((pages.len(), pages.capacity()), (NEW, CAP + EXT));
    drop((pages, s));
    let s = store(&p);
    let pages = s.page_region("p").unwrap();
    assert_eq!((pages.len(), pages.generation().get()), (NEW, 2));
    audit(&p);
}

/// On Windows the whole new extent is written with the pattern (NTFS would
/// otherwise zero below a later write past the valid data), and nothing is
/// allocated apart.
#[test]
fn test_windows_fills_the_whole_extent() {
    let probe = SimPlatform::new(SimConfig::volatile(770));
    let mut evidence = Store::create(probe, Path::new(DIR), kopts())
        .unwrap()
        .report()
        .evidence
        .clone();
    evidence.platform = PlatformKind::Windows;
    let mut o = kopts();
    // Borrowed evidence: not verifiable.
    o.trust.override_refusal = true;
    let p = fresh_with(
        SimConfig {
            evidence: Some(evidence),
            ..SimConfig::volatile(771)
        },
        &o,
    );
    let s = Store::open(p.clone(), Path::new(DIR), o).unwrap();
    s.provision_page_region("p", CAP).unwrap();
    s.provision_page_region("q", CAP).unwrap();
    let pages = s.page_region("p").unwrap();
    let allocations = p.with_world(|w| w.range_allocations());
    pages.grow(NEW).unwrap();
    assert_eq!(p.with_world(|w| w.range_allocations()), allocations);
    let f = found(&p, "p");
    let (at, len) = f.extents[1];
    assert_eq!(media_at(&p, at, len), pattern(&p, &f, CAP, len));
}
