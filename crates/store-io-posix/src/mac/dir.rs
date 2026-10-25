//! Directory handles on macOS: an `O_DIRECTORY|O_RDONLY|O_CLOEXEC`
//! descriptor that every file operation is relative to, and that
//! `F_FULLFSYNC` makes durable.

use core::sync::atomic::{AtomicU8, Ordering};
use std::ffi::{CStr, CString};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;

use store_io_core::errno::is_unsupported;
use store_io_core::evidence::Tri;
use store_io_platform::{FileName, RawResult};

use crate::PosixPlatform;
use crate::sys;

/// An open directory (closed on drop).
///
/// Files are created, opened, renamed and unlinked relative to it with a
/// validated single-component name, so no operation can leave the
/// directory. Its sync is `F_FULLFSYNC` on its descriptor; an error fails
/// the operation.
///
/// The directory and every file opened through it share their volume's
/// flush state, which the platform keeps one of for each volume: once the
/// volume has refused `F_FULLFSYNC`, every flush on it is `fsync`; once it
/// has accepted it, a refusal is a failed flush.
#[derive(Debug)]
pub struct PosixDir {
    fd: OwnedFd,
    full: Arc<FlushState>,
}

/// A volume's answer to `F_FULLFSYNC`, shared by every handle on it
/// (D-133): not asked yet, accepted, or refused.
///
/// Refused only from "not asked": every flush on the volume is `fsync`
/// from then on, and a store there is classed by the refusal (Unsafe, open
/// only overridden). Once accepted it never weakens: stores are classed by
/// that answer, so a later refusal is a failed flush, never a downgrade.
#[derive(Debug, Default)]
pub(crate) struct FlushState(AtomicU8);

const UNASKED: u8 = 0;
const ACCEPTED: u8 = 1;
const REFUSED: u8 = 2;

impl FlushState {
    /// Whether flushes on the volume are `F_FULLFSYNC` (not refused).
    pub(crate) fn full(&self) -> bool {
        self.0.load(Ordering::Acquire) != REFUSED
    }

    /// Records an acceptance (nothing changes after a refusal).
    pub(crate) fn accept(&self) {
        let _was = self
            .0
            .compare_exchange(UNASKED, ACCEPTED, Ordering::AcqRel, Ordering::Acquire);
    }

    /// Records a refusal. False, and nothing changes, if the volume has
    /// accepted `F_FULLFSYNC`: the refusal is then a failed flush.
    pub(crate) fn refuse(&self) -> bool {
        match self
            .0
            .compare_exchange(UNASKED, REFUSED, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => true,
            Err(was) => was == REFUSED,
        }
    }

    /// The volume's answer: `Yes` accepted, `No` refused, `Unknown` not
    /// asked.
    pub(crate) fn answer(&self) -> Tri {
        match self.0.load(Ordering::Acquire) {
            ACCEPTED => Tri::Yes,
            REFUSED => Tri::No,
            _ => Tri::Unknown,
        }
    }

    /// A flush of `fd`, on this volume: `F_FULLFSYNC`, its answer recorded;
    /// `fsync` once the volume has refused it, this flush's refusal
    /// included if the volume had not been asked. A refusal once accepted
    /// is the error.
    pub(crate) fn flush(&self, fd: RawFd) -> RawResult<()> {
        if !self.full() {
            return sys::fsync(fd);
        }
        match sys::full_fsync(fd) {
            Ok(()) => {
                self.accept();
                Ok(())
            }
            Err(e) if sys::full_fsync_refused(e.code) && self.refuse() => sys::fsync(fd),
            Err(e) => Err(e),
        }
    }
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
        let full = match p.flush_state(fd.as_raw_fd()) {
            Ok(full) => full,
            Err(e) => {
                remove_made(&c, id);
                return Err(e);
            }
        };
        if create {
            if let Err(e) = sync_parent(p, fd.as_raw_fd(), made) {
                remove_made(&c, id);
                return Err(e);
            }
        }
        Ok(Self { fd, full })
    }

    /// `renameat` of `from` over `to`, both inside this directory.
    pub(crate) fn rename(&self, from: &FileName, to: &FileName) -> RawResult<()> {
        sys::renameat(self.raw(), &name_cstring(from)?, &name_cstring(to)?)
    }

    /// A rename that never replaces `to` (`renameatx_np(RENAME_EXCL)`:
    /// `EEXIST` if it exists).
    pub(crate) fn rename_noreplace(&self, from: &FileName, to: &FileName) -> RawResult<()> {
        sys::renameat_noreplace(self.raw(), &name_cstring(from)?, &name_cstring(to)?)
    }

    /// `unlinkat` of `name`.
    pub(crate) fn unlink(&self, name: &FileName) -> RawResult<()> {
        sys::unlinkat(self.raw(), &name_cstring(name)?)
    }

    /// `F_FULLFSYNC` of the directory itself (`fsync` once the volume has
    /// refused it; see [`FlushState::flush`]).
    pub(crate) fn sync(&self) -> RawResult<()> {
        self.full.flush(self.raw())
    }

    /// The volume's flush state, which the files opened through this
    /// directory share.
    pub(crate) fn flush_state(&self) -> Arc<FlushState> {
        Arc::clone(&self.full)
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
/// that holds its entry, whatever the path said) and flushes it on the
/// parent's own volume ([`FlushState::flush`]). Where this caller may not
/// open the parent (`EACCES`, `EPERM`): a flush of the directory itself,
/// which commits the volume's transaction (APFS) or journal (HFS+) holding
/// its entry (a directory on another volume than its parent is a mount
/// root, never made by a create). For an existing directory (`!made`), a
/// read-only volume (`EROFS`: its entries cannot change), or one that
/// cannot sync a directory and has not accepted `F_FULLFSYNC`
/// (unsupported: `sync_dir` and the probe class such a volume), is not an
/// error; one that has accepted `F_FULLFSYNC` and now refuses it is a
/// failed flush.
fn sync_parent(p: &PosixPlatform, dirfd: RawFd, made: bool) -> RawResult<()> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
    let parent = match p.retry_open(|| sys::openat(dirfd, c"..", flags, 0)) {
        Ok(fd) => Some(fd),
        Err(e) if matches!(e.code, libc::EACCES | libc::EPERM) => None,
        Err(e) => return Err(e),
    };
    let fd = parent.as_ref().map_or(dirfd, AsRawFd::as_raw_fd);
    let state = p.flush_state(fd)?;
    match state.flush(fd) {
        Err(e)
            if !made
                && (e.code == libc::EROFS || (is_unsupported(e) && state.answer() != Tri::Yes)) =>
        {
            Ok(())
        }
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
