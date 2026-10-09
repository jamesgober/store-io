//! Data file handles: `O_DIRECT|O_CLOEXEC` descriptors opened relative to a
//! confined directory, closed on drop.

use core::ffi::c_int;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};

use store_io_platform::{FileMode, FileName, RawResult};

use crate::PosixPlatform;
use crate::dir::{PosixDir, name_cstring};
use crate::sys;

/// An open data file.
///
/// Opened `O_DIRECT|O_CLOEXEC` (plus `O_RDWR` or `O_RDONLY`), relative to its
/// directory with `openat2(RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS)` where the
/// kernel has it and `openat(O_NOFOLLOW)` otherwise; the name is a single
/// path component, so `O_NOFOLLOW` checks every component there is. A name
/// that resolves to anything but a regular file or a block device is refused
/// with `EINVAL` after the open, before any use.
#[derive(Debug)]
pub struct PosixFile {
    fd: OwnedFd,
    writable: bool,
    block_device: bool,
}

impl PosixFile {
    /// Creates a new file exclusively (`O_CREAT|O_EXCL`, mode 0600) for
    /// direct read-write I/O.
    pub(crate) fn create(p: &PosixPlatform, dir: &PosixDir, name: &FileName) -> RawResult<Self> {
        let flags = libc::O_RDWR | libc::O_DIRECT | libc::O_CREAT | libc::O_EXCL;
        let fd = open_confined(p, dir.raw(), name, flags, 0o600)?;
        Self::from_fd(fd, true)
    }

    /// Opens an existing file for direct I/O.
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
        let fd = open_confined(p, dir.raw(), name, access | libc::O_DIRECT, 0)?;
        Self::from_fd(fd, writable)
    }

    fn from_fd(fd: OwnedFd, writable: bool) -> RawResult<Self> {
        let st = sys::fstat(fd.as_raw_fd())?;
        let block_device = match st.st_mode & libc::S_IFMT {
            libc::S_IFREG => false,
            libc::S_IFBLK => true,
            _ => return Err(sys::errno(libc::EINVAL)),
        };
        Ok(Self {
            fd,
            writable,
            block_device,
        })
    }

    /// Whether the handle was opened read-write.
    #[must_use]
    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// Whether the handle is a raw block device rather than a regular file.
    #[must_use]
    pub fn is_block_device(&self) -> bool {
        self.block_device
    }

    /// The raw descriptor, valid while `self` is alive.
    #[inline]
    pub(crate) fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
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

/// Opens `name` under `dirfd` with `flags | O_CLOEXEC | O_NOFOLLOW`, confined
/// by `openat2` when available. Retries `EINTR` (and `openat2`'s `EAGAIN`),
/// counted on the platform; both precede any I/O.
pub(crate) fn open_confined(
    p: &PosixPlatform,
    dirfd: RawFd,
    name: &FileName,
    flags: c_int,
    mode: u32,
) -> RawResult<OwnedFd> {
    let c = name_cstring(name)?;
    let flags = flags | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    if p.openat2_supported() {
        p.retry_open(|| sys::openat2(dirfd, &c, flags, mode, sys::RESOLVE_CONFINED))
    } else {
        p.retry_open(|| sys::openat(dirfd, &c, flags, mode))
    }
}
