//! The simulator's own semantics: what is durable when, and what a crash keeps.

// Test setup unwraps (a failure is the test failing) and drops drained
// completion lists whose contents a given test does not inspect.
#![allow(clippy::unwrap_used, clippy::expect_used, unused_results)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use store_io_buf::{BufPool, PoolConfig};
use store_io_core::error::OsError;
use store_io_platform::{CompletionBuf, FileMode, FileName, IoOp, Platform, Queue, QueueConfig};
use store_io_sim::{
    CrashMode, FaultPlan, FlushFailure, SimConfig, SimDir, SimFile, SimPlatform, SimQueue,
};

fn setup(cfg: SimConfig) -> (SimPlatform, SimDir, SimFile, BufPool) {
    let p = SimPlatform::new(cfg);
    let dir = p.open_dir(Path::new("/s"), true).unwrap();
    let file = p.create_file(&dir, &FileName::new("f").unwrap()).unwrap();
    p.allocate(&file, 1 << 20).unwrap();
    p.flush_all(&file).unwrap();
    p.sync_dir(&dir).unwrap();
    let pool = BufPool::new(&PoolConfig::uniform(4096, 4096, 65536, 16)).unwrap();
    (p, dir, file, pool)
}

fn write(q: &mut SimQueue, f: &SimFile, pool: &BufPool, off: u64, byte: u8, dsync: bool) {
    let mut b = pool.take(4096).unwrap();
    b.as_mut_slice().fill(byte);
    q.submit(
        IoOp::Write {
            file: f,
            offset: off,
            buf: b,
            dsync,
        },
        off,
    )
    .unwrap();
}

fn drain(q: &mut SimQueue) -> Vec<(u64, Result<usize, OsError>)> {
    let mut out = CompletionBuf::with_capacity(64);
    let n = q.in_flight();
    q.wait(n, &mut out).unwrap();
    out.drain().map(|c| (c.tag, c.result)).collect()
}

fn dir() -> PathBuf {
    PathBuf::from("/s")
}

fn media(p: &SimPlatform) -> Vec<u8> {
    p.with_world(|w| w.media_of(&dir(), "f").map(<[u8]>::to_vec))
        .unwrap()
}

#[test]
fn test_volatile_write_is_lost_without_flush_and_kept_after() {
    let (p, _d, f, pool) = setup(SimConfig::volatile(1));
    let mut q = p.queue(QueueConfig::default()).unwrap();
    write(&mut q, &f, &pool, 0, 0xAA, false);
    drain(&mut q);
    let mut lost = p.with_world(|w| w.clone());
    lost.crash(&CrashMode::LoseAll);
    assert_eq!(lost.media_of(&dir(), "f").unwrap()[0], 0);

    q.submit(IoOp::FlushData { file: &f }, 99).unwrap();
    drain(&mut q);
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    assert_eq!(media(&p)[0], 0xAA);
}

#[test]
fn test_flush_covers_only_writes_completed_before_its_submission() {
    let (p, _d, f, pool) = setup(SimConfig::volatile(2));
    let mut q = p.queue(QueueConfig::default()).unwrap();
    // The flush is submitted while the write is still in flight: no coverage.
    write(&mut q, &f, &pool, 0, 0x11, false);
    q.submit(IoOp::FlushData { file: &f }, 1).unwrap();
    drain(&mut q);
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    assert_eq!(
        media(&p)[0],
        0,
        "a flush submitted before the write completed covered it"
    );
}

#[test]
fn test_dsync_and_power_safe_writes_are_durable_at_completion() {
    let (p, _d, f, pool) = setup(SimConfig::volatile(3));
    let mut q = p.queue(QueueConfig::default()).unwrap();
    write(&mut q, &f, &pool, 0, 0x22, true);
    drain(&mut q);
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    assert_eq!(media(&p)[0], 0x22);

    let (p, _d, f, pool) = setup(SimConfig::power_safe(3));
    let mut q = p.queue(QueueConfig::default()).unwrap();
    write(&mut q, &f, &pool, 4096, 0x33, false);
    drain(&mut q);
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    assert_eq!(media(&p)[4096], 0x33);
}

#[test]
fn test_directory_entries_need_a_directory_sync() {
    let p = SimPlatform::new(SimConfig::volatile(4));
    let d = p.open_dir(Path::new("/d"), true).unwrap();
    let f = p.create_file(&d, &FileName::new("x").unwrap()).unwrap();
    p.flush_all(&f).unwrap();
    p.with_world(|w| w.crash(&CrashMode::KeepAll));
    assert!(
        p.open_file(&d, &FileName::new("x").unwrap(), FileMode::ReadOnly)
            .is_err()
    );
}

#[test]
fn test_crash_invalidates_handles_and_locks() {
    let (p, _d, f, pool) = setup(SimConfig::volatile(5));
    let lock = p.lock_exclusive(&f).unwrap();
    assert!(p.lock_exclusive(&f).is_err());
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    let mut q = p.queue(QueueConfig::default()).unwrap();
    let b = pool.take(4096).unwrap();
    assert!(
        q.submit(
            IoOp::Write {
                file: &f,
                offset: 0,
                buf: b,
                dsync: false
            },
            0
        )
        .is_err()
    );
    drop(lock);
}

#[test]
fn test_failed_flush_with_drop_loses_writes_and_the_next_flush_succeeds_falsely() {
    let mut cfg = SimConfig::volatile(6);
    // Flush 1 is setup's flush_all; flush 2 fails and drops the cache.
    cfg.faults = FaultPlan {
        fail_flush: Some(2),
        flush_failure: FlushFailure::Drop,
        ..FaultPlan::default()
    };
    let (p, _d, f, pool) = setup(cfg);
    let mut q = p.queue(QueueConfig::default()).unwrap();
    write(&mut q, &f, &pool, 0, 0x44, false);
    drain(&mut q);
    q.submit(IoOp::FlushData { file: &f }, 1).unwrap();
    assert!(drain(&mut q)[0].1.is_err());
    q.submit(IoOp::FlushData { file: &f }, 2).unwrap();
    assert!(
        drain(&mut q)[0].1.is_ok(),
        "the fsyncgate trap: a later flush succeeds"
    );
    p.with_world(|w| w.crash(&CrashMode::KeepAll));
    assert_eq!(media(&p)[0], 0, "the dropped write must not reappear");
}

#[test]
fn test_lying_flush_persists_nothing() {
    let mut cfg = SimConfig::volatile(9);
    cfg.faults = FaultPlan {
        lying_flush: true,
        ..FaultPlan::default()
    };
    let p = SimPlatform::new(cfg);
    let d = p.open_dir(Path::new("/s"), true).unwrap();
    let f = p.create_file(&d, &FileName::new("f").unwrap()).unwrap();
    p.allocate(&f, 65536).unwrap();
    p.sync_dir(&d).unwrap();
    let pool = BufPool::new(&PoolConfig::uniform(4096, 4096, 4096, 4)).unwrap();
    let mut q = p.queue(QueueConfig::default()).unwrap();
    write(&mut q, &f, &pool, 0, 0x55, false);
    q.submit(IoOp::FlushData { file: &f }, 1).unwrap();
    drain(&mut q);
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    assert!(
        p.with_world(|w| w.media_of(&dir(), "f").map(<[u8]>::is_empty))
            .unwrap_or(true)
    );
}

#[test]
fn test_subset_crash_enumerates_every_combination() {
    let (p, _d, f, pool) = setup(SimConfig::volatile(7));
    let mut q = p.queue(QueueConfig::default()).unwrap();
    for i in 0..3u8 {
        write(&mut q, &f, &pool, u64::from(i) * 4096, i + 1, false);
    }
    drain(&mut q);
    let base = p.with_world(|w| w.clone());
    let mut seen = BTreeSet::new();
    for mask in 0..8u64 {
        let mut w = base.clone();
        w.crash(&CrashMode::Subset(mask));
        let m = w.media_of(&dir(), "f").unwrap();
        let _added = seen.insert((m[0], m[4096], m[8192]));
    }
    assert_eq!(seen.len(), 8);
}

#[test]
fn test_same_seed_same_trace_with_reordering_and_random_crash() {
    let run = |seed: u64| {
        let mut cfg = SimConfig::volatile(seed);
        cfg.reorder = true;
        let (p, _d, f, pool) = setup(cfg);
        let mut q = p.queue(QueueConfig::default()).unwrap();
        for i in 0..10u8 {
            write(&mut q, &f, &pool, u64::from(i) * 4096, i, false);
        }
        drain(&mut q);
        p.with_world(|w| {
            w.crash(&CrashMode::Random);
            (w.trace_hash(), w.media_of(&dir(), "f").map(<[u8]>::to_vec))
        })
    };
    assert_eq!(run(11), run(11));
    assert_ne!(run(11).0, run(12).0);
}

#[test]
fn test_reordered_completion_returns_every_operation_once() {
    let mut cfg = SimConfig::volatile(13);
    cfg.reorder = true;
    let (p, _d, f, pool) = setup(cfg);
    let mut q = p.queue(QueueConfig::default()).unwrap();
    for i in 0..12u64 {
        write(&mut q, &f, &pool, i * 4096, 1, false);
    }
    let mut tags: Vec<u64> = drain(&mut q).into_iter().map(|(t, _)| t).collect();
    let in_order = tags.windows(2).all(|w| w[0] < w[1]);
    tags.sort_unstable();
    assert_eq!(tags, (0..12).map(|i| i * 4096).collect::<Vec<_>>());
    assert!(!in_order, "seed 13 should reorder at least one completion");
}

#[test]
fn test_targeted_write_faults() {
    let mut cfg = SimConfig::volatile(8);
    cfg.faults = FaultPlan {
        fail_write: Some(1),
        short_write: Some(2),
        lose_write: Some(3),
        ..FaultPlan::default()
    };
    let (p, _d, f, pool) = setup(cfg);
    let mut q = p.queue(QueueConfig::default()).unwrap();
    write(&mut q, &f, &pool, 0, 1, true);
    let mut b = pool.take(8192).unwrap();
    b.as_mut_slice().fill(9);
    q.submit(
        IoOp::Write {
            file: &f,
            offset: 8192,
            buf: b,
            dsync: true,
        },
        2,
    )
    .unwrap();
    write(&mut q, &f, &pool, 20480, 3, true);
    let r = drain(&mut q);
    assert!(r[0].1.is_err(), "write 1 fails");
    assert_eq!(r[1].1, Ok(4096), "write 2 is short (half of 8 KiB)");
    assert_eq!(r[2].1, Ok(4096), "write 3 reports success");
    p.with_world(|w| w.crash(&CrashMode::KeepAll));
    assert_eq!(media(&p)[20480], 0, "write 3 was lost");
}

#[test]
fn test_out_of_space_on_allocate() {
    let mut cfg = SimConfig::volatile(10);
    cfg.faults = FaultPlan {
        capacity: Some(1 << 16),
        ..FaultPlan::default()
    };
    let p = SimPlatform::new(cfg);
    let d = p.open_dir(Path::new("/s"), true).unwrap();
    let f = p.create_file(&d, &FileName::new("f").unwrap()).unwrap();
    assert!(p.allocate(&f, 1 << 16).is_ok());
    assert_eq!(p.allocate(&f, 1 << 17).map_err(|e| e.code), Err(28));
}

#[test]
fn test_misaligned_direct_io_completes_with_einval_and_touches_nothing() {
    let (p, _dir, file, pool) = setup(SimConfig::volatile(40));
    let mut q = p.queue(QueueConfig { depth: 8 }).unwrap();
    let before = media(&p);
    let writes_before = p.with_world(|w| w.writes());
    // Misaligned offset, then misaligned length.
    for (tag, offset, len) in [(1u64, 512u64, 4096usize), (2, 0, 1000)] {
        let mut b = pool.take(4096).unwrap();
        assert!(b.set_len(len));
        b.as_mut_slice().fill(0xEE);
        q.submit(
            IoOp::Write {
                file: &file,
                offset,
                buf: b,
                dsync: true,
            },
            tag,
        )
        .unwrap();
    }
    let mut b = pool.take(4096).unwrap();
    assert!(b.set_len(100));
    q.submit(
        IoOp::Read {
            file: &file,
            offset: 0,
            buf: b,
        },
        3,
    )
    .unwrap();
    let done = drain(&mut q);
    assert_eq!(done.len(), 3);
    assert!(done.iter().all(|(_, r)| r.map_err(|e| e.code) == Err(22)));
    assert_eq!(p.with_world(|w| w.writes()), writes_before);
    assert_eq!(media(&p), before);
}

#[test]
fn test_a_read_past_the_end_of_the_file_transfers_nothing() {
    let (p, _dir, file, pool) = setup(SimConfig::volatile(41));
    let mut q = p.queue(QueueConfig { depth: 2 }).unwrap();
    let end = p.size(&file).unwrap();
    q.submit(
        IoOp::Read {
            file: &file,
            offset: end + 8192,
            buf: pool.take(4096).unwrap(),
        },
        9,
    )
    .unwrap();
    let done = drain(&mut q);
    assert_eq!(done, vec![(9, Ok(0))]);
}
