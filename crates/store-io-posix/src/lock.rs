//! The ownership lock: an open-file-description lock on the I/O descriptor.
//!
//! `fcntl(F_OFD_SETLK)` is owned by the open file description, so it is
//! released only when the last reference to that description is dropped,
//! including references held by in-flight kernel I/O. A POSIX `F_SETLK` is
//! owned by the process and any `close()` of any descriptor for the inode
//! releases it, so it is never used here. Kernels before 3.15 (no OFD locks)
//! fall back to `flock(LOCK_EX|LOCK_NB)`, which has the same ownership.
//! A read-only open takes the shared form (`F_RDLCK`, or `LOCK_SH`), which
//! readers hold together and which excludes the writer's exclusive lock.
//!
//! The lock handle holds a `dup` of the file's descriptor, which shares the
//! description; dropping the handle unlocks through the dup and closes it.

use std::os::fd::{AsRawFd, OwnedFd};

use store_io_platform::RawResult;

use crate::file::PosixFile;
use crate::sys;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Ofd,
    Flock,
}

/// A held ownership lock, released on drop.
#[derive(Debug)]
pub struct PosixLock {
    fd: OwnedFd,
    kind: Kind,
}

impl PosixLock {
    /// Takes a lock over the whole file on the file's own open file
    /// description: an exclusive write lock, or with `shared` a read lock
    /// that other readers may also hold. A conflicting holder gives the
    /// kernel's "would block" (`EAGAIN`, `EACCES` or `EWOULDBLOCK`). An
    /// exclusive lock on a read-only handle gives `EBADF`, because a write
    /// lock needs a descriptor open for writing; a read lock works on one.
    pub(crate) fn acquire(file: &PosixFile, shared: bool) -> RawResult<Self> {
        let fd = sys::dup_cloexec(file.raw())?;
        let raw = fd.as_raw_fd();
        let (ofd, flock) = if shared {
            (libc::F_RDLCK, libc::LOCK_SH)
        } else {
            (libc::F_WRLCK, libc::LOCK_EX)
        };
        match sys::ofd_setlk(raw, ofd) {
            Ok(()) => Ok(Self {
                fd,
                kind: Kind::Ofd,
            }),
            Err(e) if e.code == libc::EINVAL => {
                sys::flock(raw, flock | libc::LOCK_NB)?;
                Ok(Self {
                    fd,
                    kind: Kind::Flock,
                })
            }
            Err(e) => Err(e),
        }
    }

    /// Whether the lock is an OFD lock (`true`) or the `flock` fallback.
    #[must_use]
    pub fn is_ofd(&self) -> bool {
        self.kind == Kind::Ofd
    }
}

impl Drop for PosixLock {
    fn drop(&mut self) {
        let raw = self.fd.as_raw_fd();
        // An unlock cannot fail for a lock this handle holds; closing the dup
        // afterwards would release it anyway if it were the last reference.
        let _unlock_status = match self.kind {
            Kind::Ofd => sys::ofd_setlk(raw, libc::F_UNLCK),
            Kind::Flock => sys::flock(raw, libc::LOCK_UN),
        };
    }
}
