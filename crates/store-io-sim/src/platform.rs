//! [`SimPlatform`]: the simulator behind the `store-io-platform` traits.
//!
//! Operations take effect when they **complete**, not when they are
//! submitted, so a write that is still in a queue at a crash never reached the
//! device. With `reorder` enabled, [`Queue::reap`] completes a seeded random
//! subset of in-flight operations in a seeded random order.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use store_io_core::evidence::{
    Bus, DeviceFacts, Evidence, FsFacts, FsKind, OsCacheMode, PlatformKind, Privilege, Tri,
};
use store_io_platform::{
    Completion, CompletionBuf, FileMode, FileName, IoBuf, IoOp, Platform, Queue, QueueConfig,
    RangeState, RawResult, Rejected, ReleaseHow,
};

use crate::world::{DeviceKind, EBADF, FileId, SimConfig, World};

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
}

impl Queue for SimQueue {
    type File = SimFile;

    fn submit(&mut self, op: IoOp<'_, SimFile>, tag: u64) -> Result<(), Rejected> {
        if self.in_flight.len() >= self.depth {
            let buf = match op {
                IoOp::Write { buf, .. } | IoOp::Read { buf, .. } => Some(buf),
                IoOp::FlushData { .. } => None,
            };
            return Err(Rejected { buf, raw: None });
        }
        let w = lock(&self.world);
        let pending = match op {
            IoOp::Write {
                file,
                offset,
                buf,
                dsync,
            } => {
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
        drop(w);
        self.in_flight.push((tag, pending));
        Ok(())
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

impl Platform for SimPlatform {
    type File = SimFile;
    type Dir = SimDir;
    type Lock = SimLock;
    type Queue = SimQueue;

    fn open_dir(&self, path: &Path, create: bool) -> RawResult<SimDir> {
        lock(&self.world).open_dir(path.to_path_buf(), create)?;
        Ok(SimDir {
            path: path.to_path_buf(),
        })
    }

    fn create_file(&self, dir: &SimDir, name: &FileName) -> RawResult<SimFile> {
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
        let id = w.lookup(&dir.path, name.as_str())?;
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
        lock(&self.world).rename(&dir.path, from.as_str(), to.as_str())
    }

    fn unlink(&self, dir: &SimDir, name: &FileName) -> RawResult<()> {
        lock(&self.world).unlink(&dir.path, name.as_str())
    }

    fn sync_dir(&self, dir: &SimDir) -> RawResult<()> {
        lock(&self.world).sync_dir(&dir.path)
    }

    fn lock_exclusive(&self, file: &SimFile) -> RawResult<SimLock> {
        let mut w = lock(&self.world);
        file.check(&w, false)?;
        w.lock(file.id)?;
        Ok(SimLock {
            world: Arc::clone(&self.world),
            file: file.id,
            epoch: w.epoch(),
        })
    }

    fn allocate(&self, file: &SimFile, len: u64) -> RawResult<()> {
        let mut w = lock(&self.world);
        file.check(&w, true)?;
        w.allocate(file.id, len)
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
        _how: ReleaseHow,
    ) -> RawResult<()> {
        let mut w = lock(&self.world);
        file.check(&w, true)?;
        w.release(file.id, offset, len)
    }

    fn range_state(&self, file: &SimFile, _offset: u64, len: u64) -> RawResult<RangeState> {
        let w = lock(&self.world);
        file.check(&w, false)?;
        Ok(RangeState {
            unwritten: Tri::No,
            shared: Tri::No,
            cached_pages: Some(if w.cached_pages() {
                len.div_ceil(4096)
            } else {
                0
            }),
            valid_data: Tri::Yes,
        })
    }

    fn flush_all(&self, file: &SimFile) -> RawResult<()> {
        let mut w = lock(&self.world);
        file.check(&w, true)?;
        w.flush_all(file.id)
    }

    fn probe(&self, _dir: &SimDir, file: &SimFile) -> RawResult<Evidence> {
        let w = lock(&self.world);
        file.check(&w, false)?;
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
        Ok(SimQueue {
            world: Arc::clone(&self.world),
            in_flight: Vec::new(),
            depth: cfg.depth.max(1) as usize,
        })
    }
}
