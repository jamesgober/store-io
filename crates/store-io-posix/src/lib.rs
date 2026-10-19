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

#[cfg(target_os = "linux")]
pub use store_io_platform::LinuxTier;

/// Operations, in this process so far, that the io_uring tier finished with
/// one blocking system call because the kernel would have blocked (or
/// could not honour NOWAIT for the file) or transferred short. A measure
/// of how often the asynchronous path was not taken; never a failure.
#[cfg(target_os = "linux")]
#[must_use]
pub fn io_uring_fallbacks() -> u64 {
    uring::queue::FALLBACKS.load(core::sync::atomic::Ordering::Relaxed)
}

#[cfg(target_os = "linux")]
mod platform {
    use core::sync::atomic::{AtomicU64, Ordering};
    use std::path::Path;
    use std::sync::{Arc, Mutex, OnceLock, PoisonError};

    use store_io_platform::{
        Evidence, FileMode, FileName, IoInterface, IoPath, OsError, Owner, Platform, QueueConfig,
        RangeState, RawResult, ReleaseHow,
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
        /// `Sync` or `Uring` (never `Auto`: resolved at construction).
        tier: LinuxTier,
        /// Why `tier` was chosen.
        reason: Arc<str>,
        /// `Uring` came from `Auto`: a ring the kernel refuses (for want of
        /// descriptors or memory, or a filter added since the probe) gives a
        /// synchronous queue instead of an error.
        auto: bool,
        /// Queues that fell back so, and the last refusal's code (stored
        /// before the count is raised).
        fell_back: Arc<AtomicU64>,
        /// Queues on rings.
        ringed: Arc<AtomicU64>,
        /// Rings the kernel refused with an error to the caller (every
        /// refusal under an explicit `Uring`; under `Auto`, one a
        /// synchronous queue cannot get around): a submitter is not created,
        /// a parallel lane's operations run on the store's lone-operation
        /// lanes. `last_refusal` holds the latest code of either kind.
        refused: Arc<AtomicU64>,
        last_refusal: Arc<core::sync::atomic::AtomicI32>,
    }

    /// Whether this kernel accepts store-io's rings of both flavours; `Err`
    /// holds the refusal and a description. An answer that cannot change
    /// (accepted, or refused as unsupported or forbidden) is kept for the
    /// process; a refusal for want of resources is asked again next time.
    fn uring_available() -> Result<(), (OsError, String)> {
        static PROBE: OnceLock<Result<(), (OsError, String)>> = OnceLock::new();
        if let Some(known) = PROBE.get() {
            return known.clone();
        }
        let answer = probe_rings();
        let lasting = match &answer {
            Ok(()) => true,
            Err((e, _)) => [
                libc::ENOSYS,
                libc::EINVAL,
                libc::EPERM,
                libc::EACCES,
                libc::EOPNOTSUPP,
            ]
            .contains(&e.code),
        };
        if lasting {
            let _first = PROBE.set(answer.clone());
        }
        answer
    }

    /// A ring refusal a synchronous queue gets around: no descriptors or
    /// memory, or a filter (seccomp, `io_uring_disabled`) added since the
    /// probe. Any other code (a bad argument, say) is a fault to report.
    fn passes_to_sync(e: OsError) -> bool {
        [
            libc::EMFILE,
            libc::ENFILE,
            libc::ENOMEM,
            libc::EAGAIN,
            libc::EPERM,
            libc::EACCES,
            libc::ENOSYS,
        ]
        .contains(&e.code)
    }

    fn probe_rings() -> Result<(), (OsError, String)> {
        for (flavour, name) in [(Flavour::Shared, "shared"), (Flavour::Owned, "owned")] {
            if let Err(e) = Ring::new(flavour, 1) {
                let knob = std::fs::read_to_string("/proc/sys/kernel/io_uring_disabled")
                    .ok()
                    .map(|v| v.trim().to_owned())
                    .filter(|v| v != "0")
                    .map_or(String::new(), |v| {
                        format!("; kernel.io_uring_disabled = {v}")
                    });
                return Err((e, format!("io_uring refused a {name} ring ({e}){knob}")));
            }
        }
        Ok(())
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
                reason: Arc::from("synchronous by default (PosixPlatform::new)"),
                auto: false,
                fell_back: Arc::new(AtomicU64::new(0)),
                refused: Arc::new(AtomicU64::new(0)),
                ringed: Arc::new(AtomicU64::new(0)),
                last_refusal: Arc::new(core::sync::atomic::AtomicI32::new(0)),
            }
        }

        /// A platform whose queues use `tier`: [`LinuxTier::Auto`] takes
        /// io_uring where the kernel accepts store-io's rings of both
        /// flavours (probed once per process), else the synchronous tier.
        /// The choice and its reason are [`Platform::io_path`]. Under `Auto`,
        /// a queue whose ring the kernel refuses later (for want of
        /// descriptors or memory, or a filter installed since the probe) is
        /// a synchronous queue instead, counted in the reason at the time it
        /// is read (and the interface is synchronous if every queue fell
        /// back); any other refusal, and every refusal under `Uring`, is an
        /// error.
        ///
        /// # Errors
        ///
        /// For [`LinuxTier::Uring`] only: the kernel's refusal to set up a
        /// ring (io_uring disabled, too old, or without a feature store-io
        /// requires: `ENOSYS`).
        pub fn with_tier(tier: LinuxTier) -> RawResult<Self> {
            let auto = !matches!(tier, LinuxTier::Sync | LinuxTier::Uring);
            let (tier, reason) = match tier {
                LinuxTier::Sync => (LinuxTier::Sync, "chosen".to_owned()),
                LinuxTier::Uring => match uring_available() {
                    Ok(()) => (LinuxTier::Uring, "chosen".to_owned()),
                    Err((e, _)) => return Err(e),
                },
                _ => match uring_available() {
                    Ok(()) => (
                        LinuxTier::Uring,
                        "auto: the kernel accepts both ring flavours".to_owned(),
                    ),
                    Err((_, why)) => (LinuxTier::Sync, format!("auto: {why}")),
                },
            };
            Ok(Self {
                tier,
                reason: Arc::from(reason),
                auto: auto && tier == LinuxTier::Uring,
                ..Self::new()
            })
        }

        /// The tier this platform's queues use (`Sync` or `Uring`).
        #[must_use]
        pub fn tier(&self) -> LinuxTier {
            self.tier
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

        fn io_path(&self) -> IoPath {
            let fell_back = self.fell_back.load(Ordering::Acquire);
            let refused = self.refused.load(Ordering::Acquire);
            let ringed = self.ringed.load(Ordering::Relaxed);
            let mut reason = self.reason.to_string();
            let code = self.last_refusal.load(Ordering::Relaxed);
            if fell_back > 0 {
                reason.push_str(&format!(
                    "; {fell_back} queue(s) synchronous: the kernel refused a ring ({})",
                    sys::errno(code)
                ));
            }
            if refused > 0 {
                reason.push_str(&format!(
                    "; {refused} ring(s) refused by the kernel ({}): a submitter needing one was not created, a parallel lane's operations ran one at a time",
                    sys::errno(code)
                ));
            }
            IoPath {
                // Every queue so far fell back: the store is synchronous.
                interface: if self.tier == LinuxTier::Uring && (ringed > 0 || fell_back == 0) {
                    IoInterface::IoUring
                } else {
                    IoInterface::Synchronous
                },
                reason,
            }
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
            // Rings for queues that keep several operations in flight (a
            // `Submitter`, a parallel lane). A queue whose caller runs one
            // operation at a time and waits for it (`parallel: false`, a
            // blocking call's lane) does it inline with one system call on
            // the io_uring tier too: a ring round trip for a lone operation
            // costs more than the call (D-104), and buys nothing.
            let ring = cfg.parallel || cfg.owner == Owner::Thread;
            if self.tier == LinuxTier::Uring && ring {
                let owned = cfg.owner == Owner::Thread;
                match PosixQueue::uring(cfg, owned) {
                    Ok(q) => {
                        let _n = self.ringed.fetch_add(1, Ordering::Relaxed);
                        return Ok(q);
                    }
                    // `Auto` chose rings; this one was refused: a synchronous
                    // queue, which needs nothing from the kernel, serves
                    // (below), and is counted.
                    Err(e) if self.auto && passes_to_sync(e) => {
                        self.last_refusal.store(e.code, Ordering::Relaxed);
                        let _n = self.fell_back.fetch_add(1, Ordering::Release);
                    }
                    Err(e) => {
                        self.last_refusal.store(e.code, Ordering::Relaxed);
                        let _n = self.refused.fetch_add(1, Ordering::Release);
                        return Err(e);
                    }
                }
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
