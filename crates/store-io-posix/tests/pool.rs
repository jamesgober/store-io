//! The submit pool (D-096) on a real Linux filesystem: a fresh directory
//! under `$HOME` (never `/mnt/c`, which is 9p).
//!
//! One `#[test]` runs every step in order, because one step counts the
//! process's pool worker threads and other tests' pools would disturb it.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used, unused_results)]

use std::path::PathBuf;

use store_io_buf::{BufPool, PoolConfig};
use store_io_platform::{CompletionBuf, FileMode, FileName, IoOp, Platform, Queue, QueueConfig};
use store_io_posix::{PosixDir, PosixFile, PosixPlatform, PosixQueue};

const MIB: usize = 1 << 20;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let base = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|h| h.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        let d = base.join(format!(".store-io-pool-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _cleanup = std::fs::remove_dir_all(&self.0);
    }
}

fn name(s: &str) -> FileName {
    FileName::new(s).unwrap()
}

fn parallel(depth: u32, threads: u32) -> QueueConfig {
    QueueConfig {
        depth,
        parallel: true,
        pool_threads: threads,
        ..QueueConfig::default()
    }
}

/// Live threads in this process named like a pool worker.
fn pool_workers() -> usize {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|t| std::fs::read_to_string(t.ok()?.path().join("comm")).ok())
        .filter(|comm| comm.starts_with("sio-t3-"))
        .count()
}

/// The blocked-signal masks (`SigBlk`, hex) of the pool's worker threads.
fn worker_sigblk() -> Vec<u64> {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|t| {
            let dir = t.ok()?.path();
            let comm = std::fs::read_to_string(dir.join("comm")).ok()?;
            if !comm.starts_with("sio-t3-") {
                return None;
            }
            let status = std::fs::read_to_string(dir.join("status")).ok()?;
            let line = status.lines().find(|l| l.starts_with("SigBlk:"))?;
            u64::from_str_radix(line["SigBlk:".len()..].trim(), 16).ok()
        })
        .collect()
}

/// The calling thread's blocked-signal mask.
fn own_sigblk() -> u64 {
    let status = std::fs::read_to_string("/proc/thread-self/status").unwrap();
    let line = status.lines().find(|l| l.starts_with("SigBlk:")).unwrap();
    u64::from_str_radix(line["SigBlk:".len()..].trim(), 16).unwrap()
}

fn sigset_of(sigs: &[libc::c_int]) -> libc::sigset_t {
    // SAFETY: test-only; sigemptyset and sigaddset initialise and update the
    // set through a valid, exclusively borrowed pointer.
    unsafe {
        let mut set = std::mem::zeroed::<libc::sigset_t>();
        libc::sigemptyset(&mut set);
        for &sig in sigs {
            libc::sigaddset(&mut set, sig);
        }
        set
    }
}

fn sigmask(how: libc::c_int, set: &libc::sigset_t) {
    // SAFETY: test-only; pthread_sigmask reads the set and writes nothing.
    let rc = unsafe { libc::pthread_sigmask(how, set, std::ptr::null_mut()) };
    assert_eq!(rc, 0);
}

/// Pool workers once the count settles at `want`: a new thread names
/// itself a moment after it starts, and a joined one may stay listed for a
/// moment after it exits. Gives up after 5 s and returns the last count.
fn workers_settle(want: usize) -> usize {
    let mut n = pool_workers();
    for _ in 0..500 {
        if n == want {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        n = pool_workers();
    }
    n
}

fn byte_of(run: usize) -> u8 {
    0x11u8
        .wrapping_mul(u8::try_from(run % 251).unwrap())
        .wrapping_add(1)
}

/// Submits `runs` writes of `len` bytes, run `i` filled with `byte_of(i)` at
/// offset `i * len`.
fn submit_runs(q: &mut PosixQueue, f: &PosixFile, pool: &BufPool, runs: usize, len: usize) {
    for i in 0..runs {
        let mut b = pool.take(len).unwrap();
        b.as_mut_slice().fill(byte_of(i));
        q.submit(
            IoOp::Write {
                file: f,
                offset: (i * len) as u64,
                buf: b,
                dsync: false,
            },
            i as u64,
        )
        .unwrap();
    }
}

/// Reads the file back with an inline queue and checks every run's bytes.
fn verify_runs(
    p: &PosixPlatform,
    dir: &PosixDir,
    file: &str,
    pool: &BufPool,
    runs: usize,
    len: usize,
) {
    let f = p.open_file(dir, &name(file), FileMode::ReadOnly).unwrap();
    let mut q = p.queue(QueueConfig::default()).unwrap();
    let mut out = CompletionBuf::with_capacity(1);
    for i in 0..runs {
        let b = pool.take(len).unwrap();
        q.submit(
            IoOp::Read {
                file: &f,
                offset: (i * len) as u64,
                buf: b,
            },
            0,
        )
        .unwrap();
        assert_eq!(q.wait(1, &mut out).unwrap(), 1);
        let c = out.drain().next().unwrap();
        assert_eq!(c.result, Ok(len), "read of run {i}");
        let want = byte_of(i);
        assert!(
            c.buf.unwrap().as_slice().iter().all(|&x| x == want),
            "run {i} holds other bytes"
        );
    }
}

#[test]
fn test_submit_pool() {
    let t = TempDir::new();
    let pool = BufPool::new(&PoolConfig::uniform(4096, 4096, MIB, 64)).unwrap();
    assert_eq!(pool_workers(), 0, "no pool runs before a parallel queue");

    let p = PosixPlatform::new();
    let dir = p.open_dir(&t.0, false).unwrap();
    assert_eq!(p.pool_threads(), None);

    // An inline queue never starts the pool.
    let inline = p.queue(QueueConfig::default()).unwrap();
    assert_eq!(inline.workers(), None);
    assert_eq!(p.pool_threads(), None);

    // The first parallel queue starts it at its size; a later, larger
    // request does not resize it. Starting it leaves the calling thread's
    // signal mask exactly as it was: a signal it blocked stays blocked, and
    // none other becomes blocked. The caller blocks a forced signal too
    // (SIGBUS): the workers must still leave it open (checked below).
    let held = sigset_of(&[libc::SIGUSR2, libc::SIGBUS]);
    sigmask(libc::SIG_BLOCK, &held);
    let before = own_sigblk();
    assert_ne!(before & (1 << (libc::SIGUSR2 - 1)), 0);
    assert_ne!(before & (1 << (libc::SIGBUS - 1)), 0);
    let mut q = p.queue(parallel(32, 3)).unwrap();
    assert_eq!(own_sigblk(), before, "the caller's signal mask changed");
    sigmask(libc::SIG_UNBLOCK, &held);
    assert_eq!(p.pool_threads(), Some(3));
    assert_eq!(q.workers(), Some(3));
    let q2 = p.queue(parallel(8, 16)).unwrap();
    assert_eq!(q2.workers(), Some(3));
    assert_eq!(workers_settle(3), 3);
    // Workers block signals: a process's handlers never run on them
    // (job-control, termination, user and real-time signals among them).
    let bit = |s: libc::c_int| 1u64 << (s - 1);
    let wanted = [
        libc::SIGHUP,
        libc::SIGINT,
        libc::SIGQUIT,
        libc::SIGTERM,
        libc::SIGUSR1,
        libc::SIGUSR2,
        libc::SIGPIPE,
        libc::SIGALRM,
        libc::SIGCHLD,
        libc::SIGRTMIN(),
        libc::SIGRTMAX(),
    ]
    .into_iter()
    .fold(0u64, |acc, s| acc | bit(s));
    // A worker opens the forced signals as its first statement, just after
    // it has named itself: poll briefly until all three have.
    let forced = [
        libc::SIGSEGV,
        libc::SIGBUS,
        libc::SIGFPE,
        libc::SIGILL,
        libc::SIGTRAP,
        libc::SIGSYS,
    ]
    .into_iter()
    .fold(0u64, |acc, s| acc | bit(s));
    let mut masks = worker_sigblk();
    for _ in 0..500 {
        if masks.len() == 3 && masks.iter().all(|m| m & forced == 0) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        masks = worker_sigblk();
    }
    assert_eq!(masks.len(), 3);
    assert!(masks.iter().all(|m| m & wanted == wanted), "{masks:x?}");
    // ...but not the signals the kernel forces on the faulting thread:
    // blocked, a stack overflow on a worker would kill without a report.
    assert!(masks.iter().all(|m| m & forced == 0), "{masks:x?}");
    drop(q2);

    // A batch of N runs completes N completions, each once, with its
    // buffer, and the bytes land where they were sent.
    let f = p.create_file(&dir, &name("batch")).unwrap();
    p.allocate(&f, (24 * 64 * 1024) as u64).unwrap();
    submit_runs(&mut q, &f, &pool, 24, 64 * 1024);
    assert_eq!(q.in_flight(), 24);
    let mut out = CompletionBuf::with_capacity(32);
    let mut seen = [false; 24];
    while seen.iter().any(|s| !s) {
        q.wait(1, &mut out).unwrap();
        for c in out.drain() {
            let i = usize::try_from(c.tag).unwrap();
            assert!(!seen[i], "run {i} completed twice");
            seen[i] = true;
            assert_eq!(c.result, Ok(64 * 1024), "run {i}");
            assert_eq!(c.buf.map(|b| b.len()), Some(64 * 1024));
        }
    }
    assert_eq!(q.in_flight(), 0);
    // A wait asking for more than is in flight returns at once.
    assert_eq!(q.wait(5, &mut out).unwrap(), 0);
    drop(f);
    verify_runs(&p, &dir, "batch", &pool, 24, 64 * 1024);

    // A file dropped while its jobs run stays open for them: the writes
    // land in it, and a file opened right after the drop (which would get a
    // closed descriptor's number) is untouched.
    let f = p.create_file(&dir, &name("dropped")).unwrap();
    p.allocate(&f, (16 * MIB) as u64).unwrap();
    submit_runs(&mut q, &f, &pool, 16, MIB);
    drop(f);
    let other = p.create_file(&dir, &name("other")).unwrap();
    p.allocate(&other, (16 * MIB) as u64).unwrap();
    let mut out = CompletionBuf::with_capacity(16);
    while q.in_flight() > 0 {
        q.wait(q.in_flight(), &mut out).unwrap();
        for c in out.drain() {
            assert_eq!(c.result, Ok(MIB), "run {}", c.tag);
        }
    }
    verify_runs(&p, &dir, "dropped", &pool, 16, MIB);
    drop(other);
    let other = std::fs::read(t.0.join("other")).unwrap();
    assert!(
        other.iter().all(|&b| b == 0),
        "a write reached another file"
    );

    // A full pooled queue refuses before any I/O, with the buffer and no
    // OS error, while its depth is in flight.
    let f = p.create_file(&dir, &name("full")).unwrap();
    p.allocate(&f, (4 * MIB) as u64).unwrap();
    let mut small = p.queue(parallel(2, 0)).unwrap();
    submit_runs(&mut small, &f, &pool, 2, MIB);
    let rej = small
        .submit(
            IoOp::Write {
                file: &f,
                offset: (2 * MIB) as u64,
                buf: pool.take(MIB).unwrap(),
                dsync: false,
            },
            2,
        )
        .unwrap_err();
    assert!(rej.raw.is_none());
    assert_eq!(rej.buf.map(|b| b.len()), Some(MIB));
    let mut out = CompletionBuf::with_capacity(2);
    assert_eq!(small.wait(2, &mut out).unwrap(), 2);
    assert_eq!(small.in_flight(), 0);
    drop(small);
    drop(f);

    // A queue dropped with jobs in flight waits for them: every buffer is
    // back in the pool when the drop returns.
    let f = p.create_file(&dir, &name("drop")).unwrap();
    p.allocate(&f, (16 * MIB) as u64).unwrap();
    let free = |pool: &BufPool| pool.occupancy().iter().map(|&(_, n, _)| n).sum::<usize>();
    let before = free(&pool);
    submit_runs(&mut q, &f, &pool, 16, MIB);
    drop(q);
    assert_eq!(free(&pool), before, "buffers kept after the queue's drop");
    drop(f);
    verify_runs(&p, &dir, "drop", &pool, 16, MIB);

    // The workers stop once the platform and every queue are gone.
    drop(inline);
    drop(dir);
    assert_eq!(pool_workers(), 3);
    drop(p);
    assert_eq!(workers_settle(0), 0, "pool workers outlived the platform");

    // The pool is bounded whatever is asked.
    let p = PosixPlatform::new();
    let q = p.queue(parallel(1, u32::MAX)).unwrap();
    // 256 unless the host's thread limit refuses some (the pool then runs
    // with those it has).
    let n = q.workers().unwrap();
    assert!((1..=256).contains(&n), "{n} workers");
    drop(q);
    drop(p);
    assert_eq!(workers_settle(0), 0);
}
