//! The io_uring tier's queue (D-103 3b) against a real Linux filesystem: a
//! fresh directory under `$HOME` (ext4 on the development WSL2 box) for every
//! test, removed afterwards. The conformance suite runs on this tier too
//! (`tests/conformance.rs` at the workspace root); these tests cover what it
//! cannot see: the NOWAIT fallback, short reads, reaping without waiting on a
//! thread-owned ring, drops in flight and the owner thread.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used, unused_results)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use store_io_buf::{BufPool, IoBuf, PoolConfig};
use store_io_platform::{
    Completion, CompletionBuf, FileMode, FileName, IoOp, Owner, Platform, Queue, QueueConfig,
};
use store_io_posix::{LinuxTier, PosixFile, PosixPlatform, PosixQueue};

const BLOCK: usize = 4096;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let base = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|h| h.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        let d = base.join(format!(
            ".store-io-uring-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _cleanup = std::fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    p: PosixPlatform,
    file: PosixFile,
    pool: BufPool,
    d: store_io_posix::PosixDir,
    _dir: TempDir,
}

/// A uring platform, an empty file and a pool of `count` blocks; `None`
/// (the test is skipped) where the kernel refuses io_uring, unless
/// `STORE_IO_EXPECT_URING` says it works there.
fn fixture(count: u32) -> Option<Fixture> {
    let p = match PosixPlatform::with_tier(LinuxTier::Uring) {
        Ok(p) => p,
        Err(e) => {
            assert!(
                std::env::var_os("STORE_IO_EXPECT_URING").is_none(),
                "io_uring refused ({e}) where STORE_IO_EXPECT_URING says it works"
            );
            return None;
        }
    };
    let dir = TempDir::new();
    let d = p.open_dir(dir.path(), true).unwrap();
    let file = p.create_file(&d, &FileName::new("data").unwrap()).unwrap();
    let pool = BufPool::new(&PoolConfig::uniform(BLOCK, BLOCK, 2 * BLOCK, count)).unwrap();
    Some(Fixture {
        p,
        file,
        pool,
        d,
        _dir: dir,
    })
}

fn filled(pool: &BufPool, len: usize, byte: u8) -> IoBuf {
    let mut b = pool.take(len).unwrap();
    b.as_mut_slice().fill(byte);
    b
}

/// A ring queue of either flavour: `parallel`, so a shared queue gets a ring
/// too (a lone-operation queue is synchronous on either tier, D-104).
fn queue(p: &PosixPlatform, owner: Owner, depth: u32) -> PosixQueue {
    p.queue(QueueConfig {
        depth,
        owner,
        parallel: true,
        ..QueueConfig::default()
    })
    .unwrap()
}

/// Waits for exactly one completion.
fn one(q: &mut PosixQueue) -> Completion {
    let mut out = CompletionBuf::with_capacity(1);
    while out.is_empty() {
        q.wait(1, &mut out).unwrap();
    }
    out.drain().next().unwrap()
}

/// A write that extends the file is refused by NOWAIT (it needs block
/// allocation) and finished by one blocking call: the data is there, and
/// the fallback is counted.
#[test]
fn test_a_write_nowait_refuses_is_finished_by_a_blocking_call() {
    for owner in [Owner::Shared, Owner::Thread] {
        let Some(f) = fixture(4) else { return };
        let mut q = queue(&f.p, owner, 8);
        let buf = filled(&f.pool, BLOCK, 0x5A);
        q.submit(
            IoOp::Write {
                file: &f.file,
                offset: 0,
                buf,
                dsync: true,
            },
            1,
        )
        .unwrap();
        let c = one(&mut q);
        assert_eq!((c.tag, c.result), (1, Ok(BLOCK)), "{owner:?}");
        assert_eq!(q.fallbacks(), 1, "{owner:?}");
        q.submit(
            IoOp::Read {
                file: &f.file,
                offset: 0,
                buf: f.pool.take(BLOCK).unwrap(),
            },
            2,
        )
        .unwrap();
        let c = one(&mut q);
        assert_eq!(c.result, Ok(BLOCK));
        assert!(c.buf.unwrap().as_slice().iter().all(|&b| b == 0x5A));
    }
}

/// A read that reaches the end of the file returns what is there; a read
/// past it returns 0 bytes. Neither is an error.
#[test]
fn test_reads_at_the_end_of_the_file_are_short() {
    let Some(f) = fixture(4) else { return };
    f.p.allocate(&f.file, BLOCK as u64).unwrap();
    let mut q = queue(&f.p, Owner::Shared, 8);
    let read = |q: &mut PosixQueue, offset| {
        q.submit(
            IoOp::Read {
                file: &f.file,
                offset,
                buf: f.pool.take(2 * BLOCK).unwrap(),
            },
            offset,
        )
        .unwrap();
        one(q).result
    };
    assert_eq!(read(&mut q, 0), Ok(BLOCK));
    assert_eq!(read(&mut q, 2 * BLOCK as u64), Ok(0));
}

/// On a thread-owned ring the kernel posts completions only when the
/// thread enters it: `reap` alone, never waiting, collects them (E12).
#[test]
fn test_reap_alone_collects_on_an_owned_ring() {
    let Some(f) = fixture(8) else { return };
    f.p.allocate(&f.file, 8 * BLOCK as u64).unwrap();
    let mut q = queue(&f.p, Owner::Thread, 8);
    for i in 0..4u64 {
        let buf = filled(&f.pool, BLOCK, i as u8);
        q.submit(
            IoOp::Write {
                file: &f.file,
                offset: i * BLOCK as u64,
                buf,
                dsync: false,
            },
            i,
        )
        .unwrap();
    }
    let mut out = CompletionBuf::with_capacity(8);
    let mut tags = Vec::new();
    for _ in 0..10_000_000u32 {
        q.reap(&mut out);
        tags.extend(out.drain().map(|c| {
            assert_eq!(c.result, Ok(BLOCK));
            c.tag
        }));
        if tags.len() == 4 {
            break;
        }
        std::thread::yield_now();
    }
    tags.sort_unstable();
    assert_eq!(tags, [0, 1, 2, 3]);
    assert_eq!(q.in_flight(), 0);
}

/// A queue dropped with operations in flight waits for them: every buffer
/// is back in its pool when the drop returns. On an owned ring nothing is
/// even posted before the drop enters the kernel, and its wake read is
/// still armed from the first wait (a shared ring completes it before the
/// wait returns).
#[test]
fn test_a_drop_in_flight_returns_every_buffer() {
    for owner in [Owner::Shared, Owner::Thread] {
        let Some(f) = fixture(17) else { return };
        f.p.allocate(&f.file, 17 * BLOCK as u64).unwrap();
        // How many blocks the pool holds: all must come back.
        let capacity = {
            let all: Vec<IoBuf> = std::iter::from_fn(|| f.pool.take(BLOCK).ok()).collect();
            all.len()
        };
        let mut q = queue(&f.p, owner, 17);
        let w = q.waker();
        {
            let buf = filled(&f.pool, BLOCK, 7);
            q.submit(
                IoOp::Write {
                    file: &f.file,
                    offset: 16 * BLOCK as u64,
                    buf,
                    dsync: true,
                },
                16,
            )
            .unwrap();
            let c = one(&mut q);
            assert_eq!(c.result, Ok(BLOCK));
        }
        for i in 0..16u64 {
            let buf = filled(&f.pool, BLOCK, 7);
            q.submit(
                IoOp::Write {
                    file: &f.file,
                    offset: i * BLOCK as u64,
                    buf,
                    dsync: true,
                },
                i,
            )
            .unwrap();
        }
        assert_eq!(q.in_flight(), 16, "{owner:?}");
        drop(w);
        drop(q);
        let all: Vec<IoBuf> = std::iter::from_fn(|| f.pool.take(BLOCK).ok()).collect();
        assert_eq!(all.len(), capacity, "{owner:?}");
    }
}

/// Completions observed but not yet reaped still count against the depth.
#[test]
fn test_unreaped_completions_count_against_the_depth() {
    let Some(f) = fixture(4) else { return };
    f.p.allocate(&f.file, 4 * BLOCK as u64).unwrap();
    let mut q = queue(&f.p, Owner::Shared, 2);
    for i in 0..2u64 {
        q.submit(
            IoOp::Read {
                file: &f.file,
                offset: i * BLOCK as u64,
                buf: f.pool.take(BLOCK).unwrap(),
            },
            i,
        )
        .unwrap();
    }
    // Collect both into the queue without taking them (no room in `out`).
    std::thread::sleep(std::time::Duration::from_millis(50));
    let mut none = CompletionBuf::with_capacity(0);
    q.wait(1, &mut none).unwrap();
    q.reap(&mut none);
    assert_eq!(q.in_flight(), 2);
    let full = q
        .submit(
            IoOp::Read {
                file: &f.file,
                offset: 0,
                buf: f.pool.take(BLOCK).unwrap(),
            },
            2,
        )
        .unwrap_err();
    assert!(full.buf.is_some() && full.raw.is_none());
    let mut out = CompletionBuf::with_capacity(2);
    assert_eq!(q.reap(&mut out), 2);
}

/// A thread-owned queue refuses use from another thread before anything
/// reaches the kernel, and hands the buffer back.
#[test]
fn test_an_owned_queue_refuses_other_threads() {
    let Some(f) = fixture(2) else { return };
    f.p.allocate(&f.file, BLOCK as u64).unwrap();
    let q = queue(&f.p, Owner::Thread, 4);
    let buf = filled(&f.pool, BLOCK, 1);
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut q = q;
            let r = q.submit(
                IoOp::Write {
                    file: &f.file,
                    offset: 0,
                    buf,
                    dsync: false,
                },
                1,
            );
            let rejected = r.unwrap_err();
            assert_eq!(rejected.raw.map(|e| e.code), Some(libc::EXDEV));
            assert!(rejected.buf.is_some());
            assert_eq!(q.in_flight(), 0);
            // Nothing in flight: the drop on this thread has nothing to wait
            // for (a wait would be refused here too).
        });
    });
}

/// A full queue refuses with the buffer and no OS error; a write on a
/// read-only handle is refused before it reaches the kernel.
#[test]
fn test_refusals_return_the_buffer() {
    let Some(f) = fixture(4) else { return };
    f.p.allocate(&f.file, 4 * BLOCK as u64).unwrap();
    let mut q = queue(&f.p, Owner::Shared, 2);
    for i in 0..2u64 {
        q.submit(
            IoOp::Read {
                file: &f.file,
                offset: i * BLOCK as u64,
                buf: f.pool.take(BLOCK).unwrap(),
            },
            i,
        )
        .unwrap();
    }
    let full = q
        .submit(
            IoOp::Read {
                file: &f.file,
                offset: 0,
                buf: f.pool.take(BLOCK).unwrap(),
            },
            2,
        )
        .unwrap_err();
    assert!(full.buf.is_some() && full.raw.is_none());
    let mut out = CompletionBuf::with_capacity(2);
    while out.len() < 2 {
        q.wait(2, &mut out).unwrap();
    }
    drop(out);
    let ro =
        f.p.open_file(&f.d, &FileName::new("data").unwrap(), FileMode::ReadOnly)
            .unwrap();
    let refused = q
        .submit(
            IoOp::Write {
                file: &ro,
                offset: 0,
                buf: filled(&f.pool, BLOCK, 3),
                dsync: false,
            },
            3,
        )
        .unwrap_err();
    assert_eq!(refused.raw.map(|e| e.code), Some(libc::EBADF));
    assert!(refused.buf.is_some());
    assert_eq!(q.in_flight(), 0);
}

/// Which queues get a ring on the io_uring tier (D-104): one that keeps
/// several operations in flight (`parallel`, or owned by one thread) does;
/// a queue for one operation at a time performs it inline.
#[test]
fn test_only_queues_with_several_operations_in_flight_get_a_ring() {
    let Some(f) = fixture(1) else { return };
    let kind = |cfg: QueueConfig| format!("{:?}", f.p.queue(cfg).unwrap().waker());
    let lone = QueueConfig::default();
    assert!(kind(lone).contains("inline"), "{}", kind(lone));
    for cfg in [
        QueueConfig {
            parallel: true,
            ..QueueConfig::default()
        },
        QueueConfig {
            owner: Owner::Thread,
            ..QueueConfig::default()
        },
    ] {
        assert!(kind(cfg).contains("ring"), "{cfg:?}: {}", kind(cfg));
    }
}

/// A thread-owned ring with the file registered (D-078): reads and writes
/// go through the direct descriptor, and the bytes are exactly where they
/// were aimed. (No buffer is ever registered: D-107; a read into one never
/// completes on at least Linux 6.5.0 and 6.6.0.)
#[test]
fn test_a_registered_ring_uses_the_direct_descriptor() {
    let Some(f) = fixture(4) else { return };
    f.p.allocate(&f.file, 4 * BLOCK as u64).unwrap();
    let cfg = QueueConfig {
        depth: 8,
        owner: Owner::Thread,
        ..QueueConfig::default()
    };
    let mut q = f.p.queue_registered(cfg, &f.file).unwrap();
    let got = q.registered().unwrap();
    assert!(got.file, "the file was not registered: {got:?}");
    let buf = filled(&f.pool, BLOCK, 0x3C);
    q.submit(
        IoOp::Write {
            file: &f.file,
            offset: BLOCK as u64,
            buf,
            dsync: false,
        },
        1,
    )
    .unwrap();
    assert_eq!(one(&mut q).result, Ok(BLOCK));
    q.submit(
        IoOp::Read {
            file: &f.file,
            offset: BLOCK as u64,
            buf: f.pool.take(BLOCK).unwrap(),
        },
        2,
    )
    .unwrap();
    let c = one(&mut q);
    assert_eq!(c.result, Ok(BLOCK));
    assert!(c.buf.unwrap().as_slice().iter().all(|&b| b == 0x3C));
    let got = q.registered().unwrap();
    assert_eq!(got.direct_ops, 2, "the direct descriptor was not used");
}

/// On a registered ring, an operation on another file takes the ordinary
/// path, and lands exactly where aimed: the direct descriptor is never used
/// for a file it does not name (D-078; on another file's write it would
/// write the registered file).
#[test]
fn test_a_registered_ring_uses_ordinary_io_for_what_it_did_not_register() {
    let Some(f) = fixture(4) else { return };
    f.p.allocate(&f.file, 2 * BLOCK as u64).unwrap();
    let other =
        f.p.create_file(&f.d, &FileName::new("other").unwrap())
            .unwrap();
    f.p.allocate(&other, 2 * BLOCK as u64).unwrap();
    let foreign = BufPool::new(&PoolConfig::uniform(BLOCK, BLOCK, 2 * BLOCK, 2)).unwrap();
    let cfg = QueueConfig {
        depth: 8,
        owner: Owner::Thread,
        ..QueueConfig::default()
    };
    let mut q = f.p.queue_registered(cfg, &f.file).unwrap();
    // One write to the registered file, one to the other file.
    q.submit(
        IoOp::Write {
            file: &f.file,
            offset: 0,
            buf: filled(&foreign, BLOCK, 0x11),
            dsync: false,
        },
        1,
    )
    .unwrap();
    assert_eq!(one(&mut q).result, Ok(BLOCK));
    q.submit(
        IoOp::Write {
            file: &other,
            offset: 0,
            buf: filled(&f.pool, BLOCK, 0x22),
            dsync: false,
        },
        2,
    )
    .unwrap();
    assert_eq!(one(&mut q).result, Ok(BLOCK));
    let got = q.registered().unwrap();
    assert_eq!(
        got.direct_ops, 1,
        "the direct descriptor served another file"
    );
    for (file, want) in [(&f.file, 0x11u8), (&other, 0x22u8)] {
        q.submit(
            IoOp::Read {
                file,
                offset: 0,
                buf: foreign.take(BLOCK).unwrap(),
            },
            3,
        )
        .unwrap();
        let c = one(&mut q);
        assert_eq!(c.result, Ok(BLOCK));
        assert!(
            c.buf.unwrap().as_slice().iter().all(|&b| b == want),
            "a write landed in the wrong file"
        );
    }
}

/// A shared ring registers nothing (R3.3: a registration would be charged
/// once per shared ring).
#[test]
fn test_a_shared_ring_registers_nothing() {
    let Some(f) = fixture(2) else { return };
    let cfg = QueueConfig {
        parallel: true,
        ..QueueConfig::default()
    };
    let q = f.p.queue_registered(cfg, &f.file).unwrap();
    let got = q.registered().unwrap();
    assert!(!got.file, "{got:?}");
}

/// On a registered ring, a read through the direct descriptor that runs past
/// the end of the file is finished by the ordinary blocking call on the file
/// itself (the NOWAIT fallback): the bytes before the end come back, exactly.
#[test]
fn test_a_registered_ring_finishes_a_read_past_the_end_by_the_fallback() {
    let Some(f) = fixture(4) else { return };
    f.p.allocate(&f.file, BLOCK as u64).unwrap();
    let cfg = QueueConfig {
        depth: 8,
        owner: Owner::Thread,
        ..QueueConfig::default()
    };
    let mut q = f.p.queue_registered(cfg, &f.file).unwrap();
    q.submit(
        IoOp::Write {
            file: &f.file,
            offset: 0,
            buf: filled(&f.pool, BLOCK, 0x5E),
            dsync: false,
        },
        1,
    )
    .unwrap();
    assert_eq!(one(&mut q).result, Ok(BLOCK));
    q.submit(
        IoOp::Read {
            file: &f.file,
            offset: 0,
            buf: f.pool.take(2 * BLOCK).unwrap(),
        },
        2,
    )
    .unwrap();
    let c = one(&mut q);
    assert_eq!(c.result, Ok(BLOCK), "the read past the end");
    assert!(
        c.buf.unwrap().as_slice()[..BLOCK]
            .iter()
            .all(|&b| b == 0x5E)
    );
}
