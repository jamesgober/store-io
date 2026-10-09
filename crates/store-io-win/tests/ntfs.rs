//! Integration tests on the real filesystem under `%TEMP%` (NTFS on the
//! development and CI boxes).

#![cfg(windows)]
// Test setup unwraps (a failure is the test failing) and drops drained
// completion lists whose contents a given test does not inspect.
#![allow(clippy::unwrap_used, clippy::expect_used, unused_results)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use store_io_buf::{BufPool, PoolConfig};
use store_io_core::evidence::{FsKind, PlatformKind, Tri};
use store_io_platform::{
    CompletionBuf, FileMode, FileName, IoOp, Platform, Queue, QueueConfig, RawResult, ReleaseHow,
};
use store_io_win::{FlushMode, WinDir, WinFile, WinPlatform, WinQueue};

const BLOCK: usize = 4096;
const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_LOCK_VIOLATION: i32 = 33;
const ERROR_FILE_EXISTS: i32 = 80;
const ERROR_FILE_LEVEL_TRIM_NOT_SUPPORTED: i32 = 326;
const ERROR_CANT_ACCESS_FILE: i32 = 1920;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    p: WinPlatform,
    dir: WinDir,
    path: PathBuf,
    pool: BufPool,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _cleanup = std::fs::remove_dir_all(&self.path);
    }
}

fn fixture(name: &str) -> Fixture {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("store-io-win-{}-{}-{name}", std::process::id(), n));
    let p = WinPlatform::new();
    let dir = p.open_dir(&path, true).unwrap();
    let pool = BufPool::new(&PoolConfig::uniform(BLOCK, BLOCK, 1 << 16, 32)).unwrap();
    Fixture { p, dir, path, pool }
}

fn name(s: &str) -> FileName {
    FileName::new(s).unwrap()
}

fn one(q: &mut WinQueue) -> store_io_platform::Completion {
    let mut out = CompletionBuf::with_capacity(1);
    assert_eq!(q.wait(1, &mut out).unwrap(), 1);
    out.drain().next().unwrap()
}

fn write_block(
    q: &mut WinQueue,
    f: &WinFile,
    pool: &BufPool,
    off: u64,
    byte: u8,
    dsync: bool,
) -> RawResult<usize> {
    let mut buf = pool.take(BLOCK).unwrap();
    buf.as_mut_slice().fill(byte);
    q.submit(
        IoOp::Write {
            file: f,
            offset: off,
            buf,
            dsync,
        },
        off,
    )
    .unwrap();
    let c = one(q);
    assert_eq!(c.tag, off);
    assert!(c.buf.is_some());
    c.result
}

fn read_block(q: &mut WinQueue, f: &WinFile, pool: &BufPool, off: u64) -> RawResult<Vec<u8>> {
    let buf = pool.take(BLOCK).unwrap();
    q.submit(
        IoOp::Read {
            file: f,
            offset: off,
            buf,
        },
        off,
    )
    .unwrap();
    let c = one(q);
    let buf = c.buf.unwrap();
    c.result.map(|n| buf.as_slice()[..n].to_vec())
}

#[test]
fn test_create_exclusive_allocate_write_read_flush_rename_sync() {
    let fx = fixture("roundtrip");
    let f = fx.p.create_file(&fx.dir, &name("a.sio")).unwrap();
    assert_eq!(
        fx.p.create_file(&fx.dir, &name("a.sio"))
            .map(|_| ())
            .unwrap_err()
            .code,
        ERROR_FILE_EXISTS
    );
    assert_eq!(f.flush_mode(), FlushMode::DataSyncOnly);
    fx.p.allocate(&f, 1 << 20).unwrap();
    assert_eq!(fx.p.size(&f).unwrap(), 1 << 20);

    let ev = fx.p.probe(&fx.dir, &f).unwrap();
    assert!(ev.fs.dio_offset_align > 0 && BLOCK as u32 % ev.fs.dio_offset_align == 0);
    assert!(ev.fs.dio_mem_align <= BLOCK as u32);

    let mut q = fx.p.queue(QueueConfig { depth: 16 }).unwrap();
    for i in 0..8u64 {
        assert_eq!(
            write_block(&mut q, &f, &fx.pool, i * BLOCK as u64, i as u8 + 1, false),
            Ok(BLOCK)
        );
    }
    q.submit(IoOp::FlushData { file: &f }, 99).unwrap();
    let c = one(&mut q);
    assert_eq!((c.tag, c.buf.is_none(), c.result), (99, true, Ok(0)));
    for i in 0..8u64 {
        let got = read_block(&mut q, &f, &fx.pool, i * BLOCK as u64).unwrap();
        assert!(got.iter().all(|&b| b == i as u8 + 1), "block {i}");
    }
    fx.p.flush_all(&f).unwrap();
    assert_eq!(q.in_flight(), 0);

    let b = fx.p.create_file(&fx.dir, &name("b.sio")).unwrap();
    fx.p.allocate(&b, BLOCK as u64).unwrap();
    assert_eq!(write_block(&mut q, &b, &fx.pool, 0, 0xBB, true), Ok(BLOCK));
    drop(b);
    fx.p.rename_replace(&fx.dir, &name("a.sio"), &name("b.sio"), &f)
        .unwrap();
    assert!(!fx.dir.used_legacy_rename());
    fx.p.sync_dir(&fx.dir).unwrap();
    assert_eq!(
        fx.p.open_file(&fx.dir, &name("a.sio"), FileMode::ReadOnly)
            .map(|_| ())
            .unwrap_err()
            .code,
        ERROR_FILE_NOT_FOUND
    );
    let renamed =
        fx.p.open_file(&fx.dir, &name("b.sio"), FileMode::ReadOnly)
            .unwrap();
    assert_eq!(fx.p.size(&renamed).unwrap(), 1 << 20);
    assert!(
        read_block(&mut q, &renamed, &fx.pool, 0)
            .unwrap()
            .iter()
            .all(|&x| x == 1)
    );
}

#[test]
fn test_many_in_flight_complete_in_any_order_and_dsync_is_durable_at_completion() {
    let fx = fixture("inflight");
    let f = fx.p.create_file(&fx.dir, &name("c.sio")).unwrap();
    fx.p.allocate(&f, 64 * BLOCK as u64).unwrap();
    let mut q = fx.p.queue(QueueConfig { depth: 32 }).unwrap();
    for i in 0..32u64 {
        let mut buf = fx.pool.take(BLOCK).unwrap();
        buf.as_mut_slice().fill(i as u8);
        q.submit(
            IoOp::Write {
                file: &f,
                offset: i * BLOCK as u64,
                buf,
                dsync: i % 4 == 0,
            },
            i,
        )
        .unwrap();
    }
    assert_eq!(q.in_flight(), 32);
    let mut out = CompletionBuf::with_capacity(32);
    let mut got = 0;
    while got < 32 {
        got += q.wait(1, &mut out).unwrap();
    }
    assert_eq!(q.in_flight(), 0);
    let mut tags: Vec<u64> = out
        .drain()
        .map(|c| {
            assert_eq!(c.result, Ok(BLOCK), "tag {}", c.tag);
            c.tag
        })
        .collect();
    tags.sort_unstable();
    assert_eq!(tags, (0..32).collect::<Vec<_>>());
    for i in [0u64, 5, 31] {
        assert!(
            read_block(&mut q, &f, &fx.pool, i * BLOCK as u64)
                .unwrap()
                .iter()
                .all(|&b| b == i as u8)
        );
    }
    // reap never blocks and reports nothing when nothing is outstanding.
    assert_eq!(q.reap(&mut out), 0);
    assert_eq!(q.wait(1, &mut out).unwrap(), 0);
}

#[test]
fn test_queue_full_rejects_with_buffer_and_no_raw_error() {
    let fx = fixture("full");
    let f = fx.p.create_file(&fx.dir, &name("d.sio")).unwrap();
    fx.p.allocate(&f, 4 * BLOCK as u64).unwrap();
    let mut q = fx.p.queue(QueueConfig { depth: 1 }).unwrap();
    q.submit(
        IoOp::Write {
            file: &f,
            offset: 0,
            buf: fx.pool.take(BLOCK).unwrap(),
            dsync: false,
        },
        1,
    )
    .unwrap();
    let r = q.submit(
        IoOp::Write {
            file: &f,
            offset: BLOCK as u64,
            buf: fx.pool.take(BLOCK).unwrap(),
            dsync: false,
        },
        2,
    );
    let rej = r.unwrap_err();
    assert!(rej.buf.is_some());
    assert!(rej.raw.is_none());
    assert_eq!(one(&mut q).result, Ok(BLOCK));
}

#[test]
fn test_lock_contention_returns_error_and_sentinel_does_not_block_io() {
    let fx = fixture("lock");
    let f = fx.p.create_file(&fx.dir, &name("e.sio")).unwrap();
    fx.p.allocate(&f, 4 * BLOCK as u64).unwrap();
    let lock = fx.p.lock_exclusive(&f).unwrap();
    let second =
        fx.p.open_file(&fx.dir, &name("e.sio"), FileMode::ReadWrite)
            .unwrap();
    assert_eq!(
        fx.p.lock_exclusive(&second).map(|_| ()).unwrap_err().code,
        ERROR_LOCK_VIOLATION
    );
    assert_eq!(
        fx.p.lock_exclusive(&f).map(|_| ()).unwrap_err().code,
        ERROR_LOCK_VIOLATION
    );

    // I/O through the locked file and through a second handle both proceed.
    let mut q = fx.p.queue(QueueConfig::default()).unwrap();
    assert_eq!(write_block(&mut q, &f, &fx.pool, 0, 7, true), Ok(BLOCK));
    assert_eq!(
        write_block(&mut q, &second, &fx.pool, BLOCK as u64, 8, false),
        Ok(BLOCK)
    );
    assert!(
        read_block(&mut q, &second, &fx.pool, 0)
            .unwrap()
            .iter()
            .all(|&b| b == 7)
    );

    drop(lock);
    let relock = fx.p.lock_exclusive(&second).unwrap();
    drop(f);
    drop(relock);
    assert!(fx.p.lock_exclusive(&second).is_ok());
}

#[test]
fn test_read_only_handle_cannot_write_or_flush_but_reads() {
    let fx = fixture("readonly");
    let f = fx.p.create_file(&fx.dir, &name("f.sio")).unwrap();
    fx.p.allocate(&f, 2 * BLOCK as u64).unwrap();
    let mut q = fx.p.queue(QueueConfig::default()).unwrap();
    assert_eq!(write_block(&mut q, &f, &fx.pool, 0, 3, false), Ok(BLOCK));
    let ro =
        fx.p.open_file(&fx.dir, &name("f.sio"), FileMode::ReadOnly)
            .unwrap();
    assert!(!ro.is_writable());
    let rej = q
        .submit(
            IoOp::Write {
                file: &ro,
                offset: 0,
                buf: fx.pool.take(BLOCK).unwrap(),
                dsync: false,
            },
            1,
        )
        .unwrap_err();
    assert_eq!(rej.raw.map(|e| e.code), Some(ERROR_ACCESS_DENIED));
    assert!(rej.buf.is_some());
    let rej = q.submit(IoOp::FlushData { file: &ro }, 2).unwrap_err();
    assert_eq!(rej.raw.map(|e| e.code), Some(ERROR_ACCESS_DENIED));
    assert_eq!(
        fx.p.allocate(&ro, 4 * BLOCK as u64).unwrap_err().code,
        ERROR_ACCESS_DENIED
    );
    assert!(
        read_block(&mut q, &ro, &fx.pool, 0)
            .unwrap()
            .iter()
            .all(|&b| b == 3)
    );
    assert_eq!(q.in_flight(), 0);
}

#[test]
fn test_probe_reports_windows_evidence() {
    let fx = fixture("probe");
    let f = fx.p.create_file(&fx.dir, &name("g.sio")).unwrap();
    let ev = fx.p.probe(&fx.dir, &f).unwrap();
    assert_eq!(ev.platform, PlatformKind::Windows);
    assert!(
        matches!(ev.fs.kind, FsKind::Ntfs | FsKind::Refs),
        "{:?}",
        ev.fs.kind
    );
    assert_eq!(ev.fs.full_flush, Tri::Yes);
    assert_eq!(ev.fs.dir_flush, Tri::Yes);
    assert!(ev.fs.os_atomic_unit > 0);
    assert!(ev.fs.dio_offset_align > 0);
    assert!(ev.device.logical_block > 0, "{ev:?}");
    assert_ne!(ev.device.os_flush_supported, Tri::Unknown, "{ev:?}");
    assert_ne!(ev.device.user_power_protection, Tri::Unknown);
    assert!(
        ev.kernel
            .is_some_and(|(major, _, build)| major >= 10 && build > 0)
    );
    assert!(ev.missing.contains(store_io_core::class::Missing::Stack));
    if ev.fs.kind == FsKind::Ntfs {
        assert_eq!(ev.fs.direct_io, Tri::Yes);
        assert_eq!(f.flush_mode(), FlushMode::DataSyncOnly);
    }
    // The decision is the engine's; the evidence must at least be decidable.
    let d = store_io_core::decide::decide(&ev, &store_io_core::decide::Trust::default());
    assert!(!d.reasons.is_empty() || d.class != store_io_core::class::DurabilityClass::Unsafe);
}

#[test]
fn test_release_range_zero_data_and_range_state() {
    let fx = fixture("release");
    let f = fx.p.create_file(&fx.dir, &name("h.sio")).unwrap();
    fx.p.allocate(&f, 1 << 20).unwrap();
    let mut q = fx.p.queue(QueueConfig::default()).unwrap();
    for i in 0..16u64 {
        assert_eq!(
            write_block(&mut q, &f, &fx.pool, i * BLOCK as u64, 0xAA, false),
            Ok(BLOCK)
        );
    }
    fx.p.flush_all(&f).unwrap();
    let written = fx.p.range_state(&f, 0, 16 * BLOCK as u64).unwrap();
    assert_eq!(written.cached_pages, Some(0));
    assert_eq!(written.valid_data, Tri::Yes, "{written:?}");
    let never = fx.p.range_state(&f, 1 << 19, 16 * BLOCK as u64).unwrap();
    assert_eq!(never.valid_data, Tri::No, "{never:?}");

    fx.p.release_range(&f, 0, 8 * BLOCK as u64, ReleaseHow::Deallocate)
        .unwrap();
    assert!(
        read_block(&mut q, &f, &fx.pool, 0)
            .unwrap()
            .iter()
            .all(|&b| b == 0)
    );
    assert!(
        read_block(&mut q, &f, &fx.pool, 7 * BLOCK as u64)
            .unwrap()
            .iter()
            .all(|&b| b == 0)
    );
    assert!(
        read_block(&mut q, &f, &fx.pool, 8 * BLOCK as u64)
            .unwrap()
            .iter()
            .all(|&b| b == 0xAA)
    );
    assert_eq!(fx.p.size(&f).unwrap(), 1 << 20);
    // A file made sparse by Deallocate no longer accepts a file-level trim
    // (ERROR_FILE_LEVEL_TRIM_NOT_SUPPORTED); the error is reported, never
    // turned into success.
    let trim = fx.p.release_range(
        &f,
        8 * BLOCK as u64,
        8 * BLOCK as u64,
        ReleaseHow::TrimInPlace,
    );
    assert!(
        trim.is_ok()
            || trim
                == Err(store_io_platform::OsError {
                    code: ERROR_FILE_LEVEL_TRIM_NOT_SUPPORTED,
                    source: store_io_core::error::OsErrorSource::Win32
                }),
        "{trim:?}"
    );
    assert_eq!(fx.p.size(&f).unwrap(), 1 << 20);
}

#[test]
fn test_trim_in_place_keeps_allocation_on_a_plain_file() {
    let fx = fixture("trim");
    let f = fx.p.create_file(&fx.dir, &name("n.sio")).unwrap();
    fx.p.allocate(&f, 1 << 20).unwrap();
    let mut q = fx.p.queue(QueueConfig::default()).unwrap();
    for i in 0..16u64 {
        assert_eq!(
            write_block(&mut q, &f, &fx.pool, i * BLOCK as u64, 0x5A, false),
            Ok(BLOCK)
        );
    }
    fx.p.flush_all(&f).unwrap();
    // FSCTL_FILE_LEVEL_TRIM is a hint the volume may refuse: the dev box's
    // NTFS system volume answers ERROR_FILE_LEVEL_TRIM_NOT_SUPPORTED (326)
    // [measured]; the raw code is reported for the engine to classify.
    let trim =
        fx.p.release_range(&f, 0, 8 * BLOCK as u64, ReleaseHow::TrimInPlace);
    assert!(
        trim.is_ok() || trim.map_err(|e| e.code) == Err(ERROR_FILE_LEVEL_TRIM_NOT_SUPPORTED),
        "{trim:?}"
    );
    assert_eq!(fx.p.size(&f).unwrap(), 1 << 20);
    // Contents of the trimmed range are undefined; the rest is intact.
    assert!(
        read_block(&mut q, &f, &fx.pool, 8 * BLOCK as u64)
            .unwrap()
            .iter()
            .all(|&b| b == 0x5A)
    );
    assert_eq!(
        fx.p.range_state(&f, 0, 16 * BLOCK as u64)
            .unwrap()
            .valid_data,
        Tri::Yes
    );
}

#[test]
fn test_unlink_is_posix_and_open_handles_survive() {
    let fx = fixture("unlink");
    let f = fx.p.create_file(&fx.dir, &name("i.sio")).unwrap();
    fx.p.allocate(&f, BLOCK as u64).unwrap();
    fx.p.unlink(&fx.dir, &name("i.sio")).unwrap();
    assert!(!fx.dir.used_legacy_delete());
    assert_eq!(
        fx.p.open_file(&fx.dir, &name("i.sio"), FileMode::ReadOnly)
            .map(|_| ())
            .unwrap_err()
            .code,
        ERROR_FILE_NOT_FOUND
    );
    assert_eq!(fx.p.size(&f).unwrap(), BLOCK as u64);
    assert!(fx.p.create_file(&fx.dir, &name("i.sio")).is_ok());
    assert_eq!(
        fx.p.unlink(&fx.dir, &name("missing.sio")).unwrap_err().code,
        ERROR_FILE_NOT_FOUND
    );
}

#[test]
fn test_reparse_points_are_refused() {
    let fx = fixture("reparse");
    let target = fx.path.join("real");
    std::fs::create_dir(&target).unwrap();
    let link = fx.path.join("junction");
    let status = std::process::Command::new("cmd")
        .args([
            "/c",
            "mklink",
            "/J",
            &link.to_string_lossy(),
            &target.to_string_lossy(),
        ])
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "mklink /J failed: {}",
        String::from_utf16_lossy(
            &status
                .stdout
                .iter()
                .map(|&b| u16::from(b))
                .collect::<Vec<_>>()
        )
    );
    assert_eq!(
        fx.p.open_dir(&link, false).map(|_| ()).unwrap_err().code,
        ERROR_CANT_ACCESS_FILE
    );
    assert!(fx.p.open_dir(&target, false).is_ok());
    // A directory where a file is expected is refused too (CreateFileW
    // itself denies a directory opened without backup semantics).
    let d = fx.p.open_dir(&fx.path, false).unwrap();
    let code =
        fx.p.open_file(&d, &name("real"), FileMode::ReadOnly)
            .map(|_| ())
            .unwrap_err()
            .code;
    assert!(
        code == ERROR_ACCESS_DENIED || code == ERROR_CANT_ACCESS_FILE,
        "{code}"
    );
    // File symlinks need a privilege; when one can be made, it is refused.
    let file = fx.path.join("plain.sio");
    std::fs::write(&file, b"x").unwrap();
    if std::os::windows::fs::symlink_file(&file, fx.path.join("sym.sio")).is_ok() {
        assert_eq!(
            fx.p.open_file(&d, &name("sym.sio"), FileMode::ReadOnly)
                .map(|_| ())
                .unwrap_err()
                .code,
            ERROR_CANT_ACCESS_FILE
        );
    }
}

#[test]
fn test_dropping_a_queue_with_operations_in_flight_returns_every_buffer() {
    let fx = fixture("drop");
    let f = fx.p.create_file(&fx.dir, &name("j.sio")).unwrap();
    fx.p.allocate(&f, 64 * BLOCK as u64).unwrap();
    let total: usize = fx.pool.occupancy().iter().map(|c| c.1).sum();
    {
        let mut q = fx.p.queue(QueueConfig { depth: 16 }).unwrap();
        for i in 0..16u64 {
            let buf = fx.pool.take(BLOCK).unwrap();
            q.submit(
                IoOp::Write {
                    file: &f,
                    offset: i * BLOCK as u64,
                    buf,
                    dsync: false,
                },
                i,
            )
            .unwrap();
        }
        assert_eq!(q.in_flight(), 16);
    }
    let after: usize = fx.pool.occupancy().iter().map(|c| c.1).sum();
    assert_eq!(after, total);
    // The file is still usable afterwards.
    let mut q = fx.p.queue(QueueConfig::default()).unwrap();
    assert_eq!(write_block(&mut q, &f, &fx.pool, 0, 1, true), Ok(BLOCK));
}

#[test]
fn test_two_queues_share_one_file() {
    let fx = fixture("twoqueues");
    let f = fx.p.create_file(&fx.dir, &name("k.sio")).unwrap();
    fx.p.allocate(&f, 4 * BLOCK as u64).unwrap();
    let mut a = fx.p.queue(QueueConfig::default()).unwrap();
    let mut b = fx.p.queue(QueueConfig::default()).unwrap();
    assert_eq!(write_block(&mut a, &f, &fx.pool, 0, 0x11, false), Ok(BLOCK));
    assert_eq!(
        write_block(&mut b, &f, &fx.pool, BLOCK as u64, 0x22, true),
        Ok(BLOCK)
    );
    assert!(
        read_block(&mut b, &f, &fx.pool, 0)
            .unwrap()
            .iter()
            .all(|&x| x == 0x11)
    );
    assert!(
        read_block(&mut a, &f, &fx.pool, BLOCK as u64)
            .unwrap()
            .iter()
            .all(|&x| x == 0x22)
    );
    assert_eq!((a.open_files(), b.open_files()), (1, 1));
    let g = fx.p.create_file(&fx.dir, &name("l.sio")).unwrap();
    fx.p.allocate(&g, BLOCK as u64).unwrap();
    assert_eq!(write_block(&mut a, &g, &fx.pool, 0, 0x33, false), Ok(BLOCK));
    assert_eq!(a.open_files(), 2);
}

#[test]
fn test_misaligned_write_fails_as_a_completion_not_a_rejection() {
    let fx = fixture("misaligned");
    let f = fx.p.create_file(&fx.dir, &name("m.sio")).unwrap();
    fx.p.allocate(&f, 4 * BLOCK as u64).unwrap();
    let mut q = fx.p.queue(QueueConfig::default()).unwrap();
    let mut buf = fx.pool.take(BLOCK).unwrap();
    assert!(buf.set_len(100));
    q.submit(
        IoOp::Write {
            file: &f,
            offset: 0,
            buf,
            dsync: false,
        },
        7,
    )
    .unwrap();
    let c = one(&mut q);
    assert_eq!(c.tag, 7);
    assert!(c.buf.is_some());
    assert!(c.result.is_err(), "{:?}", c.result);
    assert_eq!(q.in_flight(), 0);
}
