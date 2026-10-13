//! The observer (feature `observe`): exact counters for known workloads,
//! sampled stage latencies, and nothing at all when the feature is off.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::path::Path;

use store_io_core::observe::Snapshot;
#[cfg(feature = "observe")]
use store_io_core::observe::{Counter, Stage};
use store_io_engine::{Store, StoreOptions};
use store_io_sim::{FaultPlan, SimConfig, SimPlatform};

const DIR: &str = "/db";
const BLOCK: u64 = 4096;

fn opts() -> StoreOptions {
    StoreOptions {
        queues: 2,
        buffers_per_class: 8,
        append_ring: 256,
        ..StoreOptions::default()
    }
}

#[cfg(feature = "observe")]
fn delta(after: &Snapshot, before: &Snapshot, c: Counter) -> u64 {
    after.counter(c) - before.counter(c)
}

#[cfg(feature = "observe")]
#[test]
fn test_counters_are_exact_for_known_work() {
    let p = SimPlatform::new(SimConfig::volatile(90));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    let before = s.observe();
    for i in 0..10u8 {
        wal.append_durable(&[i; 100]).unwrap();
    }
    let after = s.observe();
    // Each 100-byte append is one block written; each durable append one
    // barrier, and on a volatile device every barrier here leads a flush.
    assert_eq!(delta(&after, &before, Counter::Writes), 10);
    assert_eq!(delta(&after, &before, Counter::WriteBytes), 10 * BLOCK);
    assert_eq!(delta(&after, &before, Counter::Barriers), 10);
    assert_eq!(delta(&after, &before, Counter::Flushes), 10);
    assert_eq!(delta(&after, &before, Counter::Splits), 0);
    // The first span of each stage on a thread is sampled.
    assert!(after.stage(Stage::Write).samples() >= 1);
    assert!(after.stage(Stage::Barrier).samples() >= 1);
    assert!(after.stage(Stage::Flush).samples() >= 1);
    assert!(after.stage(Stage::Provision).samples() >= 1);

    // Reads: one device read per read of up to one buffer.
    let before = s.observe();
    let mut out = vec![0u8; BLOCK as usize];
    for i in 0..5u64 {
        wal.read(i * BLOCK, &mut out).unwrap();
    }
    let after = s.observe();
    assert_eq!(delta(&after, &before, Counter::Reads), 5);
    assert_eq!(delta(&after, &before, Counter::ReadBytes), 5 * BLOCK);
    assert!(after.stage(Stage::Read).samples() >= 1);

    // A media error is counted once.
    p.with_world(|w| {
        *w.faults_mut() = FaultPlan {
            fail_read: Some((w.reads() + 1, 5)),
            ..FaultPlan::default()
        };
    });
    let before = s.observe();
    assert!(wal.read(0, &mut out).is_err());
    let after = s.observe();
    assert_eq!(delta(&after, &before, Counter::MediaErrors), 1);
}

#[cfg(not(feature = "observe"))]
#[test]
fn test_nothing_is_recorded_without_the_feature() {
    let p = SimPlatform::new(SimConfig::volatile(91));
    let s = Store::create(p, Path::new(DIR), opts()).unwrap();
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    wal.append_durable(b"record").unwrap();
    let mut out = vec![0u8; BLOCK as usize];
    wal.read(0, &mut out).unwrap();
    assert_eq!(s.observe(), Snapshot::default());
}

/// `Store::poisoned` reports the first cause once a flush fails, and nothing
/// before (with or without the observer).
#[test]
fn test_poisoned_reports_the_first_cause() {
    let p = SimPlatform::new(SimConfig::volatile(92));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    wal.append_durable(b"kept").unwrap();
    assert!(s.poisoned().is_none());
    p.with_world(|w| {
        *w.faults_mut() = FaultPlan {
            fail_flush: Some(w.flushes() + 1),
            ..FaultPlan::default()
        };
    });
    assert!(wal.append_durable(b"lost").is_err());
    let first = s.poisoned().expect("a failed flush poisons the store");
    assert_eq!(first.op, Some(store_io_core::error::Op::FlushData));
}

/// Every read and write path counts exactly: a read larger than one buffer
/// (pipelined), a persistent media error (once per read, once per reported
/// range in a scan), a single write larger than one buffer (one split), a
/// batch of pages (no split), slot commits (timed), and barriers on a
/// power-safe device (counted, no flush).
#[cfg(feature = "observe")]
#[test]
fn test_every_read_and_write_path_counts_exactly() {
    use std::ops::ControlFlow;
    use std::path::PathBuf;

    use store_io_engine::{CONTAINER, ScanItem};

    let p = SimPlatform::new(SimConfig::volatile(93));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let pages = s.provision_page_region("pages", 4 << 20).unwrap();

    // One write of 2.5 MiB: three device writes, one split.
    let mut data = vec![7u8; (5 << 20) / 2];
    data[..12].copy_from_slice(b"OBSERVE-MARK");
    let before = s.observe();
    pages.write_durable(pages.pos(0).unwrap(), &data).unwrap();
    let after = s.observe();
    assert_eq!(delta(&after, &before, Counter::Splits), 1);
    assert_eq!(delta(&after, &before, Counter::Writes), 3);
    assert_eq!(
        delta(&after, &before, Counter::WriteBytes),
        data.len() as u64
    );

    // A batch of three pages: three device writes, no split.
    let before = s.observe();
    let mut batch = pages.batch();
    for i in 0..3u64 {
        batch
            .write(
                pages.pos((3 << 20) + i * BLOCK).unwrap(),
                &[i as u8; BLOCK as usize],
            )
            .unwrap();
    }
    batch.commit().unwrap();
    let after = s.observe();
    assert_eq!(delta(&after, &before, Counter::Splits), 0);
    assert_eq!(delta(&after, &before, Counter::Writes), 3);

    // A read of 2 MiB (two buffers): the pipelined path, two device reads.
    let before = s.observe();
    let mut out = vec![0u8; 2 << 20];
    pages.read(pages.pos(0).unwrap(), &mut out).unwrap();
    let after = s.observe();
    assert_eq!(delta(&after, &before, Counter::Reads), 2);
    assert_eq!(delta(&after, &before, Counter::ReadBytes), 2 << 20);

    // Two persistent bad blocks: one media error per failing read, one per
    // reported range in a scan, never one per sector or per rescan read.
    let (file, base) = p.with_world(|w| {
        let media = w.media_of(&PathBuf::from(DIR), CONTAINER).unwrap();
        let at = media
            .windows(12)
            .position(|x| x == b"OBSERVE-MARK")
            .unwrap() as u64;
        (w.file_id(&PathBuf::from(DIR), CONTAINER).unwrap(), at)
    });
    p.with_world(|w| {
        w.faults_mut().bad_ranges = vec![
            (file, base + 4 * BLOCK, base + 5 * BLOCK),
            // Two blocks: per-sector counting would report 3, not 2.
            (file, base + 20 * BLOCK, base + 22 * BLOCK),
        ];
    });
    let before = s.observe();
    let mut small = vec![0u8; 8 * BLOCK as usize];
    assert!(pages.read(pages.pos(0).unwrap(), &mut small).is_err());
    let after = s.observe();
    assert_eq!(
        delta(&after, &before, Counter::MediaErrors),
        1,
        "one failing read"
    );
    let before = s.observe();
    let _summary = pages
        .scan(0, |item| {
            let _ = matches!(item, ScanItem::Unreadable { .. });
            ControlFlow::Continue(())
        })
        .unwrap();
    let after = s.observe();
    assert_eq!(
        delta(&after, &before, Counter::MediaErrors),
        2,
        "two reported ranges"
    );
    // A read larger than one buffer (the pipelined path) that meets them:
    // one media error, for the one error it returns.
    let before = s.observe();
    let mut big = vec![0u8; 2 << 20];
    assert!(pages.read(pages.pos(0).unwrap(), &mut big).is_err());
    let after = s.observe();
    assert_eq!(
        delta(&after, &before, Counter::MediaErrors),
        1,
        "one failing pipelined read"
    );
    p.with_world(|w| w.faults_mut().bad_ranges.clear());

    // An append larger than one buffer that does not fit is no split: nothing
    // reached the device.
    let small = s.provision_append_region("small", 1 << 20).unwrap();
    let before = s.observe();
    assert!(small.append(&vec![1u8; 3 << 20]).is_err());
    let after = s.observe();
    assert_eq!(delta(&after, &before, Counter::Splits), 0);
    assert_eq!(delta(&after, &before, Counter::Writes), 0);

    // Slot commits are timed.
    let manifest = s.provision_slot("manifest").unwrap();
    manifest.commit(b"v1").unwrap();
    assert!(s.observe().stage(Stage::SlotCommit).samples() >= 1);

    // A power-safe device: every barrier counted, none flushes.
    let p = SimPlatform::new(SimConfig::power_safe(94));
    let s = Store::create(p, Path::new(DIR), opts()).unwrap();
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    let before = s.observe();
    for _ in 0..3 {
        wal.append_durable(b"record").unwrap();
    }
    let after = s.observe();
    assert_eq!(delta(&after, &before, Counter::Barriers), 3);
    assert_eq!(delta(&after, &before, Counter::Flushes), 0);
}
