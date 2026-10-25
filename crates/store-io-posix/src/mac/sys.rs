//! Thin wrappers over the macOS system calls the backend uses (the POSIX
//! ones both backends share are in `posix.rs`, re-exported here).
//!
//! Every `unsafe` block of the macOS backend lives here or in `posix.rs`,
//! each preceded by a `SAFETY` comment citing the contract of the call it
//! makes. Wrappers return the raw `errno` as an
//! [`OsError`](store_io_core::error::OsError) and never interpret it; the
//! engine classifies.

use core::ffi::{CStr, c_char, c_int, c_void};
use core::mem::MaybeUninit;
use std::os::fd::RawFd;

use store_io_platform::RawResult;

use crate::posix::check;
pub(crate) use crate::posix::{
    SignalsBlocked, dup_cloexec, errno, flock, fstat, fsync, ftruncate, last_errno, lstat, mkdir,
    openat, renameat, rmdir, unblock_fault_signals, unlinkat,
};

// ----- data -----------------------------------------------------------------

/// The most one `pread` or `pwrite` takes (`EINVAL` above `INT_MAX`); a
/// longer buffer transfers short, and the caller continues.
const MAX_TRANSFER: usize = i32::MAX as usize;

/// What the queue's executor needs to know of a file: whether its flushes
/// reach the drive (`F_FULLFSYNC`), or are downgraded to `fsync` because the
/// volume refused `F_FULLFSYNC` (a store opened despite that is overridden,
/// its receipts labelled with `FullFlushUnsupported`; `F_FULLFSYNC` is never
/// tried again on the volume).
#[derive(Debug, Clone, Copy)]
pub(crate) struct FileTraits {
    pub(crate) full: bool,
}

/// A data write of one buffer at `offset` (`pwrite`): durable at
/// completion when `dsync`, by a flush after the whole buffer is written (no
/// write-through primitive flushes the drive on macOS). Returns the bytes
/// written, which may be fewer than `len`.
#[inline]
pub(crate) fn write_data(
    fd: RawFd,
    ptr: *const u8,
    len: usize,
    offset: libc::off_t,
    dsync: bool,
    traits: FileTraits,
) -> RawResult<usize> {
    let len = len.min(MAX_TRANSFER);
    // SAFETY: pwrite(2): `ptr` points at at least `len` readable bytes that
    // the caller's `IoBuf` owns for the whole call (platform contract 1); the
    // kernel reads, never writes, the buffer.
    let n = unsafe { libc::pwrite(fd, ptr.cast::<c_void>(), len, offset) };
    let mut result = usize::try_from(n).map_err(|_| last_errno());
    if dsync && result == Ok(len) {
        result = flush_data(fd, traits).map(|()| len);
    }
    result
}

/// A data read of one buffer at `offset` (`pread`). Returns the bytes read,
/// which may be fewer than `len`.
#[inline]
pub(crate) fn read_data(
    fd: RawFd,
    ptr: *mut u8,
    len: usize,
    offset: libc::off_t,
) -> RawResult<usize> {
    let len = len.min(MAX_TRANSFER);
    // SAFETY: pread(2): `ptr` points at at least `len` writable bytes that
    // the caller's `IoBuf` owns exclusively for the whole call (platform
    // contract 1).
    let n = unsafe { libc::pread(fd, ptr.cast::<c_void>(), len, offset) };
    usize::try_from(n).map_err(|_| last_errno())
}

/// A data flush: `F_FULLFSYNC` (the file's data and attributes, then the
/// drive's cache: it drains the device's queue, so it covers every earlier
/// write to the device), or `fsync` where the volume refused it (see
/// [`FileTraits`]). macOS has no data-only drive flush.
#[inline]
pub(crate) fn flush_data(fd: RawFd, traits: FileTraits) -> RawResult<()> {
    if traits.full {
        full_fsync(fd)
    } else {
        fsync(fd)
    }
}

/// `fcntl(F_FULLFSYNC)`.
#[inline]
pub(crate) fn full_fsync(fd: RawFd) -> RawResult<()> {
    // SAFETY: fcntl(2) F_FULLFSYNC takes no argument and touches no user
    // memory.
    let r = unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) };
    if r == -1 { Err(last_errno()) } else { Ok(()) }
}

/// Whether an `F_FULLFSYNC` failure means the volume does not support it
/// (`ENOTSUP`, `EOPNOTSUPP`, `ENOSYS`, `ENOTTY`, `EINVAL`: SMB, some
/// external drives), as opposed to a failed flush.
pub(crate) fn full_fsync_refused(code: c_int) -> bool {
    [
        libc::ENOTSUP,
        libc::EOPNOTSUPP,
        libc::ENOSYS,
        libc::ENOTTY,
        libc::EINVAL,
    ]
    .contains(&code)
}

// ----- caching ----------------------------------------------------------------

/// `fcntl(F_NOCACHE, 1)` and `fcntl(F_RDAHEAD, 0)`: data caching and
/// read-ahead off for the descriptor. A hint (uncached only for requests
/// aligned to the kernel page, 16 KiB on Apple silicon), never part of a
/// durability argument; a refusal is not an error.
pub(crate) fn uncached(fd: RawFd) {
    // SAFETY: fcntl(2) F_NOCACHE and F_RDAHEAD take an integer argument and
    // touch no user memory.
    let _nocache = unsafe { libc::fcntl(fd, libc::F_NOCACHE, 1 as c_int) };
    // SAFETY: as above.
    let _rdahead = unsafe { libc::fcntl(fd, libc::F_RDAHEAD, 0 as c_int) };
}

// ----- space ------------------------------------------------------------------

/// `fcntl(F_PREALLOCATE)` of `len` bytes past the physical end of file
/// (`F_PEOFPOSMODE`), all or nothing (`F_ALLOCATEALL`), contiguous if
/// `contig` (`F_ALLOCATECONTIG`, which the file system may refuse). The
/// size is unchanged (`ftruncate` extends it).
pub(crate) fn preallocate(fd: RawFd, len: libc::off_t, contig: bool) -> RawResult<()> {
    let mut st = libc::fstore_t {
        fst_flags: libc::F_ALLOCATEALL | if contig { libc::F_ALLOCATECONTIG } else { 0 },
        fst_posmode: libc::F_PEOFPOSMODE,
        fst_offset: 0,
        fst_length: len,
        fst_bytesalloc: 0,
    };
    // SAFETY: fcntl(2) F_PREALLOCATE reads and updates the `fstore_t`
    // through the pointer, which is valid and exclusively borrowed for the
    // call.
    let r = unsafe { libc::fcntl(fd, libc::F_PREALLOCATE, &raw mut st) };
    if r == -1 { Err(last_errno()) } else { Ok(()) }
}

/// `fcntl(F_PUNCHHOLE)`: deallocates `[offset, offset + len)`, which must
/// be aligned to the file system's block; the size is unchanged.
pub(crate) fn punch_hole(fd: RawFd, offset: libc::off_t, len: libc::off_t) -> RawResult<()> {
    let hole = libc::fpunchhole_t {
        fp_flags: 0,
        reserved: 0,
        fp_offset: offset,
        fp_length: len,
    };
    // SAFETY: fcntl(2) F_PUNCHHOLE reads the `fpunchhole_t` through the
    // pointer, which is valid for the call.
    let r = unsafe { libc::fcntl(fd, libc::F_PUNCHHOLE, &raw const hole) };
    if r == -1 { Err(last_errno()) } else { Ok(()) }
}

// ----- directories --------------------------------------------------------------

/// `renameatx_np(RENAME_EXCL)` within one directory: `EEXIST` if `to`
/// exists, never replacing it.
pub(crate) fn renameat_noreplace(dirfd: RawFd, from: &CStr, to: &CStr) -> RawResult<()> {
    // SAFETY: renameatx_np(2): both names are NUL-terminated and outlive the
    // call; the flags are a plain integer.
    check(unsafe {
        libc::renameatx_np(dirfd, from.as_ptr(), dirfd, to.as_ptr(), libc::RENAME_EXCL)
    })
}

// ----- locks ----------------------------------------------------------------

/// `fcntl(F_OFD_SETLK)` over the whole file (macOS 14 and later): a lock of
/// the open file description, `lock_type` `F_WRLCK`, `F_RDLCK` or
/// `F_UNLCK`.
pub(crate) fn ofd_setlk(fd: RawFd, lock_type: libc::c_short) -> RawResult<()> {
    // Zeroed: the platform's `struct flock` may hold padding.
    let fl = MaybeUninit::<libc::flock>::zeroed();
    // SAFETY: zero-initialised above: every field is an integer, so the
    // zeroed value is a valid `flock`.
    let mut fl = unsafe { fl.assume_init() };
    fl.l_type = lock_type;
    fl.l_whence = libc::SEEK_SET as libc::c_short;
    fl.l_start = 0;
    fl.l_len = 0;
    // SAFETY: fcntl(2) F_OFD_SETLK reads the `struct flock` through the
    // pointer, which is valid for the call.
    let r = unsafe { libc::fcntl(fd, libc::F_OFD_SETLK, &raw const fl) };
    if r == -1 { Err(last_errno()) } else { Ok(()) }
}

// ----- the system ---------------------------------------------------------------

/// The macOS release (`kern.osproductversion`, for example `14.6.1`) as
/// `(major, minor, patch)`; `None` if it cannot be read or parsed.
pub(crate) fn os_release() -> Option<(u16, u16, u16)> {
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    let name = c"kern.osproductversion";
    // SAFETY: sysctlbyname(3): the name is NUL-terminated; `buf` is valid
    // for `len` bytes, which the call updates to the length written; no new
    // value is set (null pointer, 0 length).
    let r = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            &raw mut len,
            core::ptr::null_mut(),
            0,
        )
    };
    if r != 0 {
        return None;
    }
    parse_release(buf.get(..len)?)
}

/// `major.minor[.patch]`, NUL-terminated or not: untrusted bytes.
pub(crate) fn parse_release(raw: &[u8]) -> Option<(u16, u16, u16)> {
    let text = core::str::from_utf8(raw)
        .ok()?
        .trim_end_matches('\0')
        .trim();
    let mut parts = text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    let patch = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    Some((major, minor, patch))
}

/// Whether the kernel runs under a hypervisor (`kern.hv_vmm_present`);
/// `None` if it cannot be read.
pub(crate) fn under_hypervisor() -> Option<bool> {
    let mut v: c_int = 0;
    let mut len = core::mem::size_of::<c_int>();
    let name = c"kern.hv_vmm_present";
    // SAFETY: sysctlbyname(3): the name is NUL-terminated; `v` is valid for
    // `len` bytes; no new value is set.
    let r = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&raw mut v).cast::<c_void>(),
            &raw mut len,
            core::ptr::null_mut(),
            0,
        )
    };
    (r == 0).then_some(v != 0)
}

/// Milliseconds from `CLOCK_UPTIME_RAW_APPROX` (a value cached at context
/// switch: no system call; it stops in sleep). 0 if the call fails (it
/// cannot for a valid clock id and pointer).
pub(crate) fn monotonic_ms() -> u64 {
    let mut ts = MaybeUninit::<libc::timespec>::zeroed();
    // SAFETY: clock_gettime(3) writes one `struct timespec` through the
    // pointer, which is valid and exclusively borrowed for the call.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW_APPROX, ts.as_mut_ptr()) };
    if rc != 0 {
        return 0;
    }
    // SAFETY: zero-initialised, and filled by the successful call above.
    let ts = unsafe { ts.assume_init() };
    (ts.tv_sec as u64)
        .saturating_mul(1000)
        .saturating_add(ts.tv_nsec as u64 / 1_000_000)
}

/// What `fstatfs` says of a file's volume (the device behind it: M3).
#[derive(Debug, Clone)]
pub(crate) struct Volume {
    /// The file system's name (`f_fstypename`: `apfs`, `hfs`, `smbfs`...).
    pub(crate) fs: String,
    /// `MNT_LOCAL`.
    pub(crate) local: bool,
    /// What is mounted (`f_mntfromname`: `/dev/disk3s5`, a share).
    pub(crate) from: String,
}

/// `fstatfs(2)` of the volume holding `fd`.
pub(crate) fn volume(fd: RawFd) -> RawResult<Volume> {
    let mut sf = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: fstatfs(2) writes a complete `struct statfs` on success and
    // nothing on failure; assumed initialised only after a 0 return.
    check(unsafe { libc::fstatfs(fd, sf.as_mut_ptr()) })?;
    // SAFETY: fstatfs returned 0.
    let sf = unsafe { sf.assume_init() };
    Ok(Volume {
        fs: c_text(&sf.f_fstypename),
        local: sf.f_flags & libc::MNT_LOCAL as u32 != 0,
        from: c_text(&sf.f_mntfromname),
    })
}

/// A fixed `char` array up to its first NUL (all of it if none), lossily.
fn c_text(raw: &[c_char]) -> String {
    let bytes: Vec<u8> = raw
        .iter()
        .map(|&c| c as u8)
        .take_while(|&b| b != 0)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::parse_release;

    #[test]
    fn test_releases_parse_and_garbage_does_not() {
        assert_eq!(parse_release(b"14.6.1\0"), Some((14, 6, 1)));
        assert_eq!(parse_release(b"15.0"), Some((15, 0, 0)));
        assert_eq!(parse_release(b"13"), Some((13, 0, 0)));
        assert_eq!(parse_release(b""), None);
        assert_eq!(parse_release(b"x.1"), None);
        assert_eq!(parse_release(&[0xFF, b'.', b'1']), None);
    }
}
