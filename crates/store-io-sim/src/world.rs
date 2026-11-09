//! The simulated world: its devices (one by default), each power-safe or
//! with a volatile write cache, the files and directories on them, and the
//! rules for what survives a crash.
//!
//! # Model
//!
//! - A directory's files live on its device ([`World::place`]; device 0
//!   unless placed), each device with its own kind and durability evidence
//!   ([`SimConfig::with_device`]).
//!
//! - Every file has **visible** bytes (what reads return now) and **media**
//!   bytes (what survives a crash), plus a visible and a durable length.
//! - A completed write updates the visible bytes. On a power-safe device, or
//!   with `dsync` (FUA), it updates the media at completion. Otherwise it sits
//!   in the device's volatile cache, tagged with a completion sequence number.
//! - A flush persists exactly the flushed file's cached writes that
//!   completed **before the flush was submitted** (the NVMe Flush scope),
//!   and makes the file's length durable. A full flush persists every cached
//!   write of the file. A flush never makes another file's writes durable
//!   (a store must flush each of its files itself, MDEV-03), unless
//!   [`SimConfig::share_flushes`] models one device cache flushed for all.
//! - Losing one device ([`World::crash_device`]) is a crash of that device
//!   alone: its cached writes meet the crash mode's fates, and every handle
//!   open on its files and directories fails each later operation with
//!   `EIO`.
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

use std::collections::BTreeMap;
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
/// Simulated EROFS (a read-write open on read-only media).
pub const EROFS: OsError = OsError {
    code: 30,
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

/// One simulated device past the first ([`SimConfig::with_device`]).
#[derive(Debug, Clone)]
pub struct DeviceConfig {
    /// Its kind.
    pub kind: DeviceKind,
    /// Evidence its files' probes return (defaults derived from `kind`); its
    /// identity is the one `device_identity` reports (incomplete if none).
    pub evidence: Option<Evidence>,
}

/// Simulator configuration.
#[derive(Debug, Clone)]
pub struct SimConfig {
    /// Seed for every random choice.
    pub seed: u64,
    /// Device 0's kind.
    pub device: DeviceKind,
    /// Logical block size (tear granularity), bytes.
    pub logical_block: u32,
    /// Complete queued operations in a seeded random order instead of FIFO.
    pub reorder: bool,
    /// Faults to inject.
    pub faults: FaultPlan,
    /// Evidence returned by `probe` for device 0's files (defaults derived
    /// from `device`).
    pub evidence: Option<Evidence>,
    /// Devices past the first: device `d` is `more_devices[d - 1]`.
    pub more_devices: Vec<DeviceConfig>,
    /// A flush persists the cached writes of every file on its file's
    /// device, as one device cache flushed for all of them, instead of only
    /// its own file's (off by default, so a store relying on another file's
    /// flush is caught). Never another device's.
    pub share_flushes: bool,
    /// Model NTFS's valid data length: each file's valid data ends where
    /// its furthest write ended (a crash or a shrink cuts it back); the
    /// bytes a real file system would zero before a write starting past it
    /// are only counted, in [`World::gap_fill_bytes`] (neither zeroed nor
    /// read back as zeros: reads past the valid data return what the file
    /// holds), and `range_state` reports a range reaching past it not
    /// valid.
    pub valid_data_model: bool,
    /// The most writes the world completes in its life (default 500,000,
    /// far past any test's): a write that never stops (a bug, or a mutation
    /// of one) panics instead of exhausting memory, as does holding more
    /// than 1 GiB of unflushed writes in a volatile cache (a fixed bound). A
    /// benchmark that writes more raises it.
    pub runaway_writes: u64,
    /// Queues perform an operation at once when nothing is in flight
    /// (`Queue::perform`), as an inline synchronous queue does: the engine's
    /// fast path (no wait fault applies to an operation performed at once;
    /// one submitted, with something in flight, waits as before). The
    /// constructors' default is false (every operation submitted and waited
    /// for), or true when the environment variable `STORE_IO_SIM_INLINE` is
    /// `1`: the whole suite then runs on the fast path, as Linux's
    /// synchronous tier does.
    pub inline: bool,
}

/// [`SimConfig::inline`]'s default: `STORE_IO_SIM_INLINE` is `1` (read
/// once).
fn inline_default() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("STORE_IO_SIM_INLINE").is_some_and(|v| v == "1"))
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
            more_devices: Vec::new(),
            share_flushes: false,
            valid_data_model: false,
            runaway_writes: 500_000,
            inline: inline_default(),
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

    /// The configuration with one more device, of `kind` (its number is the
    /// devices before it: see [`World::place`]).
    #[must_use]
    pub fn with_device(mut self, kind: DeviceKind) -> Self {
        self.more_devices.push(DeviceConfig {
            kind,
            evidence: None,
        });
        self
    }

    /// The number of devices.
    #[must_use]
    pub fn devices(&self) -> usize {
        1 + self.more_devices.len()
    }

    /// Device `d`'s configuration (device 0's from `device` and
    /// `evidence`).
    ///
    /// # Panics
    ///
    /// If there is no device `d`.
    #[must_use]
    pub fn device(&self, d: usize) -> DeviceConfig {
        match d {
            0 => DeviceConfig {
                kind: self.device,
                evidence: self.evidence.clone(),
            },
            d => self.more_devices[d - 1].clone(),
        }
    }
}

/// Numbers each world, so the volumes of separate worlds never coincide.
static WORLDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// The most bytes a simulated world holds in a volatile cache at once (see
/// `SimConfig::runaway_writes`).
const RUNAWAY_CACHED_BYTES: usize = 1 << 30;

/// Identity of a simulated file.
pub type FileId = u32;

#[derive(Debug, Clone, Default)]
struct FileState {
    visible: Vec<u8>,
    media: Vec<u8>,
    durable_len: u64,
    /// Bytes nothing has written since a `Deallocate` release punched them,
    /// or since the file grew over them (`allocate`, `set_len`, a write past
    /// the end), `(start, end)`, sorted and apart: `range_state` reports
    /// them unwritten, as a real file system's holes and unwritten extents.
    holes: Vec<(u64, u64)>,
    /// Where the furthest write ended (NTFS's valid data length, see
    /// `SimConfig::valid_data_model`).
    valid_data: u64,
    /// The device it lives on: its directory's when it was created.
    device: usize,
}

/// Adds `[s, e)` to `ranges` (sorted, apart), merging what it touches.
fn mark(ranges: &mut Vec<(u64, u64)>, s: u64, e: u64) {
    if s >= e {
        return;
    }
    let (mut s, mut e) = (s, e);
    ranges.retain(|&(a, b)| {
        if b < s || a > e {
            return true;
        }
        s = s.min(a);
        e = e.max(b);
        false
    });
    let at = ranges.partition_point(|r| r.0 < s);
    ranges.insert(at, (s, e));
}

/// Removes `[s, e)` from `ranges`.
fn unmark(ranges: &mut Vec<(u64, u64)>, s: u64, e: u64) {
    if s >= e {
        return;
    }
    let mut out = Vec::with_capacity(ranges.len() + 1);
    for &(a, b) in ranges.iter() {
        if b <= s || a >= e {
            out.push((a, b));
            continue;
        }
        if a < s {
            out.push((a, s));
        }
        if b > e {
            out.push((e, b));
        }
    }
    *ranges = out;
}

#[derive(Debug, Clone, Default)]
struct DirState {
    entries: BTreeMap<String, FileId>,
    durable: BTreeMap<String, FileId>,
    /// The device it was created on ([`World::place`]): its files are
    /// created there.
    device: usize,
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
        /// Bytes asked for (not part of [`World::trace_hash`]).
        len: u64,
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
    /// Each cached write's fate, oldest first (a crash enumeration's: every
    /// combination, tears included); a write past the list is lost.
    Exact(Vec<Fate>),
}

/// What a crash leaves of one cached write ([`CrashMode::Exact`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fate {
    /// None of it reaches media.
    Lost,
    /// All of it does.
    Kept,
    /// Its first `n` logical blocks do (a torn write; at most its length).
    Torn(usize),
}

/// A test hook, run with the world unlocked: at the start of every
/// `Queue::submit` (to observe what the engine published before submitting)
/// or of every `Platform::release_range` (to observe the caller's locks).
#[derive(Clone, Default)]
pub(crate) struct SubmitProbe(pub(crate) Option<std::sync::Arc<dyn Fn() + Send + Sync>>);

impl core::fmt::Debug for SubmitProbe {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(if self.0.is_some() {
            "SubmitProbe(set)"
        } else {
            "SubmitProbe(none)"
        })
    }
}

/// The hook run before each step ([`World::set_step_probe`]); `true` cuts
/// the power there.
#[derive(Clone, Default)]
pub(crate) struct StepProbe(pub(crate) Option<std::sync::Arc<dyn Fn() -> bool + Send + Sync>>);

impl core::fmt::Debug for StepProbe {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(if self.0.is_some() {
            "StepProbe(set)"
        } else {
            "StepProbe(none)"
        })
    }
}

/// The whole simulated state.
#[derive(Debug, Clone)]
pub struct World {
    cfg: SimConfig,
    /// This world's number ([`WORLDS`]).
    id: u64,
    /// Each placed directory's device ([`World::place`]).
    placement: BTreeMap<PathBuf, usize>,
    /// Handles opened so far: each handle's serial.
    opens: u64,
    /// Per device: handles with serials up to this one were open when it
    /// was lost ([`World::crash_device`]) and fail every operation.
    lost_through: Vec<u64>,
    rng: Rng,
    files: BTreeMap<FileId, FileState>,
    next_file: FileId,
    dirs: BTreeMap<PathBuf, DirState>,
    cache: Vec<CachedWrite>,
    /// The bytes `cache` holds.
    cached_bytes: usize,
    completion_seq: u64,
    epoch: u64,
    /// Lock holders per file: `None` is an exclusive holder, `Some(n)` is
    /// `n` shared holders.
    locks: BTreeMap<FileId, Option<u32>>,
    writes_completed: u64,
    reads_completed: u64,
    misaligned_reads: u64,
    clock_ns: u64,
    waits: u64,
    write_submits: u64,
    lock_calls: u64,
    pub(crate) submit_probe: SubmitProbe,
    /// Run first by every `Platform::release_range`, outside the world lock.
    pub(crate) release_probe: SubmitProbe,
    pub(crate) step_probe: StepProbe,
    /// The power is off ([`World::set_step_probe`]): every step and every
    /// completion fails with EIO, changing nothing, until the next crash.
    pub(crate) powered_off: bool,
    flushes_completed: u64,
    dir_syncs: u64,
    reopens: u64,
    releases: u64,
    range_allocations: u64,
    gap_fill_bytes: u64,
    queues: u64,
    set_lens: u64,
    allocated: u64,
    trace: Vec<TraceEvent>,
}

impl World {
    /// A new, empty world.
    #[must_use]
    pub fn new(cfg: SimConfig) -> Self {
        Self {
            rng: Rng::new(cfg.seed),
            id: WORLDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            placement: BTreeMap::new(),
            opens: 0,
            lost_through: vec![0; cfg.devices()],
            cfg,
            files: BTreeMap::new(),
            next_file: 1,
            dirs: BTreeMap::new(),
            cache: Vec::new(),
            cached_bytes: 0,
            completion_seq: 0,
            epoch: 0,
            locks: BTreeMap::new(),
            writes_completed: 0,
            reads_completed: 0,
            misaligned_reads: 0,
            clock_ns: 0,
            waits: 0,
            write_submits: 0,
            lock_calls: 0,
            submit_probe: SubmitProbe::default(),
            release_probe: SubmitProbe::default(),
            step_probe: StepProbe::default(),
            powered_off: false,
            flushes_completed: 0,
            dir_syncs: 0,
            reopens: 0,
            releases: 0,
            range_allocations: 0,
            gap_fill_bytes: 0,
            queues: 0,
            set_lens: 0,
            allocated: 0,
            trace: Vec::new(),
        }
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &SimConfig {
        &self.cfg
    }

    /// The fault plan in force.
    #[must_use]
    pub fn faults(&self) -> &FaultPlan {
        &self.cfg.faults
    }

    /// Sets (or clears, back to the device's own) the evidence `probe`
    /// returns from now on (a store created after it sees a new device).
    pub fn set_evidence(&mut self, ev: Option<Evidence>) {
        self.cfg.evidence = ev;
    }

    /// Sets (or clears) the evidence device `d`'s files' probes return from
    /// now on (device 0: as [`Self::set_evidence`]).
    ///
    /// # Panics
    ///
    /// If there is no device `d`.
    pub fn set_device_evidence(&mut self, d: usize, ev: Option<Evidence>) {
        match d {
            0 => self.cfg.evidence = ev,
            d => self.cfg.more_devices[d - 1].evidence = ev,
        }
    }

    /// Puts `dir` and every directory below it (unless placed itself) on
    /// device `d`: each directory created from now on lives on the device
    /// its nearest placed ancestor names, and its files with it. A
    /// directory, and a file, stays on the device it was created on.
    ///
    /// # Panics
    ///
    /// If there is no device `d`.
    pub fn place(&mut self, dir: impl Into<PathBuf>, d: usize) {
        assert!(d < self.cfg.devices(), "no device {d}");
        let _was = self.placement.insert(dir.into(), d);
    }

    /// The device a directory created at `dir` now would live on.
    fn device_of_dir(&self, dir: &std::path::Path) -> usize {
        dir.ancestors()
            .find_map(|a| self.placement.get(a).copied())
            .unwrap_or(0)
    }

    /// The device `file` lives on.
    #[must_use]
    pub fn device_of(&self, file: FileId) -> usize {
        self.files.get(&file).map_or(0, |f| f.device)
    }

    /// The configuration of the device `file` lives on.
    pub(crate) fn device_config(&self, file: FileId) -> DeviceConfig {
        self.cfg.device(self.device_of(file))
    }

    /// The volume number of the device `file` lives on, qualified by the
    /// world (`FileIdentity::volume`).
    pub(crate) fn volume_of(&self, file: FileId) -> u64 {
        (self.id << 16) | self.device_of(file) as u64
    }

    /// A serial for a handle being opened.
    pub(crate) fn next_open(&mut self) -> u64 {
        self.opens += 1;
        self.opens
    }

    /// Whether the handle `opened` of the directory `dir` was open when its
    /// device was lost.
    pub(crate) fn dir_lost(&self, dir: &PathBuf, opened: u64) -> bool {
        let device = self.dirs.get(dir).map_or(0, |d| d.device);
        self.lost_through
            .get(device)
            .is_some_and(|&through| opened <= through)
    }

    /// Whether the handle `opened` of `file` was open when its device was
    /// lost.
    pub(crate) fn handle_lost(&self, file: FileId, opened: u64) -> bool {
        self.lost_through
            .get(self.device_of(file))
            .is_some_and(|&through| opened <= through)
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

    /// The id of the file `name` in `dir`, for targeting faults such as
    /// [`FaultPlan::bad_ranges`](crate::FaultPlan::bad_ranges).
    #[must_use]
    pub fn file_id(&self, dir: &PathBuf, name: &str) -> Option<FileId> {
        self.lookup(dir, name).ok()
    }

    /// Writes completed so far, successful or not. Arm
    /// `fail_write = Some(writes() + k)` to fail the k-th next one.
    #[must_use]
    pub fn writes(&self) -> u64 {
        self.writes_completed
    }

    /// Counts a queue wait; the raw error if the fault plan fails this one.
    pub(crate) fn wait_fails(&mut self) -> Option<OsError> {
        self.waits += 1;
        match self.cfg.faults.fail_wait {
            Some((at, code)) if at == self.waits => Some(OsError {
                code,
                source: OsErrorSource::Sim,
            }),
            _ => None,
        }
    }

    /// Counts a write submission; the raw error if the fault plan refuses
    /// this one.
    pub(crate) fn submit_fails(&mut self) -> Option<OsError> {
        self.write_submits += 1;
        match self.cfg.faults.fail_submit {
            Some((at, code)) if at == self.write_submits => Some(OsError {
                code,
                source: OsErrorSource::Sim,
            }),
            _ => None,
        }
    }

    /// Sets (or clears) the hook every `Queue::submit` runs first.
    pub fn set_submit_probe(&mut self, probe: Option<std::sync::Arc<dyn Fn() + Send + Sync>>) {
        self.submit_probe = SubmitProbe(probe);
    }

    /// Sets (or clears) the hook run before every step that can change what
    /// a crash leaves: each operation a queue accepts (`Queue::submit` after
    /// the submit probe, and `Queue::perform`; reads too, which change
    /// nothing but order the rest) and each `Platform::create_file`,
    /// rename, `unlink`, `sync_dir`, `allocate`, `set_len`,
    /// `release_range`, `allocate_range` and `flush_all`; on the caller's
    /// thread and outside the world lock (a crash enumeration's points),
    /// and `open_dir` creating. A probe returning `true` cuts the power
    /// there, as a crash enumeration does: that step and every later one
    /// fail with EIO, and an operation in flight completes with EIO,
    /// unapplied; nothing reaches the media until the next crash, which
    /// turns the power back on.
    pub fn set_step_probe(
        &mut self,
        probe: Option<std::sync::Arc<dyn Fn() -> bool + Send + Sync>>,
    ) {
        self.step_probe = StepProbe(probe);
    }

    /// The power is off ([`World::set_step_probe`]).
    #[must_use]
    pub fn powered_off(&self) -> bool {
        self.powered_off
    }

    /// Sets (or clears) the hook every `Platform::release_range` runs first,
    /// on the caller's thread and outside the world lock.
    pub fn set_release_probe(&mut self, probe: Option<std::sync::Arc<dyn Fn() + Send + Sync>>) {
        self.release_probe = SubmitProbe(probe);
    }

    /// Lock calls so far (see [`FaultPlan::fail_lock`]).
    #[must_use]
    pub fn lock_calls(&self) -> u64 {
        self.lock_calls
    }

    /// Write submissions so far (see [`FaultPlan::fail_submit`]).
    #[must_use]
    pub fn write_submits(&self) -> u64 {
        self.write_submits
    }

    /// Queue waits so far (see [`FaultPlan::fail_wait`]).
    #[must_use]
    pub fn waits(&self) -> u64 {
        self.waits
    }

    /// Directory syncs so far, successful or not. Arm
    /// `fail_dir_sync = Some(dir_syncs() + 1)` to fail the next one.
    #[must_use]
    pub fn dir_syncs(&self) -> u64 {
        self.dir_syncs
    }

    /// Reopens of a writable file so far, made or failed. Arm
    /// `fail_reopen = Some(reopens() + 1)` to fail the next one.
    #[must_use]
    pub fn reopens(&self) -> u64 {
        self.reopens
    }

    /// `Platform::release_range` calls so far, failed or not. Arm
    /// `fail_release = Some((releases() + 1, code))` to fail the next one.
    #[must_use]
    pub fn releases(&self) -> u64 {
        self.releases
    }

    /// `Platform::allocate_range` calls so far, failed or not. Arm
    /// `fail_allocate_range = Some((range_allocations() + 1, code))` to fail
    /// the next one.
    #[must_use]
    pub fn range_allocations(&self) -> u64 {
        self.range_allocations
    }

    /// Queues made so far (`Platform::queue`).
    #[must_use]
    pub fn queues(&self) -> u64 {
        self.queues
    }

    /// Counts a queue made.
    pub(crate) fn queue_made(&mut self) {
        self.queues += 1;
    }

    /// Reads completed so far, successful or not (misaligned ones excepted:
    /// see [`Self::misaligned_reads`]).
    #[must_use]
    pub fn reads(&self) -> u64 {
        self.reads_completed
    }

    /// The simulated monotonic clock, in milliseconds (starts at 0; moves
    /// only when a test advances it).
    #[must_use]
    pub fn clock_ms(&self) -> u64 {
        self.clock_ns / 1_000_000
    }

    /// The simulated monotonic clock, in nanoseconds (the same clock as
    /// [`Self::clock_ms`]).
    #[must_use]
    pub fn clock_ns(&self) -> u64 {
        self.clock_ns
    }

    /// Advances the simulated clock by `ms` milliseconds.
    pub fn advance_clock(&mut self, ms: u64) {
        self.clock_ns = self.clock_ns.saturating_add(ms.saturating_mul(1_000_000));
    }

    /// Advances the simulated clock by `ns` nanoseconds.
    pub fn advance_clock_ns(&mut self, ns: u64) {
        self.clock_ns = self.clock_ns.saturating_add(ns);
    }

    /// This world's id: unique in the process, the high bits of every
    /// volume number it hands out and its platform's identity scope.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Reads refused with EINVAL for breaking the device's alignment.
    #[must_use]
    pub fn misaligned_reads(&self) -> u64 {
        self.misaligned_reads
    }

    pub(crate) fn count_misaligned_read(&mut self) {
        self.misaligned_reads += 1;
    }

    /// Writes in the volatile cache.
    #[must_use]
    pub fn cached_writes(&self) -> usize {
        self.cache.len()
    }

    /// Each cached write, oldest first: its file, its offset, and its length
    /// in logical blocks (the tears [`CrashMode::Exact`] can make of it).
    #[must_use]
    pub fn cached_extents(&self) -> Vec<(FileId, u64, usize)> {
        let lb = self.cfg.logical_block.max(1) as usize;
        self.cache
            .iter()
            .map(|w| (w.file, w.offset, w.data.len().div_ceil(lb)))
            .collect()
    }

    /// The process dies and the machine stays up: its locks are released
    /// and its handles stop working (as after a crash), but the cache and
    /// the media stay as they are, so a write it completed still reaches
    /// the media at the next flush of its file, by anyone.
    pub fn kill(&mut self) {
        self.locks.clear();
        self.epoch += 1;
    }

    /// Turns the power back on without a crash, the cache intact: the
    /// device a step probe stopped works again (a process frozen at a step,
    /// then killed: [`World::kill`]).
    pub fn restore_power(&mut self) {
        self.powered_off = false;
    }

    /// Cuts the power ([`World::set_step_probe`]): every step and every
    /// completion fails with EIO until the next crash.
    pub fn cut_power(&mut self) {
        self.powered_off = true;
    }

    /// A journaling file system's metadata commit: every directory's
    /// entries and every file's length durable now, the data as it is (a
    /// crash then keeps a name or a length the store never synced; within
    /// the old durable length, bytes it never flushed read as they were,
    /// and past it as zeros, unwritten).
    pub fn settle_metadata(&mut self) {
        for f in self.files.values_mut() {
            f.durable_len = f.visible.len() as u64;
        }
        for d in self.dirs.values_mut() {
            d.durable = d.entries.clone();
        }
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
        if self.cfg.faults.read_only_media {
            return Err(EROFS);
        }
        let device = self.device_of_dir(&path);
        let _created = self.dirs.insert(
            path,
            DirState {
                device,
                ..DirState::default()
            },
        );
        Ok(())
    }

    pub(crate) fn create_file(&mut self, dir: &PathBuf, name: &str) -> Result<FileId, OsError> {
        if self.cfg.faults.read_only_media {
            return Err(EROFS);
        }
        let d = self.dirs.get_mut(dir).ok_or(ENOENT)?;
        if d.entries.contains_key(name) {
            return Err(EEXIST);
        }
        let id = self.next_file;
        self.next_file = self.next_file.checked_add(1).ok_or(EINVAL)?;
        let _new_entry = d.entries.insert(name.to_owned(), id);
        let device = d.device;
        let _new_file = self.files.insert(
            id,
            FileState {
                device,
                ..FileState::default()
            },
        );
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
        if self.cfg.faults.read_only_media {
            return Err(EROFS);
        }
        let d = self.dirs.get_mut(dir).ok_or(ENOENT)?;
        let id = d.entries.remove(from).ok_or(ENOENT)?;
        let _replaced = d.entries.insert(to.to_owned(), id);
        Ok(())
    }

    /// [`Self::rename`], refusing an existing `to` (`EEXIST`).
    pub(crate) fn rename_noreplace(
        &mut self,
        dir: &PathBuf,
        from: &str,
        to: &str,
    ) -> Result<(), OsError> {
        if self.cfg.faults.read_only_media {
            return Err(EROFS);
        }
        let d = self.dirs.get_mut(dir).ok_or(ENOENT)?;
        if d.entries.contains_key(to) {
            return Err(EEXIST);
        }
        let id = d.entries.remove(from).ok_or(ENOENT)?;
        let _none = d.entries.insert(to.to_owned(), id);
        Ok(())
    }

    pub(crate) fn unlink(&mut self, dir: &PathBuf, name: &str) -> Result<(), OsError> {
        if self.cfg.faults.read_only_media {
            return Err(EROFS);
        }
        let d = self.dirs.get_mut(dir).ok_or(ENOENT)?;
        d.entries.remove(name).map(|_| ()).ok_or(ENOENT)
    }

    /// Counts a reopen of a writable file: false when the plan fails it.
    pub(crate) fn reopen(&mut self) -> bool {
        self.reopens += 1;
        self.cfg.faults.fail_reopen != Some(self.reopens)
    }

    pub(crate) fn sync_dir(&mut self, dir: &PathBuf) -> Result<(), OsError> {
        self.dir_syncs += 1;
        if self.cfg.faults.dir_sync_unsupported {
            return Err(OsError {
                code: 95,
                source: OsErrorSource::Sim,
            });
        }
        if self.cfg.faults.fail_dir_sync == Some(self.dir_syncs) {
            return Err(EIO);
        }
        let d = self.dirs.get_mut(dir).ok_or(ENOENT)?;
        d.durable = d.entries.clone();
        Ok(())
    }

    // ----- locks -------------------------------------------------------------

    pub(crate) fn lock(&mut self, file: FileId, shared: bool) -> Result<(), OsError> {
        self.lock_calls += 1;
        if let Some((dir, from, to)) = self.cfg.faults.rename_before_lock.take() {
            self.rename(&dir, &from, &to)?;
        }
        if let Some((at, code)) = self.cfg.faults.fail_lock {
            if at == self.lock_calls {
                return Err(OsError {
                    code,
                    source: OsErrorSource::Sim,
                });
            }
        }
        match (self.locks.get_mut(&file), shared) {
            (None, _) => {
                let _none = self.locks.insert(file, shared.then_some(1));
                Ok(())
            }
            (Some(Some(n)), true) => {
                *n += 1;
                Ok(())
            }
            (Some(_), _) => Err(EWOULDBLOCK),
        }
    }

    pub(crate) fn unlock(&mut self, file: FileId) {
        match self.locks.get_mut(&file) {
            Some(Some(n)) if *n > 1 => *n -= 1,
            _ => {
                let _released = self.locks.remove(&file);
            }
        }
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
                mark(&mut f.holes, cur, len);
            }
        }
        Ok(())
    }

    pub(crate) fn set_len(&mut self, file: FileId, len: u64) -> Result<(), OsError> {
        self.set_lens += 1;
        if self.cfg.faults.fail_set_len == Some(self.set_lens) {
            return Err(EIO);
        }
        let cur = self.files.get(&file).ok_or(EBADF)?.visible.len() as u64;
        if len >= cur {
            return self.allocate(file, len);
        }
        let new_len = usize::try_from(len).map_err(|_| EINVAL)?;
        self.allocated = self.allocated.saturating_sub(cur - len);
        if let Some(f) = self.files.get_mut(&file) {
            f.visible.truncate(new_len);
            unmark(&mut f.holes, len, u64::MAX);
            f.valid_data = f.valid_data.min(len);
        }
        Ok(())
    }

    pub(crate) fn size(&self, file: FileId) -> Result<u64, OsError> {
        self.files
            .get(&file)
            .map(|f| f.visible.len() as u64)
            .ok_or(EBADF)
    }

    pub(crate) fn release(
        &mut self,
        file: FileId,
        offset: u64,
        len: u64,
        hole: bool,
    ) -> Result<(), OsError> {
        self.releases += 1;
        if let Some((at, code)) = self.cfg.faults.fail_release {
            if at == self.releases {
                return Err(OsError {
                    code,
                    source: OsErrorSource::Sim,
                });
            }
        }
        let f = self.files.get_mut(&file).ok_or(EBADF)?;
        let (s, e) = range(offset, len, f.visible.len()).ok_or(EINVAL)?;
        // Released contents are undefined: fill with a pattern no caller wrote.
        for (i, b) in f.visible[s..e].iter_mut().enumerate() {
            *b = 0xDE ^ (i as u8);
        }
        if hole {
            mark(&mut f.holes, s as u64, e as u64);
        }
        Ok(())
    }

    /// Bytes every file has grown by through allocation so far, the total
    /// `FaultPlan::capacity` is compared with (set it to this to fail any
    /// further growth).
    #[must_use]
    pub fn allocated_bytes(&self) -> u64 {
        self.allocated
    }

    /// Bytes a valid-data model's file system zeroed before writes that
    /// started past a file's valid data (see `SimConfig::valid_data_model`;
    /// counted whether or not the model is on).
    #[must_use]
    pub fn gap_fill_bytes(&self) -> u64 {
        self.gap_fill_bytes
    }

    /// Whether `[offset, offset + len)` reaches past the file's valid data
    /// (always false without the valid-data model).
    pub(crate) fn past_valid_data(&self, file: FileId, offset: u64, len: u64) -> bool {
        self.cfg.valid_data_model
            && self
                .files
                .get(&file)
                .is_some_and(|f| offset.saturating_add(len) > f.valid_data)
    }

    /// `Platform::allocate_range`: inside the file, no byte changed (a
    /// hole stays unwritten, as an unwritten extent reads).
    pub(crate) fn allocate_range(
        &mut self,
        file: FileId,
        offset: u64,
        len: u64,
    ) -> Result<(), OsError> {
        self.range_allocations += 1;
        if let Some((at, code)) = self.cfg.faults.fail_allocate_range {
            if at == self.range_allocations {
                return Err(OsError {
                    code,
                    source: OsErrorSource::Sim,
                });
            }
        }
        let f = self.files.get(&file).ok_or(EBADF)?;
        let end = offset.checked_add(len).ok_or(EINVAL)?;
        if end > f.visible.len() as u64 {
            return Err(EINVAL);
        }
        Ok(())
    }

    /// Whether any of `[offset, offset + len)` of `file` reads as never
    /// written (a hole, a growth not yet written, or past the end): what
    /// `range_state` reports as `unwritten`.
    #[must_use]
    pub fn is_unwritten(&self, file: FileId, offset: u64, len: u64) -> bool {
        self.unwritten(file, offset, len).unwrap_or(true)
    }

    /// Whether any of `[offset, offset + len)` is a hole (see `holes`) or
    /// past the end of the file.
    pub(crate) fn unwritten(&self, file: FileId, offset: u64, len: u64) -> Result<bool, OsError> {
        let f = self.files.get(&file).ok_or(EBADF)?;
        if len == 0 {
            return Ok(false);
        }
        let end = offset.saturating_add(len);
        Ok(end > f.visible.len() as u64 || f.holes.iter().any(|&(a, b)| a < end && b > offset))
    }

    pub(crate) fn cached_pages(&self) -> bool {
        self.cfg.faults.cached_pages
    }

    pub(crate) fn flush_all(&mut self, file: FileId) -> Result<(), OsError> {
        self.flushes_completed += 1;
        if self.flush_fault() {
            self.fail_flush(u64::MAX, file);
            return Err(EIO);
        }
        if self.cfg.faults.lying_flush {
            return Ok(());
        }
        self.persist_through(u64::MAX, file);
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
        // No test writes this much: a write that never stops (a bug, or a
        // mutation of one) fails here instead of exhausting memory with
        // its trace and cached copies.
        assert!(
            n <= self.cfg.runaway_writes,
            "{n} writes: a runaway (SimConfig::runaway_writes)"
        );
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
        // The file system marks the range it was asked to write as written,
        // whatever the device does with the data (a short write: only what
        // it transferred, below).
        let asked = (offset, offset.saturating_add(data.len() as u64));
        if self.cfg.faults.lose_write == Some(n) {
            self.trace.push(TraceEvent::Write {
                file,
                offset,
                len: data.len() as u64,
                err: 0,
            });
            if let Some(f) = self.files.get_mut(&file) {
                unmark(&mut f.holes, asked.0, asked.1);
                let gap = asked.0.saturating_sub(f.valid_data);
                f.valid_data = f.valid_data.max(asked.1);
                self.gap_fill_bytes += gap;
            }
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
        let durable_now = dsync || self.device_config(file).kind == DeviceKind::PowerSafe;
        let f = self.files.get_mut(&file).ok_or(EBADF)?;
        let end = usize::try_from(target)
            .ok()
            .and_then(|t| t.checked_add(len))
            .ok_or(EINVAL)?;
        if f.visible.len() < end {
            let old = f.visible.len() as u64;
            f.visible.resize(end, 0);
            mark(&mut f.holes, old, end as u64);
        }
        let start = end - len;
        f.visible[start..end].copy_from_slice(data);
        // Where the write was asked to go (a misdirected one lands
        // elsewhere, but its own range is what the file system marks).
        unmark(&mut f.holes, asked.0, asked.0.saturating_add(len as u64));
        let gap = asked.0.saturating_sub(f.valid_data);
        f.valid_data = f.valid_data.max(asked.0.saturating_add(len as u64));
        if durable_now {
            if f.media.len() < end {
                f.media.resize(end, 0);
            }
            f.media[start..end].copy_from_slice(data);
        } else {
            self.completion_seq += 1;
            self.cached_bytes += data.len();
            assert!(
                self.cached_bytes <= RUNAWAY_CACHED_BYTES,
                "{} bytes cached, not flushed: a runaway",
                self.cached_bytes
            );
            self.cache.push(CachedWrite {
                seq: self.completion_seq,
                file,
                offset: target,
                data: data.to_vec(),
            });
        }
        self.gap_fill_bytes += gap;
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
        self.reads_completed += 1;
        let nth = self.reads_completed;
        let f = self.files.get(&file).ok_or(EBADF)?;
        let len = out.len() as u64;
        let end = offset.checked_add(len).ok_or(EINVAL)?;
        // A targeted fault ("the n-th read fails with this code") wins over
        // a persistent bad range: it is the more specific instruction.
        if let Some((at, code)) = self.cfg.faults.fail_read {
            if at == nth {
                self.trace.push(TraceEvent::Read {
                    file,
                    offset,
                    len,
                    err: code,
                });
                return Err(OsError {
                    code,
                    source: OsErrorSource::Sim,
                });
            }
        }
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
                len,
                err: EIO.code,
            });
            return Err(EIO);
        }
        let avail = f.visible.len() as u64;
        let mut n = if offset >= avail {
            0
        } else {
            (avail - offset).min(len) as usize
        };
        if self.cfg.faults.short_read == Some(nth) {
            let lb = self.cfg.logical_block.max(1) as usize;
            n = n / 2 / lb * lb;
        }
        // A read wholly past the end of the file transfers nothing.
        if n > 0 {
            let s = offset as usize;
            out[..n].copy_from_slice(&f.visible[s..s + n]);
        }
        self.trace.push(TraceEvent::Read {
            file,
            offset,
            len,
            err: 0,
        });
        Ok(n)
    }

    pub(crate) fn complete_flush(&mut self, file: FileId, cover: u64) -> Result<usize, OsError> {
        self.flushes_completed += 1;
        if self.flush_fault() {
            self.fail_flush(cover, file);
            self.trace.push(TraceEvent::Flush {
                file,
                err: EIO.code,
            });
            return Err(EIO);
        }
        if !self.cfg.faults.lying_flush {
            self.persist_through(cover, file);
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

    /// Whether a flush of `file` covering `cover` covers the cached write
    /// `w`: one of its own file's, completed before the flush was submitted
    /// (every file's with [`SimConfig::share_flushes`]).
    fn covers(&self, w: &CachedWrite, cover: u64, file: FileId) -> bool {
        w.seq <= cover
            && (w.file == file
                || (self.cfg.share_flushes && self.device_of(w.file) == self.device_of(file)))
    }

    /// A failed flush of `file`: with `Drop`, the covered cached writes
    /// vanish and their visible bytes revert to media (the device lost
    /// them).
    fn fail_flush(&mut self, cover: u64, file: FileId) {
        if self.cfg.faults.flush_failure != FlushFailure::Drop {
            return;
        }
        let (lost, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.cache)
            .into_iter()
            .partition(|w| self.covers(w, cover, file));
        self.cache = kept;
        self.cached_bytes = self.cache.iter().map(|w| w.data.len()).sum();
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

    fn persist_through(&mut self, cover: u64, file: FileId) {
        let (done, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.cache)
            .into_iter()
            .partition(|w| self.covers(w, cover, file));
        self.cache = kept;
        self.cached_bytes = self.cache.iter().map(|w| w.data.len()).sum();
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
        self.cached_bytes = 0;
        let lb = self.cfg.logical_block.max(1) as usize;
        // Each file's media before the crash, and what of the cache reaches
        // it (file, start, end).
        let before: BTreeMap<FileId, u64> = self
            .files
            .iter()
            .map(|(&id, f)| (id, f.media.len() as u64))
            .collect();
        let mut reached: Vec<(FileId, u64, u64)> = Vec::new();
        for (i, w) in cached.iter().enumerate() {
            let keep = kept(&mut self.rng, i, w, mode, lb);
            if keep > 0 {
                self.persist(w, keep);
                reached.push((w.file, w.offset, w.offset + keep as u64));
            }
        }
        for (id, f) in &mut self.files {
            revert(*id, f, &before, &reached);
        }
        for d in self.dirs.values_mut() {
            d.entries = d.durable.clone();
        }
        self.locks.clear();
        self.powered_off = false;
        self.epoch += 1;
        self.trace.push(TraceEvent::Crash);
    }

    /// Loses device `d` alone, as a crash of that device: its files' cached
    /// writes are lost, they and its directories revert to what is durable,
    /// and every handle open on its files fails each later operation
    /// (completions of operations already submitted included) with `EIO`.
    /// Other devices, the locks and later opens are unaffected (the device
    /// is back, with its media, for a new open).
    ///
    /// # Panics
    ///
    /// If there is no device `d`.
    pub fn crash_device(&mut self, d: usize, mode: &CrashMode) {
        assert!(d < self.cfg.devices(), "no device {d}");
        let device = |w: &World, file: FileId| w.files.get(&file).map_or(0, |f| f.device);
        let (lost, others): (Vec<_>, Vec<_>) = std::mem::take(&mut self.cache)
            .into_iter()
            .partition(|w| device(self, w.file) == d);
        self.cache = others;
        self.cached_bytes = self.cache.iter().map(|w| w.data.len()).sum();
        let lb = self.cfg.logical_block.max(1) as usize;
        let before: BTreeMap<FileId, u64> = self
            .files
            .iter()
            .map(|(&id, f)| (id, f.media.len() as u64))
            .collect();
        // The mode's fates are this device's cached writes', oldest first.
        let mut reached: Vec<(FileId, u64, u64)> = Vec::new();
        for (i, w) in lost.iter().enumerate() {
            let keep = kept(&mut self.rng, i, w, mode, lb);
            if keep > 0 {
                self.persist(w, keep);
                reached.push((w.file, w.offset, w.offset + keep as u64));
            }
        }
        for (id, f) in self.files.iter_mut().filter(|(_, f)| f.device == d) {
            revert(*id, f, &before, &reached);
        }
        for dir in self.dirs.values_mut().filter(|dir| dir.device == d) {
            dir.entries = dir.durable.clone();
        }
        self.lost_through[d] = self.opens;
        self.trace.push(TraceEvent::Crash);
    }

    /// Each cached write of device `d`, oldest first: its file, offset and
    /// length in logical blocks (the fates [`Self::crash_device`] gives).
    #[must_use]
    pub fn cached_extents_of(&self, d: usize) -> Vec<(FileId, u64, usize)> {
        let lb = self.cfg.logical_block.max(1) as usize;
        self.cache
            .iter()
            .filter(|w| self.device_of(w.file) == d)
            .map(|w| (w.file, w.offset, w.data.len().div_ceil(lb)))
            .collect()
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
                TraceEvent::Read {
                    file, offset, err, ..
                } => {
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

/// How much of the cached write `w`, the `i`-th oldest, a crash under
/// `mode` keeps.
fn kept(rng: &mut Rng, i: usize, w: &CachedWrite, mode: &CrashMode, lb: usize) -> usize {
    match mode {
        CrashMode::LoseAll => 0,
        CrashMode::KeepAll => w.data.len(),
        CrashMode::Subset(mask) => {
            if i < 64 && mask & (1u64 << i) != 0 {
                w.data.len()
            } else {
                0
            }
        }
        CrashMode::Exact(fates) => match fates.get(i) {
            Some(Fate::Kept) => w.data.len(),
            Some(Fate::Torn(n)) => n.saturating_mul(lb).min(w.data.len()),
            Some(Fate::Lost) | None => 0,
        },
        CrashMode::Random => match rng.below(3) {
            0 => 0,
            1 => w.data.len(),
            _ => {
                let blocks = w.data.len() / lb;
                (rng.below(blocks as u64 + 1) as usize) * lb
            }
        },
    }
}

/// A crash's effect on one file: back to its media, cut to its durable
/// length. `before` holds each file's media length before the crash's kept
/// writes reached it, `reached` the ranges they wrote (file, start, end).
fn revert(
    id: FileId,
    f: &mut FileState,
    before: &BTreeMap<FileId, u64>,
    reached: &[(FileId, u64, u64)],
) {
    let len = usize::try_from(f.durable_len).unwrap_or(usize::MAX);
    f.media.resize(len, 0);
    f.visible = f.media.clone();
    unmark(&mut f.holes, f.durable_len, u64::MAX);
    // Past the media as it was, a durable length (settled metadata) holds
    // data only where a kept write put it: elsewhere unwritten, as a file
    // system's unwritten extents.
    mark(
        &mut f.holes,
        before.get(&id).copied().unwrap_or(0),
        f.durable_len,
    );
    for &(_, s, e) in reached.iter().filter(|r| r.0 == id) {
        unmark(&mut f.holes, s, e.min(f.durable_len));
    }
    f.valid_data = f.valid_data.min(f.durable_len);
}

fn range(offset: u64, len: u64, file_len: usize) -> Option<(usize, usize)> {
    let s = usize::try_from(offset).ok()?;
    let e = s.checked_add(usize::try_from(len).ok()?)?;
    (e <= file_len).then_some((s, e))
}
