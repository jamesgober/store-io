//! The format-2 open's region checks (spec §9 steps 3-6) against media an
//! independent writer crafted (`format2_util::craft`): what is usable, what
//! is unavailable and why, what is read-only, and that a read-only verdict
//! reached by the region checks writes nothing.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

mod format2_util;

use std::path::Path;

use format2_util::craft::{Region, lose_page, region, tear_newer_copy_of, write_regions};
use format2_util::*;
use store_io_core::error::{Capability, Error, NotWrittenCause};
use store_io_engine::{Store, Unavailable};
use store_io_format::meta::{RegionKind, RegionState};
use store_io_format::slot::SlotIndex;
use store_io_sim::{SimConfig, SimPlatform};

const B: u64 = 4096;
/// The table's end for `opts()`: the volume pair, then three pages.
const BASE: u64 = 2 * B + 2 * PAGES as u64 * TABLE;
const CAP: u64 = 64 * 1024;
/// Past the first region's span.
const NEXT: u64 = BASE + 2 * B + CAP;

fn fresh(seed: u64) -> SimPlatform {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    drop(create(&p));
    p
}

fn append(seq: u32, name: &str, offset: u64) -> Region {
    region(seq, RegionKind::Append, name, offset, CAP)
}

/// An append region with its headers at `headers` (past the first
/// region's span) and its one extent over the first region's data, as a
/// region placed after the first was released and its space reused would
/// be.
fn over_first(seq: u32, name: &str, headers: u64) -> Region {
    let mut r = append(seq, name, headers);
    r.entry.span_start = BASE + 2 * B;
    r.extents = vec![(BASE + 2 * B, CAP)];
    r
}

fn reason(s: &Store<SimPlatform>, name: &str) -> Option<Unavailable> {
    s.report()
        .unavailable
        .iter()
        .find(|u| u.name == name)
        .map(|u| u.reason)
}

fn is_read_only(e: &Error) -> bool {
    matches!(
        e,
        Error::NotWritten {
            cause: NotWrittenCause::ReadOnly,
            ..
        }
    )
}

#[test]
fn test_a_crafted_append_region_opens_and_serves_data() {
    let p = fresh(201);
    write_regions(&p, 0, &[append(0, "log", BASE)]);
    let s = open(&p);
    assert!(
        s.report().unavailable.is_empty(),
        "{:?}",
        s.report().unavailable
    );
    assert!(!s.report().metadata.frozen);
    let log = s.append_region("log").unwrap();
    assert_eq!(log.resume_at(0).unwrap(), 0);
    let data = vec![7u8; B as usize];
    let (pos, _receipt) = log.append_durable(&data).unwrap();
    assert_eq!(pos.offset(), 0);
    drop(log);
    drop(s);
    let s = open(&p);
    let mut out = vec![0u8; B as usize];
    assert_eq!(
        s.append_region("log").unwrap().read(0, &mut out).unwrap(),
        B as usize
    );
    assert_eq!(out, data);
    // The data landed in the crafted extent, past the two header blocks.
    let m = media(&p);
    assert_eq!(
        &m[(BASE + 2 * B) as usize..(BASE + 3 * B) as usize],
        &data[..]
    );
}

#[test]
fn test_page_and_slot_regions_open() {
    let p = fresh(202);
    write_regions(
        &p,
        0,
        &[
            region(0, RegionKind::Page, "pages", BASE, CAP),
            region(1, RegionKind::Slot, "slot", NEXT, 0),
        ],
    );
    let s = open(&p);
    assert!(
        s.report().unavailable.is_empty(),
        "{:?}",
        s.report().unavailable
    );
    assert!(s.page_region("pages").is_ok());
    assert!(s.slot("slot").is_ok());
}

#[test]
fn test_an_entry_failing_its_checks_is_unavailable_and_reserved() {
    let p = fresh(203);
    let mut bad = append(0, "bad", BASE);
    bad.entry.data_size = bad.entry.capacity + B;
    write_regions(&p, 0, &[bad, append(1, "good", NEXT)]);
    let s = open(&p);
    assert_eq!(reason(&s, "bad"), Some(Unavailable::EntryInvalid));
    assert!(matches!(
        s.append_region("bad"),
        Err(Error::Corruption { .. })
    ));
    assert!(s.append_region("good").is_ok());
}

#[test]
fn test_header_failures_each_have_their_reason() {
    let p = fresh(204);
    let mut missing = append(0, "missing", BASE);
    missing.write_header = false;
    let mut foreign = append(1, "foreign", NEXT);
    foreign.header.region_id = 77;
    let mut released = append(2, "released", NEXT + 2 * B + CAP);
    released.header.state = RegionState::Released;
    let mut short = append(3, "short", NEXT + 2 * (2 * B + CAP));
    short.extents[0].1 -= B;
    write_regions(&p, 0, &[missing, foreign, released, short]);
    let s = open(&p);
    assert_eq!(reason(&s, "missing"), Some(Unavailable::HeaderUnreadable));
    assert_eq!(reason(&s, "foreign"), Some(Unavailable::HeaderMismatch));
    assert_eq!(reason(&s, "released"), Some(Unavailable::HeaderReleased));
    assert_eq!(reason(&s, "short"), Some(Unavailable::ExtentsInvalid));
}

/// Two footprints over the same bytes: the newer stays; equal, both go.
#[test]
fn test_the_newer_of_two_overlapping_regions_stays_usable() {
    let p = fresh(205);
    let mut old = append(0, "old", BASE);
    old.entry.footprint_seq = 1;
    write_regions(&p, 0, &[old, over_first(1, "new", NEXT)]);
    let s = open(&p);
    assert_eq!(reason(&s, "old"), Some(Unavailable::Overlaps));
    assert!(s.append_region("new").is_ok());

    let p = fresh(206);
    write_regions(&p, 0, &[append(0, "a", BASE), over_first(1, "b", NEXT)]);
    let s = open(&p);
    assert_eq!(reason(&s, "a"), Some(Unavailable::Overlaps));
    assert_eq!(reason(&s, "b"), Some(Unavailable::Overlaps));
}

#[test]
fn test_a_name_goes_to_the_newer_region() {
    let p = fresh(207);
    let mut old = append(0, "same", BASE);
    old.entry.footprint_seq = 1;
    write_regions(&p, 0, &[old, append(1, "same", NEXT)]);
    let s = open(&p);
    let u = &s.report().unavailable;
    assert_eq!(u.len(), 1, "{u:?}");
    assert_eq!(u[0].reason, Unavailable::NameTaken);
    assert_eq!(u[0].id.get(), 0);
    assert!(s.append_region("same").is_ok());
}

#[test]
fn test_an_extra_extent_is_reported_and_the_region_usable() {
    let p = fresh(208);
    let mut r = append(0, "grown", BASE);
    r.header.extent_count = 2;
    r.extents.push((NEXT, 4 * B));
    write_regions(&p, 0, &[r]);
    let s = open(&p);
    assert!(s.append_region("grown").is_ok());
    let extra = &s.report().metadata.extra_extents;
    assert_eq!(extra.len(), 1);
    assert_eq!(extra[0].1, (NEXT, 4 * B));
}

#[test]
fn test_a_region_of_several_extents_is_not_yet_supported() {
    let p = fresh(209);
    let mut r = append(0, "split", BASE);
    r.entry.extent_count = 2;
    r.header.extent_count = 2;
    r.entry.span_end += 4 * B;
    r.entry.capacity += 4 * B;
    r.extents.push((BASE + 2 * B + CAP, 4 * B));
    write_regions(&p, 0, &[r]);
    let e = Store::open(p.clone(), Path::new(DIR), opts()).map(|_| ());
    assert!(
        matches!(
            e,
            Err(Error::Unsupported {
                what: Capability::FormatVersion
            })
        ),
        "{e:?}"
    );
}

/// A page whose newest copy is torn after a clean close is stale: its
/// regions open read-only, allocation is frozen, and the report says the
/// headers agree with the entries the older copy holds.
#[test]
fn test_regions_on_a_stale_page_are_read_only() {
    let p = fresh(210);
    write_regions(&p, 0, &[append(0, "log", BASE)]);
    tear_newer_copy_of(&p, 0);
    let s = open(&p);
    let m = &s.report().metadata;
    assert_eq!(m.stale_pages, vec![0]);
    assert!(m.frozen);
    assert_eq!(m.stale_page_agreement, vec![(0, true)]);
    // Positioning an append region is a write: refused, as appends are.
    let log = s.append_region("log").unwrap();
    let e = log.resume_at(0).unwrap_err();
    assert!(is_read_only(&e), "{e:?}");
    let mut out = vec![0u8; B as usize];
    assert!(log.read(0, &mut out).is_ok(), "read-only, not unreadable");
}

/// An unavailable region with a newer footprint whose own extents overlap
/// a usable region: that region may read another's bytes, so it is
/// read-only, and the overlap with the reserved span is reported.
#[test]
fn test_a_region_a_newer_unavailable_one_overlaps_is_read_only() {
    let p = fresh(211);
    let mut old = append(0, "old", BASE);
    old.entry.footprint_seq = 1;
    let mut gone = over_first(1, "gone", NEXT);
    gone.header.state = RegionState::Released;
    write_regions(&p, 0, &[old, gone]);
    let s = open(&p);
    assert_eq!(reason(&s, "gone"), Some(Unavailable::HeaderReleased));
    let m = &s.report().metadata;
    assert_eq!(m.superseded.len(), 1);
    assert_eq!(m.superseded[0].get(), 0);
    assert_eq!(m.shares_reserved, m.superseded);
    let r = s.append_region("old").unwrap();
    assert!(is_read_only(&r.resume_at(0).unwrap_err()));
}

/// A header with a read-only-compatible feature this version does not know:
/// that region is read-only, the store is not.
#[test]
fn test_a_header_with_an_unknown_ro_compat_feature_is_read_only() {
    let p = fresh(212);
    let mut r = append(0, "foreign", BASE);
    r.ro_compat = 1 << 31;
    write_regions(&p, 0, &[r, append(1, "plain", NEXT)]);
    let s = open(&p);
    assert!(!s.report().read_only);
    let f = s.append_region("foreign").unwrap();
    assert!(is_read_only(&f.resume_at(0).unwrap_err()));
    let plain = s.append_region("plain").unwrap();
    assert_eq!(plain.resume_at(0).unwrap(), 0);
    assert!(plain.append_durable(&[1u8; B as usize]).is_ok());
}

/// A 512 B append region (spec §13, not built): the store opens read-only,
/// with the reason, and the open writes nothing at all.
#[test]
fn test_a_512_byte_append_region_opens_the_store_read_only_writing_nothing() {
    let p = fresh(213);
    let mut r = append(0, "small", BASE);
    r.header.log2_block = 9;
    write_regions(&p, 0, &[r]);
    let before = media(&p);
    let s = open(&p);
    assert!(s.report().read_only);
    assert!(
        s.report()
            .metadata
            .read_only_reason
            .is_some_and(|w| w.contains("512")),
        "{:?}",
        s.report().metadata.read_only_reason
    );
    drop(s);
    assert!(media(&p) == before, "a read-only open wrote");
}

/// On a stale page, a region whose header does not agree with its entry:
/// the report says the page's headers disagree.
#[test]
fn test_a_stale_page_whose_headers_disagree_says_so() {
    let p = fresh(214);
    let mut missing = append(0, "missing", BASE);
    missing.write_header = false;
    write_regions(&p, 0, &[missing, append(1, "fine", NEXT)]);
    tear_newer_copy_of(&p, 0);
    let s = open(&p);
    assert_eq!(s.report().metadata.stale_page_agreement, vec![(0, false)]);
    assert_eq!(reason(&s, "missing"), Some(Unavailable::HeaderUnreadable));
    assert!(s.append_region("fine").is_ok());
}

/// A header winner that does not decode (a must-understand flag from a
/// newer release) is invalid, not unreadable.
#[test]
fn test_a_header_that_does_not_decode_is_invalid() {
    let p = fresh(215);
    let mut r = append(0, "newer", BASE);
    r.header.flags = 0x80;
    write_regions(&p, 0, &[r]);
    let s = open(&p);
    assert_eq!(reason(&s, "newer"), Some(Unavailable::HeaderInvalid));
}

/// A slot's object is read from the format-2 header: after its 64-byte
/// fixed part, `object_len` long; the provisioning generation reads none.
#[test]
fn test_a_slot_object_is_read_from_the_format_2_header() {
    let p = fresh(217);
    let mut committed = region(0, RegionKind::Slot, "committed", BASE, 0);
    committed.header_generation = 2;
    let fresh_slot = region(1, RegionKind::Slot, "fresh", BASE + 2 * B, 0);
    write_regions(&p, 0, &[committed, fresh_slot]);
    let s = open(&p);
    let c = s.slot("committed").unwrap();
    assert_eq!(c.read().as_deref(), Some(&b"hello"[..]));
    assert_eq!(c.capacity(), B as usize - 192);
    assert_eq!(s.slot("fresh").unwrap().read(), None);
}

/// The header pair's winner may be its slot B, at a later generation.
#[test]
fn test_a_header_won_by_its_slot_b_opens() {
    let p = fresh(218);
    let mut r = append(0, "log", BASE);
    r.header_slot = SlotIndex::B;
    r.header_generation = 3;
    write_regions(&p, 0, &[r]);
    let s = open(&p);
    assert!(
        s.report().unavailable.is_empty(),
        "{:?}",
        s.report().unavailable
    );
    assert!(s.append_region("log").is_ok());
}

/// Entries the entry checks refuse: a capacity past the span (it would
/// overflow the usage), a `footprint_seq` past the page's own write, a
/// kind from a newer release (listed by place), and a span past the file's
/// end (reported as such). None panics, and each is unavailable alone.
#[test]
fn test_entries_the_entry_checks_refuse() {
    let p = fresh(219);
    let mut huge = append(0, "huge", BASE);
    huge.entry.capacity = u64::MAX - (B - 1);
    let mut future = append(1, "future", NEXT);
    future.entry.footprint_seq = 1 << 40;
    let mut newer = append(2, "newer", NEXT + 2 * B + CAP);
    newer.kind_raw = Some(9);
    let far = NEXT + 4 * (2 * B + CAP);
    let mut short = append(3, "short", far);
    short.extend = false;
    write_regions(
        &p,
        0,
        &[
            huge,
            future,
            newer,
            short,
            append(4, "fine", NEXT + 2 * (2 * B + CAP)),
        ],
    );
    let s = open(&p);
    assert_eq!(reason(&s, "huge"), Some(Unavailable::EntryInvalid));
    assert_eq!(reason(&s, "future"), Some(Unavailable::EntryInvalid));
    assert_eq!(s.report().metadata.invalid_entries, vec![(0, 2)]);
    assert!(matches!(
        reason(&s, "short"),
        Some(Unavailable::ContainerShort { .. })
    ));
    assert!(s.append_region("fine").is_ok());
}

/// A 512 B region the open finds unavailable decides nothing: the store
/// stays writable.
#[test]
fn test_an_unavailable_512_byte_region_leaves_the_store_writable() {
    let p = fresh(220);
    let mut r = append(0, "small", BASE);
    r.header.log2_block = 9;
    r.header.state = RegionState::Released;
    write_regions(&p, 0, &[r]);
    let s = open(&p);
    assert!(!s.report().read_only);
    assert_eq!(reason(&s, "small"), Some(Unavailable::HeaderReleased));
}

/// A header one extent ahead of its entry on a stale page is a lost growth:
/// the page's headers do not agree.
#[test]
fn test_an_extra_extent_on_a_stale_page_disagrees() {
    let p = fresh(221);
    let mut r = append(0, "grown", BASE);
    r.header.extent_count = 2;
    r.extents.push((NEXT, 4 * B));
    write_regions(&p, 0, &[r]);
    tear_newer_copy_of(&p, 0);
    let s = open(&p);
    assert_eq!(s.report().metadata.stale_page_agreement, vec![(0, false)]);
}

/// An unavailable region's span over a usable region: reported, never a
/// supersession unless its own extents overlap with a newer sequence.
#[test]
fn test_a_reserved_overlap_is_not_a_supersession() {
    // Older: its span and extents overlap, its sequence is lower.
    let p = fresh(222);
    let mut gone = over_first(0, "gone", NEXT);
    gone.entry.footprint_seq = 1;
    gone.header.state = RegionState::Released;
    write_regions(&p, 0, &[gone, append(1, "live", BASE)]);
    let s = open(&p);
    let m = &s.report().metadata;
    assert!(m.superseded.is_empty(), "{m:?}");
    assert_eq!(m.shares_reserved.len(), 1);
    // Newer, but only its span overlaps: its own extent lies past `live`.
    let p = fresh(223);
    let mut old = append(0, "live", BASE);
    old.entry.footprint_seq = 1;
    let mut gone = append(1, "gone", NEXT);
    gone.entry.span_start = BASE + 2 * B;
    gone.header.state = RegionState::Released;
    write_regions(&p, 0, &[old, gone]);
    let s = open(&p);
    let m = &s.report().metadata;
    assert!(m.superseded.is_empty(), "{m:?}");
    assert_eq!(m.shares_reserved.len(), 1);
}

/// Free space is the complement: an unavailable region's span, a demoted
/// region's span and an extra extent on a stale page are not free; an
/// extra extent on a confirmed page is.
#[test]
fn test_free_space_is_the_complement_of_what_is_allocated() {
    let one = 2 * B + CAP;
    let free = |s: &Store<SimPlatform>| s.space().released;
    // One region, then a hole of one region's size, then the last.
    let far = NEXT + one;
    let p = fresh(224);
    let mut gone = append(1, "gone", far);
    gone.header.state = RegionState::Released;
    write_regions(&p, 0, &[append(0, "live", BASE), gone]);
    let s = open(&p);
    assert_eq!(s.space().container, far + one);
    assert_eq!(free(&s), one, "an unavailable span is not free");
    let p = fresh(225);
    let mut old = append(1, "name", far);
    old.entry.footprint_seq = 1;
    write_regions(&p, 0, &[append(0, "name", BASE), old]);
    let s = open(&p);
    assert_eq!(reason(&s, "name"), Some(Unavailable::NameTaken));
    assert_eq!(free(&s), one, "a demoted span is not free");
    // An extra extent in the hole.
    let grown = || {
        let mut r = append(0, "grown", BASE);
        r.header.extent_count = 2;
        r.extents.push((NEXT, 4 * B));
        r
    };
    let p = fresh(226);
    write_regions(&p, 0, &[grown(), append(1, "last", far)]);
    let s = open(&p);
    assert_eq!(free(&s), one, "an extra extent on a confirmed page is free");
    let p = fresh(227);
    write_regions(&p, 0, &[grown(), append(1, "last", far)]);
    tear_newer_copy_of(&p, 0);
    let s = open(&p);
    assert_eq!(
        free(&s),
        one - 4 * B,
        "an extra extent on a stale page is not"
    );
}

/// A Lost page's regions are unknown: not listed, allocation frozen.
#[test]
fn test_a_lost_pages_regions_are_unknown() {
    let p = fresh(228);
    write_regions(&p, 0, &[append(0, "log", BASE)]);
    lose_page(&p, 0);
    let s = open(&p);
    let m = &s.report().metadata;
    assert_eq!(m.lost_pages, vec![0]);
    assert!(m.frozen);
    assert!(s.append_region("log").is_err());
    assert!(s.report().unavailable.is_empty());
}

/// An entry whose `footprint_seq` is past its page's write fails the entry
/// checks; it may still be the newest thing written there, so a usable
/// region its span overlaps is possibly superseded.
#[test]
fn test_an_impossible_sequence_supersedes_as_the_newest() {
    let p = fresh(229);
    let mut gone = over_first(1, "gone", NEXT);
    gone.entry.footprint_seq = 1 << 40;
    write_regions(&p, 0, &[append(0, "live", BASE), gone]);
    let s = open(&p);
    assert_eq!(reason(&s, "gone"), Some(Unavailable::EntryInvalid));
    assert_eq!(s.report().metadata.superseded.len(), 1);
}

/// Across pages: an entry on page 0 whose `footprint_seq` is past page 0's
/// own write (so impossible) equals the sequence of a region page 1 holds,
/// written after it. Taken as stored it would only tie (no supersession);
/// it counts as the newest, so the usable region is possibly superseded.
#[test]
fn test_an_impossible_sequence_on_one_page_supersedes_a_later_page() {
    let p = fresh(230);
    let page0 = max_meta_seq(&p) + 1;
    let mut gone = over_first(0, "gone", NEXT);
    // Page 1's write comes next, at `page0 + 1`: its region's sequence.
    gone.entry.footprint_seq = page0 + 1;
    write_regions(&p, 0, &[gone]);
    write_regions(&p, 1, &[append(0, "live", BASE)]);
    let s = open(&p);
    assert_eq!(reason(&s, "gone"), Some(Unavailable::EntryInvalid));
    assert_eq!(s.report().metadata.superseded.len(), 1);
}
