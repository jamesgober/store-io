//! Data file handles on macOS: `O_CLOEXEC|O_NOFOLLOW` descriptors opened
//! relative to a directory, uncached where the kernel can (`F_NOCACHE`,
//! read-ahead off), closed on drop.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::sync::Arc;

use store_io_platform::{FileMode, FileName, RawResult};

use crate::PosixPlatform;
use crate::dir::{FlushState, PosixDir, name_cstring};
use crate::sys;

/// An open data file.
///
/// Opened `O_CLOEXEC|O_NOFOLLOW` (plus `O_RDWR` or `O_RDONLY`) relative to
/// its directory; the name is a single path component, so `O_NOFOLLOW`
/// checks every component there is. Anything but a regular file is refused
/// with `EINVAL` after the open, before any use. `F_NOCACHE` and read-ahead
/// off are set on it: a hint (only requests aligned to the kernel page are
/// uncached), never part of a durability argument.
///
/// The descriptor is shared with the submit pool's jobs (D-096): a job holds
/// a clone, so the descriptor stays open, and its number cannot be reused,
/// until the job's system call has returned, even if the file is dropped
/// first.
#[derive(Debug)]
pub struct PosixFile {
    fd: Arc<OwnedFd>,
    writable: bool,
    /// The volume's flush state, shared with every directory and file on
    /// it: its flushes are `F_FULLFSYNC` until the volume refuses it (then
    /// `fsync`, never `F_FULLFSYNC` again).
    full: Arc<FlushState>,
}

impl PosixFile {
    /// Creates a new file exclusively (`O_CREAT|O_EXCL`, mode 0600) for
    /// read-write I/O.
    pub(crate) fn create(p: &PosixPlatform, dir: &PosixDir, name: &FileName) -> RawResult<Self> {
        let flags = libc::O_RDWR | libc::O_CREAT | libc::O_EXCL;
        let fd = open_confined(p, dir.raw(), name, flags, 0o600)?;
        Self::from_fd(fd, true, dir.flush_state())
    }

    /// Opens an existing file.
    pub(crate) fn open(
        p: &PosixPlatform,
        dir: &PosixDir,
        name: &FileName,
        mode: FileMode,
    ) -> RawResult<Self> {
        let (access, writable) = match mode {
            FileMode::ReadWrite => (libc::O_RDWR, true),
            FileMode::ReadOnly => (libc::O_RDONLY, false),
        };
        let fd = open_confined(p, dir.raw(), name, access, 0)?;
        Self::from_fd(fd, writable, dir.flush_state())
    }

    fn from_fd(fd: OwnedFd, writable: bool, full: Arc<FlushState>) -> RawResult<Self> {
        let st = sys::fstat(fd.as_raw_fd())?;
        if st.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(sys::errno(libc::EINVAL));
        }
        sys::uncached(fd.as_raw_fd());
        Ok(Self {
            fd: Arc::new(fd),
            writable,
            full,
        })
    }

    /// Whether the handle was opened read-write.
    #[must_use]
    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// Whether the handle is a raw block device: never on macOS (only
    /// regular files are opened).
    #[must_use]
    pub fn is_block_device(&self) -> bool {
        false
    }

    /// Whether its flushes reach the drive (`F_FULLFSYNC`), or were
    /// downgraded to `fsync` when the volume refused `F_FULLFSYNC`.
    #[must_use]
    pub fn full_flush(&self) -> bool {
        self.full.full()
    }

    /// The volume's flush state.
    pub(crate) fn flush_state(&self) -> &FlushState {
        &self.full
    }

    /// What the queue's executor needs to know of the file.
    #[inline]
    pub(crate) fn traits(&self) -> sys::FileTraits {
        sys::FileTraits {
            full: self.full_flush(),
        }
    }

    /// The raw descriptor, valid while `self` is alive.
    #[inline]
    pub(crate) fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// The descriptor, shared with a pooled job (one atomic increment).
    #[inline]
    pub(crate) fn share_fd(&self) -> Arc<OwnedFd> {
        Arc::clone(&self.fd)
    }
}

impl AsFd for PosixFile {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl AsRawFd for PosixFile {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// Opens `name` under `dirfd` with `flags | O_CLOEXEC | O_NOFOLLOW`. Retries
/// `EINTR`, counted on the platform; it precedes any I/O.
fn open_confined(
    p: &PosixPlatform,
    dirfd: RawFd,
    name: &FileName,
    flags: libc::c_int,
    mode: u32,
) -> RawResult<OwnedFd> {
    let c = name_cstring(name)?;
    let flags = flags | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    p.retry_open(|| sys::openat(dirfd, &c, flags, mode))
}
