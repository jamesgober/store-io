//! The ownership lock on macOS: an open-file-description lock on the I/O
//! descriptor, chosen by OS release, never by probing the command.
//!
//! macOS 14 and later: `fcntl(F_OFD_SETLK)` over the whole file, owned by
//! the open file description (released at its last reference). macOS 13,
//! where OFD locks are private API: `flock`, which has the same ownership.
//! Never a POSIX `F_SETLK` (any close of the inode releases it), and never
//! both kinds on one file (they share one lock list). A read-only open takes
//! the shared form (`F_RDLCK`, or `LOCK_SH`), which readers hold together
//! and which excludes the writer's exclusive lock.
//!
//! The lock handle holds a `dup` of the file's descriptor, which shares the
//! description; dropping the handle unlocks through the dup and closes it.
//! The unlock is explicit, so it does not wait for operations still queued
//! on the submit pool: drop (or drain) a parallel queue before the lock.

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::OnceLock;

use store_io_platform::RawResult;

use crate::file::PosixFile;
use crate::sys;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Ofd,
    Flock,
}

/// The kind this macOS release takes: OFD from 14, `flock` before (and if
/// the release cannot be read, which would be an unknown release: OFD,
/// public since 14).
pub(crate) fn kind() -> Kind {
    static KIND: OnceLock<Kind> = OnceLock::new();
    *KIND.get_or_init(|| match sys::os_release() {
        Some((major, _, _)) if major < 14 => Kind::Flock,
        _ => Kind::Ofd,
    })
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
    /// kernel's "would block" (`EAGAIN` or `EWOULDBLOCK`). An exclusive OFD
    /// lock on a read-only handle gives `EBADF`.
    pub(crate) fn acquire(file: &PosixFile, shared: bool) -> RawResult<Self> {
        Self::acquire_as(file, shared, kind())
    }

    /// As [`Self::acquire`], of the given kind (tests force `flock`).
    pub(crate) fn acquire_as(file: &PosixFile, shared: bool, kind: Kind) -> RawResult<Self> {
        let fd = sys::dup_cloexec(file.raw())?;
        let raw = fd.as_raw_fd();
        match kind {
            Kind::Ofd => {
                let t = if shared { libc::F_RDLCK } else { libc::F_WRLCK };
                sys::ofd_setlk(raw, t)?;
            }
            Kind::Flock => {
                let op = if shared { libc::LOCK_SH } else { libc::LOCK_EX };
                sys::flock(raw, op | libc::LOCK_NB)?;
            }
        }
        Ok(Self { fd, kind })
    }

    /// Whether the lock is an OFD lock (`true`) or `flock`.
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
