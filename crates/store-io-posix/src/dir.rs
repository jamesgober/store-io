//! Directory handles: an `O_DIRECTORY|O_RDONLY|O_CLOEXEC` descriptor that
//! every file operation is relative to, and that `fsync` makes durable.

use std::ffi::{CStr, CString};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use store_io_core::errno::is_unsupported;
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
    /// Opens `path`; with `create`, first `mkdir`s it (mode 0700) if missing,
    /// then syncs its parent so the entry is durable (see [`sync_parent`]).
    /// Also when the directory exists: an earlier attempt that died, or one
    /// racing this, may have made it without the sync. A directory made
    /// here whose open or sync fails is removed again (if it is still the
    /// one made), so a retry never meets it as existing.
    pub(crate) fn open(p: &PosixPlatform, path: &Path, create: bool) -> RawResult<Self> {
        let c = path_cstring(path)?;
        let made = create
            && match sys::mkdir(&c, 0o700) {
                Ok(()) => true,
                Err(e) if e.code == libc::EEXIST => false,
                Err(e) => return Err(e),
            };
        // The directory this call made, as its path names it just after the
        // mkdir: what the open must reach, and all that is ever removed.
        let id = if made { Some(identity(&c)?) } else { None };
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
        let fd = match p.retry_open(|| sys::openat(libc::AT_FDCWD, &c, flags, 0)) {
            Ok(fd) => fd,
            Err(e) => {
                remove_made(&c, id);
                return Err(e);
            }
        };
        if let Some(made_id) = id {
            match sys::fstat(fd.as_raw_fd()) {
                Ok(st) if (st.st_dev, st.st_ino) == made_id => {}
                // Another directory swapped in at the path: syncing its
                // parent would not make this call's entry durable, and
                // nothing of this call's is there to remove.
                Ok(_) => return Err(sys::errno(libc::EEXIST)),
                Err(e) => {
                    remove_made(&c, id);
                    return Err(e);
                }
            }
        }
        if create {
            if let Err(e) = sync_parent(p, fd.as_raw_fd(), made) {
                remove_made(&c, id);
                return Err(e);
            }
        }
        Ok(Self { fd })
    }

    /// `renameat` of `from` over `to`, both inside this directory.
    pub(crate) fn rename(&self, from: &FileName, to: &FileName) -> RawResult<()> {
        sys::renameat(self.raw(), &name_cstring(from)?, &name_cstring(to)?)
    }

    /// A rename that never replaces `to` (`EEXIST` if it exists).
    pub(crate) fn rename_noreplace(&self, from: &FileName, to: &FileName) -> RawResult<()> {
        sys::renameat_noreplace(self.raw(), &name_cstring(from)?, &name_cstring(to)?)
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

/// Syncs the parent of the directory `dirfd`: opens its `..` (the directory
/// that holds its entry, whatever the path said) and `fsync`s it. Where
/// this caller may not open the parent (`EACCES`, `EPERM`): an `fsync` of
/// the directory itself, which on ext4 and XFS commits the transaction its
/// `mkdir` (and so the parent's entry) was in and reports that commit's
/// failure, then `syncfs` of its file system for the others (a directory on
/// another file system than its parent is a mount root, never made by a
/// create). For an existing directory (`!made`), a file system that cannot
/// sync a directory (unsupported: `sync_dir` and the probe class such a
/// volume) is not an error.
fn sync_parent(p: &PosixPlatform, dirfd: RawFd, made: bool) -> RawResult<()> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
    let synced = match p.retry_open(|| sys::openat(dirfd, c"..", flags, 0)) {
        Ok(fd) => sys::fsync(fd.as_raw_fd()),
        Err(e) if matches!(e.code, libc::EACCES | libc::EPERM) => {
            sys::fsync(dirfd).and_then(|()| sys::syncfs(dirfd))
        }
        Err(e) => Err(e),
    };
    match synced {
        Err(e) if !made && is_unsupported(e) => Ok(()),
        r => r,
    }
}

/// A directory's device and inode.
type Id = (libc::dev_t, libc::ino_t);

/// The identity of the directory `c` names (not following a final
/// symlink).
fn identity(c: &CStr) -> RawResult<Id> {
    sys::lstat(c).map(|st| (st.st_dev, st.st_ino))
}

/// Removes the directory `c` names if it is still the one this call made
/// (`id`; after a failed sync the caller still holds it open, so its inode
/// cannot have been reused meanwhile; after a failed open it may have been,
/// by an empty directory at worst): empty, since nothing was created inside
/// it, and `rmdir` never removes one that is not. Best effort: the caller
/// returns the error that failed the open.
fn remove_made(c: &CStr, id: Option<Id>) {
    if id.is_some() && identity(c).ok() == id {
        let _removed = sys::rmdir(c);
    }
}

/// A path as a C string; `EINVAL` if it contains a NUL byte.
fn path_cstring(path: &Path) -> RawResult<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| sys::errno(libc::EINVAL))
}

/// A validated file name as a C string (a `FileName` never contains NUL).
pub(crate) fn name_cstring(name: &FileName) -> RawResult<CString> {
    CString::new(name.as_str()).map_err(|_| sys::errno(libc::EINVAL))
}
