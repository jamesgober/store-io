//! Raw ordered scans on the deterministic simulator: order, completeness,
//! unreadable ranges, early stops, and opens that change nothing.

// Test setup unwraps (a failure is the test failing) and ignores values a
// given test does not inspect.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use store_io_core::error::{Error, NotWrittenCause};
use store_io_engine::{CONTAINER, ScanItem, ScanSummary, Store, StoreOptions};
use store_io_sim::{FaultPlan, SimConfig, SimPlatform};

const DIR: &str = "/db";
const CHUNK: usize = 1 << 17;

fn opts() -> StoreOptions {
    StoreOptions {
        queues: 2,
        buffers_per_class: 16,
        max_io: CHUNK,
        ..StoreOptions::default()
    }
}

fn record(i: u32, len: usize) -> Vec<u8> {
    (0..len)
        .map(|j| (i as usize * 131 + j * 7 + (j >> 9)) as u8)
        .collect()
}

/// Everything a scan delivered: the concatenated data, and the unreadable
/// ranges, checking that items arrive in order with no gap or overlap.
fn collect(
    scan: impl FnOnce(&mut dyn FnMut(ScanItem<'_>) -> ControlFlow<()>) -> ScanSummary,
    from: u64,
) -> (Vec<u8>, Vec<(u64, u64)>, ScanSummary) {
    let mut data = Vec::new();
    let mut bad = Vec::new();
    let mut at = from;
    let sum = scan(&mut |item| {
        match item {
            ScanItem::Data { offset, bytes } => {
                assert_eq!(offset, at, "data out of order");
                data.extend_from_slice(bytes);
                at += bytes.len() as u64;
            }
            ScanItem::Unreadable { start, end } => {
                assert_eq!(start, at, "unreadable range out of order");
                assert!(end > start);
                assert!(
                    bad.last().is_none_or(|&(_, e)| e != start),
                    "adjacent unreadable ranges are merged"
                );
                bad.push((start, end));
                data.resize(data.len() + (end - start) as usize, 0xEE);
                at = end;
            }
        }
        ControlFlow::Continue(())
    });
    (data, bad, sum)
}

/// The container offset of the first occurrence of `needle` on media.
fn media_offset(p: &SimPlatform, needle: &[u8]) -> u64 {
    p.with_world(|w| {
        let media = w.media_of(&PathBuf::from(DIR), CONTAINER).unwrap();
        media
            .windows(needle.len())
            .position(|win| win == needle)
            .unwrap() as u64
    })
}

#[test]
fn test_scan_delivers_every_byte_in_order() {
    for reorder in [false, true] {
        let mut cfg = SimConfig::volatile(31);
        cfg.reorder = reorder;
        let p = SimPlatform::new(cfg);
        let s = Store::create(p, Path::new(DIR), opts()).unwrap();
        let wal = s
            .provision_append_region("wal", 3 * CHUNK as u64 + 8192)
            .unwrap();
        let mut expect = vec![0u8; wal.len() as usize];
        let mut i = 0;
        while wal.tail() + 70_000 < wal.len() {
            let rec = record(i, 70_000);
            let t = wal.append(&rec).unwrap();
            let at = t.pos().offset() as usize;
            expect[at..at + rec.len()].copy_from_slice(&rec);
            i += 1;
        }
        let (data, bad, sum) = collect(|v| wal.scan(0, v).unwrap(), 0);
        assert!(bad.is_empty());
        assert_eq!(sum.end, wal.len());
        assert!(!sum.stopped);
        assert!(data == expect, "scan bytes differ (reorder {reorder})");
        // From the middle, block-aligned.
        let from = 3 * wal.block_size();
        let (tail, _, _) = collect(|v| wal.scan(from, v).unwrap(), from);
        assert!(tail == expect[from as usize..]);
    }
}

#[test]
fn test_scan_reports_unreadable_blocks_exactly_and_continues() {
    let p = SimPlatform::new(SimConfig::volatile(32));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let wal = s.provision_append_region("wal", 5 * CHUNK as u64).unwrap();
    let bs = wal.block_size();
    let data = record(7, 4 * CHUNK);
    wal.append_durable(&data).unwrap();
    // Two bad ranges inside the region: one block, and three blocks that
    // straddle a read boundary.
    let base = media_offset(&p, &data[..64]);
    let bad1 = (5 * bs, 6 * bs);
    let bad2 = (CHUNK as u64 - bs, CHUNK as u64 + 2 * bs);
    p.with_world(|w| {
        let file = w.file_id(&PathBuf::from(DIR), CONTAINER).unwrap();
        *w.faults_mut() = FaultPlan {
            bad_ranges: vec![
                (file, base + bad1.0, base + bad1.1),
                (file, base + bad2.0, base + bad2.1),
            ],
            ..FaultPlan::default()
        };
    });
    let reads0 = p.with_world(|w| w.reads());
    let (got, bad, sum) = collect(|v| wal.scan(0, v).unwrap(), 0);
    let reads = p.with_world(|w| w.reads()) - reads0;
    assert_eq!(bad, vec![bad1, bad2]);
    // Bisection is logarithmic: 5 chunk reads, plus at most two reads per
    // halving level for each of the three failing chunks.
    let levels = (CHUNK as u64 / bs).ilog2() as u64 + 1;
    assert!(reads <= 5 + 3 * 2 * levels, "{reads} reads");
    assert_eq!(sum.unreadable, (bad1.1 - bad1.0) + (bad2.1 - bad2.0));
    assert_eq!(sum.end, wal.len());
    for (i, (&g, &d)) in got.iter().zip(&data).enumerate() {
        let i = i as u64;
        let in_bad = (bad1.0..bad1.1).contains(&i) || (bad2.0..bad2.1).contains(&i);
        assert_eq!(g, if in_bad { 0xEE } else { d }, "byte {i}");
    }
    // A read error never poisons: writes go on.
    wal.append_durable(b"still writable").unwrap();
}

#[test]
fn test_scan_stops_where_the_visitor_says() {
    let p = SimPlatform::new(SimConfig::volatile(33));
    let s = Store::create(p, Path::new(DIR), opts()).unwrap();
    let wal = s.provision_append_region("wal", 8 * CHUNK as u64).unwrap();
    wal.append_durable(&record(1, 8 * CHUNK)).unwrap();
    let mut seen = 0u64;
    let sum = wal
        .scan(0, |item| {
            if let ScanItem::Data { bytes, .. } = item {
                seen += bytes.len() as u64;
            }
            if seen >= 2 * CHUNK as u64 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .unwrap();
    assert!(sum.stopped);
    assert_eq!(sum.end, 2 * CHUNK as u64);
    assert_eq!(seen, 2 * CHUNK as u64);
}

#[test]
fn test_scan_refuses_bad_starts_and_handles_the_end() {
    let p = SimPlatform::new(SimConfig::volatile(34));
    let s = Store::create(p, Path::new(DIR), opts()).unwrap();
    let pages = s.provision_page_region("pages", 1 << 20).unwrap();
    assert!(matches!(
        pages.scan(100, |_| ControlFlow::Continue(())),
        Err(Error::NotWritten {
            cause: NotWrittenCause::Misaligned,
            ..
        })
    ));
    assert!(matches!(
        pages.scan(pages.len() + 4096, |_| ControlFlow::Continue(())),
        Err(Error::NotWritten {
            cause: NotWrittenCause::OutOfBounds,
            ..
        })
    ));
    let sum = pages
        .scan(pages.len(), |_| panic!("nothing to deliver"))
        .unwrap();
    assert_eq!(sum.end, pages.len());
}

#[test]
fn test_read_only_scan_sees_media_and_changes_nothing() {
    let p = SimPlatform::new(SimConfig::volatile(35));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    let rec = record(3, 300_000);
    let (pos, _) = wal.append_durable(&rec).unwrap();
    drop((wal, s));
    let before = p.with_world(|w| {
        w.media_of(&PathBuf::from(DIR), CONTAINER)
            .map(<[u8]>::to_vec)
    });
    let writes = p.with_world(|w| w.writes());
    let ro = Store::open_readonly(p.clone(), Path::new(DIR), opts()).unwrap();
    let wal = ro.append_region("wal").unwrap();
    let (data, bad, _) = collect(|v| wal.scan(0, v).unwrap(), 0);
    assert!(bad.is_empty());
    let at = pos.offset() as usize;
    assert!(data[at..at + rec.len()] == rec);
    drop((wal, ro));
    assert_eq!(
        p.with_world(|w| w.writes()),
        writes,
        "a read-only scan wrote"
    );
    let after = p.with_world(|w| {
        w.media_of(&PathBuf::from(DIR), CONTAINER)
            .map(<[u8]>::to_vec)
    });
    assert!(before == after);
}

#[test]
fn test_a_short_read_reports_the_missing_bytes_as_unreadable() {
    let p = SimPlatform::new(SimConfig::volatile(36));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let wal = s.provision_append_region("wal", 2 * CHUNK as u64).unwrap();
    let data = record(9, 2 * CHUNK);
    wal.append_durable(&data).unwrap();
    p.with_world(|w| {
        let next = w.reads() + 1;
        w.faults_mut().short_read = Some(next);
    });
    let (got, bad, sum) = collect(|v| wal.scan(0, v).unwrap(), 0);
    let half = (CHUNK / 2) as u64;
    assert_eq!(bad, vec![(half, CHUNK as u64)]);
    assert_eq!(sum.unreadable, half);
    assert!(got[..half as usize] == data[..half as usize]);
    assert!(got[CHUNK..] == data[CHUNK..]);
}
