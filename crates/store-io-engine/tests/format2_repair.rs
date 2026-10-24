//! Repairs of a format-2 store (v0.5 E6): the live state they change (C1),
//! `repair_unavailable` (C2), `accept_page_rollback` (C3).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

mod format2_util;

use format2_util::audit::audit;
use format2_util::craft::{region, tear_newer_copy_of, write_regions};
use format2_util::*;
use store_io_core::error::{Capability, Error, NotWrittenCause};
use store_io_core::id::RegionId;
use store_io_engine::{Repair, Store, StoreOptions};
use store_io_format::meta::RegionKind;
use store_io_platform::{FileMode, FileName, Platform};
use store_io_sim::{CrashMode, SimConfig, SimPlatform};

const B: u64 = 4096;

/// Right after an open, the live state is what the open found: no
/// repairs, the unavailable regions as reported, frozen as reported.
#[test]
fn test_the_live_state_starts_as_the_open_found_it() {
    // Healthy.
    let p = SimPlatform::new(SimConfig::volatile(1201));
    let s = create(&p);
    s.provision_page_region("a", 8 * B).unwrap();
    drop(s);
    let s = open(&p);
    assert!(!s.frozen() && !s.report().metadata.frozen);
    assert!(s.unavailable_regions().is_empty());
    assert!(s.repairs().is_empty());
    drop(s);
    // An unavailable region (its header never written).
    let p = SimPlatform::new(SimConfig::volatile(1202));
    drop(create(&p));
    let mut r = region(
        0,
        RegionKind::Page,
        "broken",
        2 * B + 2 * u64::from(PAGES) * TABLE,
        8 * B,
    );
    r.write_header = false;
    write_regions(&p, 0, &[r]);
    let s = open(&p);
    assert_eq!(s.unavailable_regions(), s.report().unavailable);
    assert_eq!(s.unavailable_regions().len(), 1);
    assert!(matches!(
        s.page_region("broken"),
        Err(Error::Corruption { .. })
    ));
    assert!(!s.frozen());
    drop(s);
    // A stale page: frozen, its regions read-only.
    let p = SimPlatform::new(SimConfig::volatile(1203));
    let s = create(&p);
    s.provision_page_region("a", 8 * B).unwrap();
    drop(s);
    tear_newer_copy_of(&p, 0);
    let s = open(&p);
    assert!(s.frozen() && s.report().metadata.frozen);
    let a = s.page_region("a").unwrap();
    let e = a
        .write_durable(a.pos(0).unwrap(), &[1; B as usize])
        .unwrap_err();
    assert!(
        matches!(
            e,
            Error::NotWritten {
                cause: NotWrittenCause::ReadOnly,
                ..
            }
        ),
        "{e:?}"
    );
    let e = s.provision_page_region("b", 8 * B).map(drop).unwrap_err();
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
    assert!(s.repairs().is_empty());
}

const BASE: u64 = 2 * B + 2 * PAGES as u64 * TABLE;
const CAP: u64 = 64 * 1024;

fn fresh(seed: u64) -> SimPlatform {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    drop(create(&p));
    p
}

fn id_of(s: &Store<SimPlatform>, name: &str) -> RegionId {
    s.unavailable_regions()
        .into_iter()
        .find(|u| u.name == name)
        .unwrap()
        .id
}

/// An unavailable region (its header never written) repaired: its entry
/// gone at once and after a reopen, its name free, its span free (nothing
/// else inside it), the repair recorded; the container audits clean.
#[test]
fn test_a_region_without_a_header_is_repaired() {
    let p = fresh(1210);
    let mut r = region(0, RegionKind::Page, "broken", BASE, CAP);
    r.write_header = false;
    write_regions(&p, 0, &[r]);
    let s = open(&p);
    let id = id_of(&s, "broken");
    let released = s.space().released;
    s.repair_unavailable(id).unwrap();
    assert!(s.unavailable_regions().is_empty());
    assert_eq!(
        s.space().released,
        released + 2 * B + CAP,
        "its span is free"
    );
    match &s.repairs()[..] {
        [Repair::EntryReleased { region }] => assert_eq!(region.name, "broken"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        s.page_region("broken"),
        Err(Error::NotFound { .. })
    ));
    s.provision_page_region("broken", 8 * B).unwrap();
    drop(s);
    let s = open(&p);
    assert!(s.unavailable_regions().is_empty());
    assert!(s.page_region("broken").is_ok());
    drop(s);
    audit(&p);
}

/// The loser of two overlapping footprints repaired: only its own header
/// blocks become free; the bytes the winner uses inside its span stay
/// allocated, and a new region never lands on them.
#[test]
fn test_an_overlap_loser_repaired_frees_only_what_no_one_uses() {
    let p = fresh(1211);
    let mut old = region(0, RegionKind::Append, "old", BASE, CAP);
    old.entry.footprint_seq = 1;
    let next = BASE + 2 * B + CAP;
    let mut new = region(1, RegionKind::Append, "new", next, CAP);
    new.entry.span_start = BASE + 2 * B;
    new.extents = vec![(BASE + 2 * B, CAP)];
    write_regions(&p, 0, &[old, new]);
    let s = open(&p);
    let (released, held) = (s.space().released, s.space().regions);
    s.repair_unavailable(id_of(&s, "old")).unwrap();
    // Only its own header blocks: the bytes the winner uses inside its
    // span stay allocated, so no placement can hand them out.
    assert_eq!(
        s.space().released,
        released + 2 * B,
        "only its header blocks"
    );
    // Its usage (it passed the entry checks, so it was counted) is gone.
    assert_eq!(s.space().regions, held - (CAP + 2 * B));
    drop(s);
    let s = open(&p);
    assert!(s.unavailable_regions().is_empty());
    assert!(s.append_region("new").is_ok());
}

/// An unavailable entry that supersedes a live region (a newer footprint
/// over its bytes) is that region's only evidence: not repaired while the
/// region is live (`Overlap`, naming it), the region still read-only and
/// still reported superseded after a reopen (it is copied out, marked).
#[test]
fn test_an_entry_superseding_a_live_region_is_kept_while_it_lives() {
    let p = fresh(1218);
    let mut old = region(0, RegionKind::Append, "old", BASE, CAP);
    old.entry.footprint_seq = 1;
    let next = BASE + 2 * B + CAP;
    let mut gone = region(1, RegionKind::Append, "gone", next, CAP);
    gone.entry.span_start = BASE + 2 * B;
    gone.extents = vec![(BASE + 2 * B, CAP)];
    gone.header.state = store_io_format::meta::RegionState::Released;
    write_regions(&p, 0, &[old, gone]);
    let s = open(&p);
    assert_eq!(s.report().metadata.superseded.len(), 1);
    let id = id_of(&s, "gone");
    let writes = p.with_world(|w| w.writes());
    let e = s.repair_unavailable(id).unwrap_err();
    assert!(
        matches!(e, Error::NotWritten { cause: NotWrittenCause::Overlap, ctx } if ctx.region == Some(RegionId::from_raw(0))),
        "{e:?}"
    );
    assert_eq!(
        p.with_world(|w| w.writes()),
        writes,
        "refused before any write"
    );
    assert_eq!(s.unavailable_regions().len(), 1);
    // Still superseded after a reopen: the evidence is there.
    drop(s);
    let s = open(&p);
    assert_eq!(s.report().metadata.superseded.len(), 1);
    assert_eq!(s.unavailable_regions().len(), 1);
    let old = s.append_region("old").unwrap();
    assert!(old.resume_at(0).is_err(), "still read-only");
}

/// A region cut off by a truncated container repaired: growth is allowed
/// again once no entry names space past the end.
#[test]
fn test_a_truncated_region_repaired_lets_the_container_grow() {
    let p = SimPlatform::new(SimConfig::volatile(1212));
    let s = create(&p);
    let b = s.provision_append_region("b", CAP).unwrap();
    b.append_durable(&[7; B as usize]).unwrap();
    drop((b, s));
    let len = media(&p).len() as u64;
    {
        let dir = p.open_dir(std::path::Path::new(DIR), false).unwrap();
        let file = p
            .open_file(
                &dir,
                &FileName::new(store_io_engine::CONTAINER).unwrap(),
                FileMode::ReadWrite,
            )
            .unwrap();
        p.set_len(&file, len - 4 * B).unwrap();
        p.flush_all(&file).unwrap();
    }
    let s = open(&p);
    let big = |s: &Store<SimPlatform>| s.provision_page_region("c", 1 << 20).map(drop);
    assert!(
        matches!(big(&s), Err(Error::Corruption { .. })),
        "growth refused while it exists"
    );
    s.repair_unavailable(id_of(&s, "b")).unwrap();
    big(&s).unwrap();
    drop(s);
    audit(&p);
    // Two cut off: growth stays refused until both are repaired.
    let p = SimPlatform::new(SimConfig::volatile(1219));
    let s = create(&p);
    for name in ["x", "y"] {
        let r = s.provision_append_region(name, CAP).unwrap();
        r.append_durable(&[7; B as usize]).unwrap();
    }
    drop(s);
    let len = media(&p).len() as u64;
    {
        let dir = p.open_dir(std::path::Path::new(DIR), false).unwrap();
        let file = p
            .open_file(
                &dir,
                &FileName::new(store_io_engine::CONTAINER).unwrap(),
                FileMode::ReadWrite,
            )
            .unwrap();
        // All of y and the end of x.
        p.set_len(&file, len - (CAP + 2 * B) - 4 * B).unwrap();
        p.flush_all(&file).unwrap();
    }
    let s = open(&p);
    assert_eq!(s.unavailable_regions().len(), 2);
    s.repair_unavailable(id_of(&s, "y")).unwrap();
    assert!(
        matches!(big(&s), Err(Error::Corruption { .. })),
        "x still names space past the end"
    );
    s.repair_unavailable(id_of(&s, "x")).unwrap();
    big(&s).unwrap();
    drop(s);
    let s = open(&p);
    assert!(s.unavailable_regions().is_empty());
    assert!(s.page_region("c").is_ok());
    drop(s);
    audit(&p);
}

/// Past the end without a name to list: an entry of an unknown kind whose
/// span lies past the container's end keeps growth refused after a named
/// one is repaired (only the store-wide flag remembers it), until it is
/// repaired too.
#[test]
fn test_growth_stays_refused_while_any_entry_names_space_past_the_end() {
    let p = fresh(1221);
    let mut x = region(0, RegionKind::Page, "x", BASE, CAP);
    x.extend = false;
    let mut u = region(1, RegionKind::Page, "u", BASE + 2 * B + CAP, CAP);
    u.extend = false;
    // No header either: nothing extends the file over x's span.
    u.write_header = false;
    u.kind_raw = Some(9);
    write_regions(&p, 0, &[x, u]);
    let s = open(&p);
    let big = |s: &Store<SimPlatform>| s.provision_page_region("c", 1 << 20).map(drop);
    assert!(matches!(big(&s), Err(Error::Corruption { .. })));
    s.repair_unavailable(id_of(&s, "x")).unwrap();
    assert!(
        matches!(big(&s), Err(Error::Corruption { .. })),
        "the unknown entry still names space past the end"
    );
    let &[(page, seq)] = &s.report().metadata.invalid_entries[..] else {
        panic!("{:?}", s.report().metadata.invalid_entries);
    };
    s.repair_unavailable(RegionId::from_raw((u64::from(page) << 32) | u64::from(seq)))
        .unwrap();
    big(&s).unwrap();
    drop(s);
    audit(&p);
}

/// An entry of a kind this version does not know, named by its id: its
/// entry released, its name free, recorded as such.
#[test]
fn test_an_unknown_kind_entry_is_repaired_when_named() {
    let p = fresh(1213);
    let mut newer = region(0, RegionKind::Append, "newer", BASE, CAP);
    newer.kind_raw = Some(9);
    write_regions(&p, 0, &[newer]);
    let s = open(&p);
    let &[(page, seq)] = &s.report().metadata.invalid_entries[..] else {
        panic!("{:?}", s.report().metadata.invalid_entries);
    };
    assert!(
        s.provision_page_region("newer", B).is_err(),
        "its name reserved"
    );
    let id = RegionId::from_raw((u64::from(page) << 32) | u64::from(seq));
    s.repair_unavailable(id).unwrap();
    assert_eq!(s.repairs(), [Repair::InvalidEntryReleased { page, seq }]);
    s.provision_page_region("newer", B).unwrap();
    drop(s);
    let s = open(&p);
    assert!(s.report().metadata.invalid_entries.is_empty());
    drop(s);
    audit(&p);
}

/// Refused, nothing written: while allocation is frozen, on a read-only
/// store, for a usable region or an unknown id, on format 1.
#[test]
fn test_repairs_are_refused_where_they_cannot_be_trusted() {
    let p = fresh(1214);
    let mut r = region(0, RegionKind::Page, "broken", BASE, CAP);
    r.write_header = false;
    let good = region(1, RegionKind::Page, "good", BASE + 2 * B + CAP, CAP);
    write_regions(&p, 0, &[r, good]);
    let writes = |p: &SimPlatform| p.with_world(|w| w.writes());
    // Read-only.
    let s = Store::open_readonly(p.clone(), std::path::Path::new(DIR), opts()).unwrap();
    let id = id_of(&s, "broken");
    let w = writes(&p);
    let e = s.repair_unavailable(id).unwrap_err();
    assert!(
        matches!(
            e,
            Error::NotWritten {
                cause: NotWrittenCause::ReadOnly,
                ..
            }
        ),
        "{e:?}"
    );
    assert_eq!(writes(&p), w);
    drop(s);
    // A usable region, an id nothing has.
    let s = open(&p);
    let w = writes(&p);
    let good = RegionId::from_raw(1);
    for id in [good, RegionId::from_raw(77)] {
        let e = s.repair_unavailable(id).unwrap_err();
        assert!(matches!(e, Error::NotFound { .. }), "{e:?}");
    }
    assert_eq!(writes(&p), w);
    assert!(s.page_region("good").is_ok());
    drop(s);
    // Frozen: another page stale.
    tear_newer_copy_of(&p, 1);
    let s = open(&p);
    assert!(s.frozen());
    let w = writes(&p);
    let e = s.repair_unavailable(id).unwrap_err();
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
    assert_eq!(writes(&p), w);
    assert_eq!(s.unavailable_regions().len(), 1);
    assert!(s.repairs().is_empty());
    drop(s);
    // Format 1.
    let p = SimPlatform::new(SimConfig::volatile(1215));
    let s = Store::create(
        p,
        std::path::Path::new(DIR),
        StoreOptions {
            container_format: store_io_format::slot::FORMAT_1,
            ..opts()
        },
    )
    .unwrap();
    let e = s.repair_unavailable(RegionId::from_raw(0)).unwrap_err();
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

/// The repair's page write fails: refused before submission, nothing
/// changed and the store serves on (then a repair, durable: gone after a
/// power failure). Failed after submission (its barrier): the store is
/// poisoned, memory unchanged, and power failing then leaves the entry
/// there (the write lost) or gone (the write kept), the container sound.
#[test]
fn test_a_failed_repair_leaves_the_entry_or_its_absence() {
    for (seed, at_flush, crash) in [
        (1216, false, CrashMode::LoseAll),
        (1217, true, CrashMode::LoseAll),
        (1220, true, CrashMode::KeepAll),
    ] {
        let p = fresh(seed);
        let mut r = region(0, RegionKind::Page, "broken", BASE, CAP);
        r.write_header = false;
        write_regions(&p, 0, &[r]);
        let s = open(&p);
        let id = id_of(&s, "broken");
        let released = s.space().released;
        p.with_world(|w| {
            if at_flush {
                w.faults_mut().fail_flush = Some(w.flushes() + 1);
            } else {
                w.faults_mut().fail_submit = Some((w.write_submits() + 1, 5));
            }
        });
        let e = s.repair_unavailable(id).unwrap_err();
        p.with_world(|w| {
            w.faults_mut().fail_flush = None;
            w.faults_mut().fail_submit = None;
        });
        // Memory as it was, either way.
        assert_eq!(s.unavailable_regions().len(), 1, "{e:?}");
        assert!(s.repairs().is_empty());
        assert_eq!(s.space().released, released);
        let want = if at_flush {
            assert!(matches!(e, Error::DurabilityUnknown { .. }), "{e:?}");
            assert!(s.poisoned().is_some());
            usize::from(matches!(crash, CrashMode::LoseAll))
        } else {
            assert!(s.poisoned().is_none(), "{e:?}");
            s.repair_unavailable(id).unwrap();
            0
        };
        std::mem::forget(s);
        p.with_world(|w| w.crash(&crash));
        let s = open(&p);
        assert_eq!(s.unavailable_regions().len(), want, "seed {seed}");
        drop(s);
        if want == 0 {
            audit(&p);
        }
    }
}

fn is_overlap(e: &Error, region: u64) -> bool {
    matches!(e, Error::NotWritten { cause: NotWrittenCause::Overlap, ctx } if ctx.region == Some(RegionId::from_raw(region)))
}

fn is_frozen(e: &Error) -> bool {
    matches!(
        e,
        Error::NotWritten {
            cause: NotWrittenCause::Frozen,
            ..
        }
    )
}

/// An entry whose `footprint_seq` is past its page's write counts newest
/// of all; a rewrite of its page would make it count older and lift what
/// rests on it. Here it supersedes a live region on another page: no
/// repair or release that rewrites its page is made, no provisioning
/// lands there, the region stays superseded after a reopen, and the entry
/// itself is not repaired while the region lives.
#[test]
fn test_a_page_with_an_entry_past_its_write_is_never_rewritten_around_it() {
    let p = fresh(1222);
    let m0 = max_meta_seq(&p) + 1;
    // Page 0 (written at m0): X, its sequence m0 + 1, past the page's
    // write, its span over L's data; Y, a header never written; W, sound.
    let next = BASE + 2 * B + CAP;
    let mut x = region(0, RegionKind::Append, "x", next, CAP);
    x.entry.span_start = BASE + 2 * B;
    x.extents = vec![(BASE + 2 * B, CAP)];
    x.entry.footprint_seq = m0 + 1;
    let far = next + 2 * B + CAP;
    let mut y = region(1, RegionKind::Page, "y", far, CAP);
    y.write_header = false;
    let w = region(2, RegionKind::Page, "w", far + 2 * B + CAP, CAP);
    write_regions(&p, 0, &[x, y, w]);
    // Page 1 (written at m0 + 1): L, its sequence that write's.
    write_regions(&p, 1, &[region(0, RegionKind::Append, "l", BASE, CAP)]);
    let s = open(&p);
    assert_eq!(s.report().metadata.superseded.len(), 1, "l superseded by x");
    let writes = p.with_world(|w| w.writes());
    let e = s.repair_unavailable(id_of(&s, "y")).unwrap_err();
    assert!(is_frozen(&e), "{e:?}");
    let e = s.page_region("w").unwrap().release().unwrap_err();
    assert!(is_frozen(&e), "{e:?}");
    let e = s.repair_unavailable(id_of(&s, "x")).unwrap_err();
    assert!(is_overlap(&e, 1 << 32), "{e:?}");
    // A provisioning goes to another page.
    s.provision_page_region("n", B).unwrap();
    assert_eq!(
        p.with_world(|w| w.writes()) - writes,
        3,
        "n's fill, header and page 2"
    );
    drop(s);
    let s = open(&p);
    assert_eq!(s.report().metadata.superseded.len(), 1, "still superseded");
    let l = s.append_region("l").unwrap();
    assert!(
        matches!(
            l.resume_at(0),
            Err(Error::NotWritten {
                cause: NotWrittenCause::ReadOnly,
                ..
            })
        ),
        "still read-only"
    );
}

/// Overlap losers: two of equal sequence demote each other (neither is
/// repaired: each is the other's evidence); in a chain A < B < C, B (the
/// cause of A's demotion) is repaired only after A, and nothing comes back
/// served without its evidence.
#[test]
fn test_overlap_losers_are_repaired_only_from_the_bottom_of_a_chain() {
    // A tie.
    let p = fresh(1223);
    let mut a = region(0, RegionKind::Append, "a", BASE, CAP);
    a.entry.footprint_seq = 1;
    let next = BASE + 2 * B + CAP;
    let mut b = region(1, RegionKind::Append, "b", next, CAP);
    b.entry.span_start = BASE + 2 * B;
    b.extents = vec![(BASE + 2 * B, CAP)];
    b.entry.footprint_seq = 1;
    write_regions(&p, 0, &[a, b]);
    let s = open(&p);
    assert_eq!(s.unavailable_regions().len(), 2);
    let e = s.repair_unavailable(id_of(&s, "a")).unwrap_err();
    assert!(is_overlap(&e, 1), "{e:?}");
    let e = s.repair_unavailable(id_of(&s, "b")).unwrap_err();
    assert!(is_overlap(&e, 0), "{e:?}");
    drop(s);
    // A chain: A (1) under B (2) under C (3), C over B's header blocks
    // only: B is A's only evidence, C is B's.
    let p = fresh(1224);
    let mut a = region(0, RegionKind::Append, "a", BASE, CAP);
    a.entry.footprint_seq = 1;
    let b_at = BASE + 2 * B + CAP;
    let mut b = region(1, RegionKind::Append, "b", b_at, CAP);
    b.entry.span_start = BASE + 2 * B;
    b.extents = vec![(BASE + 2 * B, CAP)];
    b.entry.footprint_seq = 2;
    let c_at = b_at + 2 * B + CAP;
    let mut c = region(2, RegionKind::Append, "c", c_at, CAP);
    c.entry.span_start = b_at;
    c.extents = vec![(b_at, CAP)];
    c.entry.footprint_seq = 3;
    write_regions(&p, 0, &[a, b, c]);
    let s = open(&p);
    let names = |s: &Store<SimPlatform>| {
        let mut v: Vec<String> = s
            .unavailable_regions()
            .into_iter()
            .map(|u| u.name)
            .collect();
        v.sort();
        v
    };
    assert_eq!(names(&s), ["a", "b"]);
    let e = s.repair_unavailable(id_of(&s, "b")).unwrap_err();
    assert!(is_overlap(&e, 0), "b is a's evidence: {e:?}");
    // C, live, is B's: not released while B is there.
    let e = s.append_region("c").unwrap().release().unwrap_err();
    assert!(is_overlap(&e, 1), "{e:?}");
    s.repair_unavailable(id_of(&s, "a")).unwrap();
    s.repair_unavailable(id_of(&s, "b")).unwrap();
    drop(s);
    let s = open(&p);
    assert!(s.unavailable_regions().is_empty());
    assert!(s.append_region("a").is_err() && s.append_region("b").is_err());
    s.append_region("c").unwrap().release().unwrap();
    drop(s);
    audit(&p);
}

/// A tie that a third, live region also beats: either tied entry is
/// repaired (the other stays demoted by the live one), judged again over
/// what remains rather than by pairs.
#[test]
fn test_a_tie_another_region_also_beats_is_repaired() {
    let p = fresh(1227);
    let mut a = region(0, RegionKind::Append, "a", BASE, CAP);
    a.entry.footprint_seq = 1;
    let b_at = BASE + 2 * B + CAP;
    let mut b = region(1, RegionKind::Append, "b", b_at, CAP);
    b.entry.span_start = BASE + 2 * B;
    b.extents = vec![(BASE + 2 * B, CAP)];
    b.entry.footprint_seq = 1;
    let c_at = b_at + 2 * B + CAP;
    let mut c = region(2, RegionKind::Append, "c", c_at, CAP);
    c.entry.span_start = BASE + 2 * B;
    c.extents = vec![(BASE + 2 * B, CAP)];
    c.entry.footprint_seq = 2;
    write_regions(&p, 0, &[a, b, c]);
    let s = open(&p);
    assert_eq!(s.unavailable_regions().len(), 2);
    s.repair_unavailable(id_of(&s, "a")).unwrap();
    s.repair_unavailable(id_of(&s, "b")).unwrap();
    drop(s);
    let s = open(&p);
    assert!(s.unavailable_regions().is_empty());
    assert!(s.append_region("c").is_ok());
}

/// The winner of a name kept while the loser's entry is there: released,
/// the loser would be served at the next open (spec §9 step 4's names).
#[test]
fn test_a_name_winner_is_released_only_after_the_loser_is_repaired() {
    let p = fresh(1228);
    let mut old = region(0, RegionKind::Page, "same", BASE, CAP);
    old.entry.footprint_seq = 1;
    let new = region(1, RegionKind::Page, "same", BASE + 2 * B + CAP, CAP);
    write_regions(&p, 0, &[old, new]);
    let s = open(&p);
    assert_eq!(s.unavailable_regions().len(), 1);
    let e = s.page_region("same").unwrap().release().unwrap_err();
    assert!(is_overlap(&e, 0), "{e:?}");
    s.repair_unavailable(RegionId::from_raw(0)).unwrap();
    s.page_region("same").unwrap().release().unwrap();
    drop(s);
    let s = open(&p);
    assert!(s.unavailable_regions().is_empty());
    assert!(s.page_region("same").is_err());
}

/// The winner of an overlap is not released while the loser's entry is
/// there (the loser would be served at the next open, its bytes the
/// winner's): the loser repaired first, the winner then releases.
#[test]
fn test_an_overlap_winner_is_released_only_after_the_loser_is_repaired() {
    let p = fresh(1225);
    let mut old = region(0, RegionKind::Append, "old", BASE, CAP);
    old.entry.footprint_seq = 1;
    let next = BASE + 2 * B + CAP;
    let mut new = region(1, RegionKind::Append, "new", next, CAP);
    new.entry.span_start = BASE + 2 * B;
    new.extents = vec![(BASE + 2 * B, CAP)];
    write_regions(&p, 0, &[old, new]);
    let s = open(&p);
    let e = s.append_region("new").unwrap().release().unwrap_err();
    assert!(is_overlap(&e, 0), "{e:?}");
    assert!(s.append_region("new").is_ok(), "still served");
    drop(s);
    let s = open(&p);
    assert_eq!(s.unavailable_regions().len(), 1);
    s.repair_unavailable(id_of(&s, "old")).unwrap();
    s.append_region("new").unwrap().release().unwrap();
    drop(s);
    let s = open(&p);
    assert!(s.unavailable_regions().is_empty());
    assert!(s.append_region("old").is_err() && s.append_region("new").is_err());
}

/// A torn write of such a page after a crash is not healed: the heal's
/// later `meta_seq` would change how the entry counts. The page is left
/// stale (allocation frozen), never written.
#[test]
fn test_a_page_with_an_entry_past_its_write_is_not_healed() {
    let p = SimPlatform::new(SimConfig::volatile(1226));
    crash_open(&p, create(&p));
    let m0 = max_meta_seq(&p) + 1;
    let mut x = region(0, RegionKind::Page, "x", BASE, CAP);
    x.entry.footprint_seq = m0 + 1;
    write_regions(&p, 0, &[x]);
    tear_newer_copy_of(&p, 0);
    let s = open(&p);
    assert!(
        s.report().metadata.healed.is_none(),
        "{:?}",
        s.report().metadata.healed
    );
    assert_eq!(s.report().metadata.stale_pages, [0]);
    assert!(s.frozen());
}

/// Three of one name: repairing the middle one is allowed (the lowest is
/// still beaten by the live winner's name, though nothing overlaps it).
#[test]
fn test_a_name_loser_still_beaten_by_another_is_repaired() {
    let p = fresh(1229);
    let mut low = region(0, RegionKind::Page, "same", BASE, CAP);
    low.entry.footprint_seq = 1;
    let mut mid = region(1, RegionKind::Page, "same", BASE + 2 * B + CAP, CAP);
    mid.entry.footprint_seq = 2;
    let top = region(2, RegionKind::Page, "same", BASE + 2 * (2 * B + CAP), CAP);
    write_regions(&p, 0, &[low, mid, top]);
    let s = open(&p);
    assert_eq!(s.unavailable_regions().len(), 2);
    s.repair_unavailable(RegionId::from_raw(1)).unwrap();
    drop(s);
    let s = open(&p);
    assert_eq!(s.unavailable_regions().len(), 1, "the lowest still demoted");
    assert!(s.page_region("same").is_ok());
}

/// What an unavailable region's writes may have touched is judged by its
/// header alone, not by the file's length (a later growth would change
/// that): H's header lists an extent past the file's end, yet its
/// committed extents leave L alone, so only X (no header: its span)
/// supersedes L, and X is not repaired while L lives.
#[test]
fn test_what_a_header_may_have_written_does_not_depend_on_the_file_length() {
    let p = fresh(1230);
    assert!(
        max_meta_seq(&p) + 1 > 3,
        "the sequences below are past writes"
    );
    let b0 = BASE;
    // H: headers at b0, its committed extent far along its span, and one
    // more extent its header lists past the file's end.
    let h_ext = b0 + 6 * B + 2 * CAP;
    let mut h = region(0, RegionKind::Page, "h", b0, CAP);
    h.entry.footprint_seq = 2;
    h.entry.span_end = h_ext + CAP;
    h.entry.extent_count = 1;
    h.extents = vec![(h_ext, CAP), (64 << 20, CAP)];
    h.header.extent_count = 2;
    h.extend = false;
    // L: inside H's span, outside H's headers and committed extent.
    let mut l = region(1, RegionKind::Page, "l", b0 + 2 * B, CAP);
    l.entry.footprint_seq = 1;
    // X: no header, its span over L.
    let x_at = b0 + 4 * B + CAP;
    let mut x = region(2, RegionKind::Page, "x", x_at, CAP);
    x.entry.span_start = b0 + 2 * B;
    x.entry.footprint_seq = 3;
    x.write_header = false;
    write_regions(&p, 0, &[h, l, x]);
    // The file holds H's span, not its extra extent.
    put(&p, h_ext + CAP - B, &vec![0u8; B as usize]);
    let s = open(&p);
    assert_eq!(s.report().metadata.superseded, [RegionId::from_raw(1)]);
    let e = s.repair_unavailable(id_of(&s, "x")).unwrap_err();
    assert!(is_overlap(&e, 1), "x is l's only superseder: {e:?}");
}

/// A demoted region beaten by a live region's header blocks only: an
/// unrelated repair is allowed (judged again, the header blocks still
/// beat it).
#[test]
fn test_a_live_regions_header_blocks_count_in_the_judgement() {
    let p = fresh(1231);
    let l = region(0, RegionKind::Page, "l", BASE, CAP);
    // D, older, its one extent over L's two header blocks only.
    let d_at = BASE + 2 * B + CAP;
    let mut d = region(1, RegionKind::Page, "d", d_at, 2 * B);
    d.entry.span_start = BASE;
    d.extents = vec![(BASE, 2 * B)];
    d.entry.footprint_seq = 1;
    // X, unrelated, its header never written.
    let mut x = region(2, RegionKind::Page, "x", d_at + 4 * B, CAP);
    x.write_header = false;
    write_regions(&p, 0, &[l, d, x]);
    let s = open(&p);
    assert_eq!(s.unavailable_regions().len(), 2);
    s.repair_unavailable(id_of(&s, "x")).unwrap();
    assert_eq!(s.unavailable_regions().len(), 1, "d still demoted");
}

/// A header listing one extent past the file's end (a lost growth) is
/// judged by itself, not by the file's length: the newer region keeps its
/// name now, and still after the file grows, so a write to the name is
/// never hidden by a region that comes back.
#[test]
fn test_a_name_stays_with_its_region_when_the_file_grows() {
    let p = fresh(1232);
    let mut e = region(0, RegionKind::Page, "n", BASE, CAP);
    e.entry.footprint_seq = 1;
    let h_at = BASE + 2 * B + CAP;
    let mut h = region(1, RegionKind::Page, "n", h_at, CAP);
    h.extents.push((64 << 20, CAP));
    h.header.extent_count = 2;
    h.extend = false;
    write_regions(&p, 0, &[e, h]);
    put(&p, h_at + 2 * B + CAP - B, &vec![0u8; B as usize]);
    let s = open(&p);
    assert_eq!(
        s.unavailable_regions()
            .iter()
            .map(|u| u.name.as_str())
            .collect::<Vec<_>>(),
        ["n"],
        "the older one"
    );
    let n = s.page_region("n").unwrap();
    assert_eq!(s.unavailable_regions()[0].id, RegionId::from_raw(0));
    n.write_durable(n.pos(0).unwrap(), &[0x5A; B as usize])
        .unwrap();
    drop(n);
    s.provision_page_region("big", 1 << 20).unwrap();
    drop(s);
    let s = open(&p);
    let n = s.page_region("n").unwrap();
    let mut back = vec![0u8; B as usize];
    n.read(n.pos(0).unwrap(), &mut back).unwrap();
    assert!(
        back == [0x5A; B as usize],
        "the acknowledged write is under the name"
    );
}

/// A newer entry of a name failing step 3 (its header never written)
/// still takes the name from an older region: the older one is not
/// served, and the newer one is not repaired while the older one is there.
#[test]
fn test_an_unavailable_newer_entry_keeps_its_name_from_an_older_region() {
    let p = fresh(1233);
    let mut old = region(0, RegionKind::Page, "n", BASE, CAP);
    old.entry.footprint_seq = 1;
    let mut new = region(1, RegionKind::Page, "n", BASE + 2 * B + CAP, CAP);
    new.write_header = false;
    write_regions(&p, 0, &[old, new]);
    let s = open(&p);
    assert_eq!(s.unavailable_regions().len(), 2);
    assert!(matches!(s.page_region("n"), Err(Error::Corruption { .. })));
    let e = s.repair_unavailable(RegionId::from_raw(1)).unwrap_err();
    assert!(is_overlap(&e, 0), "{e:?}");
    s.repair_unavailable(RegionId::from_raw(0)).unwrap();
    s.repair_unavailable(RegionId::from_raw(1)).unwrap();
    drop(s);
    let s = open(&p);
    assert!(s.unavailable_regions().is_empty());
}

/// A header listing fewer extents than its entry (rolled back): what its
/// writes may have touched is its whole span, so a region inside the span
/// past the header's extents is possibly superseded.
#[test]
fn test_a_rolled_back_header_counts_its_whole_span() {
    let p = fresh(1234);
    let mut x = region(0, RegionKind::Page, "x", BASE, CAP);
    // Its entry: two extents, the second where L lies; its header lists
    // only the first.
    let l_at = BASE + 2 * B + CAP;
    x.entry.extent_count = 2;
    x.entry.capacity = CAP + 2 * B + CAP;
    x.entry.span_end = l_at + 2 * B + CAP;
    x.entry.footprint_seq = 2;
    let mut l = region(1, RegionKind::Page, "l", l_at, CAP);
    l.entry.footprint_seq = 1;
    write_regions(&p, 0, &[x, l]);
    let s = open(&p);
    assert_eq!(s.report().metadata.superseded, [RegionId::from_raw(1)]);
}

/// A newer entry of a name failing the entry checks (its span past the
/// file's end) takes the name from an older region too, at the open and in
/// the judgement before a removal: an unrelated repair is allowed (the
/// older one stays demoted by that name).
#[test]
fn test_a_cut_off_newer_entry_keeps_its_name_from_an_older_region() {
    let p = fresh(1235);
    // X first, unrelated, its header never written.
    let mut x = region(0, RegionKind::Page, "x", BASE, CAP);
    x.write_header = false;
    let old_at = BASE + 2 * B + CAP;
    let mut old = region(1, RegionKind::Page, "n", old_at, CAP);
    old.entry.footprint_seq = 1;
    let mut new = region(2, RegionKind::Page, "n", old_at + 2 * B + CAP, 1 << 30);
    new.extend = false;
    write_regions(&p, 0, &[x, old, new]);
    let s = open(&p);
    assert_eq!(s.unavailable_regions().len(), 3);
    assert!(matches!(s.page_region("n"), Err(Error::Corruption { .. })));
    s.repair_unavailable(id_of(&s, "x")).unwrap();
    let e = s.repair_unavailable(RegionId::from_raw(2)).unwrap_err();
    assert!(is_overlap(&e, 1), "{e:?}");
}

/// The judgement before a removal counts names of entries failing step 3:
/// an older region demoted by a newer one of its name with no header stays
/// demoted, so an unrelated repair is allowed.
#[test]
fn test_an_unrelated_repair_keeps_a_name_demotion_by_an_unavailable_entry() {
    let p = fresh(1236);
    let mut old = region(0, RegionKind::Page, "n", BASE, CAP);
    old.entry.footprint_seq = 1;
    let mut new = region(1, RegionKind::Page, "n", BASE + 2 * B + CAP, CAP);
    new.write_header = false;
    let mut x = region(2, RegionKind::Page, "x", BASE + 2 * (2 * B + CAP), CAP);
    x.write_header = false;
    write_regions(&p, 0, &[old, new, x]);
    let s = open(&p);
    assert_eq!(s.unavailable_regions().len(), 3);
    s.repair_unavailable(id_of(&s, "x")).unwrap();
}

/// The judgement counts what an entry with no header may have written (its
/// span): a live region inside it stays superseded, so an unrelated repair
/// is allowed.
#[test]
fn test_an_unrelated_repair_keeps_a_supersession_by_a_span() {
    let p = fresh(1237);
    let mut l = region(0, RegionKind::Page, "l", BASE + 2 * B, CAP);
    l.entry.footprint_seq = 1;
    let mut s_ = region(1, RegionKind::Page, "s", BASE, CAP);
    s_.entry.span_end = BASE + 4 * B + CAP + 2 * B + CAP;
    s_.write_header = false;
    let mut x = region(2, RegionKind::Page, "x", BASE + 6 * B + 2 * CAP, CAP);
    x.write_header = false;
    write_regions(&p, 0, &[l, s_, x]);
    let s = open(&p);
    assert_eq!(s.report().metadata.superseded, [RegionId::from_raw(0)]);
    s.repair_unavailable(id_of(&s, "x")).unwrap();
}
