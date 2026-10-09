//! Thin wrappers over the Win32 and ntdll calls this backend uses.
//!
//! This is the only module with `unsafe`. Each wrapper does one call, turns
//! `FALSE` / `INVALID_HANDLE_VALUE` / a failing `NTSTATUS` into an
//! [`OsError`], and never allocates on the data path (`write_file`,
//! `read_file`, `get_queued`, `overlapped_result`, `flush_data`).

use core::ffi::c_void;
use core::ptr;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use store_io_core::error::{OsError, OsErrorSource};
use store_io_platform::RawResult;
use windows_sys::Wdk::Storage::FileSystem::NtFlushBuffersFileEx;
use windows_sys::Wdk::System::SystemServices::RtlGetVersion;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INVALID_PARAMETER, ERROR_IO_PENDING, FALSE, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE, TRUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, CreateFileW, DeleteFileW, FILE_ATTRIBUTE_TAG_INFO, FILE_INFO_BY_HANDLE_CLASS,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileAttributeTagInfo, FlushFileBuffers,
    GetDriveTypeW, GetFileInformationByHandleEx, GetFileSizeEx, GetVolumeInformationByHandleW,
    GetVolumeNameForVolumeMountPointW, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
    LockFileEx, OPEN_EXISTING, ReOpenFile, ReadFile, SetFileCompletionNotificationModes,
    SetFileInformationByHandle, UnlockFileEx, WriteFile,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, CreateIoCompletionPort, DeviceIoControl, GetOverlappedResult,
    GetQueuedCompletionStatusEx, IO_STATUS_BLOCK, OVERLAPPED, OVERLAPPED_ENTRY,
};
use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows_sys::Win32::System::SystemServices::FLUSH_FLAGS_FILE_DATA_SYNC_ONLY;
use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE};
use windows_sys::Win32::System::WindowsProgramming::{
    FILE_SKIP_COMPLETION_PORT_ON_SUCCESS, FILE_SKIP_SET_EVENT_ON_HANDLE,
};

/// Share mode used for every open: exclusivity is the ownership lock's job.
pub const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

/// Offset of the one-byte ownership-lock sentinel: 2^63 − 2, beyond any
/// possible file data, so the lock never blocks the backend's own I/O.
pub const LOCK_SENTINEL_OFFSET: u64 = (1u64 << 63) - 2;

/// An owned kernel handle, closed on drop.
#[derive(Debug)]
pub struct Handle(HANDLE);

// SAFETY: a Win32 handle is a process-wide token; the kernel serialises use
// from any thread, and the handle is closed exactly once by `Drop`.
unsafe impl Send for Handle {}
// SAFETY: as above; `&Handle` only lends the raw value to kernel calls.
unsafe impl Sync for Handle {}

impl Handle {
    /// The raw value, for kernel calls; valid while `self` lives.
    #[inline]
    #[must_use]
    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid handle owned by this value; it is
        // closed once (CloseHandle contract). A close failure cannot be
        // recovered from in Drop and the handle is dead either way.
        let _close_status = unsafe { CloseHandle(self.0) };
    }
}

/// The calling thread's last Win32 error.
#[must_use]
pub fn last_error() -> OsError {
    // SAFETY: GetLastError has no preconditions.
    let code = unsafe { GetLastError() };
    win32(code)
}

/// A Win32 error as an [`OsError`].
#[must_use]
pub fn win32(code: u32) -> OsError {
    OsError {
        code: code as i32,
        source: OsErrorSource::Win32,
    }
}

/// An NTSTATUS as an [`OsError`].
#[must_use]
pub fn nt(status: i32) -> OsError {
    OsError {
        code: status,
        source: OsErrorSource::NtStatus,
    }
}

/// NUL-terminated UTF-16 for a path.
#[must_use]
pub fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// NUL-terminated UTF-16 for a string.
#[must_use]
pub fn wide_str(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

/// `CreateFileW` with no security attributes and no template.
///
/// # Errors
///
/// The Win32 error.
pub fn create_file(
    path: &Path,
    access: u32,
    share: u32,
    disposition: u32,
    flags: u32,
) -> RawResult<Handle> {
    let w = wide(path);
    // SAFETY: `w` is a NUL-terminated UTF-16 string that outlives the call;
    // null security attributes and template are documented as allowed.
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            access,
            share,
            ptr::null(),
            disposition,
            flags,
            ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        Err(last_error())
    } else {
        Ok(Handle(h))
    }
}

/// `ReOpenFile`: a second handle to the same file object's file, with its
/// own flags, without re-resolving the path.
///
/// # Errors
///
/// The Win32 error.
pub fn reopen(h: &Handle, access: u32, share: u32, flags: u32) -> RawResult<Handle> {
    // SAFETY: `h` is a live handle; the flags exclude attributes as the
    // ReOpenFile contract requires (callers pass FILE_FLAG_* only).
    let n = unsafe { ReOpenFile(h.raw(), access, share, flags) };
    if n == INVALID_HANDLE_VALUE {
        Err(last_error())
    } else {
        Ok(Handle(n))
    }
}

/// `CreateDirectoryW`. `Ok(false)` when it already existed.
///
/// # Errors
///
/// The Win32 error other than "already exists".
pub fn create_directory(path: &Path) -> RawResult<bool> {
    let w = wide(path);
    // SAFETY: `w` is NUL-terminated and outlives the call; null security
    // attributes are allowed.
    if unsafe { CreateDirectoryW(w.as_ptr(), ptr::null()) } != FALSE {
        return Ok(true);
    }
    let e = last_error();
    if e.code == windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS as i32 {
        Ok(false)
    } else {
        Err(e)
    }
}

/// `DeleteFileW`.
///
/// # Errors
///
/// The Win32 error.
pub fn delete_file(path: &Path) -> RawResult<()> {
    let w = wide(path);
    // SAFETY: `w` is NUL-terminated and outlives the call.
    if unsafe { DeleteFileW(w.as_ptr()) } != FALSE {
        Ok(())
    } else {
        Err(last_error())
    }
}

/// File attributes and reparse tag of an open handle.
///
/// # Errors
///
/// The Win32 error.
pub fn attribute_tag(h: &Handle) -> RawResult<(u32, u32)> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO {
        FileAttributes: 0,
        ReparseTag: 0,
    };
    // SAFETY: `info` is a correctly sized, writable FILE_ATTRIBUTE_TAG_INFO
    // matching the FileAttributeTagInfo class; it holds only integers.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            h.raw(),
            FileAttributeTagInfo,
            ptr::addr_of_mut!(info).cast::<c_void>(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if ok != FALSE {
        Ok((info.FileAttributes, info.ReparseTag))
    } else {
        Err(last_error())
    }
}

/// `GetFileInformationByHandleEx` into a raw byte buffer; returns the buffer
/// so the caller parses it with bounds checks.
///
/// # Errors
///
/// The Win32 error.
pub fn file_info_bytes(
    h: &Handle,
    class: FILE_INFO_BY_HANDLE_CLASS,
    len: usize,
) -> RawResult<Vec<u8>> {
    let mut buf = vec![0u8; len];
    // SAFETY: `buf` is `len` writable bytes that outlive the call.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            h.raw(),
            class,
            buf.as_mut_ptr().cast::<c_void>(),
            len as u32,
        )
    };
    if ok != FALSE {
        Ok(buf)
    } else {
        Err(last_error())
    }
}

/// `SetFileInformationByHandle` with a raw, caller-built record.
///
/// # Errors
///
/// The Win32 error.
pub fn set_file_info(h: &Handle, class: FILE_INFO_BY_HANDLE_CLASS, record: &[u8]) -> RawResult<()> {
    // SAFETY: `record` is a readable buffer laid out by the caller for
    // `class`, alive for the call; the kernel only reads it.
    let ok = unsafe {
        SetFileInformationByHandle(
            h.raw(),
            class,
            record.as_ptr().cast::<c_void>(),
            record.len() as u32,
        )
    };
    if ok != FALSE {
        Ok(())
    } else {
        Err(last_error())
    }
}

/// `GetFileSizeEx`.
///
/// # Errors
///
/// The Win32 error.
pub fn file_size(h: &Handle) -> RawResult<u64> {
    let mut size: i64 = 0;
    // SAFETY: `size` is a writable i64 for the duration of the call.
    if unsafe { GetFileSizeEx(h.raw(), &mut size) } != FALSE {
        Ok(size.max(0) as u64)
    } else {
        Err(last_error())
    }
}

/// `FlushFileBuffers`: data, metadata and a device cache flush.
///
/// # Errors
///
/// The Win32 error.
pub fn flush_file_buffers(h: &Handle) -> RawResult<()> {
    // SAFETY: `h` is a live handle; the call is synchronous.
    if unsafe { FlushFileBuffers(h.raw()) } != FALSE {
        Ok(())
    } else {
        Err(last_error())
    }
}

/// `NtFlushBuffersFileEx(FLUSH_FLAGS_FILE_DATA_SYNC_ONLY)`: data plus the
/// metadata needed to read it back, and a device cache flush. NTFS only.
///
/// # Errors
///
/// The NTSTATUS.
pub fn flush_data_sync_only(h: &Handle) -> RawResult<()> {
    let mut iosb = IO_STATUS_BLOCK::default();
    // SAFETY: no parameters are passed (null, 0) as the ntifs contract allows;
    // `iosb` is writable for the call; the call is synchronous.
    let status = unsafe {
        NtFlushBuffersFileEx(
            h.raw(),
            FLUSH_FLAGS_FILE_DATA_SYNC_ONLY,
            ptr::null(),
            0,
            &mut iosb,
        )
    };
    if status >= 0 { Ok(()) } else { Err(nt(status)) }
}

/// Filesystem name and flags of the volume holding `h`.
///
/// # Errors
///
/// The Win32 error.
pub fn volume_info(h: &Handle) -> RawResult<(String, u32)> {
    let mut fs_name = [0u16; 64];
    let mut flags = 0u32;
    let mut serial = 0u32;
    let mut max_comp = 0u32;
    // SAFETY: every out-pointer is writable for the call and `fs_name` is
    // passed with its true length.
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            h.raw(),
            ptr::null_mut(),
            0,
            &mut serial,
            &mut max_comp,
            &mut flags,
            fs_name.as_mut_ptr(),
            fs_name.len() as u32,
        )
    };
    if ok == FALSE {
        return Err(last_error());
    }
    let end = fs_name
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(fs_name.len());
    Ok((String::from_utf16_lossy(&fs_name[..end]), flags))
}

/// The `\\?\Volume{GUID}\` name of the volume mounted at `path`'s mount
/// point, as UTF-16 with the trailing backslash removed, NUL-terminated.
///
/// # Errors
///
/// The Win32 error.
pub fn volume_device_name(mount_root: &[u16]) -> RawResult<Vec<u16>> {
    let mut name = [0u16; 64];
    // SAFETY: `mount_root` is NUL-terminated; `name` is writable with its
    // true length.
    let ok = unsafe {
        GetVolumeNameForVolumeMountPointW(mount_root.as_ptr(), name.as_mut_ptr(), name.len() as u32)
    };
    if ok == FALSE {
        return Err(last_error());
    }
    let mut end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    if end > 0 && name[end - 1] == u16::from(b'\\') {
        end -= 1;
    }
    let mut v = name[..end].to_vec();
    v.push(0);
    Ok(v)
}

/// The volume mount root (`C:\` or a mount-point folder) of `path`,
/// NUL-terminated UTF-16.
///
/// # Errors
///
/// The Win32 error.
pub fn volume_mount_root(path: &Path) -> RawResult<Vec<u16>> {
    let w = wide(path);
    let mut root = [0u16; 512];
    // SAFETY: `w` is NUL-terminated; `root` is writable with its true length.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW(
            w.as_ptr(),
            root.as_mut_ptr(),
            root.len() as u32,
        )
    };
    if ok == FALSE {
        return Err(last_error());
    }
    let end = root.iter().position(|&c| c == 0).unwrap_or(root.len());
    let mut v = root[..end].to_vec();
    v.push(0);
    Ok(v)
}

/// `GetDriveTypeW` of a NUL-terminated root.
#[must_use]
pub fn drive_type(root: &[u16]) -> u32 {
    // SAFETY: `root` is NUL-terminated and outlives the call.
    unsafe { GetDriveTypeW(root.as_ptr()) }
}

/// Opens a device or volume path (`\\.\PhysicalDriveN`, `\\?\Volume{..}`)
/// with the given access and full sharing.
///
/// # Errors
///
/// The Win32 error.
pub fn open_device(name: &[u16], access: u32) -> RawResult<Handle> {
    // SAFETY: `name` is NUL-terminated; null security attributes and
    // template are allowed.
    let h = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            SHARE_ALL,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        Err(last_error())
    } else {
        Ok(Handle(h))
    }
}

/// An auto-reset event for synchronous overlapped calls on overlapped handles.
fn event() -> RawResult<Handle> {
    // SAFETY: null attributes and name are allowed; the event is unnamed.
    let h = unsafe { CreateEventW(ptr::null(), FALSE, FALSE, ptr::null()) };
    if h.is_null() {
        Err(last_error())
    } else {
        Ok(Handle(h))
    }
}

/// Waits (untimed) for an overlapped call issued with `ov` on `h`.
fn wait_overlapped(h: &Handle, ov: &mut OVERLAPPED) -> RawResult<u32> {
    let mut n = 0u32;
    // SAFETY: `ov` is the structure the pending call was issued with and
    // stays alive and untouched until this returns; bWait = TRUE waits on
    // `ov.hEvent` with no timeout (GetOverlappedResult contract).
    if unsafe { GetOverlappedResult(h.raw(), ov, &mut n, TRUE) } != FALSE {
        Ok(n)
    } else {
        Err(last_error())
    }
}

/// `DeviceIoControl`, synchronous even on an overlapped handle (an event is
/// used, so concurrent overlapped operations on the same handle do not
/// confuse the wait). Returns the number of bytes written to `out`.
///
/// # Errors
///
/// The Win32 error.
pub fn ioctl(h: &Handle, code: u32, input: &[u8], out: &mut [u8]) -> RawResult<usize> {
    let ev = event()?;
    let mut ov = OVERLAPPED {
        hEvent: ev.raw(),
        ..OVERLAPPED::default()
    };
    let mut n = 0u32;
    // SAFETY: `input` is readable and `out` writable for the whole call,
    // including the untimed wait below; `ov` outlives the operation.
    let ok = unsafe {
        DeviceIoControl(
            h.raw(),
            code,
            input.as_ptr().cast::<c_void>(),
            input.len() as u32,
            out.as_mut_ptr().cast::<c_void>(),
            out.len() as u32,
            &mut n,
            &mut ov,
        )
    };
    if ok != FALSE {
        return Ok(n as usize);
    }
    let e = last_error();
    if e.code == ERROR_IO_PENDING as i32 {
        wait_overlapped(h, &mut ov).map(|n| n as usize)
    } else {
        Err(e)
    }
}

/// An `OVERLAPPED` addressing the lock sentinel byte.
fn sentinel_overlapped(ev: &Handle) -> OVERLAPPED {
    let mut ov = OVERLAPPED::default();
    ov.Anonymous.Anonymous.Offset = LOCK_SENTINEL_OFFSET as u32;
    ov.Anonymous.Anonymous.OffsetHigh = (LOCK_SENTINEL_OFFSET >> 32) as u32;
    ov.hEvent = ev.raw();
    ov
}

/// `LockFileEx(EXCLUSIVE | FAIL_IMMEDIATELY)` on the sentinel byte.
///
/// # Errors
///
/// The Win32 error (`ERROR_LOCK_VIOLATION` when another handle holds it).
pub fn lock_sentinel(h: &Handle) -> RawResult<()> {
    let ev = event()?;
    let mut ov = sentinel_overlapped(&ev);
    // SAFETY: `ov` carries the offset and a private event and outlives the
    // operation, including the untimed wait on a pending result.
    let ok = unsafe {
        LockFileEx(
            h.raw(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut ov,
        )
    };
    if ok != FALSE {
        return Ok(());
    }
    let e = last_error();
    if e.code == ERROR_IO_PENDING as i32 {
        wait_overlapped(h, &mut ov).map(|_| ())
    } else {
        Err(e)
    }
}

/// `UnlockFileEx` on the sentinel byte.
///
/// # Errors
///
/// The Win32 error.
pub fn unlock_sentinel(h: &Handle) -> RawResult<()> {
    let ev = event()?;
    let mut ov = sentinel_overlapped(&ev);
    // SAFETY: as in `lock_sentinel`.
    let ok = unsafe { UnlockFileEx(h.raw(), 0, 1, 0, &mut ov) };
    if ok != FALSE {
        return Ok(());
    }
    let e = last_error();
    if e.code == ERROR_IO_PENDING as i32 {
        wait_overlapped(h, &mut ov).map(|_| ())
    } else {
        Err(e)
    }
}

/// Creates an I/O completion port with no file associated.
///
/// # Errors
///
/// The Win32 error.
pub fn create_port() -> RawResult<Handle> {
    // SAFETY: INVALID_HANDLE_VALUE with a null existing port creates a new
    // port (CreateIoCompletionPort contract); one concurrent thread.
    let p = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, ptr::null_mut(), 0, 1) };
    if p.is_null() {
        Err(last_error())
    } else {
        Ok(Handle(p))
    }
}

/// Associates `file` with `port` under `key`, and sets
/// `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS | FILE_SKIP_SET_EVENT_ON_HANDLE` so
/// a synchronously completed operation queues no packet.
///
/// # Errors
///
/// The Win32 error.
pub fn associate(file: &Handle, port: &Handle, key: usize) -> RawResult<()> {
    // SAFETY: both handles are live; associating an overlapped file handle
    // with an existing port returns the port on success.
    let p = unsafe { CreateIoCompletionPort(file.raw(), port.raw(), key, 0) };
    if p.is_null() {
        return Err(last_error());
    }
    let modes = (FILE_SKIP_COMPLETION_PORT_ON_SUCCESS | FILE_SKIP_SET_EVENT_ON_HANDLE) as u8;
    // SAFETY: `file` is an overlapped handle (the only kind this crate
    // associates), which the notification-mode call requires.
    if unsafe { SetFileCompletionNotificationModes(file.raw(), modes) } != FALSE {
        Ok(())
    } else {
        Err(last_error())
    }
}

/// The completion record of one in-flight operation. It lives in a queue's
/// slot table at a stable address for as long as the kernel may write to it.
pub struct Overlapped(core::cell::Cell<OVERLAPPED>);

impl core::fmt::Debug for Overlapped {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The record is never read here: the kernel may be writing it while
        // the operation is in flight, and a read would race that write.
        f.debug_struct("Overlapped")
            .field("at", &self.as_ptr())
            .finish()
    }
}

// SAFETY: an OVERLAPPED is plain data (status, byte count, offset) plus an
// event handle this crate always leaves null; it has no thread affinity, and
// the owning queue is used by one thread at a time (`Cell` keeps it !Sync).
unsafe impl Send for Overlapped {}

impl Default for Overlapped {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlapped {
    /// A zeroed record.
    #[must_use]
    pub fn new() -> Self {
        Self(core::cell::Cell::new(OVERLAPPED::default()))
    }

    /// Resets the record for a new operation at `offset`.
    pub fn reset(&self, offset: u64) {
        let mut ov = OVERLAPPED::default();
        ov.Anonymous.Anonymous.Offset = offset as u32;
        ov.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
        self.0.set(ov);
    }

    /// The pointer handed to the kernel; stable while `self` is not moved.
    #[must_use]
    pub fn as_ptr(&self) -> *mut OVERLAPPED {
        self.0.as_ptr()
    }
}

/// A completion packet as reaped from the port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet {
    /// Address of the operation's [`Overlapped`] (0 for a posted packet
    /// with no record).
    pub overlapped: usize,
    /// The operation's final NTSTATUS.
    pub status: i32,
    /// Bytes transferred.
    pub bytes: u32,
}

/// A preallocated array of completion entries for `get_queued`.
pub struct Entries(Box<[OVERLAPPED_ENTRY]>);

impl core::fmt::Debug for Entries {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Entries")
            .field("capacity", &self.0.len())
            .finish()
    }
}

// SAFETY: the entries hold integers and a raw pointer that is only ever
// compared against slot addresses, never dereferenced; no thread affinity.
unsafe impl Send for Entries {}

impl Entries {
    /// Room for `n` packets (at least one).
    #[must_use]
    pub fn new(n: usize) -> Self {
        Self(vec![OVERLAPPED_ENTRY::default(); n.max(1)].into_boxed_slice())
    }

    /// The `i`-th packet of the last reap.
    #[must_use]
    pub fn get(&self, i: usize) -> Option<Packet> {
        self.0.get(i).map(|e| Packet {
            overlapped: e.lpOverlapped as usize,
            status: e.Internal as u32 as i32,
            bytes: e.dwNumberOfBytesTransferred,
        })
    }
}

/// Outcome of issuing an overlapped read or write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Issued {
    /// Completed synchronously with this many bytes; no packet will follow.
    Done(usize),
    /// In flight; a completion packet will follow.
    Pending,
    /// Failed synchronously; no packet will follow.
    Failed(OsError),
}

/// Resolves a synchronous return of `WriteFile` / `ReadFile`.
fn issued(h: &Handle, ok: i32, ov: *mut OVERLAPPED) -> Issued {
    if ok != FALSE {
        return match overlapped_result(h, ov) {
            Ok(n) => Issued::Done(n),
            Err(e) => Issued::Failed(e),
        };
    }
    let e = last_error();
    if e.code == ERROR_IO_PENDING as i32 {
        Issued::Pending
    } else {
        Issued::Failed(e)
    }
}

/// Issues an overlapped `WriteFile`.
///
/// # Safety
///
/// `buf` must stay valid and untouched, and `ov` must stay at the same
/// address, until the operation completes (`Done`, `Failed`, or a reaped
/// packet). `len` must not exceed the buffer.
pub unsafe fn write_file(h: &Handle, buf: *const u8, len: u32, ov: &Overlapped) -> Issued {
    // SAFETY: caller contract (buffer and record lifetimes); a null byte
    // count is required for overlapped calls.
    let ok = unsafe { WriteFile(h.raw(), buf, len, ptr::null_mut(), ov.as_ptr()) };
    issued(h, ok, ov.as_ptr())
}

/// Issues an overlapped `ReadFile`.
///
/// # Safety
///
/// As for [`write_file`], with `buf` writable.
pub unsafe fn read_file(h: &Handle, buf: *mut u8, len: u32, ov: &Overlapped) -> Issued {
    // SAFETY: caller contract; null byte count for overlapped calls.
    let ok = unsafe { ReadFile(h.raw(), buf, len, ptr::null_mut(), ov.as_ptr()) };
    issued(h, ok, ov.as_ptr())
}

/// The result of a completed overlapped operation (no wait): bytes or the
/// Win32 error mapped from its NTSTATUS.
///
/// # Errors
///
/// The operation's Win32 error.
pub fn completed_result(h: &Handle, ov: &Overlapped) -> RawResult<usize> {
    overlapped_result(h, ov.as_ptr())
}

fn overlapped_result(h: &Handle, ov: *mut OVERLAPPED) -> RawResult<usize> {
    let mut n = 0u32;
    // SAFETY: `ov` is the completed operation's record (the caller keeps it
    // alive); bWait = FALSE never blocks.
    if unsafe { GetOverlappedResult(h.raw(), ov, &mut n, FALSE) } != FALSE {
        Ok(n as usize)
    } else {
        Err(last_error())
    }
}

/// Reaps completion packets. `block` waits `INFINITE`; otherwise polls with
/// a zero timeout. Returns how many entries were filled (0 on a poll that
/// found nothing).
///
/// # Errors
///
/// The Win32 error if the wait itself failed.
pub fn get_queued(port: &Handle, entries: &mut Entries, block: bool) -> RawResult<usize> {
    let mut n = 0u32;
    let timeout = if block { INFINITE } else { 0 };
    // SAFETY: `entries.0` is writable with its true length for the call; the
    // wait is INFINITE or 0, never a finite tick-quantised timeout.
    let ok = unsafe {
        GetQueuedCompletionStatusEx(
            port.raw(),
            entries.0.as_mut_ptr(),
            entries.0.len() as u32,
            &mut n,
            timeout,
            FALSE,
        )
    };
    if ok != FALSE {
        return Ok(n as usize);
    }
    let e = last_error();
    if !block && e.code == windows_sys::Win32::Foundation::WAIT_TIMEOUT as i32 {
        Ok(0)
    } else {
        Err(e)
    }
}

/// Cancels every outstanding operation on `h` (completions still arrive).
pub fn cancel_io(h: &Handle) {
    // SAFETY: a null OVERLAPPED cancels all requests on the handle; a
    // failure (nothing to cancel) needs no handling: every packet is still
    // awaited by the caller.
    let _cancel_status = unsafe { CancelIoEx(h.raw(), ptr::null()) };
}

/// Kernel version `(major, minor, build)` from `RtlGetVersion`.
#[must_use]
pub fn kernel_version() -> Option<(u16, u16, u16)> {
    let mut v = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..OSVERSIONINFOW::default()
    };
    // SAFETY: `v` is a correctly sized, writable OSVERSIONINFOW.
    if unsafe { RtlGetVersion(&mut v) } < 0 {
        return None;
    }
    Some((
        v.dwMajorVersion.min(u16::MAX as u32) as u16,
        v.dwMinorVersion.min(u16::MAX as u32) as u16,
        v.dwBuildNumber.min(u16::MAX as u32) as u16,
    ))
}

/// Whether a Win32 error means "this information class or flag is not
/// supported here" (the only errors that justify a documented fallback).
#[must_use]
pub fn is_unsupported(e: OsError) -> bool {
    e.source == OsErrorSource::Win32
        && (e.code == ERROR_INVALID_PARAMETER as i32
            || e.code == windows_sys::Win32::Foundation::ERROR_NOT_SUPPORTED as i32
            || e.code == windows_sys::Win32::Foundation::ERROR_INVALID_FUNCTION as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lock_sentinel_offset_is_beyond_any_data_and_splits_correctly() {
        assert_eq!(LOCK_SENTINEL_OFFSET, 0x7FFF_FFFF_FFFF_FFFE);
        assert_eq!(LOCK_SENTINEL_OFFSET as u32, 0xFFFF_FFFE);
        assert_eq!((LOCK_SENTINEL_OFFSET >> 32) as u32, 0x7FFF_FFFF);
    }

    #[test]
    fn test_is_unsupported_matches_only_the_fallback_codes() {
        assert!(is_unsupported(win32(87)));
        assert!(is_unsupported(win32(50)));
        assert!(is_unsupported(win32(1)));
        assert!(!is_unsupported(win32(5)));
        assert!(!is_unsupported(nt(87)));
    }

    #[test]
    fn test_kernel_version_is_readable() {
        let v = kernel_version();
        assert!(
            matches!(v, Some((major, _, build)) if major >= 6 && build > 0),
            "{v:?}"
        );
    }
}
