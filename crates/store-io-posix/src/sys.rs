//! Thin wrappers over the Linux system calls the backend uses.
//!
//! Every `unsafe` block in the crate lives here, each preceded by a `SAFETY`
//! comment citing the contract of the call it makes. Wrappers return the raw
//! `errno` as an [`OsError`] and never interpret it; the engine classifies.
//!
//! Structures the kernel ABI defines but `libc` does not expose (`statx` with
//! its direct-I/O and atomic-write fields, FIEMAP, `cachestat`, the NVMe
//! admin passthrough command) are declared here as `#[repr(C)]` with their
//! sizes asserted at compile time, so the layout is independent of the C
//! library in use.

use core::ffi::{CStr, c_int, c_long, c_short, c_uint, c_void};
use core::mem::{MaybeUninit, size_of};
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

/// Converts a descriptor-returning result into an owned descriptor.
fn owned(fd: c_long) -> RawResult<OwnedFd> {
    if fd < 0 {
        return Err(last_errno());
    }
    let raw = RawFd::try_from(fd).map_err(|_| errno(libc::EBADF))?;
    // SAFETY: `raw` was just returned by an open-family call and nothing else
    // owns it; `OwnedFd` closes it exactly once.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn check(r: c_int) -> RawResult<()> {
    if r == 0 { Ok(()) } else { Err(last_errno()) }
}

// ----- opening --------------------------------------------------------------

/// Resolve flags for confined opens: stay beneath the directory and follow
/// no symlink in any component (which also forbids magic links).
pub(crate) const RESOLVE_CONFINED: u64 = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS;

/// `struct open_how` (openat2(2)), 24 bytes.
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

const _: () = assert!(size_of::<OpenHow>() == 24, "struct open_how is 24 bytes");

/// Opens `name` relative to `dirfd` with `openat2(2)`.
///
/// `ENOSYS` on kernels before 5.6; `EAGAIN` when a rename raced the
/// `RESOLVE_BENEATH` check (the caller retries and counts).
pub(crate) fn openat2(
    dirfd: RawFd,
    name: &CStr,
    flags: c_int,
    mode: u32,
    resolve: u64,
) -> RawResult<OwnedFd> {
    let how = OpenHow {
        flags: u64::from(flags as u32),
        mode: u64::from(mode),
        resolve,
    };
    // SAFETY: openat2(2): `name` is NUL-terminated and outlives the call;
    // `how` is a fully initialised `struct open_how` and its size is passed,
    // so the kernel reads exactly that many bytes. Returns a descriptor or -1.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dirfd,
            name.as_ptr(),
            &raw const how,
            size_of::<OpenHow>(),
        )
    };
    owned(fd)
}

/// Whether this kernel implements `openat2(2)`: a no-op open of the current
/// directory with `RESOLVE_BENEATH` succeeds on 5.6 and later and fails with
/// `ENOSYS` before.
pub(crate) fn openat2_supported() -> bool {
    openat2(
        libc::AT_FDCWD,
        c".",
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        0,
        libc::RESOLVE_BENEATH,
    )
    .is_ok()
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

/// Duplicates a descriptor onto the same open file description with
/// `F_DUPFD_CLOEXEC` (fcntl(2)).
pub(crate) fn dup_cloexec(fd: RawFd) -> RawResult<OwnedFd> {
    // SAFETY: fcntl(2) F_DUPFD_CLOEXEC takes an integer argument (the lowest
    // acceptable descriptor number) and returns a new descriptor or -1.
    let new = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    owned(c_long::from(new))
}

// ----- metadata -------------------------------------------------------------

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

/// `fstatfs(2)`.
pub(crate) fn fstatfs(fd: RawFd) -> RawResult<libc::statfs> {
    let mut sf = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: fstatfs(2) writes a complete `struct statfs` on success and
    // nothing on failure; assumed initialised only after a 0 return.
    let r = unsafe { libc::fstatfs(fd, sf.as_mut_ptr()) };
    check(r)?;
    // SAFETY: fstatfs returned 0.
    Ok(unsafe { sf.assume_init() })
}

/// `struct statx_timestamp` (statx(2)).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct StatxTimestamp {
    pub tv_sec: i64,
    pub tv_nsec: u32,
    reserved: i32,
}

/// `struct statx` (statx(2)) as of kernel 6.16: 256 bytes, including the
/// direct-I/O alignment fields (6.1) and the atomic-write fields (6.11).
/// Kernels that predate a field leave it zero and clear its mask bit.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Statx {
    pub stx_mask: u32,
    pub stx_blksize: u32,
    pub stx_attributes: u64,
    pub stx_nlink: u32,
    pub stx_uid: u32,
    pub stx_gid: u32,
    pub stx_mode: u16,
    pad1: u16,
    pub stx_ino: u64,
    pub stx_size: u64,
    pub stx_blocks: u64,
    pub stx_attributes_mask: u64,
    pub stx_atime: StatxTimestamp,
    pub stx_btime: StatxTimestamp,
    pub stx_ctime: StatxTimestamp,
    pub stx_mtime: StatxTimestamp,
    pub stx_rdev_major: u32,
    pub stx_rdev_minor: u32,
    pub stx_dev_major: u32,
    pub stx_dev_minor: u32,
    pub stx_mnt_id: u64,
    pub stx_dio_mem_align: u32,
    pub stx_dio_offset_align: u32,
    pub stx_subvol: u64,
    pub stx_atomic_write_unit_min: u32,
    pub stx_atomic_write_unit_max: u32,
    pub stx_atomic_write_segments_max: u32,
    pub stx_dio_read_offset_align: u32,
    pub stx_atomic_write_unit_max_opt: u32,
    pad2: u32,
    pad3: [u64; 8],
}

const _: () = assert!(size_of::<Statx>() == 256, "struct statx is 256 bytes");

/// Everything the probe asks `statx` for.
pub(crate) const STATX_PROBE_MASK: u32 =
    libc::STATX_BASIC_STATS | libc::STATX_MNT_ID | libc::STATX_DIOALIGN | libc::STATX_WRITE_ATOMIC;

/// `statx(2)` on an open descriptor (`AT_EMPTY_PATH`). Mask bits the kernel
/// does not know are ignored; `stx_mask` reports what was filled.
pub(crate) fn statx(fd: RawFd, mask: u32) -> RawResult<Statx> {
    let mut sx = Statx::default();
    // SAFETY: statx(2) with AT_EMPTY_PATH and an empty path acts on `fd`; the
    // buffer is a 256-byte `struct statx` (asserted above) that the kernel
    // fills on success; the path string is NUL-terminated.
    let r = unsafe {
        libc::syscall(
            libc::SYS_statx,
            fd,
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            mask,
            &raw mut sx,
        )
    };
    if r != 0 {
        return Err(last_errno());
    }
    Ok(sx)
}

/// `FS_IOC_GETFLAGS` (ioctl_iflags(2)): the inode attribute flags, or
/// `ENOTTY` on filesystems without them.
pub(crate) fn inode_flags(fd: RawFd) -> RawResult<u64> {
    let mut flags: c_long = 0;
    // SAFETY: FS_IOC_GETFLAGS writes the flags (an `int`, within the `long`
    // the glibc ABI declares) through the pointer, which is valid for the call.
    let r = unsafe { libc::ioctl(fd, libc::FS_IOC_GETFLAGS, &raw mut flags) };
    check(r)?;
    Ok(u64::from(flags as u32))
}

/// `fsync(2)`: data, metadata and a device cache flush.
#[inline]
pub(crate) fn fsync(fd: RawFd) -> RawResult<()> {
    // SAFETY: fsync(2) takes only a descriptor.
    check(unsafe { libc::fsync(fd) })
}

/// `fdatasync(2)`: data, the metadata needed to read it, and a device flush.
#[inline]
pub(crate) fn fdatasync(fd: RawFd) -> RawResult<()> {
    // SAFETY: fdatasync(2) takes only a descriptor.
    check(unsafe { libc::fdatasync(fd) })
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

/// `fallocate(2)` with the given mode.
pub(crate) fn fallocate(
    fd: RawFd,
    mode: c_int,
    offset: libc::off_t,
    len: libc::off_t,
) -> RawResult<()> {
    // SAFETY: fallocate(2) takes a descriptor, a mode and two offsets; it
    // touches no user memory.
    check(unsafe { libc::fallocate(fd, mode, offset, len) })
}

/// `uname(2)` release string (for example `6.6.87.2-microsoft-standard-WSL2`).
pub(crate) fn kernel_release() -> Option<String> {
    let mut u = MaybeUninit::<libc::utsname>::zeroed();
    // SAFETY: uname(2) fills the structure with NUL-terminated strings on
    // success; assumed initialised only after a 0 return.
    let r = unsafe { libc::uname(u.as_mut_ptr()) };
    if r != 0 {
        return None;
    }
    // SAFETY: uname returned 0.
    let u = unsafe { u.assume_init() };
    let bytes: Vec<u8> = u
        .release
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8(bytes).ok()
}

// ----- locks ----------------------------------------------------------------

/// `fcntl(F_OFD_SETLK)` over the whole file: `F_WRLCK` takes, `F_UNLCK`
/// releases. Non-blocking: another holder gives `EAGAIN` (or `EACCES`).
/// `EINVAL` means the kernel predates OFD locks (3.15).
pub(crate) fn ofd_setlk(fd: RawFd, lock_type: c_int) -> RawResult<()> {
    // SAFETY: `struct flock` is plain data for which all-zero is a valid
    // value; the fields that matter are set below. (The field list differs
    // between architectures, so a literal would not be portable.)
    let mut fl: libc::flock = unsafe { core::mem::zeroed() };
    fl.l_type = lock_type as c_short;
    fl.l_whence = libc::SEEK_SET as c_short;
    fl.l_start = 0;
    fl.l_len = 0;
    fl.l_pid = 0;
    // SAFETY: fcntl(2) F_OFD_SETLK reads one `struct flock` through the
    // pointer, which is valid for the call; `l_pid` is 0 as the OFD contract
    // requires.
    check(unsafe { libc::fcntl(fd, libc::F_OFD_SETLK, &raw const fl) })
}

/// `flock(2)`.
pub(crate) fn flock(fd: RawFd, operation: c_int) -> RawResult<()> {
    // SAFETY: flock(2) takes a descriptor and an operation.
    check(unsafe { libc::flock(fd, operation) })
}

// ----- data -----------------------------------------------------------------

/// `pwritev2(2)` of one buffer at `offset` with the given `RWF_*` flags.
/// Returns the bytes written, which may be fewer than `len`.
#[inline]
pub(crate) fn pwritev2(
    fd: RawFd,
    ptr: *const u8,
    len: usize,
    offset: libc::off_t,
    flags: c_int,
) -> RawResult<usize> {
    let iov = libc::iovec {
        iov_base: ptr.cast_mut().cast::<c_void>(),
        iov_len: len,
    };
    // SAFETY: pwritev2(2): `iov` describes `len` readable bytes that the
    // caller's `IoBuf` owns for the whole call (platform contract 1); the
    // kernel reads, never writes, the buffer; one iovec is passed.
    let n = unsafe { libc::pwritev2(fd, &raw const iov, 1, offset, flags) };
    usize::try_from(n).map_err(|_| last_errno())
}

/// `preadv2(2)` of one buffer at `offset`. Returns the bytes read, which may
/// be fewer than `len`.
#[inline]
pub(crate) fn preadv2(
    fd: RawFd,
    ptr: *mut u8,
    len: usize,
    offset: libc::off_t,
    flags: c_int,
) -> RawResult<usize> {
    let iov = libc::iovec {
        iov_base: ptr.cast::<c_void>(),
        iov_len: len,
    };
    // SAFETY: preadv2(2): `iov` describes `len` writable bytes that the
    // caller's `IoBuf` owns exclusively for the whole call (platform contract
    // 1); one iovec is passed.
    let n = unsafe { libc::preadv2(fd, &raw const iov, 1, offset, flags) };
    usize::try_from(n).map_err(|_| last_errno())
}

// ----- FIEMAP ---------------------------------------------------------------

/// `FS_IOC_FIEMAP`: `_IOWR('f', 11, struct fiemap)`, a 32-byte header.
const FS_IOC_FIEMAP: libc::Ioctl = 0xC020_660Bu32 as libc::Ioctl;

/// Write back dirty pages before mapping (`FIEMAP_FLAG_SYNC`).
pub(crate) const FIEMAP_FLAG_SYNC: u32 = 0x1;
/// The last extent of the file.
pub(crate) const FIEMAP_EXTENT_LAST: u32 = 0x1;
/// Data location unknown.
pub(crate) const FIEMAP_EXTENT_UNKNOWN: u32 = 0x2;
/// Delayed allocation: not yet on media.
pub(crate) const FIEMAP_EXTENT_DELALLOC: u32 = 0x4;
/// Allocated but unwritten (reads as zero).
pub(crate) const FIEMAP_EXTENT_UNWRITTEN: u32 = 0x800;
/// Shared with another file (reflink).
pub(crate) const FIEMAP_EXTENT_SHARED: u32 = 0x2000;
/// Extents fetched per ioctl; the buffer is fixed so a range of any size is
/// walked without allocation.
pub(crate) const FIEMAP_BATCH: usize = 32;

/// `struct fiemap_extent` (56 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct FiemapExtent {
    pub fe_logical: u64,
    pub fe_physical: u64,
    pub fe_length: u64,
    reserved64: [u64; 2],
    pub fe_flags: u32,
    reserved: [u32; 3],
}

const ZERO_EXTENT: FiemapExtent = FiemapExtent {
    fe_logical: 0,
    fe_physical: 0,
    fe_length: 0,
    reserved64: [0; 2],
    fe_flags: 0,
    reserved: [0; 3],
};

/// `struct fiemap` followed by [`FIEMAP_BATCH`] extents.
#[repr(C)]
pub(crate) struct FiemapBuf {
    pub fm_start: u64,
    pub fm_length: u64,
    pub fm_flags: u32,
    pub fm_mapped_extents: u32,
    pub fm_extent_count: u32,
    reserved: u32,
    pub extents: [FiemapExtent; FIEMAP_BATCH],
}

const _: () = assert!(size_of::<FiemapExtent>() == 56, "fiemap_extent is 56 bytes");
const _: () = assert!(
    size_of::<FiemapBuf>() == 32 + 56 * FIEMAP_BATCH,
    "fiemap header is 32 bytes"
);

impl FiemapBuf {
    /// A request for the extents of `[start, start + len)`.
    pub(crate) fn new(start: u64, len: u64) -> Self {
        Self {
            fm_start: start,
            fm_length: len,
            fm_flags: FIEMAP_FLAG_SYNC,
            fm_mapped_extents: 0,
            fm_extent_count: FIEMAP_BATCH as u32,
            reserved: 0,
            extents: [ZERO_EXTENT; FIEMAP_BATCH],
        }
    }

    /// The extents the kernel filled (never more than the buffer holds,
    /// whatever `fm_mapped_extents` claims).
    pub(crate) fn mapped(&self) -> &[FiemapExtent] {
        let n = (self.fm_mapped_extents as usize).min(FIEMAP_BATCH);
        &self.extents[..n]
    }
}

/// `FS_IOC_FIEMAP` into a fixed buffer. `ENOTTY` or `EOPNOTSUPP` where the
/// filesystem has no extent map.
pub(crate) fn fiemap(fd: RawFd, map: &mut FiemapBuf) -> RawResult<()> {
    // SAFETY: FS_IOC_FIEMAP (Documentation/filesystems/fiemap.rst): the kernel
    // reads the header and writes at most `fm_extent_count` extents directly
    // after it; `FiemapBuf` is `repr(C)` with exactly FIEMAP_BATCH extents
    // following the 32-byte header and `fm_extent_count` never exceeds that.
    check(unsafe { libc::ioctl(fd, FS_IOC_FIEMAP, &raw mut *map) })
}

// ----- cachestat ------------------------------------------------------------

/// `struct cachestat_range` (cachestat(2)).
#[repr(C)]
struct CachestatRange {
    off: u64,
    len: u64,
}

/// `struct cachestat` (cachestat(2), kernel 6.5).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Cachestat {
    pub nr_cache: u64,
    pub nr_dirty: u64,
    pub nr_writeback: u64,
    pub nr_evicted: u64,
    pub nr_recently_evicted: u64,
}

/// `cachestat(2)` over `[off, off + len)`. `ENOSYS` on kernels before 6.5 and
/// on architectures where the syscall number is not known to this crate.
pub(crate) fn cachestat(fd: RawFd, off: u64, len: u64) -> RawResult<Cachestat> {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        /// The unified syscall table number of `cachestat`.
        const SYS_CACHESTAT: c_long = 451;
        let range = CachestatRange { off, len };
        let mut cs = Cachestat::default();
        // SAFETY: cachestat(2): the kernel reads the 16-byte range and writes
        // the 40-byte `struct cachestat`; both are valid for the call; flags
        // must be 0.
        let r = unsafe { libc::syscall(SYS_CACHESTAT, fd, &raw const range, &raw mut cs, 0u32) };
        if r != 0 {
            return Err(last_errno());
        }
        Ok(cs)
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _unused = (fd, off, len, CachestatRange { off: 0, len: 0 });
        Err(errno(libc::ENOSYS))
    }
}

// ----- NVMe admin passthrough -----------------------------------------------

/// `NVME_IOCTL_ADMIN_CMD`: `_IOWR('N', 0x41, struct nvme_admin_cmd)` (72 bytes).
const NVME_IOCTL_ADMIN_CMD: libc::Ioctl = 0xC048_4E41u32 as libc::Ioctl;

/// `struct nvme_admin_cmd` (`include/uapi/linux/nvme_ioctl.h`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NvmeAdminCmd {
    pub opcode: u8,
    pub flags: u8,
    pub rsvd1: u16,
    pub nsid: u32,
    pub cdw2: u32,
    pub cdw3: u32,
    pub metadata: u64,
    pub addr: u64,
    pub metadata_len: u32,
    pub data_len: u32,
    pub cdw10: u32,
    pub cdw11: u32,
    pub cdw12: u32,
    pub cdw13: u32,
    pub cdw14: u32,
    pub cdw15: u32,
    pub timeout_ms: u32,
    pub result: u32,
}

const _: () = assert!(
    size_of::<NvmeAdminCmd>() == 72,
    "nvme_admin_cmd is 72 bytes"
);

/// Issues one admin command whose data (if any) lands in `data`.
///
/// `EACCES` / `EPERM` when the command is not in the unprivileged allowlist
/// (Identify CNS 0/1/5/6/8 are; Get Log Page and Get Features are not). A
/// command the controller rejected (a positive NVMe status) is reported as
/// `EIO`.
pub(crate) fn nvme_admin(fd: RawFd, cmd: &mut NvmeAdminCmd, data: &mut [u8]) -> RawResult<()> {
    let len = u32::try_from(data.len()).map_err(|_| errno(libc::EINVAL))?;
    cmd.addr = if data.is_empty() {
        0
    } else {
        data.as_mut_ptr() as usize as u64
    };
    cmd.data_len = len;
    // SAFETY: NVME_IOCTL_ADMIN_CMD (drivers/nvme/host/ioctl.c): the kernel
    // reads the 72-byte command, transfers at most `data_len` bytes into the
    // user buffer at `addr`, which `data` keeps valid and exclusively borrowed
    // for the whole call, and writes `result` back into `cmd`.
    let r = unsafe { libc::ioctl(fd, NVME_IOCTL_ADMIN_CMD, &raw mut *cmd) };
    match r.cmp(&0) {
        core::cmp::Ordering::Less => Err(last_errno()),
        // A positive return is the NVMe status of a failed command.
        core::cmp::Ordering::Greater => Err(errno(libc::EIO)),
        core::cmp::Ordering::Equal => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fiemap_buf_clamps_mapped_count() {
        let mut b = FiemapBuf::new(0, 10);
        b.fm_mapped_extents = u32::MAX;
        assert_eq!(b.mapped().len(), FIEMAP_BATCH);
        b.fm_mapped_extents = 3;
        assert_eq!(b.mapped().len(), 3);
    }

    #[test]
    fn test_ioctl_numbers_match_the_uapi_encoding() {
        // _IOWR(type, nr, size) = (3 << 30) | (size << 16) | (type << 8) | nr
        let fiemap = (3u32 << 30) | (32 << 16) | (u32::from(b'f') << 8) | 11;
        assert_eq!(FS_IOC_FIEMAP, fiemap as libc::Ioctl);
        let nvme = (3u32 << 30) | (72 << 16) | (u32::from(b'N') << 8) | 0x41;
        assert_eq!(NVME_IOCTL_ADMIN_CMD, nvme as libc::Ioctl);
    }

    #[test]
    fn test_errno_wrappers_report_ebadf_on_a_closed_descriptor() {
        assert_eq!(fsync(-1).map_err(|e| e.code), Err(libc::EBADF));
        assert_eq!(fdatasync(-1).map_err(|e| e.code), Err(libc::EBADF));
        assert_eq!(fstat(-1).map(|_| ()).map_err(|e| e.code), Err(libc::EBADF));
        assert_eq!(
            fstatfs(-1).map(|_| ()).map_err(|e| e.code),
            Err(libc::EBADF)
        );
        assert_eq!(
            statx(-1, STATX_PROBE_MASK).map(|_| ()).map_err(|e| e.code),
            Err(libc::EBADF)
        );
        assert_eq!(fallocate(-1, 0, 0, 1).map_err(|e| e.code), Err(libc::EBADF));
        assert_eq!(
            pwritev2(-1, [0u8; 1].as_ptr(), 1, 0, 0).map_err(|e| e.code),
            Err(libc::EBADF)
        );
        assert_eq!(
            preadv2(-1, [0u8; 1].as_mut_ptr(), 1, 0, 0).map_err(|e| e.code),
            Err(libc::EBADF)
        );
        assert_eq!(
            inode_flags(-1).map(|_| ()).map_err(|e| e.code),
            Err(libc::EBADF)
        );
        let mut map = FiemapBuf::new(0, 1);
        assert_eq!(fiemap(-1, &mut map).map_err(|e| e.code), Err(libc::EBADF));
        assert_eq!(
            ofd_setlk(-1, libc::F_WRLCK).map_err(|e| e.code),
            Err(libc::EBADF)
        );
        assert_eq!(
            flock(-1, libc::LOCK_UN).map_err(|e| e.code),
            Err(libc::EBADF)
        );
        assert_eq!(
            dup_cloexec(-1).map(|_| ()).map_err(|e| e.code),
            Err(libc::EBADF)
        );
        assert_eq!(
            renameat(-1, c"a", c"b").map_err(|e| e.code),
            Err(libc::EBADF)
        );
        assert_eq!(unlinkat(-1, c"a").map_err(|e| e.code), Err(libc::EBADF));
        let mut cmd = NvmeAdminCmd::default();
        let mut data = [0u8; 16];
        assert_eq!(
            nvme_admin(-1, &mut cmd, &mut data).map_err(|e| e.code),
            Err(libc::EBADF)
        );
        let cs = cachestat(-1, 0, 1).map(|_| ()).map_err(|e| e.code);
        assert!(matches!(cs, Err(libc::EBADF | libc::ENOSYS)));
    }

    #[test]
    fn test_open_wrappers_report_enoent_and_mkdir_eexist() {
        let e = openat(libc::AT_FDCWD, c"/definitely/not/here", libc::O_RDONLY, 0)
            .map(|_| ())
            .map_err(|e| e.code);
        assert_eq!(e, Err(libc::ENOENT));
        assert_eq!(mkdir(c"/", 0o700).map_err(|e| e.code), Err(libc::EEXIST));
        if openat2_supported() {
            // RESOLVE_BENEATH refuses an absolute path with EXDEV.
            let e = openat2(libc::AT_FDCWD, c"/", libc::O_RDONLY, 0, RESOLVE_CONFINED)
                .map(|_| ())
                .map_err(|e| e.code);
            assert_eq!(e, Err(libc::EXDEV));
        }
    }

    #[test]
    fn test_kernel_release_parses() {
        let r = kernel_release().unwrap_or_default();
        assert!(r.starts_with(|c: char| c.is_ascii_digit()), "{r}");
    }
}
