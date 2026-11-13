//! The POSIX system calls both backends make (Linux and macOS), with the
//! errors and the signal masking their threads share. Every `unsafe` block
//! is preceded by a `SAFETY` comment citing the contract of the call it
//! makes; wrappers return the raw `errno` as an [`OsError`] and never
//! interpret it.

use core::ffi::{CStr, c_int, c_long, c_uint};
use core::mem::MaybeUninit;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use store_io_core::error::{OsError, OsErrorSource};
use store_io_platform::RawResult;

// ----- errors ---------------------------------------------------------------

/// An `errno`-sourced error.
#[inline]
#[must_use]
pub(crate) const fn errno(code: c_int) -> OsError {
    OsError {
        code,
        source: OsErrorSource::Errno,
    }
}

/// The calling thread's current `errno`.
#[inline]
pub(crate) fn last_errno() -> OsError {
    errno(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
}

/// Clears the calling thread's `errno` (a call that reports its end and its
/// failure both by a null return tells them apart by it).
fn clear_errno() {
    #[cfg(target_os = "linux")]
    // SAFETY: __errno_location returns this thread's errno slot, valid for
    // the thread's life.
    unsafe {
        *libc::__errno_location() = 0;
    }
    #[cfg(target_os = "macos")]
    // SAFETY: __error returns this thread's errno slot, valid for the
    // thread's life.
    unsafe {
        *libc::__error() = 0;
    }
}

/// Bytes free to an unprivileged writer on the file system holding `fd`
/// (`fstatvfs(2)`: `f_bavail * f_frsize`, saturating).
#[cfg(target_os = "linux")]
pub(crate) fn free_space(fd: RawFd) -> RawResult<u64> {
    let mut st = MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: fstatvfs(2): `fd` is open for the call and `st` is writable
    // memory of the structure's size, filled on success.
    if unsafe { libc::fstatvfs(fd, st.as_mut_ptr()) } != 0 {
        return Err(last_errno());
    }
    // SAFETY: the call succeeded, so it filled `st` (and it was zeroed).
    let st = unsafe { st.assume_init() };
    Ok(st.f_bavail.saturating_mul(st.f_frsize))
}

/// Bytes free to an unprivileged writer on the file system holding `fd`
/// (`fstatfs(2)`: `f_bavail * f_bsize`, saturating; its counts are 64 bits
/// wide, where `statvfs`'s are truncated to 32 on Darwin).
#[cfg(target_os = "macos")]
pub(crate) fn free_space(fd: RawFd) -> RawResult<u64> {
    let mut st = MaybeUninit::<libc::statfs>::zeroed();
    // SAFETY: fstatfs(2): `fd` is open for the call and `st` is writable
    // memory of the structure's size, filled on success.
    if unsafe { libc::fstatfs(fd, st.as_mut_ptr()) } != 0 {
        return Err(last_errno());
    }
    // SAFETY: the call succeeded, so it filled `st` (and it was zeroed).
    let st = unsafe { st.assume_init() };
    Ok(st.f_bavail.saturating_mul(u64::from(st.f_bsize)))
}

/// Whether the directory `fd` holds no entry but `.` and `..`, read on a
/// descriptor of its own (`openat(fd, ".")`, closed with its stream), so
/// `fd`'s offset and life are untouched.
pub(crate) fn dir_is_empty(fd: RawFd) -> RawResult<bool> {
    // An open interrupted by a signal is tried again (as the backends' opens
    // are, a bounded number of times).
    let mut tries = 0;
    let own = loop {
        // SAFETY: openat(2): `fd` is an open directory for the call and the
        // name is a NUL-terminated literal.
        let own = unsafe {
            libc::openat(
                fd,
                c".".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if own >= 0 {
            break own;
        }
        let e = last_errno();
        tries += 1;
        if e.code != libc::EINTR || tries >= 64 {
            return Err(e);
        }
    };
    // SAFETY: fdopendir(3): `own` is an open directory descriptor nothing
    // else owns; on success the stream owns it (closedir closes it).
    let d = unsafe { libc::fdopendir(own) };
    if d.is_null() {
        let e = last_errno();
        // SAFETY: the stream did not take `own`; it is closed once here.
        let _closed = unsafe { libc::close(own) };
        return Err(e);
    }
    let result = loop {
        clear_errno();
        // SAFETY: readdir(3) on the open stream `d`, used by this thread only.
        let e = unsafe { libc::readdir(d) };
        if e.is_null() {
            let raw = last_errno();
            break if raw.code == 0 { Ok(true) } else { Err(raw) };
        }
        // SAFETY: a non-null entry from readdir holds a NUL-terminated
        // name, valid until the next readdir or closedir on `d`.
        let name = unsafe { CStr::from_ptr((*e).d_name.as_ptr()) };
        if name != c"." && name != c".." {
            break Ok(false);
        }
    };
    // SAFETY: closedir(3) on the stream opened above, once; it closes `own`.
    let _closed = unsafe { libc::closedir(d) };
    result
}

/// Converts a descriptor-returning result into an owned descriptor.
pub(crate) fn owned(fd: c_long) -> RawResult<OwnedFd> {
    if fd < 0 {
        return Err(last_errno());
    }
    let raw = RawFd::try_from(fd).map_err(|_| errno(libc::EBADF))?;
    // SAFETY: `raw` was just returned by an open-family call and nothing else
    // owns it; `OwnedFd` closes it exactly once.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

pub(crate) fn check(r: c_int) -> RawResult<()> {
    if r == 0 { Ok(()) } else { Err(last_errno()) }
}

/// Opens `name` relative to `dirfd` with `openat(2)`.
pub(crate) fn openat(dirfd: RawFd, name: &CStr, flags: c_int, mode: u32) -> RawResult<OwnedFd> {
    // SAFETY: openat(2): `name` is NUL-terminated and outlives the call; the
    // variadic mode argument is a `mode_t` promoted to `unsigned int`, read
    // only when O_CREAT or O_TMPFILE is set. Returns a descriptor or -1.
    let fd = unsafe { libc::openat(dirfd, name.as_ptr(), flags, mode as c_uint) };
    owned(c_long::from(fd))
}

/// Creates a directory with `mkdir(2)`.
pub(crate) fn mkdir(path: &CStr, mode: u32) -> RawResult<()> {
    // SAFETY: mkdir(2): `path` is NUL-terminated and outlives the call.
    check(unsafe { libc::mkdir(path.as_ptr(), mode as libc::mode_t) })
}

/// Removes an empty directory with `rmdir(2)`.
pub(crate) fn rmdir(path: &CStr) -> RawResult<()> {
    // SAFETY: rmdir(2): `path` is NUL-terminated and outlives the call.
    check(unsafe { libc::rmdir(path.as_ptr()) })
}

/// Duplicates a descriptor onto the same open file description with
/// `F_DUPFD_CLOEXEC` (fcntl(2)).
pub(crate) fn dup_cloexec(fd: RawFd) -> RawResult<OwnedFd> {
    // SAFETY: fcntl(2) F_DUPFD_CLOEXEC takes an integer argument (the lowest
    // acceptable descriptor number) and returns a new descriptor or -1.
    let new = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    owned(c_long::from(new))
}

/// `fstat(2)`.
pub(crate) fn fstat(fd: RawFd) -> RawResult<libc::stat> {
    let mut st = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat(2) writes a complete `struct stat` through the pointer on
    // success and nothing on failure; the buffer is assumed initialised only
    // after a 0 return.
    let r = unsafe { libc::fstat(fd, st.as_mut_ptr()) };
    check(r)?;
    // SAFETY: fstat returned 0, so every field was written by the kernel.
    Ok(unsafe { st.assume_init() })
}

/// `fstatat(2)` of `name` in the directory `dirfd`, not following a final
/// link (a container is opened `O_NOFOLLOW`).
pub(crate) fn stat_in(dirfd: RawFd, name: &CStr) -> RawResult<libc::stat> {
    let mut st = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstatat(2): `dirfd` is an open directory, `name` is
    // NUL-terminated and outlives the call; a complete `struct stat` is
    // written through the pointer on success and nothing on failure; the
    // buffer is assumed initialised only after a 0 return.
    let r = unsafe {
        libc::fstatat(
            dirfd,
            name.as_ptr(),
            st.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    check(r)?;
    // SAFETY: fstatat returned 0, so every field was written by the kernel.
    Ok(unsafe { st.assume_init() })
}

/// A file's identity from its `stat`: device and inode.
pub(crate) fn identity_of(st: &libc::stat) -> store_io_platform::FileIdentity {
    // `st_ino` is unsigned everywhere; `st_dev` is signed on macOS (`i32`).
    // Both sides of every comparison take the same casts, so equal files
    // compare equal and different ones differ.
    #[allow(clippy::cast_sign_loss, clippy::unnecessary_cast)]
    store_io_platform::FileIdentity {
        volume: st.st_dev as u64,
        file: u128::from(st.st_ino as u64),
    }
}

/// `lstat(2)` (`fstatat` with `AT_SYMLINK_NOFOLLOW`) of a path.
pub(crate) fn lstat(path: &CStr) -> RawResult<libc::stat> {
    let mut st = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstatat(2): `path` is NUL-terminated and outlives the call; a
    // complete `struct stat` is written through the pointer on success and
    // nothing on failure; the buffer is assumed initialised only after a 0
    // return.
    let r = unsafe {
        libc::fstatat(
            libc::AT_FDCWD,
            path.as_ptr(),
            st.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    check(r)?;
    // SAFETY: fstatat returned 0, so every field was written by the kernel.
    Ok(unsafe { st.assume_init() })
}

/// `fsync(2)`: data, metadata and a device cache flush.
#[inline]
pub(crate) fn fsync(fd: RawFd) -> RawResult<()> {
    // SAFETY: fsync(2) takes only a descriptor.
    check(unsafe { libc::fsync(fd) })
}

/// `renameat(2)` within one directory.
pub(crate) fn renameat(dirfd: RawFd, from: &CStr, to: &CStr) -> RawResult<()> {
    // SAFETY: renameat(2): both names are NUL-terminated and outlive the call.
    check(unsafe { libc::renameat(dirfd, from.as_ptr(), dirfd, to.as_ptr()) })
}

/// `unlinkat(2)` of a file (no `AT_REMOVEDIR`).
pub(crate) fn unlinkat(dirfd: RawFd, name: &CStr) -> RawResult<()> {
    // SAFETY: unlinkat(2): `name` is NUL-terminated and outlives the call.
    check(unsafe { libc::unlinkat(dirfd, name.as_ptr(), 0) })
}

/// `ftruncate(2)`: sets the file size to `len`.
pub(crate) fn ftruncate(fd: RawFd, len: libc::off_t) -> RawResult<()> {
    // SAFETY: ftruncate(2) takes a descriptor and a length; it touches no
    // user memory.
    check(unsafe { libc::ftruncate(fd, len) })
}

/// Signals the kernel forces on the thread that caused them, whatever its
/// mask: faults, a breakpoint trap, and a seccomp trap (`SIGSYS`). Blocked,
/// such a signal kills the process without running its handler, so the
/// library's threads keep them open: a stack overflow still reports, and a
/// sandbox that emulates trapped system calls in a `SIGSYS` handler still
/// works on a worker.
const FAULT_SIGNALS: [c_int; 6] = [
    libc::SIGSEGV,
    libc::SIGBUS,
    libc::SIGFPE,
    libc::SIGILL,
    libc::SIGTRAP,
    libc::SIGSYS,
];

/// Every signal the C library lets an application block, except
/// [`FAULT_SIGNALS`], blocked on the calling thread until dropped, when the
/// previous mask is restored exactly. It only ever adds to the caller's mask,
/// never opens a signal the caller blocked. A thread created meanwhile
/// inherits that mask before any of its own code runs, so a process's
/// handlers for any other signal never run on it; the forced signals stay as
/// the caller had them until the library thread opens them itself
/// ([`unblock_fault_signals`]). Not `Send`: the saved mask belongs to the
/// thread that made it.
pub(crate) struct SignalsBlocked {
    /// The mask to restore; `None` if blocking failed (nothing changed).
    old: Option<libc::sigset_t>,
    _thread_bound: core::marker::PhantomData<*const ()>,
}

impl SignalsBlocked {
    pub(crate) fn new() -> Self {
        let mut set = MaybeUninit::<libc::sigset_t>::zeroed();
        let mut old = MaybeUninit::<libc::sigset_t>::zeroed();
        let none = Self {
            old: None,
            _thread_bound: core::marker::PhantomData,
        };
        // SAFETY: sigfillset(3) initialises the set through the pointer,
        // which is valid and exclusively borrowed for the call.
        if unsafe { libc::sigfillset(set.as_mut_ptr()) } != 0 {
            return none;
        }
        // Not blocked by this guard: the spawn itself and the new thread's
        // start-up stay as able to take a forced signal as the caller was.
        for sig in FAULT_SIGNALS {
            // SAFETY: sigdelset(3) updates the set initialised above through
            // the pointer, valid and exclusively borrowed for the call; the
            // signal number is a valid constant.
            if unsafe { libc::sigdelset(set.as_mut_ptr(), sig) } != 0 {
                return none;
            }
        }
        // SAFETY: pthread_sigmask(3) reads the set initialised above and
        // writes the previous mask through `old`, valid and exclusively
        // borrowed for the call.
        let rc = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, set.as_ptr(), old.as_mut_ptr()) };
        if rc != 0 {
            return none;
        }
        // SAFETY: the successful pthread_sigmask call above filled `old`.
        let old = unsafe { old.assume_init() };
        Self {
            old: Some(old),
            ..none
        }
    }
}

impl Drop for SignalsBlocked {
    fn drop(&mut self) {
        if let Some(old) = &self.old {
            // SAFETY: pthread_sigmask(3) reads the saved mask and, with a
            // null old-set pointer, writes nothing.
            let _status =
                unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, old, core::ptr::null_mut()) };
        }
    }
}

/// Opens [`FAULT_SIGNALS`] on the calling thread, whatever mask it inherited
/// (a caller may have blocked one): the first thing a library thread does.
/// Failure leaves the mask as it was (it cannot fail for a valid set).
pub(crate) fn unblock_fault_signals() {
    let mut set = MaybeUninit::<libc::sigset_t>::zeroed();
    // SAFETY: sigemptyset(3) initialises the set through the pointer, which
    // is valid and exclusively borrowed for the call.
    if unsafe { libc::sigemptyset(set.as_mut_ptr()) } != 0 {
        return;
    }
    for sig in FAULT_SIGNALS {
        // SAFETY: sigaddset(3) updates the set initialised above through the
        // pointer, valid and exclusively borrowed for the call; the signal
        // number is a valid constant.
        if unsafe { libc::sigaddset(set.as_mut_ptr(), sig) } != 0 {
            return;
        }
    }
    // SAFETY: pthread_sigmask(3) reads the set initialised above and, with a
    // null old-set pointer, writes nothing.
    let _status =
        unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, set.as_ptr(), core::ptr::null_mut()) };
}

/// `flock(2)`.
pub(crate) fn flock(fd: RawFd, operation: c_int) -> RawResult<()> {
    // SAFETY: flock(2) takes a descriptor and an operation.
    check(unsafe { libc::flock(fd, operation) })
}
