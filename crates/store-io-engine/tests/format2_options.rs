//! Format-2 region options (v0.5 E4 C10): a capped region (`max_size`) is
//! one extent of its cap, charged whole, grown within it one run and never
//! past it; its slack allocated unwritten (Linux, the simulator), or
//! written where that costs later or cannot be done (Windows, macOS).

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

use format2_util::audit::{Found, audit, header_of};
use format2_util::*;
use store_io_core::error::{Capability, Error, NotWrittenCause, ReserveTag};
use store_io_core::evidence::PlatformKind;
use store_io_engine::{RegionOptions, Store, StoreOptions};
use store_io_format::fill::fill_keyed_v1;
use store_io_format::meta2::FLAG_CAPPED;
use store_io_sim::{CrashMode, SimConfig, SimPlatform};

const B: u64 = 4096;
const SIZE: u64 = 16 * B;
/// The cap: 1 MiB.
const CAP: u64 = 1 << 20;

fn capped() -> RegionOptions {
    RegionOptions::default().size(SIZE).max_size(CAP)
}

fn fresh(seed: u64) -> SimPlatform {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    drop(create(&p));
    p
}

/// A store whose probe reports `platform` (borrowed evidence, so the
/// refusal is overridden), with the valid-data model as asked.
fn on(platform: PlatformKind, seed: u64, model: bool) -> (SimPlatform, StoreOptions) {
    let probe = SimPlatform::new(SimConfig::volatile(seed));
    let mut evidence = Store::create(probe, Path::new(DIR), opts())
        .unwrap()
        .report()
        .evidence
        .clone();
    evidence.platform = platform;
    let mut o = opts();
    o.trust.override_refusal = true;
    o.keyed_fill = true;
    let p = SimPlatform::new(SimConfig {
        evidence: Some(evidence),
        valid_data_model: model,
        ..SimConfig::volatile(seed + 1)
    });
    drop(Store::create(p.clone(), Path::new(DIR), o.clone()).unwrap());
    (p, o)
}

fn is_cause(e: &Error, want: NotWrittenCause) -> bool {
    matches!(e, Error::NotWritten { cause, .. } if *cause == want)
}

fn found(p: &SimPlatform, name: &str) -> Found {
    audit(p).into_iter().find(|f| f.name == name).unwrap()
}

/// Whether any of the container's `[at, at + len)` reads as never written.
fn unwritten(p: &SimPlatform, at: u64, len: u64) -> bool {
    p.with_world(|w| {
        let file = w
            .file_id(&std::path::PathBuf::from(DIR), store_io_engine::CONTAINER)
            .unwrap();
        w.is_unwritten(file, at, len)
    })
}

/// Region `f`'s keyed pattern for region offsets `[from, from + len)`.
fn pattern(p: &SimPlatform, f: &Found, from: u64, len: u64) -> Vec<u8> {
    let h = header_of(p, f.id, f.header_at).unwrap();
    let mut out = vec![0u8; len as usize];
    fill_keyed_v1(&h.header.fill_key, from, &mut out);
    out
}

fn tag0(s: &Store<SimPlatform>) -> (u64, u64) {
    s.space()
        .tags
        .into_iter()
        .find(|t| t.tag == 0)
        .map_or((0, 0), |t| (t.logical, t.physical))
}

/// A cap below 1 MiB: placed unaligned, so the region after it starts
/// where its slack ends.
fn small_cap() -> RegionOptions {
    RegionOptions::default().size(SIZE).max_size(CAP / 2)
}

/// A capped region is one extent of its cap, flagged in its entry and
/// header, its data the size asked, its tag charged the whole cap; its
/// slack allocated once, unwritten (never verified); it grows to the cap in
/// steps, one run, with no new space; past the cap `NoSpace(RegionLimit)`
/// with nothing written; all as the next open finds it.
#[test]
fn test_a_capped_region_is_one_extent_of_its_cap() {
    let p = fresh(801);
    let s = open(&p);
    let (logical, physical) = tag0(&s);
    let allocations = p.with_world(|w| w.range_allocations());
    let pages = s.provision_page_region_with("p", capped()).unwrap();
    assert_eq!((pages.len(), pages.capacity()), (SIZE, CAP));
    assert_eq!(p.with_world(|w| w.range_allocations()), allocations + 1);
    assert_eq!(tag0(&s), (logical + SIZE, physical + CAP + 2 * B));
    let f = found(&p, "p");
    assert_eq!(f.extents, [(f.data_offset, CAP)]);
    assert_eq!((f.data_size, f.capacity), (SIZE, CAP));
    let h = header_of(&p, f.id, f.header_at).unwrap();
    assert_eq!(h.header.flags & FLAG_CAPPED, FLAG_CAPPED);
    // The slack is allocated, never written; the data written.
    assert!(unwritten(&p, f.data_offset + SIZE, CAP - SIZE), "the slack");
    assert!(
        !unwritten(&p, f.header_at[0], 2 * B + SIZE),
        "headers and data"
    );
    let container = s.space().container;
    for to in [2 * SIZE, CAP / 2, CAP] {
        assert_eq!(pages.grow(to).unwrap(), to);
    }
    assert_eq!(s.space().container, container, "no new space");
    assert_eq!(found(&p, "p").extents.len(), 1, "one extent");
    let writes = p.with_world(|w| w.writes());
    let e = pages.grow(CAP + B).unwrap_err();
    assert!(
        matches!(
            e,
            Error::NoSpace {
                tag: ReserveTag::RegionLimit,
                ..
            }
        ),
        "{e:?}"
    );
    assert_eq!(p.with_world(|w| w.writes()), writes, "nothing written");
    pages
        .write_durable(pages.pos(CAP - B).unwrap(), &[7u8; B as usize])
        .unwrap();
    drop((pages, s));
    let s = open(&p);
    let pages = s.page_region("p").unwrap();
    assert_eq!((pages.len(), pages.capacity()), (CAP, CAP));
    let mut out = vec![0u8; B as usize];
    pages.read(pages.pos(CAP - B).unwrap(), &mut out).unwrap();
    assert_eq!(out, [7u8; B as usize]);
    assert!(pages.grow(CAP + B).is_err(), "the cap is on the media");
    assert_eq!(tag0(&s), (logical + CAP, physical + CAP + 2 * B));
    audit(&p);
}

/// Only the header blocks and the data are verified: the slack, allocated
/// and unwritten, is never read for readiness (it would not pass).
#[test]
fn test_the_slack_is_allocated_not_verified() {
    let p = fresh(802);
    let s = open(&p);
    // An append region, so its frontier's limit follows the growth.
    let a = s.provision_append_region_with("a", capped()).unwrap();
    // (Provisioning succeeded with the slack unwritten: a check of it
    // would have refused.)
    let f = found(&p, "a");
    // Appends to the size, then the growth fills [SIZE, 2 * SIZE) only.
    a.resume_at(0).ok();
    for _ in 0..16 {
        a.append(&[1u8; B as usize]).unwrap();
    }
    assert!(a.append(&[1u8; B as usize]).is_err(), "full at the size");
    a.grow(2 * SIZE).unwrap();
    // The growth filled [SIZE, 2 * SIZE) only.
    assert!(
        !unwritten(&p, f.data_offset + SIZE, SIZE),
        "the grown range"
    );
    assert!(
        unwritten(&p, f.data_offset + 2 * SIZE, CAP - 2 * SIZE),
        "the rest"
    );
    let t = a.append(&[2u8; B as usize]).unwrap();
    a.sync_through(&t).unwrap();
    assert_eq!(found(&p, "a").extents, f.extents, "the same extent");
}

/// Options that contradict each other are refused before any I/O; a cap
/// in format 1 is unsupported; a cap below one block still holds the
/// region's one block.
#[test]
fn test_options_are_checked_before_any_write() {
    let p = fresh(803);
    let s = open(&p);
    let writes = p.with_world(|w| w.writes());
    let bad = RegionOptions::default().size(2 * B).max_size(B);
    let e = s
        .provision_page_region_with("p", bad)
        .map(|_| ())
        .unwrap_err();
    assert!(is_cause(&e, NotWrittenCause::InvalidOptions), "{e:?}");
    let e = s
        .provision_append_region_with("a", bad)
        .map(|_| ())
        .unwrap_err();
    assert!(is_cause(&e, NotWrittenCause::InvalidOptions), "{e:?}");
    assert_eq!(p.with_world(|w| w.writes()), writes);
    let tiny = s
        .provision_page_region_with("t", RegionOptions::default().max_size(1))
        .unwrap();
    assert_eq!((tiny.len(), tiny.capacity()), (B, B));
    // Format 1.
    let p1 = SimPlatform::new(SimConfig::volatile(804));
    let s1 = Store::create(
        p1.clone(),
        Path::new(DIR),
        StoreOptions {
            container_format: 1,
            ..opts()
        },
    )
    .unwrap();
    let e = s1
        .provision_page_region_with("p", capped())
        .map(|_| ())
        .unwrap_err();
    assert!(
        matches!(
            e,
            Error::Unsupported {
                what: Capability::FormatVersion
            }
        ),
        "{e:?}"
    );
    // Without a cap format 1 provisions as before.
    s1.provision_page_region_with("q", RegionOptions::default().size(SIZE))
        .unwrap();
    // A capped default is refused there too.
    drop(s1);
    let s1 = Store::open(
        p1.clone(),
        Path::new(DIR),
        StoreOptions {
            container_format: 1,
            region_defaults: Some(capped()),
            ..opts()
        },
    )
    .unwrap();
    let e = s1
        .provision_page_region_default("r")
        .map(|_| ())
        .unwrap_err();
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

/// A reservation pays for a capped region's whole capacity.
#[test]
fn test_a_reservation_pays_for_the_whole_cap() {
    let p = fresh(805);
    let s = open(&p);
    let mut short = s.reserve(SIZE + 2 * B, 0).unwrap();
    let e = short
        .provision_page_region_with("p", capped())
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(e, Error::NoSpace { .. }), "{e:?}");
    drop(short);
    let mut r = s.reserve(CAP + 2 * B, 0).unwrap();
    r.provision_page_region_with("p", capped()).unwrap();
    assert_eq!(r.remaining(), 0);
}

/// The slack's allocation failing (no space, or another error) leaves no
/// region and the space as it was (the container may have grown); nothing
/// is poisoned.
#[test]
fn test_the_slack_failing_leaves_nothing() {
    for code in [28, 5] {
        let p = fresh(806);
        let s = open(&p);
        let before = s.space();
        p.with_world(|w| {
            w.faults_mut().fail_allocate_range = Some((w.range_allocations() + 1, code))
        });
        let e = s
            .provision_page_region_with("p", capped())
            .map(|_| ())
            .unwrap_err();
        match code {
            28 => assert!(matches!(e, Error::NoSpace { .. }), "{e:?}"),
            _ => assert!(matches!(e, Error::Io { .. }), "{e:?}"),
        }
        assert!(s.poisoned().is_none());
        assert!(s.page_region("p").is_err());
        assert_eq!(s.space().regions, before.regions);
        assert_eq!(tag0(&s), (0, 0));
        drop(s);
        let s = open(&p);
        assert!(s.page_region("p").is_err());
        audit(&p);
    }
}

/// Power fails at each write submission of a capped provisioning: the next
/// open finds no region, or the whole capped region.
#[test]
fn test_a_power_failure_at_any_point_of_a_capped_provisioning() {
    let mut k = 1u64;
    loop {
        let p = fresh(807);
        let s = open(&p);
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
            s.provision_page_region_with("p", capped()).map(|_| ())
        }));
        let finished = matches!(r, Ok(Ok(())));
        std::mem::forget(s);
        p.with_world(|w| {
            w.set_submit_probe(None);
            w.crash(&CrashMode::Random);
        });
        let s = open(&p);
        if let Ok(pages) = s.page_region("p") {
            assert_eq!((pages.len(), pages.capacity()), (SIZE, CAP), "write {k}");
        }
        audit(&p);
        if finished {
            break;
        }
        k += 1;
        assert!(k < 64, "the provisioning never finished");
    }
}

/// Windows: a capped region's whole capacity is written at provisioning,
/// so the next region placed after it pays no zeroing of its slack (the
/// valid-data model); a growth within the cap writes the range again
/// (nothing on disk says those bytes are this region's) with the same
/// pattern, and is still checked.
#[test]
fn test_windows_writes_the_whole_cap() {
    let (p, o) = on(PlatformKind::Windows, 810, true);
    let s = Store::open(p.clone(), Path::new(DIR), o).unwrap();
    let allocations = p.with_world(|w| w.range_allocations());
    let pages = s.provision_page_region_with("p", small_cap()).unwrap();
    let gap = p.with_world(|w| w.gap_fill_bytes());
    s.provision_page_region("q", SIZE).unwrap();
    assert!(found(&p, "q").header_at[0] >= found(&p, "p").data_offset + CAP / 2);
    assert_eq!(p.with_world(|w| w.gap_fill_bytes()), gap, "no zeroing");
    assert_eq!(p.with_world(|w| w.range_allocations()), allocations);
    let f = found(&p, "p");
    let before = media(&p);
    let slack = (f.data_offset + SIZE) as usize..(f.data_offset + CAP / 2) as usize;
    assert_eq!(
        &before[slack.clone()],
        &pattern(&p, &f, SIZE, CAP / 2 - SIZE)[..],
        "written"
    );
    // Foreign bytes there (a container moved from another system, say):
    // the growth writes the range again.
    put(&p, f.data_offset + SIZE, &[0x5A; B as usize]);
    pages.grow(CAP / 2).unwrap();
    assert_eq!(
        &media(&p)[slack],
        &before[(f.data_offset + SIZE) as usize..(f.data_offset + CAP / 2) as usize],
        "the pattern again"
    );
    let mut out = vec![0u8; B as usize];
    pages
        .read(pages.pos(CAP / 2 - B).unwrap(), &mut out)
        .unwrap();
    assert_eq!(out, pattern(&p, &f, CAP / 2 - B, B));
    // Still checked: pages in the cache refuse it.
    let pages = s.provision_page_region_with("r", capped()).unwrap();
    p.with_world(|w| w.faults_mut().cached_pages = true);
    let e = pages.grow(CAP).unwrap_err();
    assert!(is_cause(&e, NotWrittenCause::NotReady), "{e:?}");
    p.with_world(|w| w.faults_mut().cached_pages = false);
    audit(&p);
}

/// Elsewhere than Windows, the valid-data model is moot but the zeroing is
/// counted: the unwritten slack would cost the next region a gap there,
/// which is why Windows writes it.
#[test]
fn test_elsewhere_the_slack_is_left_unwritten() {
    let p = fresh(812);
    let s = open(&p);
    s.provision_page_region_with("p", small_cap()).unwrap();
    let gap = p.with_world(|w| w.gap_fill_bytes());
    s.provision_page_region("q", SIZE).unwrap();
    assert!(
        p.with_world(|w| w.gap_fill_bytes()) > gap,
        "the slack's gap"
    );
}

/// macOS cannot allocate inside a file: a capped region's slack, and the
/// rest of a growth's extent, are written instead (placed by first fit as
/// anywhere, a released hole included), and nothing is allocated apart; a
/// reservation still covers a capped region.
#[test]
fn test_macos_writes_the_slack() {
    let (p, o) = on(PlatformKind::MacOs, 820, false);
    let s = Store::open(p.clone(), Path::new(DIR), o).unwrap();
    s.provision_page_region("hole", 2 * CAP).unwrap();
    s.provision_page_region("q", SIZE).unwrap();
    let hole = found(&p, "hole");
    s.page_region("hole").unwrap().release().unwrap();
    let allocations = p.with_world(|w| w.range_allocations());
    s.provision_page_region_with("p", capped()).unwrap();
    let f = found(&p, "p");
    assert!(
        f.header_at[0] >= hole.header_at[0] && f.data_offset + CAP <= hole.data_offset + 2 * CAP,
        "in the hole"
    );
    assert!(!unwritten(&p, f.data_offset, CAP), "the slack written");
    assert_eq!(
        &media(&p)[(f.data_offset + SIZE) as usize..(f.data_offset + CAP) as usize],
        &pattern(&p, &f, SIZE, CAP - SIZE)[..]
    );
    // Growth beyond capacity: the whole new extent written.
    let q = s.page_region("q").unwrap();
    q.grow(SIZE + B).unwrap();
    let g = found(&p, "q");
    let (at, len) = g.extents[1];
    assert!(!unwritten(&p, at, len), "the extent written");
    assert_eq!(p.with_world(|w| w.range_allocations()), allocations);
    // From a reservation of exactly the cap.
    let mut r = s.reserve(CAP + 2 * B, 0).unwrap();
    r.provision_page_region_with("r", capped()).unwrap();
    assert_eq!(r.remaining(), 0);
    audit(&p);
}

fn with_defaults(defaults: Option<RegionOptions>) -> StoreOptions {
    StoreOptions {
        region_defaults: defaults,
        ..opts()
    }
}

/// Store-wide defaults (an open-time choice, never stored): both kinds of
/// region take them, capped or not; without them the calls are refused
/// before any I/O; a later open with other defaults changes only regions
/// provisioned from then on; defaults a call would refuse never fail the
/// open, only the call.
#[test]
fn test_store_wide_region_defaults() {
    let p = fresh(830);
    let s = Store::open(p.clone(), Path::new(DIR), with_defaults(Some(capped()))).unwrap();
    let a = s.provision_append_region_default("a").unwrap();
    let pg = s.provision_page_region_default("p").unwrap();
    assert_eq!((a.len(), a.capacity()), (SIZE, CAP));
    assert_eq!((pg.len(), pg.capacity()), (SIZE, CAP));
    drop((a, pg, s));
    // Unset: refused, nothing written.
    let s = open(&p);
    let writes = p.with_world(|w| w.writes());
    let e = s
        .provision_page_region_default("q")
        .map(|_| ())
        .unwrap_err();
    assert!(is_cause(&e, NotWrittenCause::InvalidOptions), "{e:?}");
    let e = s
        .provision_append_region_default("q")
        .map(|_| ())
        .unwrap_err();
    assert!(is_cause(&e, NotWrittenCause::InvalidOptions), "{e:?}");
    assert_eq!(p.with_world(|w| w.writes()), writes);
    drop(s);
    // Other defaults: only new regions change.
    let s = Store::open(
        p.clone(),
        Path::new(DIR),
        with_defaults(Some(RegionOptions::default().size(2 * SIZE))),
    )
    .unwrap();
    assert_eq!(s.page_region("p").unwrap().capacity(), CAP);
    let q = s.provision_page_region_default("q").unwrap();
    assert_eq!((q.len(), q.capacity()), (2 * SIZE, 2 * SIZE));
    drop((q, s));
    // Defaults the call refuses: the open succeeds, the call does not.
    let bad = RegionOptions::default().size(2 * B).max_size(B);
    let s = Store::open(p.clone(), Path::new(DIR), with_defaults(Some(bad))).unwrap();
    let e = s
        .provision_page_region_default("r")
        .map(|_| ())
        .unwrap_err();
    assert!(is_cause(&e, NotWrittenCause::InvalidOptions), "{e:?}");
    // Explicit options still work beside them.
    s.provision_page_region_with("r", RegionOptions::default().size(SIZE))
        .unwrap();
    drop(s);
    // From a reservation: the defaults' whole cap.
    let s = Store::open(p.clone(), Path::new(DIR), with_defaults(Some(capped()))).unwrap();
    let mut r = s.reserve(2 * (CAP + 2 * B), 0).unwrap();
    let a = r.provision_append_region_default("ra").unwrap();
    let pg = r.provision_page_region_default("rp").unwrap();
    assert_eq!((a.capacity(), pg.capacity(), r.remaining()), (CAP, CAP, 0));
    audit(&p);
}
