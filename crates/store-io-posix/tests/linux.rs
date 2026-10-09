//! Integration tests against a real Linux filesystem: a fresh directory under
//! `$HOME` (ext4 on the development WSL2 box; never `/mnt/c`, which is 9p)
//! for every test, removed afterwards. The 9p probe test uses `/mnt/c` on
//! purpose and skips itself where that mount does not exist.

#![cfg(target_os = "linux")]
// Test setup unwraps (a failure is the test failing) and ignores results it
// does not inspect.
#![allow(clippy::unwrap_used, clippy::expect_used, unused_results)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use store_io_buf::{BufPool, IoBuf, PoolConfig};
use store_io_core::class::Missing;
use store_io_core::error::OsError;
use store_io_core::evidence::{FsKind, OsCacheMode, Tri};
use store_io_platform::{
    CompletionBuf, FileMode, FileName, IoOp, Platform, Queue, QueueConfig, ReleaseHow,
};
use store_io_posix::{PosixDir, PosixFile, PosixPlatform, PosixQueue};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let base = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|h| h.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        let d = base.join(format!(
            ".store-io-posix-test-{}-{}",
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

fn name(s: &str) -> FileName {
    FileName::new(s).unwrap()
}

fn pool() -> BufPool {
    BufPool::new(&PoolConfig::uniform(4096, 4096, 65536, 32)).unwrap()
}

fn setup() -> (
    TempDir,
    PosixPlatform,
    PosixDir,
    PosixFile,
    BufPool,
    PosixQueue,
) {
    let t = TempDir::new();
    let p = PosixPlatform::new();
    let dir = p.open_dir(t.path(), false).unwrap();
    let file = p.create_file(&dir, &name("data")).unwrap();
    let q = p.queue(QueueConfig::default()).unwrap();
    (t, p, dir, file, pool(), q)
}

fn drain(q: &mut PosixQueue) -> Vec<(u64, Result<usize, OsError>, Option<IoBuf>)> {
    let mut out = CompletionBuf::with_capacity(64);
    let n = q.in_flight();
    assert_eq!(q.wait(n, &mut out).unwrap(), n);
    out.drain().map(|c| (c.tag, c.result, c.buf)).collect()
}

fn write(
    q: &mut PosixQueue,
    f: &PosixFile,
    pool: &BufPool,
    off: u64,
    len: usize,
    byte: u8,
    dsync: bool,
) -> Result<usize, OsError> {
    let mut b = pool.take(len).unwrap();
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
    .map_err(|_| ())
    .unwrap();
    let mut done = drain(q);
    assert_eq!(done.len(), 1);
    let (tag, result, buf) = done.remove(0);
    assert_eq!(tag, off);
    assert!(buf.is_some(), "buffer comes back in the completion");
    result
}

fn read(
    q: &mut PosixQueue,
    f: &PosixFile,
    pool: &BufPool,
    off: u64,
    len: usize,
) -> (Result<usize, OsError>, IoBuf) {
    let b = pool.take(len).unwrap();
    q.submit(
        IoOp::Read {
            file: f,
            offset: off,
            buf: b,
        },
        off,
    )
    .map_err(|_| ())
    .unwrap();
    let mut done = drain(q);
    let (_, result, buf) = done.remove(0);
    (result, buf.unwrap())
}

#[test]
fn test_create_allocate_write_read_roundtrip_with_probe_alignment() {
    let (_t, p, dir, file, pool, mut q) = setup();
    assert_eq!(
        p.create_file(&dir, &name("data"))
            .map(|_| ())
            .unwrap_err()
            .code,
        libc::EEXIST
    );
    p.allocate(&file, 1 << 20).unwrap();
    assert_eq!(p.size(&file).unwrap(), 1 << 20);
    // Growing to a smaller length never truncates.
    p.allocate(&file, 4096).unwrap();
    assert_eq!(p.size(&file).unwrap(), 1 << 20);

    let ev = p.probe(&dir, &file).unwrap();
    assert!(ev.fs.dio_offset_align > 0, "{ev:?}");
    assert_eq!(4096 % ev.fs.dio_offset_align, 0);
    assert!(ev.fs.dio_mem_align <= 4096);

    assert_eq!(write(&mut q, &file, &pool, 0, 4096, 0xA1, false), Ok(4096));
    assert_eq!(
        write(&mut q, &file, &pool, 65536, 65536, 0xB2, false),
        Ok(65536)
    );
    let (r, b) = read(&mut q, &file, &pool, 0, 4096);
    assert_eq!(r, Ok(4096));
    assert!(b.as_slice().iter().all(|&x| x == 0xA1));
    let (r, b) = read(&mut q, &file, &pool, 65536, 65536);
    assert_eq!(r, Ok(65536));
    assert!(b.as_slice().iter().all(|&x| x == 0xB2));
    // Allocated but never written reads as zero.
    let (r, b) = read(&mut q, &file, &pool, 4096, 4096);
    assert_eq!(r, Ok(4096));
    assert!(b.as_slice().iter().all(|&x| x == 0));
    // A read past the end is short, not an error.
    let (r, _) = read(&mut q, &file, &pool, (1 << 20) - 4096, 8192);
    assert_eq!(r, Ok(4096));
    assert_eq!(q.in_flight(), 0);
}

#[test]
fn test_dsync_write_flush_data_and_flush_all_complete_inline() {
    let (_t, p, _dir, file, pool, mut q) = setup();
    p.allocate(&file, 1 << 16).unwrap();
    assert_eq!(write(&mut q, &file, &pool, 0, 4096, 0xC3, true), Ok(4096));
    q.submit(IoOp::FlushData { file: &file }, 42).unwrap();
    assert_eq!(q.in_flight(), 1);
    let done = drain(&mut q);
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].0, 42);
    assert_eq!(done[0].1, Ok(0));
    assert!(done[0].2.is_none());
    p.flush_all(&file).unwrap();
    // Misaligned direct I/O is an error returned in the completion, not a
    // rejection: the kernel saw it.
    let mut b = pool.take(4096).unwrap();
    assert!(b.set_len(100));
    q.submit(
        IoOp::Write {
            file: &file,
            offset: 3,
            buf: b,
            dsync: false,
        },
        1,
    )
    .unwrap();
    let done = drain(&mut q);
    assert_eq!(done[0].1.map_err(|e| e.code), Err(libc::EINVAL));
    assert!(done[0].2.is_some());
}

#[test]
fn test_queue_full_rejects_and_returns_the_buffer() {
    let (_t, p, _dir, file, pool, _q) = setup();
    p.allocate(&file, 1 << 16).unwrap();
    let mut q = p.queue(QueueConfig { depth: 2 }).unwrap();
    for i in 0..2u64 {
        let b = pool.take(4096).unwrap();
        q.submit(
            IoOp::Write {
                file: &file,
                offset: i * 4096,
                buf: b,
                dsync: false,
            },
            i,
        )
        .unwrap();
    }
    let b = pool.take(4096).unwrap();
    let rej = q
        .submit(
            IoOp::Write {
                file: &file,
                offset: 8192,
                buf: b,
                dsync: false,
            },
            9,
        )
        .unwrap_err();
    assert!(rej.raw.is_none());
    assert!(rej.buf.is_some());
    let rej = q.submit(IoOp::FlushData { file: &file }, 10).unwrap_err();
    assert!(rej.buf.is_none() && rej.raw.is_none());
    // Reap into a small buffer: order kept, remainder stays queued.
    let mut out = CompletionBuf::with_capacity(1);
    assert_eq!(q.reap(&mut out), 1);
    assert_eq!(out.drain().next().unwrap().tag, 0);
    assert_eq!(q.in_flight(), 1);
    assert_eq!(q.wait(1, &mut out), Ok(1));
    assert_eq!(out.drain().next().unwrap().tag, 1);
    assert_eq!(q.in_flight(), 0);
    // Nothing in flight: wait returns at once.
    assert_eq!(q.wait(1, &mut out), Ok(0));
}

#[test]
fn test_rename_replace_then_sync_dir_and_unlink() {
    let (_t, p, dir, file, pool, mut q) = setup();
    p.allocate(&file, 4096).unwrap();
    assert_eq!(write(&mut q, &file, &pool, 0, 4096, 0x5A, true), Ok(4096));
    let old = p.create_file(&dir, &name("target")).unwrap();
    p.rename_replace(&dir, &name("data"), &name("target"), &file)
        .unwrap();
    p.sync_dir(&dir).unwrap();
    drop(old);
    assert_eq!(
        p.open_file(&dir, &name("data"), FileMode::ReadOnly)
            .map(|_| ())
            .unwrap_err()
            .code,
        libc::ENOENT
    );
    let renamed = p
        .open_file(&dir, &name("target"), FileMode::ReadOnly)
        .unwrap();
    let (r, b) = read(&mut q, &renamed, &pool, 0, 4096);
    assert_eq!(r, Ok(4096));
    assert!(b.as_slice().iter().all(|&x| x == 0x5A));
    p.unlink(&dir, &name("target")).unwrap();
    p.sync_dir(&dir).unwrap();
    assert_eq!(
        p.unlink(&dir, &name("target")).unwrap_err().code,
        libc::ENOENT
    );
    // The open handle still works after the unlink.
    assert_eq!(p.size(&renamed).unwrap(), 4096);
}

#[test]
fn test_ofd_lock_blocks_a_second_open_file_description() {
    let (_t, p, dir, file, _pool, _q) = setup();
    let second = p
        .open_file(&dir, &name("data"), FileMode::ReadWrite)
        .unwrap();
    let lock = p.lock_exclusive(&file).unwrap();
    assert!(lock.is_ofd(), "kernel 3.15+ gives OFD locks");
    let e = p.lock_exclusive(&second).map(|_| ()).unwrap_err().code;
    assert!(
        matches!(e, libc::EAGAIN | libc::EACCES),
        "another description must be refused, got errno {e}"
    );
    // The same description may take it again (it already holds it).
    let again = p.lock_exclusive(&file).unwrap();
    drop(again);
    drop(lock);
    // Released on drop: the second description now succeeds.
    let _taken = p.lock_exclusive(&second).unwrap();
    // A read-only description cannot take a write lock.
    let ro = p
        .open_file(&dir, &name("data"), FileMode::ReadOnly)
        .unwrap();
    assert_eq!(
        p.lock_exclusive(&ro).map(|_| ()).unwrap_err().code,
        libc::EBADF
    );
}

#[test]
fn test_read_only_handle_cannot_write_allocate_or_flush_data() {
    let (_t, p, dir, file, pool, mut q) = setup();
    p.allocate(&file, 8192).unwrap();
    assert_eq!(write(&mut q, &file, &pool, 0, 4096, 0x11, false), Ok(4096));
    let ro = p
        .open_file(&dir, &name("data"), FileMode::ReadOnly)
        .unwrap();
    assert!(!ro.is_writable());
    let b = pool.take(4096).unwrap();
    let rej = q
        .submit(
            IoOp::Write {
                file: &ro,
                offset: 0,
                buf: b,
                dsync: false,
            },
            1,
        )
        .unwrap_err();
    assert_eq!(rej.raw.map(|e| e.code), Some(libc::EBADF));
    assert!(rej.buf.is_some());
    assert_eq!(q.in_flight(), 0);
    let rej = q.submit(IoOp::FlushData { file: &ro }, 2).unwrap_err();
    assert_eq!(rej.raw.map(|e| e.code), Some(libc::EBADF));
    assert_eq!(p.allocate(&ro, 1 << 20).unwrap_err().code, libc::EBADF);
    assert_eq!(
        p.release_range(&ro, 0, 4096, ReleaseHow::Deallocate)
            .unwrap_err()
            .code,
        libc::EBADF
    );
    // Reads work and see the writer's data.
    let (r, b) = read(&mut q, &ro, &pool, 0, 4096);
    assert_eq!(r, Ok(4096));
    assert!(b.as_slice().iter().all(|&x| x == 0x11));
    // The on-disk data is still readable through std too.
    let bytes = std::fs::read(_t.path().join("data")).unwrap();
    assert_eq!(bytes.len(), 8192);
}

#[test]
fn test_fiemap_reports_unwritten_until_a_direct_fill() {
    let (_t, p, _dir, file, pool, mut q) = setup();
    let len = 1u64 << 20;
    p.allocate(&file, len).unwrap();
    let s = p.range_state(&file, 0, len).unwrap();
    assert_eq!(s.unwritten, Tri::Yes, "{s:?}");
    assert_eq!(s.shared, Tri::No);
    for i in 0..16u64 {
        assert_eq!(
            write(&mut q, &file, &pool, i * 65536, 65536, 0x77, false),
            Ok(65536)
        );
    }
    q.submit(IoOp::FlushData { file: &file }, 0).unwrap();
    assert_eq!(drain(&mut q)[0].1, Ok(0));
    let s = p.range_state(&file, 0, len).unwrap();
    assert_eq!(s.unwritten, Tri::No, "{s:?}");
    assert_eq!(s.shared, Tri::No);
    assert_eq!(s.valid_data, Tri::Unknown);
    // A sub-range inside the written area is written too.
    let s = p.range_state(&file, 65536, 4096).unwrap();
    assert_eq!(s.unwritten, Tri::No);
    // Beyond the end of the file is not written data.
    let s = p.range_state(&file, len - 4096, 8192).unwrap();
    assert_eq!(s.unwritten, Tri::Yes);
    // Punching a hole makes the range not-written again, keeps the size.
    p.release_range(&file, 0, 65536, ReleaseHow::Deallocate)
        .unwrap();
    assert_eq!(p.size(&file).unwrap(), len);
    assert_eq!(p.range_state(&file, 0, 65536).unwrap().unwritten, Tri::Yes);
    assert_eq!(
        p.range_state(&file, 65536, len - 65536).unwrap().unwritten,
        Tri::No
    );
    assert_eq!(
        p.release_range(&file, 0, 4096, ReleaseHow::TrimInPlace)
            .unwrap_err()
            .code,
        libc::EOPNOTSUPP
    );
    assert_eq!(p.range_state(&file, 0, 0).unwrap().unwritten, Tri::No);
}

#[test]
fn test_cachestat_sees_a_buffered_read_and_direct_writes_leave_none() {
    let (t, p, dir, file, pool, mut q) = setup();
    p.allocate(&file, 1 << 16).unwrap();
    for i in 0..16u64 {
        assert_eq!(
            write(&mut q, &file, &pool, i * 4096, 4096, 0x33, false),
            Ok(4096)
        );
    }
    let ev = p.probe(&dir, &file).unwrap();
    let has_cachestat = ev.kernel.is_some_and(|k| k >= (6, 5, 0));
    let s = p.range_state(&file, 0, 1 << 16).unwrap();
    if !has_cachestat {
        assert_eq!(s.cached_pages, None);
        assert!(ev.missing.contains(Missing::PageCache));
        return;
    }
    assert_eq!(
        s.cached_pages,
        Some(0),
        "direct writes must not populate the cache"
    );
    assert!(!ev.missing.contains(Missing::PageCache));
    // A foreign buffered reader populates the page cache.
    let bytes = std::fs::read(t.path().join("data")).unwrap();
    assert_eq!(bytes.len(), 1 << 16);
    let s = p.range_state(&file, 0, 1 << 16).unwrap();
    assert!(s.cached_pages.is_some_and(|n| n > 0), "{s:?}");
}

#[test]
fn test_probe_on_ext4_reports_filesystem_alignment_stack_and_device() {
    let (_t, p, dir, file, _pool, _q) = setup();
    p.allocate(&file, 4096).unwrap();
    let ev = p.probe(&dir, &file).unwrap();
    assert_eq!(ev.fs.kind, FsKind::Ext4, "{ev:?}");
    assert!(
        ev.fs.dio_mem_align > 0 && ev.fs.dio_offset_align > 0,
        "{ev:?}"
    );
    assert!(!ev.missing.contains(Missing::DioAlignment));
    assert!(!ev.missing.contains(Missing::MountOptions), "{ev:?}");
    assert!(!ev.missing.contains(Missing::InodeFlags), "{ev:?}");
    assert!(!ev.missing.contains(Missing::Stack), "{ev:?}");
    assert!(!ev.missing.contains(Missing::OsCacheMode), "{ev:?}");
    assert_ne!(ev.device.os_cache_mode, OsCacheMode::Unknown);
    assert!(
        ev.device.logical_block > 0 && ev.device.physical_block > 0,
        "{ev:?}"
    );
    assert!(ev.device.max_transfer > 0);
    assert!(ev.device.model.is_some(), "{ev:?}");
    assert_ne!(ev.fs.direct_io, Tri::No);
    assert_eq!(ev.fs.cow, Tri::No);
    assert_eq!(ev.fs.full_flush, Tri::Yes);
    assert!(!ev.fs.data_journal && !ev.fs.no_barrier);
    assert!(ev.kernel.is_some_and(|(maj, _, _)| maj >= 4), "{ev:?}");
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    if release.contains("microsoft") {
        assert_eq!(
            ev.hypervisor,
            Some(store_io_core::evidence::Hypervisor::HyperV)
        );
    }
    // Identify needs an NVMe node the user can open; whatever happened, the
    // gaps are listed rather than guessed.
    if ev.device.cache_present == Tri::Unknown {
        assert!(ev.missing.contains(Missing::CachePresence), "{ev:?}");
    }
    if ev.device.backup_failed == Tri::Unknown {
        assert!(ev.missing.contains(Missing::Smart), "{ev:?}");
    }
    // A probe of a read-only handle gives the same filesystem facts.
    let ro = p
        .open_file(&dir, &name("data"), FileMode::ReadOnly)
        .unwrap();
    let ev2 = p.probe(&dir, &ro).unwrap();
    assert_eq!(ev2.fs, ev.fs);
    assert_eq!(ev2.stack, ev.stack);
}

#[test]
fn test_probe_on_drvfs_is_passthrough() {
    let p = PosixPlatform::new();
    if !Path::new("/mnt/c").is_dir() {
        return;
    }
    let candidates = ["/mnt/c/Windows/Temp", "/mnt/c/Users/Public", "/mnt/c/Temp"];
    let fname = name(&format!("store-io-posix-{}.tmp", std::process::id()));
    for c in candidates {
        let Ok(dir) = p.open_dir(Path::new(c), false) else {
            continue;
        };
        let file = match p.create_file(&dir, &fname) {
            Ok(f) => f,
            Err(e)
                if matches!(
                    e.code,
                    libc::EACCES | libc::EPERM | libc::EROFS | libc::EINVAL
                ) =>
            {
                continue;
            }
            Err(e) => panic!("create on {c}: errno {}", e.code),
        };
        let ev = p.probe(&dir, &file);
        let _removed = p.unlink(&dir, &fname);
        let ev = ev.unwrap();
        assert_eq!(ev.fs.kind, FsKind::Passthrough, "{ev:?}");
        assert!(
            ev.missing.contains(Missing::Stack),
            "no block device behind 9p: {ev:?}"
        );
        return;
    }
    panic!("/mnt/c exists but no candidate directory accepted a direct-I/O file");
}

#[test]
fn test_open_dir_creates_and_refuses_symlinked_files() {
    let t = TempDir::new();
    let p = PosixPlatform::new();
    let nested = t.path().join("a");
    assert_eq!(
        p.open_dir(&nested, false).map(|_| ()).unwrap_err().code,
        libc::ENOENT
    );
    let dir = p.open_dir(&nested, true).unwrap();
    assert!(nested.is_dir());
    // Creating again is fine: it exists.
    let _again = p.open_dir(&nested, true).unwrap();
    let _f = p.create_file(&dir, &name("real")).unwrap();
    std::os::unix::fs::symlink("real", nested.join("link")).unwrap();
    let e = p
        .open_file(&dir, &name("link"), FileMode::ReadOnly)
        .map(|_| ())
        .unwrap_err()
        .code;
    assert_eq!(e, libc::ELOOP, "symlinks are refused");
    std::os::unix::fs::symlink("/etc/hostname", nested.join("escape")).unwrap();
    assert_eq!(
        p.open_file(&dir, &name("escape"), FileMode::ReadOnly)
            .map(|_| ())
            .unwrap_err()
            .code,
        libc::ELOOP
    );
    // A directory is not a data file.
    std::fs::create_dir(nested.join("sub")).unwrap();
    let e = p
        .open_file(&dir, &name("sub"), FileMode::ReadWrite)
        .map(|_| ())
        .unwrap_err()
        .code;
    assert!(matches!(e, libc::EISDIR | libc::EINVAL), "errno {e}");
    // A FIFO is neither a regular file nor a block device.
    let fifo = std::ffi::CString::new(nested.join("fifo").to_str().unwrap()).unwrap();
    // SAFETY: mkfifo(3) with a NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let e = p
        .open_file(&dir, &name("fifo"), FileMode::ReadWrite)
        .map(|_| ())
        .unwrap_err()
        .code;
    assert_eq!(e, libc::EINVAL);
    assert_eq!(p.eintr_retries(), 0);
}
