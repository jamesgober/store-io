//! Data file handles: `O_DIRECT|O_CLOEXEC` descriptors opened relative to a
//! confined directory, closed on drop.

use core::ffi::c_int;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::sync::Arc;

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
///
/// The descriptor is shared with the submit pool's jobs (D-096): a job holds
/// a clone, so the descriptor stays open, and its number cannot be reused,
/// until the job's system call has returned, even if the file is dropped
/// first.
#[derive(Debug)]
pub struct PosixFile {
    fd: Arc<OwnedFd>,
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
            fd: Arc::new(fd),
            writable,
            block_device,
        })
    }

    /// A read-write twin of this handle (D-124): the same file through
    /// `/proc/thread-self/fd` (never a file that has since taken its name),
    /// an open file description of its own, checked to be the same inode.
    ///
    /// # Errors
    ///
    /// As [`read_only_twin`]; `EBADF` for a read-only handle.
    pub(crate) fn read_write_twin(&self) -> RawResult<Self> {
        if !self.writable {
            return Err(sys::errno(libc::EBADF));
        }
        Self::from_fd(twin(&self.fd, libc::O_RDWR)?, true)
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

    /// What the queue's executor needs to know of the file.
    #[inline]
    pub(crate) fn traits(&self) -> sys::FileTraits {
        sys::FileTraits {
            bdev: self.block_device,
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

    /// Whether this is the open file `fd` names (exact: the same
    /// descriptor, kept open by its holder).
    #[inline]
    pub(crate) fn same_fd(&self, fd: &Arc<OwnedFd>) -> bool {
        Arc::ptr_eq(&self.fd, fd)
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

/// A read-only twin of the open file `fd`: a second open file description
/// of the very same file (`O_RDONLY|O_DIRECT|O_CLOEXEC`, opened through
/// `/proc/thread-self/fd`, checked to be the same inode), for the kernel to
/// hold as an io_uring direct descriptor that serves reads (D-078, D-107).
///
/// Why a twin: the store's lock lives on the store's own description and is
/// released at that description's last reference, so a ring holding that
/// description would keep the store locked after a killed process, until
/// the ring's teardown. Why read-only, and reads only: a write in flight
/// holds a reference to the description it was issued on; issued on the
/// store's own, it keeps the store locked until it completes (lock fencing:
/// no successor writes before a dead owner's last write has landed). On the
/// twin it would not, so writes never use it, and the twin cannot write.
///
/// # Errors
///
/// The open's failure (no `/proc`, say), or `ESTALE` if what opened is not
/// this file.
pub(crate) fn read_only_twin(fd: &OwnedFd) -> RawResult<OwnedFd> {
    twin(fd, libc::O_RDONLY)
}

/// As [`read_only_twin`], opened with `access` (`O_RDONLY` or `O_RDWR`).
fn twin(fd: &OwnedFd, access: libc::c_int) -> RawResult<OwnedFd> {
    let path = std::ffi::CString::new(format!("/proc/thread-self/fd/{}", fd.as_raw_fd()))
        .map_err(|_| sys::errno(libc::EINVAL))?;
    let twin = sys::openat(
        libc::AT_FDCWD,
        &path,
        access | libc::O_DIRECT | libc::O_CLOEXEC,
        0,
    )?;
    let (mine, other) = (sys::fstat(fd.as_raw_fd())?, sys::fstat(twin.as_raw_fd())?);
    if (mine.st_dev, mine.st_ino) != (other.st_dev, other.st_ino) {
        return Err(sys::errno(libc::ESTALE));
    }
    Ok(twin)
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::alloc::{Layout, alloc_zeroed, dealloc};
    use std::os::fd::{AsRawFd, OwnedFd};

    use super::read_only_twin;
    use crate::sys;

    /// The twin is a separate open file description of the same file: a
    /// read lock through it conflicts with a write lock on the original
    /// (locks of one description never conflict with each other); it cannot
    /// write; and it reads the file's bytes.
    #[test]
    fn test_the_read_only_twin_is_a_separate_description_of_the_same_file() {
        let dir = std::env::var_os("STORE_IO_TEST_DIR")
            .or_else(|| std::env::var_os("HOME"))
            .map_or_else(std::env::temp_dir, std::path::PathBuf::from)
            .join(format!(".store-io-twin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f");
        std::fs::write(&path, vec![0x5Au8; 4096]).unwrap();
        let fd: OwnedFd = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap()
            .into();
        let twin = read_only_twin(&fd).unwrap();
        assert_ne!(twin.as_raw_fd(), fd.as_raw_fd());
        sys::ofd_setlk(fd.as_raw_fd(), libc::F_WRLCK).unwrap();
        assert!(
            sys::ofd_setlk(twin.as_raw_fd(), libc::F_RDLCK).is_err(),
            "the twin shares the original's description"
        );
        // An aligned block, as O_DIRECT requires.
        let layout = Layout::from_size_align(4096, 4096).unwrap();
        // SAFETY: a non-zero-sized layout; freed below.
        let p = unsafe { alloc_zeroed(layout) };
        assert!(!p.is_null());
        // SAFETY: pread(2) into the 4096-byte block just allocated.
        let n = unsafe { libc::pread(twin.as_raw_fd(), p.cast(), 4096, 0) };
        // SAFETY: `p` holds 4096 initialised bytes.
        let same = unsafe { core::slice::from_raw_parts(p, 4096) }
            .iter()
            .all(|&b| b == 0x5A);
        // An aligned whole-block write, which O_DIRECT would take on a
        // writable descriptor: the twin refuses it for its access mode.
        // SAFETY: pwrite(2) from the same live 4096-byte block.
        let w = unsafe { libc::pwrite(twin.as_raw_fd(), p.cast(), 4096, 0) };
        let werr = std::io::Error::last_os_error().raw_os_error();
        // SAFETY: allocated above with this layout.
        unsafe { dealloc(p, layout) };
        assert_eq!(n, 4096);
        assert!(same, "the twin read other bytes");
        assert_eq!((w, werr), (-1, Some(libc::EBADF)), "the twin can write");
        drop((twin, fd));
        let _cleanup = std::fs::remove_dir_all(&dir);
    }
}
