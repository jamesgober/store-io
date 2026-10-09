//! Directory mode: whole files replaced atomically.
//!
//! A [`Directory`] holds small objects as ordinary files (a manifest, a
//! `CURRENT` pointer, a configuration). [`Directory::replace`] swaps a file's
//! contents so that after any crash it holds exactly the old bytes or exactly
//! the new ones:
//!
//! 1. write the new bytes to a temporary file in the same directory (direct
//!    I/O), set its exact length, and flush it, data and metadata;
//! 2. rename it over the target (replace-if-exists, POSIX semantics);
//! 3. flush the directory, so the rename itself is durable.
//!
//! Success is reported only after all three. A failure before the rename
//! leaves the target untouched. Any failure from the rename on is
//! "durability unknown": the handle is poisoned and refuses every later
//! change, because the directory may now name either version. A directory
//! flush the platform cannot do is an error, never a success.
//!
//! The device is probed once, with the first file the handle opens, and the
//! durability class decides as it does for a store: unsafe or unverified
//! devices are refused unless the caller's [`Trust`] says otherwise.

use std::path::Path;
use std::sync::{Mutex, OnceLock, PoisonError};

use store_io_buf::{BufPool, PoolConfig};
use store_io_core::align::align_up;
use store_io_core::class::DurabilityClass;
use store_io_core::decide::{ClassDecision, Trust, decide};
use store_io_core::error::{
    CorruptionKind, Error, ErrorContext, FirstCause, Named, NotWrittenCause, Op, OsError,
};
use store_io_platform::{FileMode, FileName, IoOp, Platform, QueueConfig};

use crate::exec::{QueueSet, run};
use crate::io::not_written;

/// Block granularity of directory-mode transfers.
const BLOCK: u64 = 4096;
/// Largest single transfer.
const CHUNK: usize = 1 << 20;
/// Suffix of the temporary file a replace writes first.
const TEMP_SUFFIX: &str = ".sio-tmp";

fn io(op: Op, raw: OsError) -> Error {
    Error::Io {
        op,
        raw,
        ctx: ErrorContext::default(),
    }
}

fn name_of(name: &str) -> Result<FileName, Error> {
    FileName::new(name)
        .map_err(|_| not_written(NotWrittenCause::InvalidName, ErrorContext::default()))
}

/// A directory of small objects, each replaced atomically.
pub struct Directory<P: Platform> {
    platform: P,
    dir: P::Dir,
    trust: Trust,
    decision: OnceLock<ClassDecision>,
    poisoned: OnceLock<FirstCause>,
    pool: BufPool,
    queue: QueueSet<P::Queue>,
    /// Serialises replaces and removes (one temporary name per target).
    change: Mutex<()>,
}

impl<P: Platform> core::fmt::Debug for Directory<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Directory")
            .field("class", &self.class())
            .field("poisoned", &self.poisoned.get().is_some())
            .finish()
    }
}

impl<P: Platform> Directory<P> {
    /// Opens the directory at `path`, creating it (and syncing its parent)
    /// when `create` is true. `trust` is applied when the device is probed.
    ///
    /// # Errors
    ///
    /// `NotFound` if it does not exist and `create` is false; `Io`.
    pub fn open(platform: P, path: &Path, create: bool, trust: Trust) -> Result<Self, Error> {
        let dir = platform.open_dir(path, create).map_err(|raw| {
            if store_io_core::errno::is_not_found(raw) {
                Error::NotFound { what: Named::Store }
            } else {
                io(Op::Open, raw)
            }
        })?;
        let queue = platform
            .queue(QueueConfig { depth: 8 })
            .map_err(|raw| io(Op::Open, raw))?;
        let pool = BufPool::new(&PoolConfig::uniform(4096, 4096, CHUNK, 4))
            .map_err(|_| not_written(NotWrittenCause::PoolExhausted, ErrorContext::default()))?;
        Ok(Self {
            platform,
            dir,
            trust,
            decision: OnceLock::new(),
            poisoned: OnceLock::new(),
            pool,
            queue: QueueSet::new(vec![queue], 8),
            change: Mutex::new(()),
        })
    }

    /// The durability class of the directory's device, once a file has been
    /// opened through this handle.
    #[must_use]
    pub fn class(&self) -> Option<DurabilityClass> {
        self.decision.get().map(|d| d.class)
    }

    /// The class decision (evidence, reasons, label), once probed.
    #[must_use]
    pub fn decision(&self) -> Option<&ClassDecision> {
        self.decision.get()
    }

    /// Probes the device with `file` the first time, and refuses unsafe or
    /// unverified devices as the trust settings require.
    fn admit(&self, file: &P::File) -> Result<(), Error> {
        if self.decision.get().is_none() {
            let ev = self
                .platform
                .probe(&self.dir, file)
                .map_err(|raw| io(Op::Probe, raw))?;
            let _first = self.decision.set(decide(&ev, &self.trust));
        }
        match self.decision.get() {
            Some(d) => crate::store::refuse(d),
            None => Err(not_written(
                NotWrittenCause::NotReady,
                ErrorContext::default(),
            )),
        }
    }

    fn poison(&self, op: Op, raw: Option<OsError>) -> Error {
        let _first_kept = self.poisoned.set(FirstCause { op: Some(op), raw });
        Error::DurabilityUnknown {
            op,
            raw,
            ctx: ErrorContext::default(),
        }
    }

    fn writable(&self) -> Result<(), Error> {
        match self.poisoned.get() {
            Some(first) => Err(Error::Poisoned { first: *first }),
            None => Ok(()),
        }
    }

    /// Replaces the file `name` with `data`, atomically and durably: after
    /// any crash the file holds exactly the old or exactly the new bytes, and
    /// once this returns, exactly the new ones.
    ///
    /// # Errors
    ///
    /// `NotWritten`, `UnsafeDevice`, `Unverified`, `NoSpace` or `Io` before
    /// anything reached the device (the target is untouched).
    /// `DurabilityUnknown` if a write or flush of the new bytes failed (the
    /// target is untouched) or anything from the rename on failed (the
    /// directory may name either version); the handle is then poisoned and
    /// later changes fail with `Poisoned`.
    pub fn replace(&self, name: &str, data: &[u8]) -> Result<(), Error> {
        self.writable()?;
        let target = name_of(name)?;
        let temp = name_of(&format!("{name}{TEMP_SUFFIX}"))?;
        let _one_at_a_time = self.change.lock().unwrap_or_else(PoisonError::into_inner);
        // A leftover from an interrupted replace is never the live version.
        let _absent_or_removed = self.platform.unlink(&self.dir, &temp);
        let file = self
            .platform
            .create_file(&self.dir, &temp)
            .map_err(|raw| io(Op::Create, raw))?;
        if let Err(e) = self.admit(&file).and_then(|()| self.fill(&file, data)) {
            drop(file);
            let _cleanup = self.platform.unlink(&self.dir, &temp);
            return Err(e);
        }
        // From here on the directory may name either version.
        let renamed = self
            .platform
            .rename_replace(&self.dir, &temp, &target, &file);
        drop(file);
        renamed.map_err(|raw| self.poison(Op::Rename, Some(raw)))?;
        self.platform
            .sync_dir(&self.dir)
            .map_err(|raw| self.poison(Op::SyncDir, Some(raw)))
    }

    /// Writes `data` to a fresh file, sets its exact length and flushes it.
    fn fill(&self, file: &P::File, data: &[u8]) -> Result<(), Error> {
        let c = ErrorContext::default();
        let len = data.len() as u64;
        let padded =
            align_up(len, BLOCK).ok_or_else(|| not_written(NotWrittenCause::TooLarge, c))?;
        self.platform
            .allocate(file, padded)
            .map_err(|raw| crate::store::space_err(Op::Allocate, raw, 0))?;
        let mut lane = self.queue.get(0);
        for (i, piece) in data.chunks(CHUNK).enumerate() {
            let piece_len = align_up(piece.len() as u64, BLOCK).unwrap_or(BLOCK) as usize;
            let mut buf = self
                .pool
                .take(piece_len)
                .map_err(|_| not_written(NotWrittenCause::PoolExhausted, c))?;
            let dst = buf.as_mut_slice();
            dst[..piece.len()].copy_from_slice(piece);
            dst[piece.len()..].fill(0);
            let done = run(
                &mut lane,
                IoOp::Write {
                    file,
                    offset: (i * CHUNK) as u64,
                    buf,
                    dsync: false,
                },
            )
            .map_err(|(_b, raw)| {
                raw.map_or_else(
                    || not_written(NotWrittenCause::QueueFull, c),
                    |r| io(Op::Write, r),
                )
            })?;
            // A failed or short write, or a failed flush, poisons the handle
            // (fail-stop), even though the target itself is untouched.
            match done.result {
                Ok(n) if n == piece_len => {}
                Ok(_) => return Err(self.poison(Op::Write, None)),
                Err(raw) => return Err(self.poison(Op::Write, Some(raw))),
            }
        }
        drop(lane);
        self.platform
            .set_len(file, len)
            .map_err(|raw| io(Op::Allocate, raw))?;
        self.platform
            .flush_all(file)
            .map_err(|raw| self.poison(Op::FlushAll, Some(raw)))
    }

    /// Reads the whole file `name` (direct I/O: never the OS page cache).
    ///
    /// # Errors
    ///
    /// `NotFound`; `Corruption(MediaError)` for an unreadable range; `Io`.
    pub fn read(&self, name: &str) -> Result<Vec<u8>, Error> {
        let c = ErrorContext::default();
        let file = self
            .platform
            .open_file(&self.dir, &name_of(name)?, FileMode::ReadOnly)
            .map_err(|raw| {
                if store_io_core::errno::is_not_found(raw) {
                    Error::NotFound { what: Named::Slot }
                } else {
                    io(Op::Open, raw)
                }
            })?;
        let len = self.platform.size(&file).map_err(|raw| io(Op::Open, raw))?;
        let len = usize::try_from(len).map_err(|_| not_written(NotWrittenCause::TooLarge, c))?;
        let mut out = vec![0u8; len];
        let mut lane = self.queue.get(0);
        let mut at = 0usize;
        while at < len {
            let want = (len - at).min(CHUNK);
            let padded = align_up(want as u64, BLOCK).unwrap_or(BLOCK) as usize;
            let buf = self
                .pool
                .take(padded)
                .map_err(|_| not_written(NotWrittenCause::PoolExhausted, c))?;
            let done = run(
                &mut lane,
                IoOp::Read {
                    file: &file,
                    offset: at as u64,
                    buf,
                },
            )
            .map_err(|(_b, raw)| {
                raw.map_or_else(
                    || not_written(NotWrittenCause::QueueFull, c),
                    |r| io(Op::Read, r),
                )
            })?;
            let n = done.result.map_err(|raw| {
                if store_io_core::errno::is_media_error(raw) {
                    Error::Corruption {
                        kind: CorruptionKind::MediaError,
                        ctx: c,
                    }
                } else {
                    io(Op::Read, raw)
                }
            })?;
            let Some(buf) = done.buf else {
                return Err(not_written(NotWrittenCause::QueueFull, c));
            };
            let got = n.min(want);
            out[at..at + got].copy_from_slice(&buf.as_slice()[..got]);
            if got < want {
                // The file shrank under us: report what is there.
                out.truncate(at + got);
                break;
            }
            at += got;
        }
        Ok(out)
    }

    /// Removes the file `name` durably (unlink, then flush the directory).
    ///
    /// # Errors
    ///
    /// `NotFound` (nothing changed); `DurabilityUnknown` if the unlink or the
    /// directory flush failed (the handle is poisoned); `Poisoned`.
    pub fn remove(&self, name: &str) -> Result<(), Error> {
        self.writable()?;
        let target = name_of(name)?;
        let _one_at_a_time = self.change.lock().unwrap_or_else(PoisonError::into_inner);
        match self.platform.unlink(&self.dir, &target) {
            Ok(()) => {}
            Err(raw) if store_io_core::errno::is_not_found(raw) => {
                return Err(Error::NotFound { what: Named::Slot });
            }
            Err(raw) => return Err(self.poison(Op::Unlink, Some(raw))),
        }
        self.platform
            .sync_dir(&self.dir)
            .map_err(|raw| self.poison(Op::SyncDir, Some(raw)))
    }

    /// Flushes the directory: every create, rename and unlink in it so far
    /// becomes durable.
    ///
    /// # Errors
    ///
    /// `DurabilityUnknown` (the handle is poisoned); `Poisoned`.
    pub fn sync(&self) -> Result<(), Error> {
        self.writable()?;
        self.platform
            .sync_dir(&self.dir)
            .map_err(|raw| self.poison(Op::SyncDir, Some(raw)))
    }
}
