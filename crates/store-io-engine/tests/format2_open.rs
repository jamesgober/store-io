//! The format-2 open reads its region header pairs many at a time, a pair
//! whose slot B follows its slot A in one read of both (E2b/E3 C8): the
//! pairs it finds are the pairs a slot-by-slot read finds, whatever the
//! slots' placement, a failed or short read, a bad sector under one slot of
//! a pair, or a queue shallower than the batch.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

mod format2_util;

use std::path::{Path, PathBuf};

use format2_util::craft::{Region, region, slot, write_regions};
use format2_util::*;
use store_io_core::error::Error;
use store_io_engine::{CONTAINER, Store, StoreOptions, Unavailable};
use store_io_format::meta::RegionKind;
use store_io_format::meta2::encode_region_header;
use store_io_format::slot::SlotIndex;
use store_io_sim::{FaultPlan, SimConfig, SimPlatform};

const B: u64 = 4096;
/// The table's end for `opts()`: the volume pair, then three pages.
const BASE: u64 = 2 * B + 2 * PAGES as u64 * TABLE;
const CAP: u64 = 4 * B;
/// One region's span.
const SPAN: u64 = 2 * B + CAP;

fn fresh(seed: u64) -> SimPlatform {
    fresh_ordered(seed, false)
}

/// A fresh store whose queues complete in a seeded random order when
/// `reorder`, else in submission order.
fn fresh_ordered(seed: u64, reorder: bool) -> SimPlatform {
    // Completions reordered and waits failed (also with
    // `STORE_IO_SIM_INLINE`): every read submitted and waited for.
    let p = SimPlatform::new(SimConfig {
        reorder,
        inline: false,
        ..SimConfig::volatile(seed)
    });
    drop(create(&p));
    p
}

/// A page region at `offset` whose header is in `slot`.
fn page(seq: u32, name: &str, offset: u64, slot: SlotIndex) -> Region {
    let mut r = region(seq, RegionKind::Page, name, offset, CAP);
    r.header_slot = slot;
    r
}

/// Three page regions, each header in slot B: "next" (B right after A),
/// "apart" (the extent between A and B) and "reversed" (B before A).
fn three() -> Vec<Region> {
    let next = page(0, "next", BASE, SlotIndex::B);
    let mut apart = page(1, "apart", BASE + SPAN, SlotIndex::B);
    let at = BASE + SPAN;
    apart.entry.header_b = at + B + CAP;
    apart.extents = vec![(at + B, CAP)];
    let mut reversed = page(2, "reversed", BASE + 2 * SPAN, SlotIndex::B);
    let at = BASE + 2 * SPAN;
    reversed.entry.header_a = at + B;
    reversed.entry.header_b = at;
    vec![next, apart, reversed]
}

/// [`three`] and 13 more ("p0" to "p12", B right after A): enough header
/// pairs for the open to read them on a queue of its own.
fn sixteen() -> Vec<Region> {
    let mut rs = three();
    for i in 0..13u32 {
        let slot = if i % 2 == 0 {
            SlotIndex::A
        } else {
            SlotIndex::B
        };
        rs.push(page(
            3 + i,
            &format!("p{i}"),
            BASE + u64::from(3 + i) * SPAN,
            slot,
        ));
    }
    rs
}

/// The names of `rs`, sorted.
fn names_of(rs: &[Region]) -> Vec<String> {
    let mut n: Vec<String> = rs
        .iter()
        .map(|r| r.entry.name.as_str().to_owned())
        .collect();
    n.sort();
    n
}

fn names(s: &Store<SimPlatform>) -> Vec<String> {
    let mut n: Vec<String> = s.regions().into_iter().map(|(n, _)| n).collect();
    n.sort();
    n
}

fn file_id(p: &SimPlatform) -> u32 {
    p.with_world(|w| w.file_id(&PathBuf::from(DIR), CONTAINER).unwrap())
}

fn reason(s: &Store<SimPlatform>, name: &str) -> Option<Unavailable> {
    s.report()
        .unavailable
        .iter()
        .find(|u| u.name == name)
        .map(|u| u.reason)
}

#[test]
fn test_header_slots_in_any_placement_are_read_and_judged() {
    let p = fresh(401);
    write_regions(&p, 0, &three());
    let s = open(&p);
    assert!(
        s.report().unavailable.is_empty(),
        "{:?}",
        s.report().unavailable
    );
    assert_eq!(names(&s), ["apart", "next", "reversed"]);
    assert_eq!(s.report().metadata.retried_reads, 0);
    for n in ["apart", "next", "reversed"] {
        s.page_region(n).unwrap();
    }
}

/// From 16 header pairs the open reads them on a queue of its own (a
/// blocking call's lane may run one operation at a time); below, on its
/// lane.
#[test]
fn test_sixteen_header_pairs_are_read_on_a_queue_of_the_opens_own() {
    let mut made = Vec::new();
    for n in [15, 16] {
        let p = fresh(406);
        write_regions(&p, 0, &sixteen()[..n]);
        let before = p.with_world(|w| w.queues());
        let s = Store::open_readonly(p.clone(), Path::new(DIR), opts()).unwrap();
        made.push(p.with_world(|w| w.queues()) - before);
        assert_eq!(s.regions().len(), n);
    }
    assert_eq!(
        made[1],
        made[0] + 1,
        "queues made at 15 and 16 regions: {made:?}"
    );
}

/// A pair read as one whose read fails on one slot's block is read again
/// slot by slot: the other slot's header is found, as a slot-by-slot read
/// finds it.
#[test]
fn test_a_bad_sector_under_one_slot_of_a_pair_read_as_one_leaves_the_other() {
    for (written, bad) in [(SlotIndex::A, B), (SlotIndex::B, 0)] {
        let p = fresh(402);
        write_regions(&p, 0, &[page(0, "r", BASE, written)]);
        let file = file_id(&p);
        p.with_world(|w| w.faults_mut().bad_ranges = vec![(file, BASE + bad, BASE + bad + B)]);
        let s = Store::open_readonly(p.clone(), Path::new(DIR), opts()).unwrap();
        assert_eq!(
            reason(&s, "r"),
            None,
            "header in {written:?}: {:?}",
            s.report().unavailable
        );
        s.page_region("r").unwrap();
        assert_eq!(s.report().metadata.retried_reads, 1, "{written:?}");
        drop(s);
        // Both blocks bad: the header cannot be read.
        p.with_world(|w| w.faults_mut().bad_ranges = vec![(file, BASE, BASE + 2 * B)]);
        let s = Store::open_readonly(p.clone(), Path::new(DIR), opts()).unwrap();
        assert_eq!(
            reason(&s, "r"),
            Some(Unavailable::HeaderUnreadable),
            "{written:?}"
        );
    }
}

/// Every read of an open, failed or cut short in turn: a short read or a
/// media error that does not repeat is read again and nothing is lost; any
/// other failure fails the open with its code, never weighed as damage.
/// With 3 regions on the open's lane, and with 16 on its own queue; reads
/// completing in order and out of it.
#[test]
fn test_any_one_failed_or_short_read_of_an_open_is_read_again_or_returned() {
    for regions in [three(), sixteen()] {
        for reorder in [false, true] {
            any_one_failed_or_short_read(&regions, reorder);
        }
    }
}

fn any_one_failed_or_short_read(regions: &[Region], reorder: bool) {
    let p = fresh_ordered(403, reorder);
    write_regions(&p, 0, regions);
    let want = names_of(regions);
    let before = p.with_world(|w| w.reads());
    drop(open(&p));
    let reads = p.with_world(|w| w.reads()) - before;
    assert!(reads > 6, "the open made only {reads} reads");
    for fault in ["short", "media", "einval"] {
        let (mut retried, mut failed) = (0, 0);
        for k in 1..=reads {
            let next = p.with_world(|w| w.reads()) + k;
            p.with_world(|w| {
                *w.faults_mut() = match fault {
                    "short" => FaultPlan {
                        short_read: Some(next),
                        ..FaultPlan::default()
                    },
                    "media" => FaultPlan {
                        fail_read: Some((next, 5)),
                        ..FaultPlan::default()
                    },
                    _ => FaultPlan {
                        fail_read: Some((next, 22)),
                        ..FaultPlan::default()
                    },
                };
            });
            match Store::open(p.clone(), Path::new(DIR), opts()) {
                // Every read of the open is a metadata read: none may pass
                // a failure that says nothing about the media as success.
                Ok(_) if fault == "einval" => panic!("read {k}: the open succeeded"),
                Ok(s) => {
                    assert!(
                        s.report().unavailable.is_empty(),
                        "{fault}: read {k}: {:?}",
                        s.report().unavailable
                    );
                    assert_eq!(names(&s), want, "{fault}: {k}");
                    retried += s.report().metadata.retried_reads;
                }
                Err(Error::Io { raw, .. }) if fault == "einval" => {
                    assert_eq!(raw.code, 22, "read {k}");
                    failed += 1;
                }
                Err(e) => panic!("{fault}: read {k}: {e:?}"),
            }
            p.with_world(|w| *w.faults_mut() = FaultPlan::default());
        }
        if fault == "einval" {
            assert_eq!(failed, reads);
        } else {
            assert!(retried > 0, "{fault}: no read was read again");
        }
    }
}

/// A failed wait with header reads in flight is never an unreadable slot:
/// the open fails, or (a wait outside the region checks) succeeds whole.
/// With 3 regions on the open's lane, and with 16 on its own queue (left
/// with reads in flight when the open returns); reads completing in order
/// and out of it.
#[test]
fn test_a_failed_wait_during_the_header_reads_fails_the_open() {
    for regions in [three(), sixteen()] {
        for reorder in [false, true] {
            a_failed_wait(&regions, reorder);
        }
    }
}

fn a_failed_wait(regions: &[Region], reorder: bool) {
    let p = fresh_ordered(404, reorder);
    write_regions(&p, 0, regions);
    let want = names_of(regions);
    let before = p.with_world(|w| w.waits());
    drop(Store::open_readonly(p.clone(), Path::new(DIR), opts()).unwrap());
    let waits = p.with_world(|w| w.waits()) - before;
    let mut failed = 0;
    for k in 1..=waits {
        let next = p.with_world(|w| w.waits()) + k;
        p.with_world(|w| w.faults_mut().fail_wait = Some((next, 4)));
        // Read-only: the open's own writes are not under test.
        match Store::open_readonly(p.clone(), Path::new(DIR), opts()) {
            Ok(s) => {
                assert!(
                    s.report().unavailable.is_empty(),
                    "wait {k}: {:?}",
                    s.report().unavailable
                );
                assert_eq!(names(&s), want, "wait {k}");
            }
            Err(Error::Io { raw, .. }) => {
                assert_eq!(raw.code, 4, "wait {k}");
                failed += 1;
            }
            Err(e) => panic!("wait {k}: {e:?}"),
        }
        p.with_world(|w| *w.faults_mut() = FaultPlan::default());
    }
    assert!(failed > 0, "no failed wait surfaced");
}

/// More regions than one batch of header reads, on a queue two deep and on
/// a pool of four buffers a class: every region is found, each header pair
/// in one read.
#[test]
fn test_more_regions_than_a_batch_on_a_shallow_queue_or_pool() {
    const N: u32 = 4100;
    // 169 entries a page.
    const PER_PAGE: u32 = 169;
    let o = StoreOptions {
        region_capacity: PER_PAGE * N.div_ceil(PER_PAGE),
        ..opts()
    };
    // Completions out of order: each is matched to its pair by its tag.
    let p = SimPlatform::new(SimConfig {
        reorder: true,
        ..SimConfig::volatile(405)
    });
    drop(Store::create(p.clone(), Path::new(DIR), o.clone()).unwrap());
    let base = media(&p).len() as u64;
    // The pages one at a time; every header in one durable write (one
    // write each would take a minute).
    let mut area = vec![0u8; (u64::from(N) * SPAN) as usize];
    for pg in 0..N.div_ceil(PER_PAGE) {
        let rs: Vec<Region> = (pg * PER_PAGE..N.min((pg + 1) * PER_PAGE))
            .map(|i| {
                let slot = if i % 2 == 0 {
                    SlotIndex::A
                } else {
                    SlotIndex::B
                };
                let mut r = page(
                    i - pg * PER_PAGE,
                    &format!("r{i}"),
                    base + u64::from(i) * SPAN,
                    slot,
                );
                r.write_header = false;
                r.extend = false;
                r
            })
            .collect();
        write_regions(&p, pg, &rs);
        for r in &rs {
            let id = (u64::from(pg) << 32) | u64::from(r.entry.seq);
            let mut h = r.header;
            h.region_id = id;
            let mut payload = vec![0u8; B as usize];
            let n = encode_region_header(&mut payload, &h, &r.extents, &[]).unwrap();
            let at = if r.header_slot == SlotIndex::A {
                r.entry.header_a
            } else {
                r.entry.header_b
            };
            let bytes = slot(
                &p,
                (1 << 62) | id,
                at,
                r.header_slot,
                12,
                1,
                0,
                0,
                &payload[..n],
                0,
            );
            let i = (at - base) as usize;
            area[i..i + B as usize].copy_from_slice(&bytes);
        }
    }
    put(&p, base, &area);
    for (depth, buffers) in [(2, 64), (64, 4)] {
        let o = StoreOptions {
            queue_depth: depth,
            buffers_per_class: buffers,
            ..o.clone()
        };
        let before = p.with_world(|w| w.reads());
        let s = Store::open_readonly(p.clone(), Path::new(DIR), o).unwrap();
        let reads = p.with_world(|w| w.reads()) - before;
        assert!(
            s.report().unavailable.is_empty(),
            "depth {depth}: {:?}",
            s.report().unavailable.first()
        );
        assert_eq!(s.regions().len(), N as usize, "depth {depth}");
        assert!(s.page_region("r0").is_ok() && s.page_region("r4099").is_ok());
        // One read a header pair, and the volume record and the table.
        assert!(
            (u64::from(N)..u64::from(N) + 200).contains(&reads),
            "depth {depth}, buffers {buffers}: {reads} reads"
        );
    }
}
