//! Directory handles: an `O_DIRECTORY|O_RDONLY|O_CLOEXEC` descriptor that
//! every file operation is relative to, and that `fsync` makes durable.

use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use store_io_platform::{FileName, RawResult};

use crate::PosixPlatform;
use crate::sys;

/// An open directory (closed on drop).
///
/// Held as a real `O_RDONLY|O_DIRECTORY` descriptor, never `O_PATH`, because
/// `fsync` needs an open file. Files are created, opened, renamed and
/// unlinked relative to it with a validated single-component name, so no
/// operation can leave the directory.
#[derive(Debug)]
pub struct PosixDir {
    fd: OwnedFd,
}

impl PosixDir {
    /// Opens `path`; with `create`, first `mkdir`s it (mode 0700) if missing
    /// and `fsync`s the parent so the new entry is durable.
    pub(crate) fn open(p: &PosixPlatform, path: &Path, create: bool) -> RawResult<Self> {
        let c = path_cstring(path)?;
        if create {
            match sys::mkdir(&c, 0o700) {
                Ok(()) => sync_parent(p, path)?,
                Err(e) if e.code == libc::EEXIST => {}
                Err(e) => return Err(e),
            }
        }
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
        let fd = p.retry_open(|| sys::openat(libc::AT_FDCWD, &c, flags, 0))?;
        Ok(Self { fd })
    }

    /// `renameat` of `from` over `to`, both inside this directory.
    pub(crate) fn rename(&self, from: &FileName, to: &FileName) -> RawResult<()> {
        sys::renameat(self.raw(), &name_cstring(from)?, &name_cstring(to)?)
    }

    /// `unlinkat` of `name`.
    pub(crate) fn unlink(&self, name: &FileName) -> RawResult<()> {
        sys::unlinkat(self.raw(), &name_cstring(name)?)
    }

    /// `fsync` of the directory itself.
    pub(crate) fn sync(&self) -> RawResult<()> {
        sys::fsync(self.raw())
    }

    /// The raw descriptor, valid while `self` is alive.
    #[inline]
    pub(crate) fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl AsFd for PosixDir {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl AsRawFd for PosixDir {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// Opens the parent of `path` and `fsync`s it.
fn sync_parent(p: &PosixPlatform, path: &Path) -> RawResult<()> {
    let parent = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let c = path_cstring(parent)?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
    let fd = p.retry_open(|| sys::openat(libc::AT_FDCWD, &c, flags, 0))?;
    sys::fsync(fd.as_raw_fd())
}

/// A path as a C string; `EINVAL` if it contains a NUL byte.
fn path_cstring(path: &Path) -> RawResult<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| sys::errno(libc::EINVAL))
}

/// A validated file name as a C string (a `FileName` never contains NUL).
pub(crate) fn name_cstring(name: &FileName) -> RawResult<CString> {
    CString::new(name.as_str()).map_err(|_| sys::errno(libc::EINVAL))
}
