//! # store-io-posix
//!
//! The Linux synchronous backend (tier T3) of store-io. `PosixPlatform`
//! implements `store_io_platform::Platform` over `O_DIRECT` file descriptors,
//! and `PosixQueue` performs every data operation inside `submit` with one
//! system call, completing it in the same call: no thread hop, no timer, no
//! allocation.
//!
//! What this backend does:
//!
//! - opens data files `O_RDWR|O_DIRECT|O_CLOEXEC` (read-only:
//!   `O_RDONLY|O_DIRECT|O_CLOEXEC`) relative to a directory descriptor, with
//!   `openat2(RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS)` on kernels 5.6 and later
//!   and `openat(O_NOFOLLOW)` before; symlinks are refused and so is anything
//!   that is neither a regular file nor a block device;
//! - creates files `O_CREAT|O_EXCL`, mode 0600, and directories mode 0700
//!   followed by an `fsync` of the parent;
//! - takes the ownership lock with `fcntl(F_OFD_SETLK)` on the very open file
//!   description used for I/O (`flock` on kernels without OFD locks), never a
//!   POSIX `F_SETLK`, which any `close()` of the inode would drop;
//! - allocates with `fallocate` mode 0 only (never `KEEP_SIZE`); the engine
//!   fills the unwritten extents with direct writes before a region is ready;
//! - reports extents with `FS_IOC_FIEMAP` and page-cache residency with
//!   `cachestat(2)`;
//! - writes with `pwritev2(RWF_DSYNC)` when durability at completion is
//!   requested, except on raw block devices, where the synchronous
//!   `RWF_DSYNC` path issues both a FUA write and a cache flush; there a plain
//!   write is followed by one `fdatasync`;
//! - runs the operations of a parallel queue (`QueueConfig::parallel`) on a
//!   bounded pool of worker threads, started on first use and parked untimed
//!   while idle (D-051, D-096); every other queue stays inline;
//! - gathers `Evidence` from statfs, statx, `/proc/self/mountinfo`, the ext4
//!   effective options, inode flags, the sysfs block tree, NVMe Identify (when
//!   the device node can be opened) and the DMI tables. Every sysfs, procfs
//!   and device byte is parsed as hostile input. The probe never decides a
//!   class and never writes a file.
//!
//! Errors are raw `errno` values; the engine classifies them. `EINTR` is
//! retried, and the retry counted, only on opens, which provably precede any
//! I/O. Data system calls on `O_DIRECT` files are not interrupted before I/O
//! in practice, so their `EINTR` is returned as the raw outcome.
//!
//! Every module is gated on `target_os = "linux"`; on other operating systems
//! the crate compiles to an empty shell so a workspace build succeeds
//! everywhere. Every `unsafe` block lives in `sys.rs`, each preceded by a
//! `SAFETY` comment citing the system-call contract it relies on.

#![deny(warnings)]

#[cfg(target_os = "linux")]
mod alloc;
#[cfg(target_os = "linux")]
mod dir;
#[cfg(target_os = "linux")]
mod file;
/// Fuzz entry point for the probe's parsers (cargo-fuzz builds only).
#[cfg(all(target_os = "linux", fuzzing))]
#[doc(hidden)]
pub mod fuzzing;
#[cfg(target_os = "linux")]
mod lock;
#[cfg(target_os = "linux")]
mod pool;
#[cfg(target_os = "linux")]
mod probe;
#[cfg(target_os = "linux")]
mod queue;
#[cfg(target_os = "linux")]
mod sync;
#[cfg(target_os = "linux")]
mod sys;
#[cfg(target_os = "linux")]
mod uring;

#[cfg(target_os = "linux")]
pub use dir::PosixDir;
#[cfg(target_os = "linux")]
pub use file::PosixFile;
#[cfg(target_os = "linux")]
pub use lock::PosixLock;
#[cfg(target_os = "linux")]
pub use queue::{PosixQueue, PosixWaker};

/// Which kernel interface a [`PosixPlatform`]'s queues use (D-082).
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum LinuxTier {
    /// One blocking system call per operation (T3), inline or on the
    /// submit pool.
    #[default]
    Sync,
    /// io_uring rings (store-io's own binding, D-093): operations complete
    /// asynchronously and never run on kernel worker threads.
    Uring,
}

#[cfg(target_os = "linux")]
mod platform {
    use core::sync::atomic::{AtomicU64, Ordering};
    use std::path::Path;
    use std::sync::{Arc, Mutex, PoisonError};

    use store_io_platform::{
        Evidence, FileMode, FileName, Owner, Platform, QueueConfig, RangeState, RawResult,
        ReleaseHow,
    };

    use super::LinuxTier;
    use crate::uring::ring::{Flavour, Ring};

    use crate::alloc;
    use crate::dir::PosixDir;
    use crate::file::PosixFile;
    use crate::lock::PosixLock;
    use crate::pool::{SubmitPool, default_threads};
    use crate::probe;
    use crate::queue::PosixQueue;
    use crate::sys;

    /// Upper bound on `EINTR` / `EAGAIN` retries of one open.
    const OPEN_RETRY_LIMIT: u32 = 64;

    /// The Linux synchronous platform.
    ///
    /// Cheap to create; probes once whether `openat2(2)` exists so every
    /// confined open afterwards takes the right path without a syscall.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::path::Path;
    /// use store_io_platform::{FileName, Platform};
    /// use store_io_posix::PosixPlatform;
    ///
    /// // Raw OS errors carry only the errno; the engine classifies them.
    /// let p = PosixPlatform::new();
    /// let dir = p.open_dir(Path::new("/var/lib/example"), true)?;
    /// let name = FileName::new("store.sio")?;
    /// let file = p.create_file(&dir, &name)?;
    /// p.allocate(&file, 1 << 20)?;
    /// let _lock = p.lock_exclusive(&file)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// Clones are handles to the same platform: they share the retry counters
    /// and the submit pool.
    #[derive(Debug, Clone)]
    pub struct PosixPlatform {
        openat2: bool,
        eintr_retries: Arc<AtomicU64>,
        eagain_retries: Arc<AtomicU64>,
        /// The submit pool, started by the first parallel queue (D-096).
        /// Parallel queues hold it too: its workers stop once the last
        /// reference, platform or queue, is gone.
        pool: Arc<Mutex<Option<Arc<SubmitPool>>>>,
        tier: LinuxTier,
        /// Testing: every queue gets a thread-owned ring.
        owned_rings: bool,
    }

    impl Default for PosixPlatform {
        fn default() -> Self {
            Self::new()
        }
    }

    impl PosixPlatform {
        /// Creates the platform, probing `openat2(2)` support once.
        #[must_use]
        pub fn new() -> Self {
            Self {
                openat2: sys::openat2_supported(),
                eintr_retries: Arc::new(AtomicU64::new(0)),
                eagain_retries: Arc::new(AtomicU64::new(0)),
                pool: Arc::new(Mutex::new(None)),
                tier: LinuxTier::Sync,
                owned_rings: false,
            }
        }

        /// A platform whose queues use `tier`.
        ///
        /// # Errors
        ///
        /// For [`LinuxTier::Uring`], the kernel's refusal to set up a ring of
        /// either kind (io_uring disabled, too old, or without a feature
        /// store-io requires: `ENOSYS`).
        pub fn with_tier(tier: LinuxTier) -> RawResult<Self> {
            if tier == LinuxTier::Uring {
                drop(Ring::new(Flavour::Shared, 1)?);
                drop(Ring::new(Flavour::Owned, 1)?);
            }
            Ok(Self {
                tier,
                ..Self::new()
            })
        }

        /// The tier this platform's queues use.
        #[must_use]
        pub fn tier(&self) -> LinuxTier {
            self.tier
        }

        /// Testing only: on the io_uring tier, every queue gets a ring only
        /// its creating thread may use, whatever its `QueueConfig::owner`,
        /// so suites written for shared queues cover thread-owned rings too.
        #[doc(hidden)]
        #[must_use]
        pub fn with_owned_rings(mut self) -> Self {
            self.owned_rings = true;
            self
        }

        /// Worker threads in the submit pool, or `None` before the first
        /// parallel queue starts it. Its size is fixed by that first queue
        /// (`QueueConfig::pool_threads`, at most 256; else one per CPU up to
        /// 32).
        #[must_use]
        pub fn pool_threads(&self) -> Option<usize> {
            self.pool
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .map(|p| p.threads())
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

        /// Whether confined opens use `openat2(RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS)`
        /// (kernel 5.6 and later) rather than `openat(O_NOFOLLOW)`.
        #[must_use]
        pub fn openat2_supported(&self) -> bool {
            self.openat2
        }

        /// Opens retried after `EINTR` since creation (each retry counted).
        #[must_use]
        pub fn eintr_retries(&self) -> u64 {
            self.eintr_retries.load(Ordering::Relaxed)
        }

        /// `openat2` calls retried after a `RESOLVE_BENEATH` race (`EAGAIN`)
        /// since creation.
        #[must_use]
        pub fn eagain_retries(&self) -> u64 {
            self.eagain_retries.load(Ordering::Relaxed)
        }

        /// Runs an open, retrying `EINTR` (and `EAGAIN` when `openat2` is in
        /// use) a bounded number of times. Both precede any I/O.
        pub(crate) fn retry_open<T>(&self, mut open: impl FnMut() -> RawResult<T>) -> RawResult<T> {
            let mut attempts = 0;
            loop {
                match open() {
                    Err(e) if e.code == libc::EINTR && attempts < OPEN_RETRY_LIMIT => {
                        attempts += 1;
                        let _count = self.eintr_retries.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e)
                        if e.code == libc::EAGAIN
                            && self.openat2
                            && attempts < OPEN_RETRY_LIMIT =>
                    {
                        attempts += 1;
                        let _count = self.eagain_retries.fetch_add(1, Ordering::Relaxed);
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

        fn open_file(
            &self,
            dir: &PosixDir,
            name: &FileName,
            mode: FileMode,
        ) -> RawResult<PosixFile> {
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
            sys::monotonic_coarse_ms()
        }

        fn allocate(&self, file: &PosixFile, len: u64) -> RawResult<()> {
            alloc::allocate(file, len)
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

        fn flush_all(&self, file: &PosixFile) -> RawResult<()> {
            sys::fsync(file.raw())
        }

        fn probe(&self, dir: &PosixDir, file: &PosixFile) -> RawResult<Evidence> {
            probe::probe(dir, file)
        }

        fn queue(&self, cfg: QueueConfig) -> RawResult<PosixQueue> {
            if self.tier == LinuxTier::Uring {
                let owned = self.owned_rings || cfg.owner == Owner::Thread;
                return PosixQueue::uring(cfg, owned);
            }
            if cfg.parallel {
                Ok(PosixQueue::pooled(cfg, self.submit_pool(cfg.pool_threads)?))
            } else {
                Ok(PosixQueue::new(cfg))
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub use platform::PosixPlatform;
