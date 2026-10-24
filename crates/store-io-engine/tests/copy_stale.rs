//! `copy_store` with `accept_stale_pages` (v0.5 E6 C4, spec §12 "What
//! `copy_store` copies"): a source the open left frozen, or with entries it
//! cannot serve, copied as far as it can be. A region of a stale page in
//! full (its capacity, and the extra extent its header lists when that
//! overlaps nothing else), its recorded data size reported as verified-up-
//! to; unavailable and unreadable entries left out and reported; a lost
//! page reported; a possibly superseded region copied and reported. The
//! source is never written; the copy opens writable and audits clean.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

mod format2_util;

use std::path::Path;

use format2_util::audit::audit_in;
use format2_util::craft::{lose_page, region, tear_newer_copy_of, write_regions};
use format2_util::*;
use store_io_core::error::{Error, NotWrittenCause};
use store_io_core::id::RegionId;
use store_io_engine::{CopyOptions, CopyReport, Store, copy_store};
use store_io_format::meta::{RegionKind, RegionState};
use store_io_sim::{SimConfig, SimPlatform};

const B: u64 = 4096;
const BASE: u64 = 2 * B + 2 * PAGES as u64 * TABLE;
const CAP: u64 = 16 * B;
/// The copy's target directory.
const OTHER: &str = "/other";

fn fresh(seed: u64) -> SimPlatform {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    drop(create(&p));
    p
}

fn copy(p: &SimPlatform, accept: bool) -> Result<CopyReport, Error> {
    copy_store(
        p.clone(),
        Path::new(DIR),
        Path::new(OTHER),
        CopyOptions {
            store: opts(),
            accept_stale_pages: accept,
        },
    )
}

/// `len` bytes of a pattern of `seed`, changing every 509 bytes (a byte
/// off by a block's length, or by a piece's, reads different).
fn pattern(seed: u8, len: u64) -> Vec<u8> {
    (0..len)
        .map(|i| seed.wrapping_mul(31).wrapping_add((i / 509) as u8))
        .collect()
}

fn target(p: &SimPlatform) -> Store<SimPlatform> {
    Store::open(p.clone(), Path::new(OTHER), opts()).unwrap()
}

fn page_bytes(s: &Store<SimPlatform>, name: &str) -> Vec<u8> {
    let r = s.page_region(name).unwrap();
    let mut out = vec![0u8; r.len() as usize];
    r.read(r.pos(0).unwrap(), &mut out).unwrap();
    out
}

fn append_bytes(s: &Store<SimPlatform>, name: &str) -> Vec<u8> {
    let r = s.append_region(name).unwrap();
    let mut out = vec![0u8; r.len() as usize];
    r.read(0, &mut out).unwrap();
    out
}

/// A stale page's regions copied in full: a page region's whole capacity
/// and the extra extent its header lists (a growth that may be the lost
/// commit; it overlaps nothing), an append region's whole capacity; their
/// recorded data sizes reported; the source untouched; the copy writable
/// and clean. Without `accept_stale_pages`, refused as before.
#[test]
fn test_a_stale_page_is_copied_in_full() {
    let p = fresh(1600);
    let first = region(0, RegionKind::Page, "first", BASE, CAP);
    let first_size = first.entry.data_size;
    write_regions(&p, 1, &[first]);
    let p_at = BASE + 2 * B + CAP;
    let mut pr = region(0, RegionKind::Page, "p", p_at, CAP);
    let extra = (p_at + 2 * B + CAP, 4 * B);
    pr.extents.push(extra);
    pr.header.extent_count = 2;
    let q_at = extra.0 + extra.1;
    let qr = region(1, RegionKind::Append, "q", q_at, CAP);
    let (p_size, q_size) = (pr.entry.data_size, qr.entry.data_size);
    write_regions(&p, 0, &[pr, qr]);
    let pdata = pattern(1, CAP + 4 * B);
    put(&p, p_at + 2 * B, &pdata);
    let qdata = pattern(2, CAP);
    put(&p, q_at + 2 * B, &qdata);
    tear_newer_copy_of(&p, 0);
    let e = copy(&p, false).unwrap_err();
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
    let before = media(&p);
    let r = copy(&p, true).unwrap();
    assert!(media(&p) == before, "the source was written");
    assert_eq!(r.regions, 3);
    assert_eq!(r.bytes, first_size + CAP + 4 * B + CAP);
    assert_eq!(
        r.verified_up_to,
        [("p".to_owned(), p_size), ("q".to_owned(), q_size)]
    );
    assert!(r.omitted.is_empty() && r.lost_pages.is_empty() && r.superseded.is_empty());
    let s = target(&p);
    assert!(!s.frozen());
    assert!(
        page_bytes(&s, "p") == pdata,
        "p's capacity and extra extent"
    );
    assert!(append_bytes(&s, "q") == qdata, "q's capacity");
    let pages = s.page_region("p").unwrap();
    pages
        .write_durable(pages.pos(0).unwrap(), &[9; B as usize])
        .unwrap();
    drop((pages, s));
    audit_in(&p, OTHER);
}

/// An extra extent overlapping another region's footprint (a region of a
/// confirmed page: its header blocks, which a footprint counts) is left
/// out: the stale page's region copied over its capacity only.
#[test]
fn test_an_extra_extent_over_another_region_is_left_out() {
    let p = fresh(1601);
    let mut pr = region(0, RegionKind::Page, "p", BASE, CAP);
    let other_at = BASE + 2 * B + CAP;
    // Over "o"'s header blocks only.
    pr.extents.push((other_at, 2 * B));
    pr.header.extent_count = 2;
    let p_size = pr.entry.data_size;
    write_regions(&p, 0, &[pr]);
    write_regions(&p, 1, &[region(0, RegionKind::Page, "o", other_at, CAP)]);
    let pdata = pattern(3, CAP);
    put(&p, BASE + 2 * B, &pdata);
    tear_newer_copy_of(&p, 0);
    let r = copy(&p, true).unwrap();
    assert_eq!(r.regions, 2);
    assert_eq!(r.verified_up_to, [("p".to_owned(), p_size)]);
    let s = target(&p);
    assert!(page_bytes(&s, "p") == pdata, "the capacity only");
    drop(s);
    audit_in(&p, OTHER);
}

/// A lost page reported (its regions unknown, so no extra extent of a
/// stale page is taken: one of them may own it); an unavailable region
/// (its header never written) and an entry of a newer release's kind left
/// out, reported; the other regions copied by their data sizes, a stale
/// page's in full.
#[test]
fn test_lost_pages_and_unusable_entries_are_reported() {
    let p = fresh(1602);
    write_regions(&p, 0, &[region(0, RegionKind::Page, "lost", BASE, CAP)]);
    let at = BASE + 2 * B + CAP;
    let good = region(0, RegionKind::Page, "good", at, CAP);
    let good_size = good.entry.data_size as usize;
    let mut broken = region(1, RegionKind::Page, "broken", at + 2 * B + CAP, CAP);
    broken.write_header = false;
    let mut newer = region(2, RegionKind::Append, "newer", at + 4 * B + 2 * CAP, CAP);
    newer.kind_raw = Some(9);
    write_regions(&p, 1, &[good, broken, newer]);
    let gdata = pattern(4, CAP);
    put(&p, at + 2 * B, &gdata);
    let st_at = at + 6 * B + 3 * CAP;
    let mut st = region(0, RegionKind::Page, "stale", st_at, CAP);
    st.extents.push((st_at + 2 * B + CAP, 4 * B));
    st.header.extent_count = 2;
    let st_size = st.entry.data_size;
    write_regions(&p, 2, &[st]);
    tear_newer_copy_of(&p, 2);
    lose_page(&p, 0);
    let e = copy(&p, false).unwrap_err();
    assert!(matches!(e, Error::NotWritten { .. }), "{e:?}");
    let r = copy(&p, true).unwrap();
    assert_eq!(r.lost_pages, [0]);
    let id = |seq: u64| RegionId::from_raw((1 << 32) | seq);
    assert_eq!(r.omitted, [id(1), id(2)]);
    assert_eq!(r.regions, 2);
    assert_eq!(r.verified_up_to, [("stale".to_owned(), st_size)]);
    assert_eq!(r.superseded, ["stale".to_owned()], "page 0 is lost");
    let s = target(&p);
    assert!(
        page_bytes(&s, "good") == gdata[..good_size],
        "by its data size"
    );
    assert_eq!(
        s.page_region("stale").unwrap().len(),
        CAP,
        "no extra extent: a page is lost"
    );
    assert!(s.page_region("lost").is_err() && s.page_region("broken").is_err());
    drop(s);
    audit_in(&p, OTHER);
}

/// A possibly superseded region (an unavailable entry's newer extents over
/// its bytes) is copied, by its data size, and reported; the entry that
/// superseded it is left out.
#[test]
fn test_a_possibly_superseded_region_is_copied_and_reported() {
    let p = fresh(1603);
    let mut old = region(0, RegionKind::Append, "old", BASE, CAP);
    old.entry.footprint_seq = 1;
    let old_size = old.entry.data_size;
    let next = BASE + 2 * B + CAP;
    let mut gone = region(1, RegionKind::Append, "gone", next, CAP);
    gone.entry.span_start = BASE + 2 * B;
    gone.extents = vec![(BASE + 2 * B, CAP)];
    gone.header.state = RegionState::Released;
    write_regions(&p, 0, &[old, gone]);
    let odata = pattern(5, CAP);
    put(&p, BASE + 2 * B, &odata);
    let r = copy(&p, true).unwrap();
    assert_eq!(r.superseded, ["old".to_owned()]);
    assert_eq!(r.omitted, [RegionId::from_raw(1)]);
    assert!(r.verified_up_to.is_empty());
    let s = target(&p);
    assert!(append_bytes(&s, "old") == odata[..old_size as usize]);
    drop(s);
    audit_in(&p, OTHER);
}

/// An extra extent overlapping a reserved span (an unavailable entry's) or
/// another region's extra extent is left out: each region over its
/// capacity only.
#[test]
fn test_an_extra_extent_over_a_reserved_span_or_another_extra_is_left_out() {
    // A reserved span: an entry on a confirmed page whose header was never
    // written, its span where the extra extent lies.
    let p = fresh(1604);
    let mut pr = region(0, RegionKind::Page, "p", BASE, CAP);
    let x = BASE + 2 * B + CAP;
    pr.extents.push((x, 4 * B));
    pr.header.extent_count = 2;
    write_regions(&p, 0, &[pr]);
    let mut broken = region(0, RegionKind::Page, "broken", x, CAP);
    broken.write_header = false;
    write_regions(&p, 1, &[broken]);
    tear_newer_copy_of(&p, 0);
    let r = copy(&p, true).unwrap();
    assert_eq!(r.omitted, [RegionId::from_raw(1 << 32)]);
    let s = target(&p);
    assert_eq!(s.page_region("p").unwrap().len(), CAP, "the capacity only");
    drop(s);
    audit_in(&p, OTHER);
    // Two regions of the stale page whose extra extents overlap.
    let p = fresh(1605);
    let mut a = region(0, RegionKind::Page, "a", BASE, CAP);
    let x = BASE + 4 * B + 2 * CAP;
    a.extents.push((x, 4 * B));
    a.header.extent_count = 2;
    let mut b = region(1, RegionKind::Page, "b", BASE + 2 * B + CAP, CAP);
    b.extents.push((x + 2 * B, 4 * B));
    b.header.extent_count = 2;
    write_regions(&p, 0, &[a, b]);
    tear_newer_copy_of(&p, 0);
    copy(&p, true).unwrap();
    let s = target(&p);
    for name in ["a", "b"] {
        assert_eq!(s.page_region(name).unwrap().len(), CAP, "{name}");
    }
    drop(s);
    audit_in(&p, OTHER);
}

/// A full copy's read past the data size that meets a media error fails
/// the copy (`Corruption(MediaError)`): nothing is put in place.
#[test]
fn test_a_media_error_in_a_full_copy_fails_it() {
    let p = fresh(1606);
    let pr = region(0, RegionKind::Page, "p", BASE, CAP);
    write_regions(&p, 0, &[pr]);
    tear_newer_copy_of(&p, 0);
    let file = p.with_world(|w| {
        w.file_id(&std::path::PathBuf::from(DIR), store_io_engine::CONTAINER)
            .unwrap()
    });
    // Past the data size (half the capacity), inside the capacity.
    let bad = BASE + 2 * B + CAP - B;
    p.with_world(|w| w.faults_mut().bad_ranges = vec![(file, bad, bad + B)]);
    let e = copy(&p, true).unwrap_err();
    p.with_world(|w| w.faults_mut().bad_ranges.clear());
    assert!(
        matches!(
            e,
            Error::Corruption {
                kind: store_io_core::error::CorruptionKind::MediaError,
                ..
            }
        ),
        "{e:?}"
    );
    let placed = p.with_world(|w| {
        w.media_of(&std::path::PathBuf::from(OTHER), store_io_engine::CONTAINER)
            .is_some()
    });
    assert!(!placed, "a failed copy put a store in place");
}

/// A slot of a stale page: its committed object copied as it is (an object
/// is whole: never "in full", never reported as verified-up-to).
#[test]
fn test_a_slot_of_a_stale_page_is_copied_as_committed() {
    let p = SimPlatform::new(SimConfig::volatile(1607));
    let s = create(&p);
    s.provision_slot("s")
        .unwrap()
        .commit(b"the manifest")
        .unwrap();
    s.provision_page_region("x", 8 * B).unwrap();
    drop(s);
    tear_newer_copy_of(&p, 0);
    let r = copy(&p, true).unwrap();
    assert_eq!(r.regions, 2);
    assert_eq!(r.stale_pages, [0]);
    assert_eq!(r.verified_up_to, [("x".to_owned(), 8 * B)], "page 0 stale");
    let s = target(&p);
    assert_eq!(
        s.slot("s").unwrap().read().as_deref(),
        Some(&b"the manifest"[..])
    );
    drop(s);
    audit_in(&p, OTHER);
}

/// An extra extent over the volume record and table, over the region's own
/// header, or over what an unavailable entry may have written (an extent
/// its released header lists, outside its span) is left out.
#[test]
fn test_an_extra_extent_over_metadata_or_possible_writes_is_left_out() {
    let cases: [(&str, u64); 3] = [("table", 1607), ("own header", 1608), ("released", 1609)];
    for (case, seed) in cases {
        let p = fresh(seed);
        let at = BASE + 2 * B + CAP;
        let mut pr = region(0, RegionKind::Page, "p", at, CAP);
        let x = match case {
            "table" => (BASE - B, B),
            "own header" => (at, B),
            _ => (at + 2 * B + CAP, 4 * B),
        };
        pr.extents.push(x);
        pr.header.extent_count = 2;
        write_regions(&p, 0, &[pr]);
        if case == "released" {
            // Released (unavailable), its header listing x outside its span.
            let mut gone = region(0, RegionKind::Page, "gone", BASE, CAP);
            gone.extents.push(x);
            gone.header.extent_count = 2;
            gone.header.state = RegionState::Released;
            write_regions(&p, 1, &[gone]);
        }
        tear_newer_copy_of(&p, 0);
        copy(&p, true).unwrap();
        let s = target(&p);
        assert_eq!(s.page_region("p").unwrap().len(), CAP, "{case}");
        drop(s);
        audit_in(&p, OTHER);
    }
}

/// A region larger than a copy's chunk, its extra extent elsewhere in the
/// file (another region between): every piece read from its own place.
#[test]
fn test_a_large_region_and_a_distant_extra_extent_are_copied_piece_by_piece() {
    let p = fresh(1610);
    let big = 3 << 20;
    let mid_at = BASE + 2 * B + big;
    let x = (mid_at + 2 * B + CAP, 8 * B);
    let mut pr = region(0, RegionKind::Page, "big", BASE, big);
    pr.extents.push(x);
    pr.header.extent_count = 2;
    write_regions(&p, 0, &[pr]);
    write_regions(&p, 1, &[region(0, RegionKind::Page, "mid", mid_at, CAP)]);
    let data = pattern(7, big + x.1);
    put(&p, BASE + 2 * B, &data[..big as usize]);
    put(&p, x.0, &data[big as usize..]);
    tear_newer_copy_of(&p, 0);
    // 128 KiB pieces (the source opened with these options too): every
    // copy chunk read in several.
    copy_store(
        p.clone(),
        Path::new(DIR),
        Path::new(OTHER),
        CopyOptions {
            store: store_io_engine::StoreOptions {
                max_io: 128 << 10,
                ..opts()
            },
            accept_stale_pages: true,
        },
    )
    .unwrap();
    let s = target(&p);
    assert!(
        page_bytes(&s, "big") == data,
        "the capacity, then the extra extent"
    );
    drop(s);
    audit_in(&p, OTHER);
}

/// An extra extent in the tail of an unavailable entry's reserved span
/// (its span reaching past what its header lists: reserved all the same)
/// is left out.
#[test]
fn test_an_extra_extent_in_an_unavailable_span_is_left_out() {
    let p = fresh(1611);
    let tail = (BASE + 2 * B + CAP, 4 * B);
    let mut gone = region(0, RegionKind::Page, "gone", BASE, CAP);
    gone.entry.span_end += tail.1;
    gone.header.state = RegionState::Released;
    write_regions(&p, 1, &[gone]);
    let at = tail.0 + tail.1;
    let mut pr = region(0, RegionKind::Page, "p", at, CAP);
    pr.extents.push(tail);
    pr.header.extent_count = 2;
    write_regions(&p, 0, &[pr]);
    tear_newer_copy_of(&p, 0);
    let r = copy(&p, true).unwrap();
    assert_eq!(r.omitted, [RegionId::from_raw(1 << 32)]);
    let s = target(&p);
    assert_eq!(s.page_region("p").unwrap().len(), CAP);
    drop(s);
    audit_in(&p, OTHER);
}

/// A stale page whose regions are all slots (or none at all): nothing is
/// copied in full, yet the page is reported: its newest commit, given up,
/// may have created a region (here "lost", absent from the copy).
#[test]
fn test_a_stale_page_holding_only_a_slot_is_reported() {
    let p = SimPlatform::new(SimConfig::volatile(1612));
    let s = create(&p);
    s.provision_slot("s").unwrap().commit(b"kept").unwrap();
    s.provision_page_region("lost", 8 * B).unwrap();
    drop(s);
    // The newer copy (the one with "lost") unreadable: page 0 stale, its
    // older copy (only the slot) served.
    let file = p.with_world(|w| {
        w.file_id(&std::path::PathBuf::from(DIR), store_io_engine::CONTAINER)
            .unwrap()
    });
    let m = media(&p);
    let (a, b) = (page_at(&p, 0, 0), page_at(&p, 0, 1));
    let newer = if u64_at(&m, a as usize + 56) > u64_at(&m, b as usize + 56) {
        a
    } else {
        b
    };
    p.with_world(|w| w.faults_mut().bad_ranges = vec![(file, newer, newer + TABLE)]);
    let r = copy(&p, true).unwrap();
    p.with_world(|w| w.faults_mut().bad_ranges.clear());
    assert_eq!(r.stale_pages, [0]);
    assert_eq!(r.regions, 1);
    assert!(r.verified_up_to.is_empty() && r.omitted.is_empty());
    let s = target(&p);
    assert!(
        s.page_region("lost").is_err(),
        "the given-up commit's region"
    );
    assert_eq!(s.slot("s").unwrap().read().as_deref(), Some(&b"kept"[..]));
}

/// With two pages stale, each one's regions may hold the other's lost
/// region's bytes (D-130): both listed; and a page accepted in the source
/// (possibly accepted: its slots twins) while another stays stale: its
/// regions listed too.
#[test]
fn test_regions_two_given_up_commits_may_share_are_reported() {
    let p = fresh(1613);
    write_regions(&p, 0, &[region(0, RegionKind::Page, "a", BASE, CAP)]);
    let at = BASE + 2 * B + CAP;
    write_regions(&p, 1, &[region(0, RegionKind::Page, "b", at, CAP)]);
    tear_newer_copy_of(&p, 0);
    tear_newer_copy_of(&p, 1);
    let r = copy(&p, true).unwrap();
    assert_eq!(r.stale_pages, [0, 1]);
    assert_eq!(r.superseded, ["a".to_owned(), "b".to_owned()]);
    // Page 0 accepted; page 1 still stale.
    let p = fresh(1614);
    write_regions(&p, 0, &[region(0, RegionKind::Page, "a", BASE, CAP)]);
    write_regions(&p, 1, &[region(0, RegionKind::Page, "b", at, CAP)]);
    tear_newer_copy_of(&p, 0);
    tear_newer_copy_of(&p, 1);
    open(&p).accept_page_rollback(&[0]).unwrap();
    let r = copy(&p, true).unwrap();
    assert_eq!(r.stale_pages, [1]);
    assert_eq!(
        r.superseded,
        ["a".to_owned(), "b".to_owned()],
        "a: possibly accepted"
    );
}
