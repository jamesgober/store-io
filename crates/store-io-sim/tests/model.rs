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
    let mut q = p
        .queue(QueueConfig {
            depth: 8,
            ..QueueConfig::default()
        })
        .unwrap();
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
    let mut q = p
        .queue(QueueConfig {
            depth: 2,
            ..QueueConfig::default()
        })
        .unwrap();
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

/// A deallocated range reads as unwritten (a hole) until written; a trim in
/// place keeps it written; `allocate_range` changes no byte and no size,
/// refuses a range past the end, and fails when the plan says.
#[test]
fn test_holes_read_unwritten_until_written_and_allocate_range_keeps_bytes() {
    use store_io_core::evidence::Tri;
    use store_io_platform::ReleaseHow;
    let (p, _d, f, pool) = setup(SimConfig::volatile(91));
    let mut q = p.queue(QueueConfig::default()).unwrap();
    for i in 0..4u64 {
        write(&mut q, &f, &pool, i * 4096, 1, false);
    }
    drain(&mut q);
    let state = |off: u64, len: u64| p.range_state(&f, off, len).unwrap().unwritten;
    assert_eq!(state(0, 16384), Tri::No);
    p.release_range(&f, 0, 8192, ReleaseHow::Deallocate)
        .unwrap();
    assert_eq!(state(0, 8192), Tri::Yes);
    assert_eq!(state(4096, 4096), Tri::Yes);
    assert_eq!(state(8192, 8192), Tri::No);
    p.release_range(&f, 8192, 4096, ReleaseHow::TrimInPlace)
        .unwrap();
    assert_eq!(state(8192, 4096), Tri::No, "a trim punches no hole");
    p.allocate_range(&f, 0, 16384).unwrap();
    assert_eq!(
        state(0, 8192),
        Tri::Yes,
        "an allocated hole stays unwritten"
    );
    assert_eq!(p.size(&f).unwrap(), 1 << 20);
    write(&mut q, &f, &pool, 0, 2, false);
    drain(&mut q);
    assert_eq!(state(0, 4096), Tri::No, "a write fills the hole");
    assert_eq!(state(4096, 4096), Tri::Yes);
    assert_eq!(
        p.allocate_range(&f, (1 << 20) - 4096, 8192)
            .unwrap_err()
            .code,
        22
    );
    let next = p.with_world(|w| w.range_allocations()) + 1;
    p.with_world(|w| w.faults_mut().fail_allocate_range = Some((next, 28)));
    assert_eq!(p.allocate_range(&f, 0, 4096).unwrap_err().code, 28);
    // Past the end is not written data (the last block written, the rest of
    // the range past the end), nor is a file's growth until it is written;
    // a zero-length query is never unwritten.
    write(&mut q, &f, &pool, (1 << 20) - 4096, 5, false);
    drain(&mut q);
    assert_eq!(state((1 << 20) - 4096, 4096), Tri::No);
    assert_eq!(state((1 << 20) - 4096, 8192), Tri::Yes);
    assert_eq!(state(0, 0), Tri::No);
    p.set_len(&f, 4096).unwrap();
    p.set_len(&f, 8192).unwrap();
    assert_eq!(state(4096, 4096), Tri::Yes, "grown, never written");
    // A write past the end: the gap before it is a hole, the write is not.
    write(&mut q, &f, &pool, 16384, 3, false);
    drain(&mut q);
    assert_eq!(state(8192, 8192), Tri::Yes);
    assert_eq!(state(16384, 4096), Tri::No);
    // A write the device lost is still written to the file system.
    let lost = p.with_world(|w| w.writes()) + 1;
    p.with_world(|w| w.faults_mut().lose_write = Some(lost));
    write(&mut q, &f, &pool, 8192, 4, false);
    drain(&mut q);
    assert_eq!(state(8192, 4096), Tri::No);
}

/// The valid-data model (NTFS): valid data ends where the furthest write
/// ended; a write past it costs the zeroing of the gap; a range reaching
/// past it is reported not valid. Without the model the gap is still
/// counted and every range is valid.
#[test]
fn test_the_valid_data_model() {
    use store_io_core::evidence::Tri;
    for model in [true, false] {
        let (p, _d, f, pool) = setup(SimConfig {
            valid_data_model: model,
            ..SimConfig::volatile(93)
        });
        let mut q = p.queue(QueueConfig::default()).unwrap();
        let valid = |off: u64, len: u64| p.range_state(&f, off, len).unwrap().valid_data;
        let gap = || p.with_world(|w| w.gap_fill_bytes());
        let past = if model { Tri::No } else { Tri::Yes };
        let before = gap();
        write(&mut q, &f, &pool, 0, 1, false);
        drain(&mut q);
        assert_eq!(gap(), before, "a write from the start: no gap");
        assert_eq!(valid(0, 4096), Tri::Yes);
        assert_eq!(valid(4096, 4096), past);
        // Allocation moves nothing.
        p.allocate_range(&f, 0, 16384).unwrap();
        assert_eq!(valid(8192, 4096), past);
        write(&mut q, &f, &pool, 12288, 2, false);
        drain(&mut q);
        assert_eq!(gap(), before + 8192, "the gap zeroed first");
        assert_eq!(valid(0, 16384), Tri::Yes);
        // A write below the valid data costs nothing.
        write(&mut q, &f, &pool, 4096, 3, false);
        drain(&mut q);
        assert_eq!(gap(), before + 8192);
        // A shrink cuts the valid data.
        p.set_len(&f, 8192).unwrap();
        p.set_len(&f, 16384).unwrap();
        assert_eq!(valid(8192, 4096), past);
    }
}

/// A crash takes a file back to its durable length: growth, holes and
/// writes past it are gone, and a later growth over those bytes is new
/// (unwritten until written).
#[test]
fn test_a_crash_drops_holes_past_the_durable_length() {
    use store_io_core::evidence::Tri;
    use store_io_platform::ReleaseHow;
    let (p, _d, f, pool) = setup(SimConfig::volatile(92));
    let mut q = p.queue(QueueConfig::default()).unwrap();
    for i in 0..256u64 {
        write(&mut q, &f, &pool, i * 4096, 1, false);
        drain(&mut q);
    }
    p.flush_all(&f).unwrap();
    // Grown and punched, never flushed: gone at the crash.
    p.allocate(&f, 2 << 20).unwrap();
    for i in 256..512u64 {
        write(&mut q, &f, &pool, i * 4096, 2, false);
        drain(&mut q);
    }
    p.release_range(&f, 1 << 20, 4096, ReleaseHow::Deallocate)
        .unwrap();
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    let d = p.open_dir(&dir(), false).unwrap();
    let f = p
        .open_file(&d, &FileName::new("f").unwrap(), FileMode::ReadWrite)
        .unwrap();
    assert_eq!(p.size(&f).unwrap(), 1 << 20);
    assert_eq!(p.range_state(&f, 0, 1 << 20).unwrap().unwritten, Tri::No);
    // Grown again and written: no hole left from before the crash.
    p.allocate(&f, 2 << 20).unwrap();
    let mut q = p.queue(QueueConfig::default()).unwrap();
    for i in 256..512u64 {
        write(&mut q, &f, &pool, i * 4096, 3, false);
        drain(&mut q);
    }
    assert_eq!(
        p.range_state(&f, 1 << 20, 1 << 20).unwrap().unwritten,
        Tri::No
    );
}

#[test]
fn test_an_exact_crash_gives_each_cached_write_its_fate() {
    use store_io_sim::Fate;
    let (p, _d, f, pool) = setup(SimConfig::volatile(8));
    let mut q = p.queue(QueueConfig::default()).unwrap();
    // Writes of 3, 1, 1 and 2 blocks, each its own byte, back to back.
    let mut at = 0u64;
    for (byte, blocks) in [(1u8, 3usize), (2, 1), (3, 1), (4, 2)] {
        let mut b = pool.take(blocks * 4096).unwrap();
        b.as_mut_slice().fill(byte);
        q.submit(
            IoOp::Write {
                file: &f,
                offset: at,
                buf: b,
                dsync: false,
            },
            at,
        )
        .unwrap();
        at += blocks as u64 * 4096;
    }
    drain(&mut q);
    let base = p.with_world(|w| w.clone());
    let extents: Vec<(u64, usize)> = base
        .cached_extents()
        .iter()
        .map(|&(_, at, blocks)| (at, blocks))
        .collect();
    assert_eq!(
        extents,
        [(0, 3), (3 * 4096, 1), (4 * 4096, 1), (5 * 4096, 2)]
    );
    let after = |fates: Vec<Fate>| {
        let mut w = base.clone();
        w.crash(&CrashMode::Exact(fates));
        let m = w.media_of(&dir(), "f").unwrap().to_vec();
        (0..7).map(|i| m[i * 4096]).collect::<Vec<u8>>()
    };
    // The first torn after two blocks, the second kept, the third lost,
    // the fourth past the list (lost).
    assert_eq!(
        after(vec![Fate::Torn(2), Fate::Kept, Fate::Lost]),
        [1, 1, 0, 2, 0, 0, 0]
    );
    // A tear longer than the write keeps it whole; a tear of none loses it.
    assert_eq!(
        after(vec![Fate::Torn(0), Fate::Lost, Fate::Kept, Fate::Torn(5)]),
        [0, 0, 0, 0, 3, 4, 4]
    );
}

#[test]
fn test_the_step_probe_runs_before_each_step_that_can_change_a_crash() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use store_io_platform::ReleaseHow;
    let (p, d, f, pool) = setup(SimConfig::volatile(9));
    let steps = Arc::new(AtomicUsize::new(0));
    let s = Arc::clone(&steps);
    p.with_world(|w| {
        w.set_step_probe(Some(Arc::new(move || {
            s.fetch_add(1, Ordering::Relaxed);
            false
        })))
    });
    let mut last = 0;
    let mut stepped = |what: &str, n: usize| {
        let now = steps.load(Ordering::Relaxed);
        assert_eq!(now - last, n, "{what}");
        last = now;
    };
    let name = |s: &str| FileName::new(s).unwrap();
    let _made = p.open_dir(Path::new("/t"), true).unwrap();
    stepped("open_dir creating", 1);
    let g = p.create_file(&d, &name("g")).unwrap();
    stepped("create_file", 1);
    p.rename_noreplace(&d, &name("g"), &name("h"), &g).unwrap();
    stepped("rename_noreplace", 1);
    p.rename_replace(&d, &name("h"), &name("g"), &g).unwrap();
    stepped("rename_replace", 1);
    p.unlink(&d, &name("g")).unwrap();
    stepped("unlink", 1);
    p.sync_dir(&d).unwrap();
    stepped("sync_dir", 1);
    p.allocate(&f, 2 << 20).unwrap();
    stepped("allocate", 1);
    p.set_len(&f, 2 << 20).unwrap();
    stepped("set_len", 1);
    p.release_range(&f, 0, 4096, ReleaseHow::Deallocate)
        .unwrap();
    stepped("release_range", 1);
    p.allocate_range(&f, 0, 4096).unwrap();
    stepped("allocate_range", 1);
    p.flush_all(&f).unwrap();
    stepped("flush_all", 1);
    let mut q = p.queue(QueueConfig::default()).unwrap();
    write(&mut q, &f, &pool, 0, 1, false);
    q.submit(IoOp::FlushData { file: &f }, 1).unwrap();
    drain(&mut q);
    stepped("a write and a flush submitted", 2);
    // Nothing else is a step.
    let _f = p.open_file(&d, &name("f"), FileMode::ReadWrite).unwrap();
    let _len = p.size(&f).unwrap();
    let _dir = p.open_dir(&dir(), false).unwrap();
    stepped("opens and a size", 0);
}

#[test]
fn test_settled_metadata_keeps_names_and_lengths_not_data() {
    let (p, d, _f, pool) = setup(SimConfig::volatile(10));
    let name = FileName::new("g").unwrap();
    let g = p.create_file(&d, &name).unwrap();
    let mut q = p.queue(QueueConfig::default()).unwrap();
    write(&mut q, &g, &pool, 4096, 7, false);
    drain(&mut q);
    let base = p.with_world(|w| w.clone());
    // Unsettled: the name was never synced, so the file is gone.
    let mut w = base.clone();
    w.crash(&CrashMode::LoseAll);
    assert!(w.media_of(&dir(), "g").is_none());
    // Settled: the name and the length survive, the unflushed bytes read
    // as zeros, or as written when the write is kept.
    let mut w = base.clone();
    w.settle_metadata();
    w.crash(&CrashMode::LoseAll);
    assert_eq!(w.media_of(&dir(), "g").unwrap(), &[0u8; 8192][..]);
    let mut w = base;
    w.settle_metadata();
    w.crash(&CrashMode::KeepAll);
    let m = w.media_of(&dir(), "g").unwrap();
    assert_eq!((m.len(), m[0], m[4096]), (8192, 0, 7));
}

#[test]
fn test_a_durable_length_past_the_media_is_unwritten_after_a_crash() {
    use store_io_core::evidence::Tri;
    let (p, d, _f, pool) = setup(SimConfig::volatile(12));
    let name = FileName::new("g").unwrap();
    let g = p.create_file(&d, &name).unwrap();
    p.sync_dir(&d).unwrap();
    let mut q = p.queue(QueueConfig::default()).unwrap();
    write(&mut q, &g, &pool, 0, 1, false);
    write(&mut q, &g, &pool, 4096, 2, false);
    drain(&mut q);
    p.with_world(|w| {
        w.settle_metadata();
        // The first write kept, the second lost: past the first, the file's
        // durable length holds nothing.
        w.crash(&CrashMode::Exact(vec![store_io_sim::Fate::Kept]));
    });
    let d = p.open_dir(&dir(), false).unwrap();
    let g = p.open_file(&d, &name, FileMode::ReadWrite).unwrap();
    assert_eq!(p.size(&g).unwrap(), 8192);
    assert_eq!(p.range_state(&g, 0, 4096).unwrap().unwritten, Tri::No);
    assert_eq!(p.range_state(&g, 4096, 4096).unwrap().unwritten, Tri::Yes);
}

#[test]
fn test_a_step_probe_cuts_the_power() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (p, d, f, pool) = setup(SimConfig::volatile(11));
    let mut q = p.queue(QueueConfig::default()).unwrap();
    write(&mut q, &f, &pool, 0, 1, false);
    drain(&mut q);
    let steps = Arc::new(AtomicUsize::new(0));
    let s = Arc::clone(&steps);
    // The second step from here cuts the power.
    p.with_world(|w| {
        w.set_step_probe(Some(Arc::new(move || {
            s.fetch_add(1, Ordering::Relaxed) + 1 == 2
        })))
    });
    write(&mut q, &f, &pool, 4096, 2, false);
    assert!(!p.with_world(|w| w.powered_off()));
    // The cut: this step fails, so does every later one, and the write in
    // flight completes with EIO, unapplied.
    let eio = p.flush_all(&f).unwrap_err().code;
    assert!(p.with_world(|w| w.powered_off()));
    assert_eq!(p.sync_dir(&d).unwrap_err().code, eio);
    let mut b = pool.take(4096).unwrap();
    b.as_mut_slice().fill(3);
    let refused = q
        .submit(
            IoOp::Write {
                file: &f,
                offset: 8192,
                buf: b,
                dsync: false,
            },
            8192,
        )
        .unwrap_err();
    assert_eq!(refused.raw.map(|e| e.code), Some(eio));
    // A read and a flush submitted before the cut (as steps 0 and 1 of a
    // second probe would be: the probe counts from here) complete with EIO
    // too, the flush persisting nothing.
    let done = drain(&mut q);
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].1.as_ref().err().map(|e| e.code), Some(eio));
    assert_eq!(
        p.with_world(|w| w.cached_extents().len()),
        1,
        "only the first write"
    );
    // The crash keeps what was cached before the cut and turns the power on.
    p.with_world(|w| {
        w.set_step_probe(None);
        w.crash(&CrashMode::KeepAll);
    });
    assert!(!p.with_world(|w| w.powered_off()));
    let m = media(&p);
    assert_eq!((m[0], m[4096], m[8192]), (1, 0, 0));
    let d = p.open_dir(&dir(), false).unwrap();
    let f = p
        .open_file(&d, &FileName::new("f").unwrap(), FileMode::ReadWrite)
        .unwrap();
    p.flush_all(&f).unwrap();
}

#[test]
fn test_a_flush_and_a_read_in_flight_at_the_cut_complete_unapplied() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (p, _d, f, pool) = setup(SimConfig::volatile(13));
    let mut q = p.queue(QueueConfig::default()).unwrap();
    write(&mut q, &f, &pool, 0, 1, false);
    drain(&mut q);
    let steps = Arc::new(AtomicUsize::new(0));
    let s = Arc::clone(&steps);
    // The third step from here cuts the power: a flush and a read go in
    // first and are still in flight.
    p.with_world(|w| {
        w.set_step_probe(Some(Arc::new(move || {
            s.fetch_add(1, Ordering::Relaxed) + 1 == 3
        })))
    });
    q.submit(IoOp::FlushData { file: &f }, 1).unwrap();
    let mut b = pool.take(4096).unwrap();
    b.as_mut_slice().fill(9);
    q.submit(
        IoOp::Read {
            file: &f,
            offset: 0,
            buf: b,
        },
        2,
    )
    .unwrap();
    assert!(p.allocate(&f, 2 << 20).is_err(), "the cut");
    let done = drain(&mut q);
    assert_eq!(done.len(), 2);
    assert!(done.iter().all(|(_, r)| r.is_err()), "{done:?}");
    // The flush persisted nothing: the cached write is still cached, and a
    // crash losing it leaves the media as before it.
    assert_eq!(p.with_world(|w| w.cached_extents().len()), 1);
    p.with_world(|w| {
        w.set_step_probe(None);
        w.crash(&CrashMode::LoseAll);
    });
    assert_eq!(media(&p)[0], 0);
}

#[test]
fn test_a_lost_write_below_a_kept_one_is_unwritten_after_a_settled_crash() {
    use store_io_core::evidence::Tri;
    use store_io_sim::Fate::{Kept, Lost, Torn};
    let (p, d, _f, pool) = setup(SimConfig::volatile(14));
    let name = FileName::new("g").unwrap();
    let g = p.create_file(&d, &name).unwrap();
    let mut q = p.queue(QueueConfig::default()).unwrap();
    // A two-block write at 0, one at 3 blocks.
    let mut b = pool.take(8192).unwrap();
    b.as_mut_slice().fill(1);
    q.submit(
        IoOp::Write {
            file: &g,
            offset: 0,
            buf: b,
            dsync: false,
        },
        0,
    )
    .unwrap();
    write(&mut q, &g, &pool, 3 * 4096, 2, false);
    drain(&mut q);
    let base = p.with_world(|w| w.clone());
    let unwritten = |fates: Vec<store_io_sim::Fate>| {
        p.with_world(|w| {
            *w = base.clone();
            w.settle_metadata();
            w.crash(&CrashMode::Exact(fates));
        });
        let d = p.open_dir(&dir(), false).unwrap();
        let g = p.open_file(&d, &name, FileMode::ReadWrite).unwrap();
        (0..4u64)
            .map(|i| p.range_state(&g, i * 4096, 4096).unwrap().unwritten == Tri::Yes)
            .collect::<Vec<bool>>()
    };
    // The first lost, the second kept: below the kept one, nothing written.
    assert_eq!(unwritten(vec![Lost, Kept]), [true, true, true, false]);
    // The first torn after a block: its lost block, and the gap, unwritten.
    assert_eq!(unwritten(vec![Torn(1), Kept]), [false, true, true, false]);
    assert_eq!(unwritten(vec![Kept, Kept]), [false, false, true, false]);
}

#[test]
fn test_a_killed_process_leaves_its_cache_for_the_next_flush() {
    let (p, d, f, pool) = setup(SimConfig::volatile(15));
    let lock = p.lock_exclusive(&f).unwrap();
    let mut q = p.queue(QueueConfig::default()).unwrap();
    write(&mut q, &f, &pool, 0, 4, false);
    drain(&mut q);
    // Frozen by a power cut (its flush refused), then killed, the power
    // back without a crash.
    p.with_world(store_io_sim::World::cut_power);
    assert!(p.flush_all(&f).is_err());
    p.with_world(|w| {
        w.kill();
        w.restore_power();
    });
    assert!(!p.with_world(|w| w.powered_off()));
    // Its handle is dead, its lock released, its write still cached.
    assert!(p.flush_all(&f).is_err());
    drop(lock);
    let f = p
        .open_file(&d, &FileName::new("f").unwrap(), FileMode::ReadWrite)
        .unwrap();
    let _lock = p.lock_exclusive(&f).unwrap();
    assert_eq!(p.with_world(|w| w.cached_extents().len()), 1);
    // Anyone's flush makes it durable.
    p.flush_all(&f).unwrap();
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    assert_eq!(media(&p)[0], 4);
}
