//! The macOS platform: synchronous system calls, inline for lone
//! operations, on the submit pool for parallel queues (no POSIX aio: small
//! limits, and its `aio_fsync` never reaches the drive).

use core::sync::atomic::{AtomicU64, Ordering};
use std::collections::HashMap;
use std::os::fd::RawFd;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use store_io_platform::{
    Evidence, FileMode, FileName, IoInterface, IoPath, Platform, QueueConfig, RangeState,
    RawResult, ReleaseHow,
};

use crate::alloc;
use crate::dir::{FlushState, PosixDir};
use crate::file::PosixFile;
use crate::lock::PosixLock;
use crate::pool::{SubmitPool, default_threads};
use crate::probe;
use crate::queue::PosixQueue;
use crate::sys;

/// Upper bound on `EINTR` retries of one open.
const OPEN_RETRY_LIMIT: u32 = 64;

/// The volumes' flush states, by device number and what is mounted.
type Volumes = HashMap<(libc::dev_t, String), Arc<FlushState>>;

/// The macOS platform.
///
/// Every data operation is one system call: `pwrite`, `pread`, and
/// `F_FULLFSYNC` for every flush (macOS has no data-only drive flush, and
/// `fsync` never reaches the drive's cache), or `fsync` on a volume that
/// refused `F_FULLFSYNC` (a store opened there despite its class). A queue
/// whose caller runs one operation at a time does it inline; a parallel queue
/// (`QueueConfig::parallel`) runs on a bounded pool of worker threads,
/// started on first use and parked untimed while idle (D-096).
///
/// Clones are handles to the same platform: they share the retry counter,
/// the submit pool and the volumes' flush states.
#[derive(Debug, Clone, Default)]
pub struct PosixPlatform {
    eintr_retries: Arc<AtomicU64>,
    /// The submit pool, started by the first parallel queue (D-096).
    pool: Arc<Mutex<Option<Arc<SubmitPool>>>>,
    /// Each volume's flush state (D-133), by device number and what is
    /// mounted (a device number alone may be reused by another volume
    /// after an unmount): one for each volume, so an answer met through any
    /// handle reaches every handle on it (a copy's target directory, a
    /// second open of a store's directory).
    volumes: Arc<Mutex<Volumes>>,
}

impl PosixPlatform {
    /// Creates the platform.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Worker threads in the submit pool, or `None` before the first
    /// parallel queue starts it.
    #[must_use]
    pub fn pool_threads(&self) -> Option<usize> {
        self.pool
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|p| p.threads())
    }

    /// Opens retried after `EINTR` since creation (each retry counted).
    #[must_use]
    pub fn eintr_retries(&self) -> u64 {
        self.eintr_retries.load(Ordering::Relaxed)
    }

    /// The submit pool, started with `threads` workers (0 = the default)
    /// if it is not running yet. A running pool is never resized.
    fn submit_pool(&self, threads: u32) -> RawResult<Arc<SubmitPool>> {
        let mut slot = self.pool.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(p) = slot.as_ref() {
            return Ok(Arc::clone(p));
        }
        let n = match usize::try_from(threads) {
            Ok(0) | Err(_) => default_threads(),
            Ok(n) => n,
        };
        let p = Arc::new(SubmitPool::start(n)?);
        *slot = Some(Arc::clone(&p));
        Ok(p)
    }

    /// The flush state of the volume holding `fd`, made (not asked yet) on
    /// first use.
    pub(crate) fn flush_state(&self, fd: RawFd) -> RawResult<Arc<FlushState>> {
        let key = (sys::fstat(fd)?.st_dev, sys::volume(fd)?.from);
        let mut volumes = self.volumes.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(Arc::clone(volumes.entry(key).or_default()))
    }

    /// Runs an open, retrying `EINTR` a bounded number of times; it
    /// precedes any I/O.
    pub(crate) fn retry_open<T>(&self, mut open: impl FnMut() -> RawResult<T>) -> RawResult<T> {
        let mut attempts = 0;
        loop {
            match open() {
                Err(e) if e.code == libc::EINTR && attempts < OPEN_RETRY_LIMIT => {
                    attempts += 1;
                    let _count = self.eintr_retries.fetch_add(1, Ordering::Relaxed);
                }
                r => return r,
            }
        }
    }
}

impl Platform for PosixPlatform {
    type File = PosixFile;
    type Dir = PosixDir;
    type Lock = PosixLock;
    type Queue = PosixQueue;

    fn open_dir(&self, path: &Path, create: bool) -> RawResult<PosixDir> {
        PosixDir::open(self, path, create)
    }

    fn create_file(&self, dir: &PosixDir, name: &FileName) -> RawResult<PosixFile> {
        PosixFile::create(self, dir, name)
    }

    fn open_file(&self, dir: &PosixDir, name: &FileName, mode: FileMode) -> RawResult<PosixFile> {
        PosixFile::open(self, dir, name, mode)
    }

    fn rename_replace(
        &self,
        dir: &PosixDir,
        from: &FileName,
        to: &FileName,
        _file: &PosixFile,
    ) -> RawResult<()> {
        dir.rename(from, to)
    }

    fn rename_noreplace(
        &self,
        dir: &PosixDir,
        from: &FileName,
        to: &FileName,
        _file: &PosixFile,
    ) -> RawResult<()> {
        dir.rename_noreplace(from, to)
    }

    fn unlink(&self, dir: &PosixDir, name: &FileName) -> RawResult<()> {
        dir.unlink(name)
    }

    fn sync_dir(&self, dir: &PosixDir) -> RawResult<()> {
        dir.sync()
    }

    fn lock_exclusive(&self, file: &PosixFile) -> RawResult<PosixLock> {
        PosixLock::acquire(file, false)
    }

    fn lock_shared(&self, file: &PosixFile) -> RawResult<PosixLock> {
        PosixLock::acquire(file, true)
    }

    fn monotonic_ms(&self) -> u64 {
        sys::monotonic_ms()
    }

    fn io_path(&self) -> IoPath {
        IoPath {
            interface: IoInterface::Synchronous,
            reason: "macOS: one system call per operation, inline or on the submit pool".to_owned(),
        }
    }

    fn allocate(&self, file: &PosixFile, len: u64) -> RawResult<()> {
        alloc::allocate(file, len)
    }

    fn allocate_range(&self, file: &PosixFile, offset: u64, len: u64) -> RawResult<()> {
        alloc::allocate_range(file, offset, len)
    }

    fn set_len(&self, file: &PosixFile, len: u64) -> RawResult<()> {
        alloc::set_len(file, len)
    }

    fn size(&self, file: &PosixFile) -> RawResult<u64> {
        alloc::size(file)
    }

    fn release_range(
        &self,
        file: &PosixFile,
        offset: u64,
        len: u64,
        how: ReleaseHow,
    ) -> RawResult<()> {
        alloc::release(file, offset, len, how)
    }

    fn range_state(&self, file: &PosixFile, offset: u64, len: u64) -> RawResult<RangeState> {
        alloc::range_state(file, offset, len)
    }

    /// `F_FULLFSYNC` (or `fsync` where the volume refused it at the flush
    /// check): the file's data and attributes, then the drive's cache.
    fn flush_all(&self, file: &PosixFile) -> RawResult<()> {
        if !file.is_writable() {
            return Err(sys::errno(libc::EBADF));
        }
        file.flush_state().flush(file.raw())
    }

    fn probe(&self, dir: &PosixDir, file: &PosixFile) -> RawResult<Evidence> {
        probe::probe(dir, file)
    }

    fn queue(&self, cfg: QueueConfig) -> RawResult<PosixQueue> {
        if cfg.parallel {
            Ok(PosixQueue::pooled(cfg, self.submit_pool(cfg.pool_threads)?))
        } else {
            Ok(PosixQueue::new(cfg))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::PathBuf;

    use store_io_core::evidence::Tri;

    use super::*;

    /// A fresh directory path under the system's temporary directory.
    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sio-mac-{name}-{}", std::process::id()));
        let _gone = std::fs::remove_dir_all(&d);
        d
    }

    fn name(s: &str) -> FileName {
        FileName::new(s).unwrap()
    }

    /// A refusal met through one handle reaches every handle on the volume:
    /// the other files, the directory's own syncs, a second directory
    /// handle; and the probe then reports it without asking again.
    #[test]
    fn test_a_refusal_reaches_every_handle_on_the_volume() {
        let path = scratch("refusal");
        std::fs::create_dir(&path).unwrap();
        let p = PosixPlatform::new();
        let dir = p.open_dir(&path, false).unwrap();
        let a = p.create_file(&dir, &name("a")).unwrap();
        let b = p.create_file(&dir, &name("b")).unwrap();
        assert!(a.full_flush() && b.full_flush());
        // The volume refuses before anything accepted F_FULLFSYNC.
        assert!(a.flush_state().refuse());
        assert!(!b.full_flush() && !a.traits().full && !dir.flush_state().full());
        let again = p.open_dir(&path, false).unwrap();
        assert!(!again.flush_state().full());
        let c = p
            .open_file(&again, &name("b"), FileMode::ReadWrite)
            .unwrap();
        assert!(!c.full_flush());
        let ev = p.probe(&again, &c).unwrap();
        assert_eq!((ev.fs.full_flush, ev.fs.dir_flush), (Tri::No, Tri::No));
        let r = p.open_file(&again, &name("b"), FileMode::ReadOnly).unwrap();
        assert_eq!(p.probe(&again, &r).unwrap().fs.full_flush, Tri::No);
        // The downgraded flushes still work (`fsync`).
        p.flush_all(&c).unwrap();
        p.sync_dir(&again).unwrap();
        drop((a, b, c, r, again, dir));
        std::fs::remove_dir_all(&path).unwrap();
    }

    /// Once the volume has accepted `F_FULLFSYNC`, nothing weakens it: a
    /// later refusal is a failed flush, and readers see the acceptance.
    #[test]
    fn test_an_accepted_volume_never_weakens() {
        let path = scratch("accepted");
        let p = PosixPlatform::new();
        let dir = p.open_dir(&path, true).unwrap();
        let f = p.create_file(&dir, &name("a")).unwrap();
        let ev = p.probe(&dir, &f).unwrap();
        assert_eq!((ev.fs.full_flush, ev.fs.dir_flush), (Tri::Yes, Tri::Yes));
        assert!(!f.flush_state().refuse());
        assert!(f.full_flush() && dir.flush_state().full());
        let r = p.open_file(&dir, &name("a"), FileMode::ReadOnly).unwrap();
        assert_eq!(p.probe(&dir, &r).unwrap().fs.full_flush, Tri::Yes);
        // A second create of the existing directory succeeds (and syncs
        // its parent again).
        drop(p.open_dir(&path, true).unwrap());
        drop((f, r, dir));
        std::fs::remove_dir_all(&path).unwrap();
    }

    /// A create under a parent this caller may write and search but not
    /// read: its `..` cannot be opened, so the directory itself is flushed
    /// instead (committing the volume's transaction that holds its entry),
    /// and both a new directory and an existing one open. (Skipped as root,
    /// which can open anything.)
    #[test]
    fn test_an_unreadable_parent_is_synced_through_the_directory() {
        use std::os::unix::fs::PermissionsExt;
        let parent = scratch("parent");
        std::fs::create_dir(&parent).unwrap();
        let set = |mode| {
            std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        std::fs::create_dir(parent.join("old")).unwrap();
        // Write and search, no read: mkdir works, opening the parent fails.
        set(0o300);
        let root = std::fs::read_dir(&parent).is_ok();
        let p = PosixPlatform::new();
        let made = p.open_dir(&parent.join("new"), true);
        let old = p.open_dir(&parent.join("old"), true);
        set(0o700);
        let new_exists = parent.join("new").is_dir();
        let (made, old) = (made.map(drop).map_err(|e| e.code), old.map(drop));
        std::fs::remove_dir_all(&parent).unwrap();
        if !root {
            assert_eq!(made, Ok(()));
            assert!(new_exists);
            assert!(old.is_ok());
        }
    }

    /// A reader's probe makes no flush check; a writer's store is Unverified
    /// (`DeviceNotInternal`) exactly when its device is not known to be
    /// internal.
    #[test]
    fn test_a_reader_makes_no_flush_check_and_the_device_decides_internal() {
        use store_io_core::class::Reason;
        use store_io_core::decide::{Trust, decide};
        let path = scratch("reader");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("a"), b"").unwrap();
        // Nothing on this platform has asked the volume yet.
        let p = PosixPlatform::new();
        let dir = p.open_dir(&path, false).unwrap();
        let r = p.open_file(&dir, &name("a"), FileMode::ReadOnly).unwrap();
        let ev = p.probe(&dir, &r).unwrap();
        assert_eq!(
            (ev.fs.full_flush, ev.fs.dir_flush),
            (Tri::Unknown, Tri::Unknown)
        );
        let w = p.open_file(&dir, &name("a"), FileMode::ReadWrite).unwrap();
        let ev = p.probe(&dir, &w).unwrap();
        assert_eq!(ev.fs.full_flush, Tri::Yes);
        let d = decide(&ev, &Trust::default());
        assert_eq!(
            d.reasons.contains(Reason::DeviceNotInternal),
            ev.device.internal != Tri::Yes,
            "{:?} {d:?}",
            ev.device
        );
        drop((r, w, dir));
        std::fs::remove_dir_all(&path).unwrap();
    }

    /// The ownership lock is the open file description's: a second
    /// description of the file can take neither the exclusive nor the
    /// shared lock while the first holds the exclusive one; readers share;
    /// dropping the lock releases it even while its file stays open.
    #[test]
    fn test_the_lock_excludes_another_description_and_drops_cleanly() {
        use store_io_core::errno::is_lock_held;
        let path = scratch("lock");
        let p = PosixPlatform::new();
        let dir = p.open_dir(&path, true).unwrap();
        let a = p.create_file(&dir, &name("f")).unwrap();
        let b = p.open_file(&dir, &name("f"), FileMode::ReadWrite).unwrap();
        let held = p.lock_exclusive(&a).unwrap();
        assert!(is_lock_held(p.lock_exclusive(&b).unwrap_err()));
        assert!(is_lock_held(p.lock_shared(&b).unwrap_err()));
        drop(held);
        let held = p.lock_exclusive(&b).unwrap();
        drop(held);
        let r1 = p.open_file(&dir, &name("f"), FileMode::ReadOnly).unwrap();
        let r2 = p.open_file(&dir, &name("f"), FileMode::ReadOnly).unwrap();
        let (s1, s2) = (p.lock_shared(&r1).unwrap(), p.lock_shared(&r2).unwrap());
        assert!(is_lock_held(p.lock_exclusive(&a).unwrap_err()));
        drop((s1, s2, r1, r2, a, b, dir));
        std::fs::remove_dir_all(&path).unwrap();
    }

    /// The lock of an owner that is killed (no drop runs) is released by
    /// the kernel: a child process of this test takes it, is killed, and the
    /// lock is free.
    #[test]
    fn test_a_killed_owners_lock_is_released() {
        use std::io::{BufRead, BufReader, Write};

        use store_io_core::errno::is_lock_held;
        const CHILD: &str = "STORE_IO_LOCK_CHILD";
        const LOCKED: &str = "store-io lock child: locked";
        if let Some(path) = std::env::var_os(CHILD) {
            // The child: take the lock, say so, wait to be killed.
            let p = PosixPlatform::new();
            let dir = p.open_dir(std::path::Path::new(&path), false).unwrap();
            let f = p.open_file(&dir, &name("f"), FileMode::ReadWrite).unwrap();
            let _held = p.lock_exclusive(&f).unwrap();
            // On its own line: the harness may have printed the test's name
            // without one.
            let _said = writeln!(std::io::stdout(), "\n{LOCKED}");
            let _flushed = std::io::stdout().flush();
            std::thread::sleep(std::time::Duration::from_secs(120));
            return;
        }
        let path = scratch("killed");
        let p = PosixPlatform::new();
        let dir = p.open_dir(&path, true).unwrap();
        let f = p.create_file(&dir, &name("f")).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "mac_platform::tests::test_a_killed_owners_lock_is_released",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, &path)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        let locked = out
            .lines()
            .map_while(Result::ok)
            .any(|l| l.contains(LOCKED));
        assert!(locked, "the child never took the lock");
        let refused = p.lock_exclusive(&f).map(drop);
        child.kill().unwrap();
        let _status = child.wait().unwrap();
        assert!(refused.is_err_and(is_lock_held), "{refused:?}");
        drop(p.lock_exclusive(&f).unwrap());
        drop((f, dir));
        std::fs::remove_dir_all(&path).unwrap();
    }

    /// `allocate` reserves the blocks (`F_PREALLOCATE`), not just the size;
    /// `release_range` frees a written range's blocks (`F_PUNCHHOLE`) and
    /// keeps the size.
    #[test]
    fn test_allocate_reserves_blocks_and_release_frees_them() {
        const MIB: u64 = 1 << 20;
        let path = scratch("space");
        let p = PosixPlatform::new();
        let dir = p.open_dir(&path, true).unwrap();
        let f = p.create_file(&dir, &name("grown")).unwrap();
        p.allocate(&f, 8 * MIB).unwrap();
        let held = |f: &PosixFile| {
            let st = sys::fstat(f.raw()).unwrap();
            u64::try_from(st.st_blocks).unwrap() * 512
        };
        assert_eq!(p.size(&f).unwrap(), 8 * MIB);
        assert!(held(&f) >= 8 * MIB, "{} bytes held", held(&f));
        std::fs::write(path.join("written"), vec![7u8; (8 * MIB) as usize]).unwrap();
        let w = p
            .open_file(&dir, &name("written"), FileMode::ReadWrite)
            .unwrap();
        // The data on the media, so its blocks are counted.
        p.flush_all(&w).unwrap();
        let before = held(&w);
        p.release_range(&w, 2 * MIB, 4 * MIB, ReleaseHow::Deallocate)
            .unwrap();
        assert_eq!(p.size(&w).unwrap(), 8 * MIB);
        assert!(held(&w) + 3 * MIB <= before, "{before} then {}", held(&w));
        drop((f, w, dir));
        std::fs::remove_dir_all(&path).unwrap();
    }
}
