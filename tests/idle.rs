//! v0.4 exit criterion: an idle store makes zero system calls, wakeups,
//! timers or I/O, from the moment it is idle (no exemption window).
//!
//! Linux, on both tiers. The store is exercised first (durable appends, a
//! page batch, a `Submitter` with writes and a barrier), so its flush thread
//! and any submit-pool workers exist. Once every thread store-io started is
//! asleep and their counters have stopped moving (the idle moment; nothing
//! is exempted after it), they are watched for `STORE_IO_IDLE_SECS` (default
//! 60): none may be switched in (any wakeup or timer is a context switch),
//! run on a CPU (scheduler time, so a spin without a switch is caught), make
//! a read or write system call, or move a byte; and the process moves no
//! byte to or from storage (ring I/O included), per
//! `/proc/self/task/<tid>/{status,io,schedstat,stat}` and `/proc/self/io`.
//!
//! Ignored by default (it sleeps for a minute per tier); CI runs it with
//! `cargo test --test idle -- --ignored`. `STORE_IO_EXPECT_URING` makes an
//! io_uring refusal fail the io_uring test instead of skipping it.

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    unused_results,
    unused_must_use
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use store_io::engine::completion::Done;
use store_io::engine::{SubmitterConfig, WriteMode};
use store_io::{DurableOpen, IoInterface, LinuxTier, Store, StoreOptions, Trust};

/// What one thread has done so far.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Activity {
    name: String,
    asleep: bool,
    switches: u64,
    cpu_ns: u64,
    syscalls: u64,
    bytes: u64,
}

fn field(text: &str, key: &str) -> u64 {
    text.lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

/// Every thread of this process and its activity counters.
fn threads() -> BTreeMap<u32, Activity> {
    let mut out = BTreeMap::new();
    for e in std::fs::read_dir("/proc/self/task").unwrap() {
        let e = e.unwrap();
        let Ok(tid) = e.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let base = e.path();
        let read = |f: &str| std::fs::read_to_string(base.join(f));
        let (Ok(status), Ok(io), Ok(sched), Ok(stat)) =
            (read("status"), read("io"), read("schedstat"), read("stat"))
        else {
            continue; // exited meanwhile
        };
        // stat: "tid (comm) S ...": the state follows the last ')'.
        let state = stat.rsplit(')').next().unwrap_or("").trim_start();
        out.insert(
            tid,
            Activity {
                name: status
                    .lines()
                    .find_map(|l| l.strip_prefix("Name:"))
                    .unwrap_or("")
                    .trim()
                    .to_owned(),
                asleep: state.starts_with('S'),
                switches: field(&status, "voluntary_ctxt_switches:")
                    + field(&status, "nonvoluntary_ctxt_switches:"),
                cpu_ns: sched
                    .split_whitespace()
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0),
                syscalls: field(&io, "syscr:") + field(&io, "syscw:"),
                bytes: field(&io, "rchar:") + field(&io, "wchar:"),
            },
        );
    }
    out
}

/// Bytes this process has moved to and from storage.
fn storage_bytes() -> u64 {
    let io = std::fs::read_to_string("/proc/self/io").unwrap();
    field(&io, "read_bytes:") + field(&io, "write_bytes:")
}

fn is_store_io(name: &str) -> bool {
    name.starts_with("store-io") || name.starts_with("sio-") || name.starts_with("iou-")
}

fn base_dir() -> PathBuf {
    std::env::var_os("STORE_IO_TEST_DIR")
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(std::env::temp_dir, PathBuf::from)
}

fn idle_on(tier: LinuxTier) {
    let secs: u64 = std::env::var("STORE_IO_IDLE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let dir = base_dir().join(format!(".store-io-idle-{}-{tier:?}", std::process::id()));
    let _gone = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let before = threads();
    let leftover: Vec<&str> = before
        .values()
        .map(|a| a.name.as_str())
        .filter(|n| is_store_io(n))
        .collect();
    // A failed earlier test leaves its store behind: its threads predate
    // this test's snapshot and are not watched, so this result stands.
    if !leftover.is_empty() {
        eprintln!("store-io threads from an earlier test, not watched: {leftover:?}");
    }
    let before: BTreeSet<u32> = before.into_keys().collect();
    let probe = store_io::probe(&dir).unwrap();
    let opts = StoreOptions {
        linux_tier: tier,
        t3_pool_threads: 2,
        trust: Trust {
            override_refusal: !matches!(probe.decision.durable_open, DurableOpen::Allowed),
            ..Trust::default()
        },
        ..StoreOptions::default()
    };
    let store = match Store::create(dir.join("s"), opts) {
        Ok(s) => s,
        Err(e) if tier == LinuxTier::Uring => {
            assert!(
                std::env::var_os("STORE_IO_EXPECT_URING").is_none(),
                "io_uring refused ({e}) where STORE_IO_EXPECT_URING says it works"
            );
            eprintln!("io_uring refused here ({e}): its idle test is skipped");
            return;
        }
        Err(e) => panic!("{e}"),
    };
    let interface = store.report().io.interface;
    let want = if tier == LinuxTier::Uring {
        IoInterface::IoUring
    } else {
        IoInterface::Synchronous
    };
    assert_eq!(interface, want, "{:?}", store.report().io);
    // Exercise it: durable appends, a multi-page batch (the synchronous
    // tier's pool), and a submitter with writes and a barrier (the flush
    // thread).
    let wal = store.provision_append_region("wal", 1 << 20).unwrap();
    for i in 0..8u8 {
        wal.append_durable(&[i; 100]).unwrap();
    }
    let pages = store.provision_page_region("pages", 1 << 20).unwrap();
    let blk = pages.block_size();
    let mut batch = pages.batch();
    for i in 0..16u64 {
        batch
            .write(pages.pos(i * blk).unwrap(), &vec![i as u8; blk as usize])
            .unwrap();
    }
    batch.commit().unwrap();
    let mut sub = store.submitter(SubmitterConfig::default()).unwrap();
    for i in 0..4u64 {
        let mut b = sub.take_buf(blk as usize).unwrap();
        b.as_mut_slice().fill(i as u8);
        sub.write(&pages, pages.pos(i * blk).unwrap(), b, WriteMode::Plain, i)
            .map_err(|r| r.error)
            .unwrap();
    }
    let mut n = 0;
    while n < 4 {
        let got = sub.wait(1).unwrap().count();
        assert!(got > 0 || sub.in_flight() > 0, "writes lost");
        n += got;
    }
    sub.barrier_all(9).map_err(|r| r.error).unwrap();
    let mut flushed = false;
    while !flushed {
        let got: Vec<_> = sub.wait(1).unwrap().collect();
        assert!(
            !got.is_empty() || sub.in_flight() > 0,
            "the barrier was lost"
        );
        for c in got {
            assert_eq!(c.tag, 9);
            assert!(matches!(c.outcome, Ok(Done::Flushed)), "{:?}", c.outcome);
            flushed = true;
        }
    }
    // The submitter stays alive through the window: a live submitter must
    // be idle too.
    let watched = || -> BTreeMap<u32, Activity> {
        threads()
            .into_iter()
            .filter(|(tid, _)| !before.contains(tid))
            .collect()
    };
    // The idle moment: every store-io thread asleep and nothing moving.
    let deadline = Instant::now() + Duration::from_secs(10);
    let start = loop {
        let a = watched();
        std::thread::sleep(Duration::from_millis(100));
        let b = watched();
        if a == b && b.values().all(|t| t.asleep) {
            break b;
        }
        assert!(
            Instant::now() < deadline,
            "store-io never went idle: {b:#?}"
        );
    };
    // The counters see activity (the flush thread has run), so a quiet
    // window below means a quiet store, not blind counters.
    assert!(
        start
            .values()
            .any(|a| a.switches > 0 && a.cpu_ns > 0 && a.name.contains("flush")),
        "{tier:?}: the flush thread shows no activity: {start:?}"
    );
    eprintln!(
        "{tier:?} ({interface}): watching {} store-io thread(s) for {secs} s: {:?}",
        start.len(),
        start.values().map(|a| a.name.as_str()).collect::<Vec<_>>()
    );
    // After the line above, so the test's own output is not counted.
    let disk = storage_bytes();
    std::thread::sleep(Duration::from_secs(secs));
    let end = watched();
    let mut busy = Vec::new();
    for (tid, a) in &start {
        match end.get(tid) {
            Some(b) if b == a => {}
            Some(b) => busy.push(format!(
                "{} ({tid}): {} switches, {} ns on a CPU, {} syscalls, {} bytes",
                a.name,
                b.switches - a.switches,
                b.cpu_ns - a.cpu_ns,
                b.syscalls - a.syscalls,
                b.bytes - a.bytes
            )),
            None => busy.push(format!("{} ({tid}) exited while idle", a.name)),
        }
    }
    for (tid, b) in &end {
        if !start.contains_key(tid) {
            busy.push(format!("{} ({tid}) started while idle", b.name));
        }
    }
    let moved = storage_bytes() - disk;
    if moved != 0 {
        busy.push(format!("{moved} bytes moved to or from storage"));
    }
    assert!(
        busy.is_empty(),
        "{tier:?}: an idle store was active: {busy:#?}"
    );
    drop((sub, wal, pages, batch, store));
    let _cleanup = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "sleeps for STORE_IO_IDLE_SECS (60); run by CI with --ignored"]
fn test_an_idle_store_is_silent_on_io_uring() {
    idle_on(LinuxTier::Uring);
}

#[test]
#[ignore = "sleeps for STORE_IO_IDLE_SECS (60); run by CI with --ignored"]
fn test_an_idle_store_is_silent_on_the_synchronous_tier() {
    idle_on(LinuxTier::Sync);
}
