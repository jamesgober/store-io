//! `SimPlatform::fork`: a deep copy of a world, independent of it in both
//! directions, refused while a handle of the world is open.

#![allow(clippy::unwrap_used, clippy::expect_used, unused_results)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use store_io_buf::{BufPool, PoolConfig};
use store_io_platform::{CompletionBuf, FileName, IoOp, Platform, Queue, QueueConfig};
use store_io_sim::{CrashMode, SimConfig, SimPlatform};

const B: usize = 4096;

/// Writes `byte` over the first block of `/d/name` (creating it) and makes
/// it durable; every handle dropped before it returns.
fn put(p: &SimPlatform, name: &str, byte: u8) {
    let dir = p.open_dir(Path::new("/d"), true).unwrap();
    let n = FileName::new(name).unwrap();
    let file = match p.create_file(&dir, &n) {
        Ok(f) => f,
        Err(_) => p
            .open_file(&dir, &n, store_io_platform::FileMode::ReadWrite)
            .unwrap(),
    };
    p.allocate(&file, B as u64).unwrap();
    let pool = BufPool::new(&PoolConfig::uniform(B, B, B, 2)).unwrap();
    let mut q = p.queue(QueueConfig::default()).unwrap();
    let mut buf = pool.take(B).unwrap();
    buf.as_mut_slice().fill(byte);
    q.submit(
        IoOp::Write {
            file: &file,
            offset: 0,
            buf,
            dsync: false,
        },
        1,
    )
    .unwrap();
    let mut out = CompletionBuf::with_capacity(1);
    q.wait(1, &mut out).unwrap();
    p.flush_all(&file).unwrap();
    p.sync_dir(&dir).unwrap();
}

/// The first byte of `/d/name`'s durable bytes, if it is there.
fn first(p: &SimPlatform, name: &str) -> Option<u8> {
    p.with_world(|w| {
        w.media_of(&PathBuf::from("/d"), name)
            .and_then(|m| m.first().copied())
    })
}

/// A fork holds what the world held; a write to either afterwards, a
/// crash of either, its clock and its counters, are its own.
#[test]
fn test_a_fork_is_independent_of_its_world_both_ways() {
    let p = SimPlatform::new(SimConfig::volatile(7));
    put(&p, "a", 1);
    let f = p.fork();
    assert_eq!(first(&f, "a"), Some(1), "the fork holds the world's bytes");
    // The world changes: the fork does not.
    put(&p, "a", 2);
    put(&p, "b", 3);
    assert_eq!(first(&f, "a"), Some(1));
    assert_eq!(first(&f, "b"), None);
    // The fork changes: the world does not.
    put(&f, "c", 4);
    assert_eq!(first(&p, "c"), None);
    assert_eq!(first(&p, "a"), Some(2));
    // Its clock, counters and crash are its own.
    let writes = p.with_world(|w| w.writes());
    f.with_world(|w| {
        w.advance_clock_ns(5_000);
        w.crash(&CrashMode::LoseAll);
    });
    assert_eq!(p.with_world(|w| w.writes()), writes);
    assert_ne!(
        p.with_world(|w| w.clock_ns()),
        f.with_world(|w| w.clock_ns())
    );
    assert_eq!(
        first(&p, "a"),
        Some(2),
        "a crash of the fork reached the world"
    );
    // A fresh identity: its volumes are not the world's.
    assert_ne!(p.identity_scope(), f.identity_scope());
}

/// A fork goes on as its world would: the same calls from the same state
/// give the same trace.
#[test]
fn test_a_fork_goes_on_as_its_world_would() {
    let p = SimPlatform::new(SimConfig {
        reorder: true,
        ..SimConfig::volatile(8)
    });
    put(&p, "a", 1);
    let f = p.fork();
    put(&p, "b", 2);
    put(&f, "b", 2);
    p.with_world(|w| w.crash(&CrashMode::Random));
    f.with_world(|w| w.crash(&CrashMode::Random));
    assert_eq!(
        p.with_world(|w| w.trace_hash()),
        f.with_world(|w| w.trace_hash())
    );
    assert_eq!(first(&p, "b"), first(&f, "b"));
}

/// No probe of the world is the fork's (a probe's closure acts on the
/// world it was made for): its submit, step, lock and release probes each
/// run on the world, never on the fork.
#[test]
fn test_a_fork_has_no_probe() {
    let p = SimPlatform::new(SimConfig::volatile(9));
    put(&p, "a", 1);
    let fired: Vec<Arc<AtomicU64>> = (0..4).map(|_| Arc::new(AtomicU64::new(0))).collect();
    let counter = |i: usize| {
        let n = Arc::clone(&fired[i]);
        move || {
            n.fetch_add(1, Ordering::Relaxed);
        }
    };
    let (submit, step, lock, release) = (counter(0), counter(1), counter(2), counter(3));
    p.with_world(|w| {
        w.set_submit_probe(Some(Arc::new(submit)));
        w.set_step_probe(Some(Arc::new(move || {
            step();
            false
        })));
        w.set_lock_probe(Some(Arc::new(lock)));
        w.set_release_probe(Some(Arc::new(release)));
    });
    // Each probe's operation: a write (submit, step), a lock, a release.
    let every = |p: &SimPlatform| {
        put(p, "a", 2);
        let dir = p.open_dir(Path::new("/d"), false).unwrap();
        let file = p
            .open_file(
                &dir,
                &FileName::new("a").unwrap(),
                store_io_platform::FileMode::ReadWrite,
            )
            .unwrap();
        drop(p.lock_exclusive(&file).unwrap());
        let _r = p.release_range(
            &file,
            0,
            B as u64,
            store_io_platform::ReleaseHow::Deallocate,
        );
    };
    let f = p.fork();
    every(&f);
    for (i, n) in fired.iter().enumerate() {
        assert_eq!(
            n.load(Ordering::Relaxed),
            0,
            "the world's probe {i} ran on the fork"
        );
    }
    every(&p);
    for (i, n) in fired.iter().enumerate() {
        assert!(
            n.load(Ordering::Relaxed) > 0,
            "probe {i} never ran on its world"
        );
    }
}

/// A world with a handle open (a directory, a file, a lock, a queue) is
/// not forked; once every handle is dropped it is. A fork's own handles
/// never count against its world.
#[test]
fn test_a_fork_is_refused_while_a_handle_is_open() {
    let p = SimPlatform::new(SimConfig::volatile(10));
    put(&p, "a", 1);
    let refused = |p: &SimPlatform| {
        let q = p.clone();
        std::panic::catch_unwind(move || drop(q.fork())).is_err()
    };
    let dir = p.open_dir(Path::new("/d"), false).unwrap();
    assert!(refused(&p), "forked with a directory open");
    let file = p
        .open_file(
            &dir,
            &FileName::new("a").unwrap(),
            store_io_platform::FileMode::ReadWrite,
        )
        .unwrap();
    let held = p.lock_exclusive(&file).unwrap();
    drop(dir);
    assert!(refused(&p), "forked with a file and a lock open");
    drop(held);
    assert!(refused(&p), "forked with a file open");
    drop(file);
    let q = p.queue(QueueConfig::default()).unwrap();
    assert!(refused(&p), "forked with a queue open");
    drop(q);
    // A handle's clone is a handle of its own, counted until it drops.
    let dir = p.open_dir(Path::new("/d"), false).unwrap();
    let again = dir.clone();
    drop(dir);
    assert!(refused(&p), "forked with a directory's clone open");
    drop(again);
    let f = p.fork();
    let dir = f.open_dir(Path::new("/d"), false).unwrap();
    // The fork's handle is the fork's.
    drop(p.fork());
    assert!(refused(&f));
    drop(dir);
    drop(f.fork());
}
