//! [`SimPlatform`]: the simulator behind the `store-io-platform` traits.
//!
//! Operations take effect when they **complete**, not when they are
//! submitted, so a write that is still in a queue at a crash never reached the
//! device. With `reorder` enabled, [`Queue::reap`] completes a seeded random
//! subset of in-flight operations in a seeded random order.
//!
//! Data files behave as direct I/O: a read or write whose offset or length is
//! not a multiple of the logical block, or whose buffer is not 4096-aligned,
//! completes with `EINVAL` and never reaches the device, as `O_DIRECT` and
//! `FILE_FLAG_NO_BUFFERING` do.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use store_io_core::error::{OsError, OsErrorSource};
use store_io_core::evidence::{
    Bus, DeviceFacts, Evidence, FsFacts, FsKind, OsCacheMode, PlatformKind, Privilege, Tri,
};
use store_io_platform::{
    Completion, CompletionBuf, FileMode, FileName, IoBuf, IoOp, Platform, Queue, QueueConfig,
    RangeState, RawResult, Rejected, ReleaseHow, Wake,
};

use crate::world::{DeviceKind, EBADF, EINVAL, EIO, FileId, SimConfig, World};

/// Shared handle to a simulated world.
#[derive(Clone, Debug)]
pub struct SimPlatform {
    world: Arc<Mutex<World>>,
}

fn lock(w: &Mutex<World>) -> MutexGuard<'_, World> {
    // A panic while holding the lock is a test failure already; keep going so
    // the failure is reported by the test, not hidden behind a poison error.
    w.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl SimPlatform {
    /// Creates a platform over a fresh world.
    #[must_use]
    pub fn new(cfg: SimConfig) -> Self {
        Self {
            world: Arc::new(Mutex::new(World::new(cfg))),
        }
    }

    /// Runs `f` with exclusive access to the world (inspect, arm faults, crash).
    pub fn with_world<R>(&self, f: impl FnOnce(&mut World) -> R) -> R {
        f(&mut lock(&self.world))
    }
}

/// An open simulated file. Invalid after a crash.
#[derive(Debug)]
pub struct SimFile {
    id: FileId,
    epoch: u64,
    writable: bool,
}

impl SimFile {
    /// The file's id in the world.
    #[must_use]
    pub fn id(&self) -> FileId {
        self.id
    }

    fn check(&self, w: &World, write: bool) -> RawResult<()> {
        if w.epoch() != self.epoch || (write && !self.writable) {
            Err(EBADF)
        } else {
            Ok(())
        }
    }
}

/// An open simulated directory.
#[derive(Debug, Clone)]
pub struct SimDir {
    path: PathBuf,
}

/// A held simulated ownership lock.
#[derive(Debug)]
pub struct SimLock {
    world: Arc<Mutex<World>>,
    file: FileId,
    epoch: u64,
}

impl Drop for SimLock {
    fn drop(&mut self) {
        let mut w = lock(&self.world);
        if w.epoch() == self.epoch {
            w.unlock(self.file);
        }
    }
}

enum Pending {
    Write {
        file: FileId,
        epoch: u64,
        offset: u64,
        buf: IoBuf,
        dsync: bool,
    },
    Read {
        file: FileId,
        epoch: u64,
        offset: u64,
        buf: IoBuf,
    },
    Flush {
        file: FileId,
        epoch: u64,
        cover: u64,
    },
}

/// A simulated per-thread queue.
pub struct SimQueue {
    world: Arc<Mutex<World>>,
    in_flight: Vec<(u64, Pending)>,
    depth: usize,
    woken: Arc<AtomicBool>,
}

/// Ends a [`SimQueue`]'s wait from another thread (D-083): the next wait sees
/// the flag and does only what `reap` does, then returns. (A simulated wait
/// never blocks, so a wake always ends the next wait, not one in progress.)
#[derive(Debug, Clone)]
pub struct SimWaker(Arc<AtomicBool>);

impl Wake for SimWaker {
    fn wake(&self) {
        self.0.store(true, Ordering::Release);
    }
}

impl SimQueue {
    /// The checks `submit` makes (the submit probe first, then room, the
    /// injected submission fault, the file): the operation as it will
    /// complete, or its refusal.
    fn accept(&mut self, op: IoOp<'_, SimFile>) -> Result<Pending, Rejected> {
        let probe = lock(&self.world).submit_probe.0.clone();
        if let Some(probe) = probe {
            probe();
        }
        if let Err(raw) = step(&self.world) {
            let buf = match op {
                IoOp::Write { buf, .. } | IoOp::Read { buf, .. } => Some(buf),
                IoOp::FlushData { .. } => None,
            };
            return Err(Rejected {
                buf,
                raw: Some(raw),
            });
        }
        if self.in_flight.len() >= self.depth {
            let buf = match op {
                IoOp::Write { buf, .. } | IoOp::Read { buf, .. } => Some(buf),
                IoOp::FlushData { .. } => None,
            };
            return Err(Rejected { buf, raw: None });
        }
        let mut w = lock(&self.world);
        let pending = match op {
            IoOp::Write {
                file,
                offset,
                buf,
                dsync,
            } => {
                if let Some(e) = w.submit_fails() {
                    return Err(Rejected {
                        buf: Some(buf),
                        raw: Some(e),
                    });
                }
                if let Err(e) = file.check(&w, true) {
                    return Err(Rejected {
                        buf: Some(buf),
                        raw: Some(e),
                    });
                }
                Pending::Write {
                    file: file.id,
                    epoch: file.epoch,
                    offset,
                    buf,
                    dsync,
                }
            }
            IoOp::Read { file, offset, buf } => {
                if let Err(e) = file.check(&w, false) {
                    return Err(Rejected {
                        buf: Some(buf),
                        raw: Some(e),
                    });
                }
                Pending::Read {
                    file: file.id,
                    epoch: file.epoch,
                    offset,
                    buf,
                }
            }
            IoOp::FlushData { file } => {
                if let Err(e) = file.check(&w, true) {
                    return Err(Rejected {
                        buf: None,
                        raw: Some(e),
                    });
                }
                // The flush covers exactly what completed before this instant.
                Pending::Flush {
                    file: file.id,
                    epoch: file.epoch,
                    cover: w.flush_cover(),
                }
            }
        };
        Ok(pending)
    }
}

impl Queue for SimQueue {
    type File = SimFile;
    type Waker = SimWaker;

    fn submit(&mut self, op: IoOp<'_, SimFile>, tag: u64) -> Result<(), Rejected> {
        let pending = self.accept(op)?;
        self.in_flight.push((tag, pending));
        Ok(())
    }

    /// With [`crate::SimConfig::inline`] and nothing in flight: the checks
    /// `submit` makes, then the completion at once (its faults as any
    /// completion's), as an inline queue performs inside `submit`.
    fn performs(&self) -> bool {
        self.in_flight.is_empty() && lock(&self.world).config().inline
    }

    fn perform<'a>(
        &mut self,
        op: IoOp<'a, SimFile>,
    ) -> Result<Result<(Option<IoBuf>, RawResult<usize>), IoOp<'a, SimFile>>, Rejected> {
        if !self.performs() {
            return Ok(Err(op));
        }
        let pending = self.accept(op)?;
        let c = complete(&mut lock(&self.world), 0, pending);
        Ok(Ok((c.buf, c.result)))
    }

    fn reap(&mut self, out: &mut CompletionBuf) -> usize {
        let mut w = lock(&self.world);
        let reorder = w.config().reorder;
        let mut added = 0;
        let mut i = 0;
        while i < self.in_flight.len() && out.room() > 0 {
            let pick = if reorder {
                if !w.rng().chance(500_000) {
                    i += 1;
                    continue;
                }
                i
            } else {
                0
            };
            let (tag, p) = self.in_flight.remove(pick);
            // The loop checked room() > 0 before completing, so the push
            // cannot be refused.
            if out.push(complete(&mut w, tag, p)).is_err() {
                break;
            }
            added += 1;
            if !reorder {
                i = 0;
            }
        }
        added
    }

    fn wait(&mut self, min: usize, out: &mut CompletionBuf) -> RawResult<usize> {
        if let Some(raw) = lock(&self.world).wait_fails() {
            return Err(raw);
        }
        if self.woken.swap(false, Ordering::AcqRel) {
            return Ok(self.reap(out));
        }
        let mut added = self.reap(out);
        let mut w = lock(&self.world);
        while added < min && !self.in_flight.is_empty() && out.room() > 0 {
            let pick = if w.config().reorder {
                w.rng().below(self.in_flight.len() as u64) as usize
            } else {
                0
            };
            let (tag, p) = self.in_flight.remove(pick);
            let c = complete(&mut w, tag, p);
            if out.push(c).is_err() {
                break;
            }
            added += 1;
        }
        Ok(added)
    }

    fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    fn waker(&self) -> SimWaker {
        SimWaker(Arc::clone(&self.woken))
    }
}

/// Buffer address alignment the simulated device requires
/// (`dio_mem_align` in the evidence).
const MEM_ALIGN: usize = 4096;

/// Whether a data transfer meets the direct-I/O alignment rules.
fn aligned(w: &World, offset: u64, buf: &IoBuf) -> bool {
    let lb = u64::from(w.config().logical_block.max(1));
    offset % lb == 0 && buf.len() as u64 % lb == 0 && buf.as_ptr() as usize % MEM_ALIGN == 0
}

/// Applies an operation's effect at completion time.
fn complete(w: &mut World, tag: u64, p: Pending) -> Completion {
    match p {
        Pending::Write {
            file,
            epoch,
            offset,
            buf,
            dsync,
        } => {
            let result = if epoch != w.epoch() {
                Err(EBADF)
            } else if w.powered_off() {
                Err(EIO)
            } else if !aligned(w, offset, &buf) {
                Err(EINVAL)
            } else {
                w.complete_write(file, offset, buf.as_slice(), dsync)
            };
            Completion {
                tag,
                buf: Some(buf),
                result,
            }
        }
        Pending::Read {
            file,
            epoch,
            offset,
            mut buf,
        } => {
            let result = if epoch != w.epoch() {
                Err(EBADF)
            } else if w.powered_off() {
                Err(EIO)
            } else if !aligned(w, offset, &buf) {
                w.count_misaligned_read();
                Err(EINVAL)
            } else {
                w.complete_read(file, offset, buf.as_mut_slice())
            };
            Completion {
                tag,
                buf: Some(buf),
                result,
            }
        }
        Pending::Flush { file, epoch, cover } => {
            let result = if epoch != w.epoch() {
                Err(EBADF)
            } else if w.powered_off() {
                Err(EIO)
            } else {
                w.complete_flush(file, cover)
            };
            Completion {
                tag,
                buf: None,
                result,
            }
        }
    }
}

/// Runs the step probe ([`World::set_step_probe`]) outside the world lock;
/// EIO once the power is off.
fn step(world: &Mutex<World>) -> RawResult<()> {
    let probe = lock(world).step_probe.0.clone();
    let cut = probe.is_some_and(|p| p());
    let mut w = lock(world);
    if cut {
        w.cut_power();
    }
    if w.powered_off() {
        return Err(EIO);
    }
    Ok(())
}

impl Platform for SimPlatform {
    type File = SimFile;
    type Dir = SimDir;
    type Lock = SimLock;
    type Queue = SimQueue;

    fn open_dir(&self, path: &Path, create: bool) -> RawResult<SimDir> {
        if create {
            step(&self.world)?;
        }
        lock(&self.world).open_dir(path.to_path_buf(), create)?;
        Ok(SimDir {
            path: path.to_path_buf(),
        })
    }

    fn create_file(&self, dir: &SimDir, name: &FileName) -> RawResult<SimFile> {
        step(&self.world)?;
        let mut w = lock(&self.world);
        let id = w.create_file(&dir.path, name.as_str())?;
        Ok(SimFile {
            id,
            epoch: w.epoch(),
            writable: true,
        })
    }

    fn open_file(&self, dir: &SimDir, name: &FileName, mode: FileMode) -> RawResult<SimFile> {
        let w = lock(&self.world);
        if let Some((d, n, code)) = &w.faults().fail_open {
            if *d == dir.path && n == name.as_str() {
                return Err(OsError {
                    code: *code,
                    source: OsErrorSource::Sim,
                });
            }
        }
        let id = w.lookup(&dir.path, name.as_str())?;
        if mode == FileMode::ReadWrite && w.faults().read_only_media {
            return Err(crate::world::EROFS);
        }
        Ok(SimFile {
            id,
            epoch: w.epoch(),
            writable: mode == FileMode::ReadWrite,
        })
    }

    fn rename_replace(
        &self,
        dir: &SimDir,
        from: &FileName,
        to: &FileName,
        _file: &SimFile,
    ) -> RawResult<()> {
        step(&self.world)?;
        lock(&self.world).rename(&dir.path, from.as_str(), to.as_str())
    }

    fn rename_noreplace(
        &self,
        dir: &SimDir,
        from: &FileName,
        to: &FileName,
        _file: &SimFile,
    ) -> RawResult<()> {
        step(&self.world)?;
        lock(&self.world).rename_noreplace(&dir.path, from.as_str(), to.as_str())
    }

    fn unlink(&self, dir: &SimDir, name: &FileName) -> RawResult<()> {
        step(&self.world)?;
        lock(&self.world).unlink(&dir.path, name.as_str())
    }

    fn sync_dir(&self, dir: &SimDir) -> RawResult<()> {
        step(&self.world)?;
        lock(&self.world).sync_dir(&dir.path)
    }

    fn lock_exclusive(&self, file: &SimFile) -> RawResult<SimLock> {
        // As on Linux, a write lock needs a handle open for writing.
        let mut w = lock(&self.world);
        file.check(&w, true)?;
        w.lock(file.id, false)?;
        Ok(SimLock {
            world: Arc::clone(&self.world),
            file: file.id,
            epoch: w.epoch(),
        })
    }

    fn monotonic_ms(&self) -> u64 {
        lock(&self.world).clock_ms()
    }

    fn flush_errors_per_handle(&self) -> bool {
        // A flush persists the file whatever the handle, and reports its
        // own outcome.
        true
    }

    fn reopen(&self, file: &SimFile) -> Option<SimFile> {
        (file.writable && lock(&self.world).reopen()).then_some(SimFile {
            id: file.id,
            epoch: file.epoch,
            writable: true,
        })
    }

    fn io_path(&self) -> store_io_platform::IoPath {
        store_io_platform::IoPath {
            interface: store_io_platform::IoInterface::Simulator,
            reason: "the deterministic simulator".to_owned(),
        }
    }

    fn lock_shared(&self, file: &SimFile) -> RawResult<SimLock> {
        let mut w = lock(&self.world);
        file.check(&w, false)?;
        w.lock(file.id, true)?;
        Ok(SimLock {
            world: Arc::clone(&self.world),
            file: file.id,
            epoch: w.epoch(),
        })
    }

    fn allocate(&self, file: &SimFile, len: u64) -> RawResult<()> {
        step(&self.world)?;
        let mut w = lock(&self.world);
        file.check(&w, true)?;
        w.allocate(file.id, len)
    }

    fn set_len(&self, file: &SimFile, len: u64) -> RawResult<()> {
        step(&self.world)?;
        let mut w = lock(&self.world);
        file.check(&w, true)?;
        w.set_len(file.id, len)
    }

    fn size(&self, file: &SimFile) -> RawResult<u64> {
        let w = lock(&self.world);
        file.check(&w, false)?;
        w.size(file.id)
    }

    fn release_range(
        &self,
        file: &SimFile,
        offset: u64,
        len: u64,
        how: ReleaseHow,
    ) -> RawResult<()> {
        let probe = lock(&self.world).release_probe.0.clone();
        if let Some(probe) = probe {
            probe();
        }
        step(&self.world)?;
        let mut w = lock(&self.world);
        file.check(&w, true)?;
        // A deallocation punches a hole; a trim in place leaves the space
        // allocated.
        w.release(file.id, offset, len, how == ReleaseHow::Deallocate)
    }

    fn allocate_range(&self, file: &SimFile, offset: u64, len: u64) -> RawResult<()> {
        step(&self.world)?;
        let mut w = lock(&self.world);
        file.check(&w, true)?;
        w.allocate_range(file.id, offset, len)
    }

    fn range_state(&self, file: &SimFile, offset: u64, len: u64) -> RawResult<RangeState> {
        let w = lock(&self.world);
        file.check(&w, false)?;
        let unwritten = w.unwritten(file.id, offset, len)?;
        Ok(RangeState {
            unwritten: if unwritten { Tri::Yes } else { Tri::No },
            shared: Tri::No,
            cached_pages: Some(if w.cached_pages() {
                len.div_ceil(4096)
            } else {
                0
            }),
            valid_data: if w.past_valid_data(file.id, offset, len) {
                Tri::No
            } else {
                Tri::Yes
            },
        })
    }

    fn flush_all(&self, file: &SimFile) -> RawResult<()> {
        step(&self.world)?;
        let mut w = lock(&self.world);
        file.check(&w, true)?;
        w.flush_all(file.id)
    }

    fn probe(&self, _dir: &SimDir, file: &SimFile) -> RawResult<Evidence> {
        let w = lock(&self.world);
        file.check(&w, false)?;
        if let Some(code) = w.faults().fail_probe {
            return Err(OsError {
                code,
                source: OsErrorSource::Sim,
            });
        }
        if let Some(ev) = &w.config().evidence {
            return Ok(ev.clone());
        }
        let power_safe = w.config().device == DeviceKind::PowerSafe;
        let mut ev = Evidence::unknown(PlatformKind::Sim);
        ev.device = DeviceFacts {
            model: Some("store-io simulated device".to_owned()),
            bus: Bus::Nvme,
            cache_present: if power_safe { Tri::No } else { Tri::Yes },
            cache_enabled: if power_safe { Tri::No } else { Tri::Yes },
            os_cache_mode: if power_safe {
                OsCacheMode::WriteThrough
            } else {
                OsCacheMode::WriteBack
            },
            backup_failed: Tri::No,
            logical_block: w.config().logical_block,
            physical_block: w.config().logical_block,
            max_transfer: 1 << 20,
            ..DeviceFacts::default()
        };
        ev.fs = FsFacts {
            kind: FsKind::Sim,
            direct_io: Tri::Yes,
            cow: Tri::No,
            dio_mem_align: 4096,
            dio_offset_align: w.config().logical_block,
            ..FsFacts::default()
        };
        ev.privilege = Privilege::Full;
        Ok(ev)
    }

    fn queue(&self, cfg: QueueConfig) -> RawResult<SimQueue> {
        lock(&self.world).queue_made();
        Ok(SimQueue {
            world: Arc::clone(&self.world),
            in_flight: Vec::new(),
            depth: cfg.depth.max(1) as usize,
            woken: Arc::new(AtomicBool::new(false)),
        })
    }
}
