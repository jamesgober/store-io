//! The simulated world: one device with a volatile write cache, the files and
//! directories on it, and the rules for what survives a crash.
//!
//! # Model
//!
//! - Every file has **visible** bytes (what reads return now) and **media**
//!   bytes (what survives a crash), plus a visible and a durable length.
//! - A completed write updates the visible bytes. On a power-safe device, or
//!   with `dsync` (FUA), it updates the media at completion. Otherwise it sits
//!   in the device's volatile cache, tagged with a completion sequence number.
//! - A flush persists exactly the cached writes that completed **before the
//!   flush was submitted** (the NVMe Flush scope), and makes the flushed
//!   file's length durable. A full flush also persists every cached write.
//! - Directory entries (create, rename, unlink) are visible at once and
//!   durable only after a directory sync. Created directories are durable at
//!   once (the platform contract syncs the parent).
//! - A **crash** persists any chosen subset of cached writes (optionally torn
//!   at logical-block granularity), drops the rest, truncates every file to
//!   its durable length, restores every directory to its durable entries,
//!   releases every lock, and invalidates every open handle.
//!
//! The world is `Clone`, so a crash-state enumerator can branch it once per
//! crash state.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use store_io_core::error::{OsError, OsErrorSource};
use store_io_core::evidence::Evidence;

use crate::fault::{FaultPlan, FlushFailure};
use crate::rng::Rng;

/// Simulated EIO.
pub const EIO: OsError = OsError {
    code: 5,
    source: OsErrorSource::Sim,
};
/// Simulated ENOSPC.
pub const ENOSPC: OsError = OsError {
    code: 28,
    source: OsErrorSource::Sim,
};
/// Simulated EEXIST.
pub const EEXIST: OsError = OsError {
    code: 17,
    source: OsErrorSource::Sim,
};
/// Simulated ENOENT.
pub const ENOENT: OsError = OsError {
    code: 2,
    source: OsErrorSource::Sim,
};
/// Simulated EWOULDBLOCK (lock held).
pub const EWOULDBLOCK: OsError = OsError {
    code: 11,
    source: OsErrorSource::Sim,
};
/// Simulated EBADF (handle invalidated by a crash, or read-only).
pub const EBADF: OsError = OsError {
    code: 9,
    source: OsErrorSource::Sim,
};
/// Simulated EINVAL.
pub const EINVAL: OsError = OsError {
    code: 22,
    source: OsErrorSource::Sim,
};

/// Kind of simulated device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// No volatile cache: completed writes are durable.
    PowerSafe,
    /// Volatile cache: writes are durable only after a flush or with FUA.
    VolatileCache,
}

/// Simulator configuration.
#[derive(Debug, Clone)]
pub struct SimConfig {
    /// Seed for every random choice.
    pub seed: u64,
    /// Device kind.
    pub device: DeviceKind,
    /// Logical block size (tear granularity), bytes.
    pub logical_block: u32,
    /// Complete queued operations in a seeded random order instead of FIFO.
    pub reorder: bool,
    /// Faults to inject.
    pub faults: FaultPlan,
    /// Evidence returned by `probe` (defaults derived from `device`).
    pub evidence: Option<Evidence>,
}

impl SimConfig {
    /// A volatile-cache device with 4 KiB blocks, FIFO completion, no faults.
    #[must_use]
    pub fn volatile(seed: u64) -> Self {
        Self {
            seed,
            device: DeviceKind::VolatileCache,
            logical_block: 4096,
            reorder: false,
            faults: FaultPlan::default(),
            evidence: None,
        }
    }

    /// A power-safe device with 4 KiB blocks, FIFO completion, no faults.
    #[must_use]
    pub fn power_safe(seed: u64) -> Self {
        Self {
            device: DeviceKind::PowerSafe,
            ..Self::volatile(seed)
        }
    }
}

/// Identity of a simulated file.
pub type FileId = u32;

#[derive(Debug, Clone, Default)]
struct FileState {
    visible: Vec<u8>,
    media: Vec<u8>,
    durable_len: u64,
}

#[derive(Debug, Clone, Default)]
struct DirState {
    entries: BTreeMap<String, FileId>,
    durable: BTreeMap<String, FileId>,
}

#[derive(Debug, Clone)]
struct CachedWrite {
    seq: u64,
    file: FileId,
    offset: u64,
    data: Vec<u8>,
}

/// One recorded event, for replay comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TraceEvent {
    /// A write completed.
    Write {
        /// File.
        file: FileId,
        /// Offset.
        offset: u64,
        /// Bytes transferred.
        len: u64,
        /// Error code, or 0.
        err: i32,
    },
    /// A flush completed.
    Flush {
        /// File.
        file: FileId,
        /// Error code, or 0.
        err: i32,
    },
    /// A read completed.
    Read {
        /// File.
        file: FileId,
        /// Offset.
        offset: u64,
        /// Error code, or 0.
        err: i32,
    },
    /// A crash.
    Crash,
}

/// How a crash treats the volatile cache.
#[derive(Debug, Clone)]
pub enum CrashMode {
    /// Every cached write is lost.
    LoseAll,
    /// Every cached write reaches media.
    KeepAll,
    /// Bit `i` of the mask keeps cached write `i` (oldest first); at most 64.
    Subset(u64),
    /// Each cached write is kept, dropped or torn at random from the seed.
    Random,
}

/// The whole simulated state.
#[derive(Debug, Clone)]
pub struct World {
    cfg: SimConfig,
    rng: Rng,
    files: BTreeMap<FileId, FileState>,
    next_file: FileId,
    dirs: BTreeMap<PathBuf, DirState>,
    cache: Vec<CachedWrite>,
    completion_seq: u64,
    epoch: u64,
    locks: BTreeSet<FileId>,
    writes_completed: u64,
    flushes_completed: u64,
    allocated: u64,
    trace: Vec<TraceEvent>,
}

impl World {
    /// A new, empty world.
    #[must_use]
    pub fn new(cfg: SimConfig) -> Self {
        Self {
            rng: Rng::new(cfg.seed),
            cfg,
            files: BTreeMap::new(),
            next_file: 1,
            dirs: BTreeMap::new(),
            cache: Vec::new(),
            completion_seq: 0,
            epoch: 0,
            locks: BTreeSet::new(),
            writes_completed: 0,
            flushes_completed: 0,
            allocated: 0,
            trace: Vec::new(),
        }
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &SimConfig {
        &self.cfg
    }

    /// Mutable fault plan (to arm faults mid-run).
    pub fn faults_mut(&mut self) -> &mut FaultPlan {
        &mut self.cfg.faults
    }

    /// Handle epoch; bumped by every crash so stale handles fail.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Flushes (data and full) completed so far, successful or not. Arm
    /// `fail_flush = Some(flushes() + 1)` to fail the next one.
    #[must_use]
    pub fn flushes(&self) -> u64 {
        self.flushes_completed
    }

    /// Writes in the volatile cache.
    #[must_use]
    pub fn cached_writes(&self) -> usize {
        self.cache.len()
    }

    /// The event trace.
    #[must_use]
    pub fn trace(&self) -> &[TraceEvent] {
        &self.trace
    }

    /// The seeded generator (for queues choosing completion order).
    pub fn rng(&mut self) -> &mut Rng {
        &mut self.rng
    }

    // ----- directories ------------------------------------------------------

    pub(crate) fn open_dir(&mut self, path: PathBuf, create: bool) -> Result<(), OsError> {
        if self.dirs.contains_key(&path) {
            return Ok(());
        }
        if !create {
            return Err(ENOENT);
        }
        let _created = self.dirs.insert(path, DirState::default());
        Ok(())
    }

    pub(crate) fn create_file(&mut self, dir: &PathBuf, name: &str) -> Result<FileId, OsError> {
        let d = self.dirs.get_mut(dir).ok_or(ENOENT)?;
        if d.entries.contains_key(name) {
            return Err(EEXIST);
        }
        let id = self.next_file;
        self.next_file = self.next_file.checked_add(1).ok_or(EINVAL)?;
        let _new_entry = d.entries.insert(name.to_owned(), id);
        let _new_file = self.files.insert(id, FileState::default());
        Ok(id)
    }

    pub(crate) fn lookup(&self, dir: &PathBuf, name: &str) -> Result<FileId, OsError> {
        self.dirs
            .get(dir)
            .and_then(|d| d.entries.get(name))
            .copied()
            .ok_or(ENOENT)
    }

    pub(crate) fn rename(&mut self, dir: &PathBuf, from: &str, to: &str) -> Result<(), OsError> {
        let d = self.dirs.get_mut(dir).ok_or(ENOENT)?;
        let id = d.entries.remove(from).ok_or(ENOENT)?;
        let _replaced = d.entries.insert(to.to_owned(), id);
        Ok(())
    }

    pub(crate) fn unlink(&mut self, dir: &PathBuf, name: &str) -> Result<(), OsError> {
        let d = self.dirs.get_mut(dir).ok_or(ENOENT)?;
        d.entries.remove(name).map(|_| ()).ok_or(ENOENT)
    }

    pub(crate) fn sync_dir(&mut self, dir: &PathBuf) -> Result<(), OsError> {
        let d = self.dirs.get_mut(dir).ok_or(ENOENT)?;
        d.durable = d.entries.clone();
        Ok(())
    }

    // ----- locks -------------------------------------------------------------

    pub(crate) fn lock(&mut self, file: FileId) -> Result<(), OsError> {
        if self.locks.insert(file) {
            Ok(())
        } else {
            Err(EWOULDBLOCK)
        }
    }

    pub(crate) fn unlock(&mut self, file: FileId) {
        let _released = self.locks.remove(&file);
    }

    // ----- space and metadata ------------------------------------------------

    pub(crate) fn allocate(&mut self, file: FileId, len: u64) -> Result<(), OsError> {
        let f = self.files.get(&file).ok_or(EBADF)?;
        let cur = f.visible.len() as u64;
        if len > cur {
            let grow = len - cur;
            if let Some(cap) = self.cfg.faults.capacity {
                if self.allocated.saturating_add(grow) > cap {
                    return Err(ENOSPC);
                }
            }
            self.allocated += grow;
            let new_len = usize::try_from(len).map_err(|_| EINVAL)?;
            if let Some(f) = self.files.get_mut(&file) {
                f.visible.resize(new_len, 0);
            }
        }
        Ok(())
    }

    pub(crate) fn size(&self, file: FileId) -> Result<u64, OsError> {
        self.files
            .get(&file)
            .map(|f| f.visible.len() as u64)
            .ok_or(EBADF)
    }

    pub(crate) fn release(&mut self, file: FileId, offset: u64, len: u64) -> Result<(), OsError> {
        let f = self.files.get_mut(&file).ok_or(EBADF)?;
        let (s, e) = range(offset, len, f.visible.len()).ok_or(EINVAL)?;
        // Released contents are undefined: fill with a pattern no caller wrote.
        for (i, b) in f.visible[s..e].iter_mut().enumerate() {
            *b = 0xDE ^ (i as u8);
        }
        Ok(())
    }

    pub(crate) fn cached_pages(&self) -> bool {
        self.cfg.faults.cached_pages
    }

    pub(crate) fn flush_all(&mut self, file: FileId) -> Result<(), OsError> {
        self.flushes_completed += 1;
        if self.flush_fault() {
            self.fail_flush(u64::MAX);
            return Err(EIO);
        }
        if self.cfg.faults.lying_flush {
            return Ok(());
        }
        self.persist_through(u64::MAX);
        let f = self.files.get_mut(&file).ok_or(EBADF)?;
        f.durable_len = f.visible.len() as u64;
        f.media.resize(f.visible.len(), 0);
        Ok(())
    }

    // ----- data operations (called at completion time by the queue) ----------

    /// Snapshot of the completion sequence at flush submission.
    #[must_use]
    pub fn flush_cover(&self) -> u64 {
        self.completion_seq
    }

    pub(crate) fn complete_write(
        &mut self,
        file: FileId,
        offset: u64,
        data: &[u8],
        dsync: bool,
    ) -> Result<usize, OsError> {
        self.writes_completed += 1;
        let n = self.writes_completed;
        let faults = &self.cfg.faults;
        let fail = faults.fail_write == Some(n) || {
            let ppm = faults.write_eio_ppm;
            ppm > 0 && self.rng.chance(ppm)
        };
        if fail {
            self.trace.push(TraceEvent::Write {
                file,
                offset,
                len: 0,
                err: EIO.code,
            });
            return Err(EIO);
        }
        if self.cfg.faults.lose_write == Some(n) {
            self.trace.push(TraceEvent::Write {
                file,
                offset,
                len: data.len() as u64,
                err: 0,
            });
            return Ok(data.len());
        }
        let mut target = offset;
        if let Some((k, delta)) = self.cfg.faults.misdirect_write {
            if k == n {
                target = offset.checked_add_signed(delta).ok_or(EINVAL)?;
            }
        }
        let mut len = data.len();
        if self.cfg.faults.short_write == Some(n) {
            let lb = self.cfg.logical_block as usize;
            len = (len / 2) / lb * lb;
        }
        let data = &data[..len];
        let durable_now = dsync || self.cfg.device == DeviceKind::PowerSafe;
        let f = self.files.get_mut(&file).ok_or(EBADF)?;
        let end = usize::try_from(target)
            .ok()
            .and_then(|t| t.checked_add(len))
            .ok_or(EINVAL)?;
        if f.visible.len() < end {
            f.visible.resize(end, 0);
        }
        let start = end - len;
        f.visible[start..end].copy_from_slice(data);
        if durable_now {
            if f.media.len() < end {
                f.media.resize(end, 0);
            }
            f.media[start..end].copy_from_slice(data);
        } else {
            self.completion_seq += 1;
            self.cache.push(CachedWrite {
                seq: self.completion_seq,
                file,
                offset: target,
                data: data.to_vec(),
            });
        }
        self.trace.push(TraceEvent::Write {
            file,
            offset,
            len: len as u64,
            err: 0,
        });
        Ok(len)
    }

    pub(crate) fn complete_read(
        &mut self,
        file: FileId,
        offset: u64,
        out: &mut [u8],
    ) -> Result<usize, OsError> {
        let f = self.files.get(&file).ok_or(EBADF)?;
        let len = out.len() as u64;
        let end = offset.checked_add(len).ok_or(EINVAL)?;
        let bad = self
            .cfg
            .faults
            .bad_ranges
            .iter()
            .any(|&(bf, s, e)| bf == file && offset < e && s < end);
        if bad {
            self.trace.push(TraceEvent::Read {
                file,
                offset,
                err: EIO.code,
            });
            return Err(EIO);
        }
        let avail = f.visible.len() as u64;
        let n = if offset >= avail {
            0
        } else {
            (avail - offset).min(len) as usize
        };
        let s = offset as usize;
        out[..n].copy_from_slice(&f.visible[s..s + n]);
        self.trace.push(TraceEvent::Read {
            file,
            offset,
            err: 0,
        });
        Ok(n)
    }

    pub(crate) fn complete_flush(&mut self, file: FileId, cover: u64) -> Result<usize, OsError> {
        self.flushes_completed += 1;
        if self.flush_fault() {
            self.fail_flush(cover);
            self.trace.push(TraceEvent::Flush {
                file,
                err: EIO.code,
            });
            return Err(EIO);
        }
        if !self.cfg.faults.lying_flush {
            self.persist_through(cover);
            if let Some(f) = self.files.get_mut(&file) {
                f.durable_len = f.visible.len() as u64;
                f.media.resize(f.visible.len(), 0);
            }
        }
        self.trace.push(TraceEvent::Flush { file, err: 0 });
        Ok(0)
    }

    fn flush_fault(&mut self) -> bool {
        let n = self.flushes_completed;
        let faults = &self.cfg.faults;
        faults.fail_flush == Some(n) || {
            let ppm = faults.flush_eio_ppm;
            ppm > 0 && self.rng.chance(ppm)
        }
    }

    /// A failed flush: with `Drop`, the covered cached writes vanish and their
    /// visible bytes revert to media (the device lost them).
    fn fail_flush(&mut self, cover: u64) {
        if self.cfg.faults.flush_failure != FlushFailure::Drop {
            return;
        }
        let (lost, kept): (Vec<_>, Vec<_>) = self.cache.drain(..).partition(|w| w.seq <= cover);
        self.cache = kept;
        for w in lost {
            if let Some(f) = self.files.get_mut(&w.file) {
                let s = w.offset as usize;
                let e = s + w.data.len();
                for i in s..e.min(f.visible.len()) {
                    f.visible[i] = f.media.get(i).copied().unwrap_or(0);
                }
            }
        }
    }

    fn persist_through(&mut self, cover: u64) {
        let (done, kept): (Vec<_>, Vec<_>) = self.cache.drain(..).partition(|w| w.seq <= cover);
        self.cache = kept;
        for w in done {
            self.persist(&w, w.data.len());
        }
    }

    fn persist(&mut self, w: &CachedWrite, upto: usize) {
        if let Some(f) = self.files.get_mut(&w.file) {
            let s = w.offset as usize;
            let e = s + upto;
            if f.media.len() < e {
                f.media.resize(e, 0);
            }
            f.media[s..e].copy_from_slice(&w.data[..upto]);
        }
    }

    // ----- crash ---------------------------------------------------------------

    /// Simulates power loss.
    pub fn crash(&mut self, mode: &CrashMode) {
        let cached: Vec<CachedWrite> = std::mem::take(&mut self.cache);
        let lb = self.cfg.logical_block.max(1) as usize;
        for (i, w) in cached.iter().enumerate() {
            let keep = match mode {
                CrashMode::LoseAll => 0,
                CrashMode::KeepAll => w.data.len(),
                CrashMode::Subset(mask) => {
                    if i < 64 && mask & (1u64 << i) != 0 {
                        w.data.len()
                    } else {
                        0
                    }
                }
                CrashMode::Random => match self.rng.below(3) {
                    0 => 0,
                    1 => w.data.len(),
                    _ => {
                        let blocks = w.data.len() / lb;
                        (self.rng.below(blocks as u64 + 1) as usize) * lb
                    }
                },
            };
            if keep > 0 {
                self.persist(w, keep);
            }
        }
        for f in self.files.values_mut() {
            let len = usize::try_from(f.durable_len).unwrap_or(usize::MAX);
            f.media.resize(len, 0);
            f.visible = f.media.clone();
        }
        for d in self.dirs.values_mut() {
            d.entries = d.durable.clone();
        }
        self.locks.clear();
        self.epoch += 1;
        self.trace.push(TraceEvent::Crash);
    }

    /// The durable bytes of a file reachable at `dir/name`, as a crash would
    /// leave them right now (for oracles).
    #[must_use]
    pub fn media_of(&self, dir: &PathBuf, name: &str) -> Option<&[u8]> {
        let id = self.dirs.get(dir)?.durable.get(name)?;
        self.files.get(id).map(|f| &f.media[..])
    }

    /// The visible bytes of a file by id.
    #[must_use]
    pub fn visible(&self, file: FileId) -> Option<&[u8]> {
        self.files.get(&file).map(|f| &f.visible[..])
    }

    /// A stable hash of the trace, for same-seed-same-run checks.
    #[must_use]
    pub fn trace_hash(&self) -> u64 {
        let mut h: u64 = 0xCBF2_9CE4_8422_2325;
        let mut eat = |v: u64| {
            for b in v.to_le_bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0000_0100_0000_01B3);
            }
        };
        for e in &self.trace {
            match *e {
                TraceEvent::Write {
                    file,
                    offset,
                    len,
                    err,
                } => {
                    eat(1);
                    eat(u64::from(file));
                    eat(offset);
                    eat(len);
                    eat(err as u64);
                }
                TraceEvent::Flush { file, err } => {
                    eat(2);
                    eat(u64::from(file));
                    eat(err as u64);
                }
                TraceEvent::Read { file, offset, err } => {
                    eat(3);
                    eat(u64::from(file));
                    eat(offset);
                    eat(err as u64);
                }
                TraceEvent::Crash => eat(4),
            }
        }
        h
    }
}

fn range(offset: u64, len: u64, file_len: usize) -> Option<(usize, usize)> {
    let s = usize::try_from(offset).ok()?;
    let e = s.checked_add(usize::try_from(len).ok()?)?;
    (e <= file_len).then_some((s, e))
}
