//! [`WinPlatform`]: the `Platform` implementation.

use std::path::Path;

use store_io_core::evidence::Evidence;
use store_io_platform::{
    FileMode, FileName, Platform, QueueConfig, RangeState, RawResult, ReleaseHow,
};

use crate::alloc;
use crate::dir::WinDir;
use crate::file::WinFile;
use crate::lock::WinLock;
use crate::probe;
use crate::queue::WinQueue;
use crate::sys;

/// The Windows backend. Stateless: every handle it opens carries its own
/// facts (flush mode, access), and every queue owns its own completion port.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use store_io_platform::{FileName, Platform, RawResult};
/// use store_io_win::WinPlatform;
///
/// fn create_store(name: &FileName) -> RawResult<()> {
///     let p = WinPlatform::new();
///     let dir = p.open_dir(Path::new(r"C:\data\store"), true)?;
///     let file = p.create_file(&dir, name)?;
///     let _lock = p.lock_exclusive(&file)?;
///     p.allocate(&file, 1 << 20)?;
///     p.flush_all(&file)?;
///     p.sync_dir(&dir)
/// }
///
/// let name = FileName::new("container.sio").ok();
/// assert!(name.is_some());
/// # let _ = create_store;
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct WinPlatform {
    _private: (),
}

impl WinPlatform {
    /// Creates the backend.
    #[must_use]
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Platform for WinPlatform {
    type File = WinFile;
    type Dir = WinDir;
    type Lock = WinLock;
    type Queue = WinQueue;

    fn open_dir(&self, path: &Path, create: bool) -> RawResult<WinDir> {
        WinDir::open(path, create)
    }

    fn create_file(&self, dir: &WinDir, name: &FileName) -> RawResult<WinFile> {
        dir.create_file(name)
    }

    fn open_file(&self, dir: &WinDir, name: &FileName, mode: FileMode) -> RawResult<WinFile> {
        dir.open_file(name, mode)
    }

    /// Renames by handle: `from` names the file for the caller's benefit;
    /// the open `file` is what Windows renames.
    fn rename_replace(
        &self,
        dir: &WinDir,
        _from: &FileName,
        to: &FileName,
        file: &WinFile,
    ) -> RawResult<()> {
        dir.rename_replace(to, file)
    }

    fn unlink(&self, dir: &WinDir, name: &FileName) -> RawResult<()> {
        dir.unlink(name)
    }

    fn sync_dir(&self, dir: &WinDir) -> RawResult<()> {
        dir.sync()
    }

    fn lock_exclusive(&self, file: &WinFile) -> RawResult<WinLock> {
        WinLock::take(file)
    }

    fn allocate(&self, file: &WinFile, len: u64) -> RawResult<()> {
        alloc::allocate(file, len)
    }

    fn size(&self, file: &WinFile) -> RawResult<u64> {
        alloc::size(file)
    }

    fn release_range(
        &self,
        file: &WinFile,
        offset: u64,
        len: u64,
        how: ReleaseHow,
    ) -> RawResult<()> {
        alloc::release_range(file, offset, len, how)
    }

    fn range_state(&self, file: &WinFile, offset: u64, len: u64) -> RawResult<RangeState> {
        alloc::range_state(file, offset, len)
    }

    fn flush_all(&self, file: &WinFile) -> RawResult<()> {
        sys::flush_file_buffers(file.handle())
    }

    fn probe(&self, dir: &WinDir, file: &WinFile) -> RawResult<Evidence> {
        probe::probe(dir, file)
    }

    fn queue(&self, cfg: QueueConfig) -> RawResult<WinQueue> {
        WinQueue::new(cfg)
    }
}
