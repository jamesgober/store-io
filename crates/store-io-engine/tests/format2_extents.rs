//! Format-2 regions whose data lies in several extents (spec §11), crafted
//! by an independent writer: the open maps them by their runs (extents
//! adjacent in the file coalesced), reads and scans cross runs with the
//! bytes of each in data order, a media error is named at its region range,
//! and (until writes split at runs, E4 C4 and C5) a region in several runs
//! is read-only and a Submitter's read across a run is refused.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

mod format2_util;

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use format2_util::craft::{Region, region, write_regions};
use format2_util::*;
use store_io_core::error::{ByteRange, CorruptionKind, Error, NotWrittenCause};
use store_io_engine::{CONTAINER, ScanItem, Store, StoreOptions, SubmitterConfig};
use store_io_format::meta::RegionKind;
use store_io_sim::{SimConfig, SimPlatform};

const B: u64 = 4096;
/// The table's end for `opts()`: the volume pair, then three pages.
const BASE: u64 = 2 * B + 2 * PAGES as u64 * TABLE;
/// An extent: 16 blocks.
const X: u64 = 16 * B;

/// One device read is at most 128 KiB, so a read of three extents is
/// pipelined.
fn small_io() -> StoreOptions {
    StoreOptions {
        max_io: 128 << 10,
        ..opts()
    }
}

fn fresh(seed: u64) -> SimPlatform {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    drop(Store::create(p.clone(), Path::new(DIR), small_io()).unwrap());
    p
}

fn reopen(p: &SimPlatform) -> Store<SimPlatform> {
    Store::open(p.clone(), Path::new(DIR), small_io()).unwrap()
}

/// A region of `kind` with its headers at `at`, its header listing
/// `extents` (data order) and its entry the first `committed` of them (one
/// fewer: a growth beyond capacity that crashed before its page write), the
/// first `data` bytes data.
fn split(
    seq: u32,
    kind: RegionKind,
    name: &str,
    at: u64,
    extents: &[(u64, u64)],
    data: u64,
    committed: usize,
) -> Region {
    let mut r = region(seq, kind, name, at, X);
    r.extents = extents.to_vec();
    r.header.extent_count = extents.len() as u16;
    r.entry.extent_count = committed as u16;
    r.entry.capacity = extents[..committed].iter().map(|e| e.1).sum();
    r.entry.data_size = data;
    r.entry.span_end = extents[..committed]
        .iter()
        .map(|e| e.0 + e.1)
        .max()
        .unwrap()
        .max(at + 2 * B);
    r
}

/// The byte at region offset `pos` of a region named by `seed`.
fn byte(seed: u8, pos: u64) -> u8 {
    ((pos / B) as u8).wrapping_mul(29) ^ (pos % 251) as u8 ^ seed
}

/// Writes the pattern of `seed` over `extents`, by region offset.
fn fill(p: &SimPlatform, seed: u8, extents: &[(u64, u64)]) {
    let mut pos = 0;
    for &(file, len) in extents {
        let bytes: Vec<u8> = (pos..pos + len).map(|x| byte(seed, x)).collect();
        put(p, file, &bytes);
        pos += len;
    }
}

fn expect(seed: u8, from: u64, len: u64) -> Vec<u8> {
    (from..from + len).map(|x| byte(seed, x)).collect()
}

/// "two": a page region of two extents, the second lower in the file (data
/// order, not file order); "three": an append region of three runs, its
/// data short of its capacity; "adj": a page region of two adjacent
/// extents (one run).
fn regions() -> Vec<(Region, u8)> {
    let two = [(BASE + 2 * B + X, X), (BASE + 2 * B, X)];
    let h = BASE + 2 * B + 2 * X;
    let three = [
        (h + 2 * B + 2 * X, X),
        (h + 2 * B, X),
        (h + 2 * B + 4 * X, X),
    ];
    let j = h + 2 * B + 5 * X;
    let adj = [(j + 2 * B, X), (j + 2 * B + X, X)];
    vec![
        (split(0, RegionKind::Page, "two", BASE, &two, 2 * X, 2), 1),
        (
            split(1, RegionKind::Append, "three", h, &three, 3 * X - 2 * B, 3),
            2,
        ),
        (split(2, RegionKind::Page, "adj", j, &adj, 2 * X, 2), 3),
    ]
}

fn crafted(seed: u64) -> SimPlatform {
    let p = fresh(seed);
    let rs = regions();
    let only: Vec<Region> = rs.iter().map(|r| r.0.clone()).collect();
    write_regions(&p, 0, &only);
    for (r, s) in &rs {
        fill(&p, *s, &r.extents);
    }
    p
}

fn is_cause(e: &Error, want: NotWrittenCause) -> bool {
    matches!(e, Error::NotWritten { cause, .. } if *cause == want)
}

#[test]
fn test_regions_of_several_extents_open_and_read_in_data_order() {
    let p = crafted(501);
    let s = reopen(&p);
    assert!(
        s.report().unavailable.is_empty(),
        "{:?}",
        s.report().unavailable
    );
    let two = s.page_region("two").unwrap();
    // Every block offset, one to three blocks long, across the boundary.
    for k in 0..2 * X / B {
        for n in 1..=3u64 {
            let (from, len) = (k * B, n * B);
            if from + len > 2 * X {
                continue;
            }
            let mut out = vec![0u8; len as usize];
            two.read(two.pos(from).unwrap(), &mut out).unwrap();
            assert_eq!(out, expect(1, from, len), "two at {from}+{len}");
        }
    }
    // Three runs in one pipelined read (more than one device read's
    // length), and the data's end.
    let three = s.append_region("three").unwrap();
    let data = 3 * X - 2 * B;
    let mut out = vec![0u8; data as usize];
    three.read(0, &mut out).unwrap();
    assert_eq!(out, expect(2, 0, data));
    let mut past = vec![0u8; 4 * B as usize];
    let e = three.read(data - 2 * B, &mut past).unwrap_err();
    assert!(is_cause(&e, NotWrittenCause::OutOfBounds), "{e:?}");
}

/// A scan crosses runs, each byte where its run says, and ends at the
/// data's length.
#[test]
fn test_a_scan_crosses_runs() {
    let p = crafted(502);
    let s = reopen(&p);
    let three = s.append_region("three").unwrap();
    let mut got = Vec::new();
    let sum = three
        .scan(0, |item| {
            match item {
                ScanItem::Data { offset, bytes } => {
                    assert_eq!(offset, got.len() as u64);
                    got.extend_from_slice(bytes);
                }
                ScanItem::Unreadable { start, end } => panic!("unreadable {start}..{end}"),
            }
            ControlFlow::Continue(())
        })
        .unwrap();
    let data = 3 * X - 2 * B;
    assert_eq!(sum.end, data);
    assert_eq!(got, expect(2, 0, data));
}

/// Adjacent extents are one run: the region is writable, and a write across
/// the two lands in both, in data order.
#[test]
fn test_adjacent_extents_are_one_writable_run() {
    let p = crafted(503);
    let s = reopen(&p);
    let adj = s.page_region("adj").unwrap();
    let data: Vec<u8> = (0..2 * B).map(|i| (i % 253) as u8).collect();
    adj.write_durable(adj.pos(X - B).unwrap(), &data).unwrap();
    let mut out = vec![0u8; 2 * B as usize];
    adj.read(adj.pos(X - B).unwrap(), &mut out).unwrap();
    assert_eq!(out, data);
    let m = media(&p);
    let j = BASE + 2 * B + 2 * X + 2 * B + 5 * X;
    let at = (j + 2 * B + X - B) as usize;
    assert_eq!(&m[at..at + 2 * B as usize], &data[..]);
}

/// Until writes split at runs, a region in several runs is read-only, and a
/// Submitter's read across a run is refused (one within a run is not).
#[test]
fn test_several_runs_are_read_only_and_a_submitter_read_stays_in_a_run() {
    let p = crafted(504);
    let s = reopen(&p);
    let two = s.page_region("two").unwrap();
    let e = two.write(two.pos(0).unwrap(), &[7u8; 4096]).unwrap_err();
    assert!(is_cause(&e, NotWrittenCause::ReadOnly), "{e:?}");
    let mut sub = s.submitter(SubmitterConfig::default()).unwrap();
    let buf = sub.take_buf(2 * B as usize).unwrap();
    let r = sub
        .read_page(&two, two.pos(X - B).unwrap(), buf, 0)
        .unwrap_err();
    assert!(
        is_cause(&r.error, NotWrittenCause::NotReady),
        "{:?}",
        r.error
    );
    // Up to a run's end, and from a run's start: one device read each.
    for (k, at) in [(1, X - 2 * B), (2, X)] {
        let buf = sub.take_buf(2 * B as usize).unwrap();
        sub.read_page(&two, two.pos(at).unwrap(), buf, k).unwrap();
        let c = sub.wait(1).unwrap().next().unwrap();
        assert!(c.outcome.is_ok(), "{:?}", c.outcome);
        assert_eq!(c.buf.unwrap().as_slice(), &expect(1, at, 2 * B)[..]);
    }
    drop(r);
}

/// A bad sector in the second run is named at its region range.
#[test]
fn test_a_media_error_in_a_later_run_is_named_at_its_region_range() {
    let p = crafted(505);
    let s = reopen(&p);
    let file = p.with_world(|w| w.file_id(&PathBuf::from(DIR), CONTAINER).unwrap());
    // "two"'s second extent, region offset X, lies at BASE + 2B.
    let bad = BASE + 2 * B + 3 * B;
    p.with_world(|w| w.faults_mut().bad_ranges = vec![(file, bad, bad + B)]);
    let two = s.page_region("two").unwrap();
    let mut out = vec![0u8; 2 * X as usize];
    let e = two.read(two.pos(0).unwrap(), &mut out).unwrap_err();
    let Error::Corruption { kind, ctx } = e else {
        panic!("{e:?}");
    };
    assert_eq!(kind, CorruptionKind::MediaError);
    assert_eq!(
        ctx.range,
        Some(ByteRange {
            start: X + 3 * B,
            end: X + 4 * B
        })
    );
}

/// A header one extent ahead of its entry (a growth beyond capacity that
/// crashed before its page write), the entry with two extents: the region
/// opens with its two, the third reported.
#[test]
fn test_a_header_one_extent_ahead_of_two_committed_opens() {
    let p = fresh(506);
    let listed = [
        (BASE + 2 * B + X, X),
        (BASE + 2 * B, X),
        (BASE + 2 * B + 3 * X, X),
    ];
    let r = split(0, RegionKind::Page, "ahead", BASE, &listed, 2 * X, 2);
    write_regions(&p, 0, &[r]);
    fill(&p, 4, &listed[..2]);
    let s = reopen(&p);
    assert!(
        s.report().unavailable.is_empty(),
        "{:?}",
        s.report().unavailable
    );
    let extra = &s.report().metadata.extra_extents;
    assert_eq!(extra.len(), 1, "{extra:?}");
    assert_eq!(extra[0].1, (BASE + 2 * B + 3 * X, X));
    let ahead = s.page_region("ahead").unwrap();
    let mut out = vec![0u8; 2 * X as usize];
    ahead.read(ahead.pos(0).unwrap(), &mut out).unwrap();
    assert_eq!(out, expect(4, 0, 2 * X));
}
