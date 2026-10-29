//! Fencing after a killed owner: Windows must not release the ownership
//! lock before the owner's in-flight writes have landed.
//!
//! The lock lives on the store's own handle, and a queue writes through its
//! own `ReOpenFile` handle, so the release of one does not by itself wait
//! for the other. Observed, not documented by Microsoft: the lock of a
//! killed process is not free until its in-flight writes have completed or
//! been cancelled. This test holds Windows to that: a child process takes
//! the lock and keeps unbuffered writes in flight over the whole file; it is
//! killed (`TerminateProcess`); the moment this process holds the lock it
//! writes its own byte over the file, durably, and reads the file back once
//! the child is gone. A block of the child's is a write of the dead owner
//! that landed after the new owner's.
//!
//! Measured on the dev box (Windows 11 build 26200, NTFS, a consumer NVMe
//! drive; `.dev/experiments/win-kill`): with the lock, no late block in 250
//! rounds with 512 MiB in flight (1,250 rounds in all), the wait for the
//! lock growing with what was in flight (0.4 ms for 256 KiB, 110-220 ms
//! medians for 512 MiB); writing at the kill without waiting for the lock,
//! late blocks in 7 of 10 rounds at 512 MiB in flight and in none at 32 MiB
//! or less. Hence this test's 512 MiB (64 writes of 8 MiB), over a file
//! written once beforehand: without the wait for the lock it failed 5 runs
//! of 5, each in its first round (over a fresh file, only in later rounds,
//! whose writes no longer extend the file's valid data). Each round prints
//! its wait for the lock, so a run shows whether writes were in flight. The
//! child issues odd rounds' writes from threads that exit while their
//! writes are in flight.
//!
//! Ignored by default (it writes about 3 GiB per round); CI runs it once on
//! Windows.

#![cfg(windows)]
// Test setup unwraps (a failure is the test failing).
#![allow(clippy::unwrap_used, clippy::expect_used, unused_results)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use store_io_buf::{BufPool, PoolConfig};
use store_io_platform::{
    CompletionBuf, FileMode, FileName, IoOp, Platform, Queue, QueueConfig, RawResult,
};
use store_io_win::{WinFile, WinPlatform, WinQueue};

const BLOCK: usize = 4096;
const WRITE: usize = 8 << 20;
const FILE: u64 = 1 << 30;
const DEPTH: u32 = 64;
const CHILD_BYTE: u8 = 0xA5;
const ERROR_LOCK_VIOLATION: i32 = 33;
const CHILD: &str = "STORE_IO_KILL_CHILD";
const THREADS: &str = "STORE_IO_KILL_THREADS";
const GO: &str = "store-io kill child: writing";

fn name() -> FileName {
    FileName::new("c.sio").unwrap()
}

/// Removes the test's directory, however the test ends. Declared before
/// the handles, so they close first.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _cleanup = std::fs::remove_dir_all(&self.0);
    }
}

/// Kills the child, however the test ends.
struct Kid(Child);

impl Drop for Kid {
    fn drop(&mut self) {
        let _killed = self.0.kill();
        let _status = self.0.wait();
    }
}

/// The child: take the lock, keep `DEPTH` writes in flight until killed
/// (or two minutes have passed, should the parent have died).
fn child(dir: &Path, threads: bool) {
    let p = WinPlatform::new();
    let d = p.open_dir(dir, false).unwrap();
    let f = p.open_file(&d, &name(), FileMode::ReadWrite).unwrap();
    let _held = p.lock_exclusive(&f).unwrap();
    let pool = BufPool::new(&PoolConfig::uniform(BLOCK, WRITE, WRITE, DEPTH)).unwrap();
    let mut q = p
        .queue(QueueConfig {
            depth: DEPTH,
            ..QueueConfig::default()
        })
        .unwrap();
    let slots = FILE / WRITE as u64;
    let mut next = 0u64;
    let mut submit = |q: &mut WinQueue, mut buf: store_io_buf::IoBuf| {
        buf.as_mut_slice().fill(CHILD_BYTE);
        let off = (next % slots) * WRITE as u64;
        next += 1;
        let op = IoOp::Write {
            file: &f,
            offset: off,
            buf,
            dsync: false,
        };
        if threads {
            // The issuing thread exits while its write is in flight.
            std::thread::scope(|s| s.spawn(|| q.submit(op, off).map_err(drop).unwrap()).join())
                .unwrap();
        } else {
            q.submit(op, off).map_err(drop).unwrap();
        }
    };
    for _ in 0..DEPTH {
        submit(&mut q, pool.take(WRITE).unwrap());
    }
    let mut out = CompletionBuf::with_capacity(DEPTH as usize);
    let (mut done, mut said) = (0u32, false);
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(120) {
        if !said && done >= DEPTH {
            // On its own line: the harness may have printed the test's name
            // without one.
            let _said = writeln!(std::io::stdout(), "\n{GO}");
            let _flushed = std::io::stdout().flush();
            said = true;
        }
        q.wait(1, &mut out).unwrap();
        for c in out.drain() {
            assert_eq!(c.result, Ok(WRITE));
            done += 1;
            submit(&mut q, c.buf.unwrap());
        }
    }
}

/// Writes `byte` over the whole file, durably, 8 writes deep.
fn write_all(q: &mut WinQueue, f: &WinFile, pool: &BufPool, byte: u8) {
    let mut out = CompletionBuf::with_capacity(8);
    let mut pending = 0u32;
    let mut reap = |q: &mut WinQueue, pending: &mut u32| {
        *pending -= u32::try_from(q.wait(1, &mut out).unwrap()).unwrap();
        for c in out.drain() {
            assert_eq!(c.result, Ok(WRITE));
        }
    };
    for s in 0..FILE / WRITE as u64 {
        if pending == 8 {
            reap(q, &mut pending);
        }
        let mut buf = pool.take(WRITE).unwrap();
        buf.as_mut_slice().fill(byte);
        let op = IoOp::Write {
            file: f,
            offset: s * WRITE as u64,
            buf,
            dsync: true,
        };
        q.submit(op, s).map_err(drop).unwrap();
        pending += 1;
    }
    while pending > 0 {
        reap(q, &mut pending);
    }
}

fn lock_violation<T>(r: &RawResult<T>) -> bool {
    r.as_ref().is_err_and(|e| e.code == ERROR_LOCK_VIOLATION)
}

#[test]
#[ignore = "writes about 3 GiB per round; CI runs it once on Windows"]
fn test_a_killed_owners_lock_waits_for_its_writes() {
    if let Some(path) = std::env::var_os(CHILD) {
        child(Path::new(&path), std::env::var_os(THREADS).is_some());
        return;
    }
    let path = std::env::temp_dir().join(format!("store-io-win-kill-{}", std::process::id()));
    let _stale = std::fs::remove_dir_all(&path);
    let _scratch = Scratch(path.clone());
    let p = WinPlatform::new();
    let d = p.open_dir(&path, true).unwrap();
    let f = p.create_file(&d, &name()).unwrap();
    p.allocate(&f, FILE).unwrap();
    let pool = BufPool::new(&PoolConfig::uniform(BLOCK, WRITE, WRITE, 8)).unwrap();
    let mut q = p
        .queue(QueueConfig {
            depth: 8,
            ..QueueConfig::default()
        })
        .unwrap();
    // Written once, so that no round's writes extend the valid data.
    write_all(&mut q, &f, &pool, 0);
    let mut out = CompletionBuf::with_capacity(1);
    let want_child = vec![CHILD_BYTE; BLOCK];
    for round in 0..4u8 {
        let mine = 0x10 + round;
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "test_a_killed_owners_lock_waits_for_its_writes",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, &path)
        .stdout(Stdio::piped());
        if round % 2 == 1 {
            cmd.env(THREADS, "1");
        }
        let mut kid = Kid(cmd.spawn().unwrap());
        let lines = BufReader::new(kid.0.stdout.take().unwrap());
        let writing = lines.lines().map_while(Result::ok).any(|l| l.contains(GO));
        assert!(writing, "round {round}: the child never started writing");
        let refused = p.lock_exclusive(&f);
        assert!(lock_violation(&refused), "round {round}: {refused:?}");
        assert!(
            kid.0.try_wait().unwrap().is_none(),
            "round {round}: the child ended before the kill"
        );
        let killed = Instant::now();
        kid.0.kill().unwrap();
        let lock = loop {
            let taken = p.lock_exclusive(&f);
            if !lock_violation(&taken) {
                break taken.unwrap();
            }
            assert!(
                killed.elapsed() < Duration::from_secs(120),
                "round {round}: the lock was never released"
            );
        };
        let waited = killed.elapsed();
        write_all(&mut q, &f, &pool, mine);
        // `Child::kill` is `TerminateProcess(_, 1)`; a child that failed
        // on its own (its writes cancelled as its queue dropped) exits 101.
        let status = kid.0.wait().unwrap();
        assert_eq!(
            status.code(),
            Some(1),
            "round {round}: the child ended other than by the kill: {status}"
        );
        let _said = writeln!(
            std::io::stderr(),
            "round {round}: the lock was free {waited:?} after the kill"
        );
        let want_mine = vec![mine; BLOCK];
        let mut late = 0usize;
        for s in 0..FILE / WRITE as u64 {
            let op = IoOp::Read {
                file: &f,
                offset: s * WRITE as u64,
                buf: pool.take(WRITE).unwrap(),
            };
            q.submit(op, s).map_err(drop).unwrap();
            q.wait(1, &mut out).unwrap();
            let c = out.drain().next().unwrap();
            assert_eq!(c.result, Ok(WRITE));
            let buf = c.buf.unwrap();
            for (i, b) in buf.as_slice().chunks(BLOCK).enumerate() {
                if b == want_child.as_slice() {
                    late += 1;
                } else {
                    assert!(
                        b == want_mine.as_slice(),
                        "round {round}: block {i} of slot {s} is neither owner's whole write"
                    );
                }
            }
        }
        assert_eq!(
            late, 0,
            "round {round}: blocks of the dead owner's landed late"
        );
        drop(lock);
    }
    drop((q, f, d));
}
