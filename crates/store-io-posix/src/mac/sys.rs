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
    sysctl_int(c"kern.hv_vmm_present").ok().map(|v| v != 0)
}

/// Whether the machine is Apple silicon, whatever this build's architecture
/// (an x86_64 build runs under Rosetta there): `hw.optional.arm64`. An
/// Intel Mac has no such key (`ENOENT`) or reports 0; any other failure to
/// read it counts as Apple silicon, whose rules are the stricter.
pub(crate) fn apple_silicon() -> bool {
    if cfg!(target_arch = "aarch64") {
        return true;
    }
    match sysctl_int(c"hw.optional.arm64") {
        Ok(v) => v != 0,
        Err(code) => code != libc::ENOENT,
    }
}

/// An integer `sysctlbyname(3)` value, or the `errno` of the failure.
fn sysctl_int(name: &CStr) -> Result<c_int, c_int> {
    let mut v: c_int = 0;
    let mut len = core::mem::size_of::<c_int>();
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
    if r == 0 {
        Ok(v)
    } else {
        Err(last_errno().code)
    }
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

/// `MNT_REMOVABLE` (`<sys/mount.h>`): the volume is on removable media.
const MNT_REMOVABLE: u32 = 0x0000_0200;

/// What `fstatfs` says of a file's volume (the device behind it:
/// [`device`]).
#[derive(Debug, Clone)]
pub(crate) struct Volume {
    /// The file system's name (`f_fstypename`: `apfs`, `hfs`, `smbfs`...).
    pub(crate) fs: String,
    /// `MNT_LOCAL`.
    pub(crate) local: bool,
    /// `MNT_REMOVABLE`.
    pub(crate) removable: bool,
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
        removable: sf.f_flags & MNT_REMOVABLE != 0,
        from: c_text(&sf.f_mntfromname),
    })
}

// ----- the device (DiskArbitration, IOKit) ------------------------------------

/// A Core Foundation object reference.
type CfRef = *const c_void;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: CfRef);
    fn CFGetTypeID(cf: CfRef) -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFBooleanGetValue(boolean: CfRef) -> u8;
    fn CFStringGetTypeID() -> usize;
    fn CFStringGetCString(string: CfRef, buf: *mut c_char, size: isize, encoding: u32) -> u8;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFDictionaryGetValue(dict: CfRef, key: CfRef) -> CfRef;
    fn CFStringCreateWithCString(allocator: CfRef, cstr: *const c_char, encoding: u32) -> CfRef;
}

#[link(name = "DiskArbitration", kind = "framework")]
#[allow(non_upper_case_globals)]
unsafe extern "C" {
    fn DASessionCreate(allocator: CfRef) -> CfRef;
    fn DADiskCreateFromBSDName(allocator: CfRef, session: CfRef, name: *const c_char) -> CfRef;
    fn DADiskCopyDescription(disk: CfRef) -> CfRef;
    static kDADiskDescriptionDeviceInternalKey: CfRef;
    static kDADiskDescriptionDeviceProtocolKey: CfRef;
    static kDADiskDescriptionDeviceModelKey: CfRef;
    static kDADiskDescriptionMediaRemovableKey: CfRef;
    static kDADiskDescriptionMediaEjectableKey: CfRef;
    static kDADiskDescriptionDevicePathKey: CfRef;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOBSDNameMatching(main_port: u32, options: u32, name: *const c_char) -> CfRef;
    fn IOServiceGetMatchingService(main_port: u32, matching: CfRef) -> u32;
    fn IORegistryEntryCreateCFProperty(
        entry: u32,
        key: CfRef,
        allocator: CfRef,
        options: u32,
    ) -> CfRef;
    fn IORegistryEntrySearchCFProperty(
        entry: u32,
        plane: *const c_char,
        key: CfRef,
        allocator: CfRef,
        options: u32,
    ) -> CfRef;
    fn IORegistryEntryGetParentIterator(entry: u32, plane: *const c_char, it: *mut u32) -> i32;
    fn IOIteratorNext(iterator: u32) -> u32;
    fn IOIteratorIsValid(iterator: u32) -> i32;
    fn IOObjectRelease(object: u32) -> i32;
}

/// `kCFStringEncodingUTF8`.
const CF_UTF8: u32 = 0x0800_0100;
/// `kIORegistryIterateRecursively | kIORegistryIterateParents`.
const IO_ITERATE_PARENTS: u32 = 0x1 | 0x2;
/// The service plane (`kIOServicePlane`).
const IO_SERVICE_PLANE: &CStr = c"IOService";
/// The most registry entries a walk to the root visits.
const IO_WALK_LIMIT: usize = 4096;

/// An owned Core Foundation object: a +1 reference from a Create or Copy
/// call, never null, released on drop.
struct Cf(CfRef);

impl Cf {
    /// Owns `r`; `None` (and nothing to release) for null. Never
    /// `then_some(Self(r))`: that builds, and so drops, a `Cf` of null too.
    fn new(r: CfRef) -> Option<Self> {
        if r.is_null() { None } else { Some(Self(r)) }
    }

    /// A CFString of `s` (UTF-8).
    fn string(s: &CStr) -> Option<Self> {
        // SAFETY: CFStringCreateWithCString(3): the default allocator, a
        // NUL-terminated string; it returns a +1 string or null.
        let r = unsafe { CFStringCreateWithCString(core::ptr::null(), s.as_ptr(), CF_UTF8) };
        Self::new(r)
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a non-null +1 reference this value owns (the
        // Create and Copy rule), released exactly once, here.
        unsafe { CFRelease(self.0) }
    }
}

/// An owned IOKit object (an entry or an iterator), never 0, released on
/// drop.
struct Io(u32);

impl Io {
    /// Owns `object`; `None` (and nothing to release) for 0.
    fn new(object: u32) -> Option<Self> {
        if object == 0 {
            None
        } else {
            Some(Self(object))
        }
    }
}

impl Drop for Io {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a non-zero object this value owns, released
        // exactly once, here.
        let _released = unsafe { IOObjectRelease(self.0) };
    }
}

/// What DiskArbitration and IOKit say of the device behind a disk: each
/// fact `None` where it cannot be read or holds another type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Device {
    /// `DADeviceInternal`: built into the machine (the device's physical
    /// interconnect location is internal).
    pub(crate) internal: Option<bool>,
    /// `DADeviceProtocol`: `Apple Fabric`, `PCI-Express`, `SATA`, `USB`,
    /// `Disk Image`, `Virtual Interface`...
    pub(crate) protocol: Option<String>,
    /// `DADeviceModel`.
    pub(crate) model: Option<String>,
    /// `DAMediaRemovable`.
    pub(crate) removable: Option<bool>,
    /// `DAMediaEjectable`.
    pub(crate) ejectable: Option<bool>,
    /// `DADevicePath`: the device's IOKit registry path, which names the
    /// controllers it hangs from (a Thunderbolt or USB bridge, sometimes).
    pub(crate) path: Option<String>,
    /// Whether the disk's media, or anything below it, hangs from a PCIe
    /// link tunnelled over Thunderbolt (`IOPCITunnelled` on it or an
    /// ancestor in the service plane).
    pub(crate) tunnelled: Option<bool>,
    /// Whether every registry entry from the disk's media down to the
    /// platform has one parent: no RAID set or container of several
    /// physical stores, whose description names only one of them.
    pub(crate) single: Option<bool>,
}

/// The device behind the BSD disk `name` (`disk3s5`; an APFS volume's
/// device facts are its physical store's), from its DiskArbitration
/// description and its IOKit media entry: `None` if there is no such disk
/// or no description. Needs no privilege and no run loop. Everything read
/// is untrusted: each value's type is checked before it is read.
pub(crate) fn device(name: &CStr) -> Option<Device> {
    // SAFETY: DASessionCreate(3) with the default allocator (null) returns
    // a +1 session or null.
    let session = unsafe { DASessionCreate(core::ptr::null()) };
    let session = Cf::new(session)?;
    // SAFETY: `name` is NUL-terminated and outlives the call, the session
    // is live; it returns a +1 disk or null.
    let disk = unsafe { DADiskCreateFromBSDName(core::ptr::null(), session.0, name.as_ptr()) };
    let disk = Cf::new(disk)?;
    // SAFETY: the disk is live; it returns a +1 dictionary or null.
    let desc = unsafe { DADiskCopyDescription(disk.0) };
    let desc = Cf::new(desc)?;
    // SAFETY: CFGetTypeID takes a live object, CFDictionaryGetTypeID
    // nothing.
    if unsafe { CFGetTypeID(desc.0) != CFDictionaryGetTypeID() } {
        return None;
    }
    // SAFETY: the keys are constant CFStrings DiskArbitration exports,
    // valid for the life of the process.
    let keys = unsafe {
        [
            kDADiskDescriptionDeviceInternalKey,
            kDADiskDescriptionDeviceProtocolKey,
            kDADiskDescriptionDeviceModelKey,
            kDADiskDescriptionMediaRemovableKey,
            kDADiskDescriptionMediaEjectableKey,
            kDADiskDescriptionDevicePathKey,
        ]
    };
    let below = media_below(name);
    Some(Device {
        internal: cf_bool(&desc, keys[0]),
        protocol: cf_string(&desc, keys[1]),
        model: cf_string(&desc, keys[2]),
        removable: cf_bool(&desc, keys[3]),
        ejectable: cf_bool(&desc, keys[4]),
        path: cf_string(&desc, keys[5]),
        tunnelled: below.map(|b| b.0),
        single: below.map(|b| b.1),
    })
}

/// From the BSD disk `name`'s IOKit media entry down to the platform:
/// whether anything there is tunnelled over Thunderbolt, and whether every
/// entry has one parent. `None` if the media is not found, IOKit refuses to
/// read it (its own `BSD Name` must read back), or the walk fails.
fn media_below(name: &CStr) -> Option<(bool, bool)> {
    // SAFETY: IOBSDNameMatching(3): the default main port (0), no options,
    // a NUL-terminated name; it returns a +1 matching dictionary or null.
    let matching = unsafe { IOBSDNameMatching(0, 0, name.as_ptr()) };
    if matching.is_null() {
        return None;
    }
    // SAFETY: IOServiceGetMatchingService(3) consumes the dictionary's one
    // reference whatever it returns: an entry the caller releases, or 0.
    let media = unsafe { IOServiceGetMatchingService(0, matching) };
    let media = Io::new(media)?;
    let bsd = Cf::string(c"BSD Name")?;
    // SAFETY: a live entry and a live CFString key; it returns a +1 value or
    // null (absent, or the read refused).
    let readable = unsafe { IORegistryEntryCreateCFProperty(media.0, bsd.0, core::ptr::null(), 0) };
    let readable = Cf::new(readable)?;
    if string_of(readable.0)?.as_bytes() != name.to_bytes() {
        return None;
    }
    let tunnel = Cf::string(c"IOPCITunnelled")?;
    // SAFETY: a live entry, a NUL-terminated plane name, a live CFString
    // key; it returns a +1 value or null (absent everywhere: IOPCIFamily
    // never sets the property false).
    let found = unsafe {
        IORegistryEntrySearchCFProperty(
            media.0,
            IO_SERVICE_PLANE.as_ptr(),
            tunnel.0,
            core::ptr::null(),
            IO_ITERATE_PARENTS,
        )
    };
    let tunnelled = match Cf::new(found) {
        None => false,
        Some(v) => bool_of(v.0)?,
    };
    Some((tunnelled, one_parent_each(&media)?))
}

/// Whether every entry from `entry` to the root of the service plane has at
/// most one parent (a RAID set or a container of several physical stores has
/// an entry with several: its members); `None` if a parent lookup fails or
/// the walk passes [`IO_WALK_LIMIT`] entries.
fn one_parent_each(entry: &Io) -> Option<bool> {
    let mut at = entry.0;
    // Holds the entry `at` names while its parents are read.
    let mut _held: Option<Io> = None;
    let plane = IO_SERVICE_PLANE.as_ptr();
    for _ in 0..IO_WALK_LIMIT {
        let mut it: u32 = 0;
        // SAFETY: a live entry, a NUL-terminated plane name, `it` writable;
        // on success an iterator the caller releases.
        let kr = unsafe { IORegistryEntryGetParentIterator(at, plane, &raw mut it) };
        let it = if kr == 0 { Io::new(it)? } else { return None };
        // SAFETY: a live iterator; it returns a +1 entry, or 0 at the end.
        let first = unsafe { IOIteratorNext(it.0) };
        let Some(parent) = Io::new(first) else {
            // The root, unless the registry changed under the walk.
            // SAFETY: a live iterator; the call only reads it.
            let valid = unsafe { IOIteratorIsValid(it.0) } != 0;
            return valid.then_some(true);
        };
        // SAFETY: as above.
        let second = unsafe { IOIteratorNext(it.0) };
        if Io::new(second).is_some() {
            return Some(false);
        }
        at = parent.0;
        _held = Some(parent);
    }
    None
}

/// The value of `key` in the dictionary `dict` (not retained: valid while
/// `dict` is), or `None`.
fn cf_get(dict: &Cf, key: CfRef) -> Option<CfRef> {
    if key.is_null() {
        return None;
    }
    // SAFETY: `dict` is a live CFDictionary (checked by the caller), `key` a
    // live CFString; the result follows the Get rule (not owned).
    let v = unsafe { CFDictionaryGetValue(dict.0, key) };
    (!v.is_null()).then_some(v)
}

/// The live object `v`'s value if it is a CFBoolean.
fn bool_of(v: CfRef) -> Option<bool> {
    // SAFETY: `v` is a live object; both calls only read it.
    let is_bool = unsafe { CFGetTypeID(v) == CFBooleanGetTypeID() };
    // SAFETY: as above, and `v` is a CFBoolean.
    is_bool.then(|| unsafe { CFBooleanGetValue(v) } != 0)
}

/// The value of `key` if it is a CFBoolean.
fn cf_bool(dict: &Cf, key: CfRef) -> Option<bool> {
    bool_of(cf_get(dict, key)?)
}

/// The value of `key` if it is a CFString that fits 1 KiB of UTF-8.
fn cf_string(dict: &Cf, key: CfRef) -> Option<String> {
    string_of(cf_get(dict, key)?)
}

/// The live object `v`'s value if it is a CFString that fits 1 KiB of
/// UTF-8.
fn string_of(v: CfRef) -> Option<String> {
    // SAFETY: `v` is a live object; the call only reads it.
    if unsafe { CFGetTypeID(v) != CFStringGetTypeID() } {
        return None;
    }
    let mut buf = [0 as c_char; 1024];
    // SAFETY: `v` is a CFString; `buf` is writable for its whole length,
    // which is passed, and the call NUL-terminates what it writes.
    let ok = unsafe { CFStringGetCString(v, buf.as_mut_ptr(), buf.len() as isize, CF_UTF8) };
    (ok != 0).then(|| c_text(&buf))
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
    use super::{Cf, Io, parse_release};

    /// Null and 0 own nothing: never released (a `CFRelease` of null traps).
    #[test]
    fn test_null_references_are_never_released() {
        assert!(Cf::new(core::ptr::null()).is_none());
        assert!(Io::new(0).is_none());
    }

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
