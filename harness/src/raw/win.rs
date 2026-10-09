//! Raw Windows primitives (see the parent module).

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr;
use std::time::{Duration, Instant};

use windows_sys::Wdk::Storage::FileSystem::NtFlushBuffersFileEx;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, FALSE, GetLastError, HANDLE, STATUS_PENDING, TRUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_NO_BUFFERING, FILE_FLAG_OVERLAPPED, FlushFileBuffers, WriteFile,
};
use windows_sys::Win32::System::IO::{GetOverlappedResult, IO_STATUS_BLOCK, OVERLAPPED};
use windows_sys::Win32::System::SystemServices::FLUSH_FLAGS_FILE_DATA_SYNC_ONLY;
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetProcessIoCounters, INFINITE, IO_COUNTERS,
    WaitForSingleObject,
};

use super::{FILL_CHUNK, IoCounters};
use crate::aligned::AlignedBuf;

fn handle(f: &File) -> HANDLE {
    f.as_raw_handle()
}

fn nt_error(status: i32) -> io::Error {
    io::Error::other(format!("NTSTATUS 0x{:08x}", status as u32))
}

/// `NtFlushBuffersFileEx(FLUSH_FLAGS_FILE_DATA_SYNC_ONLY)` on `h`.
/// Returns whether the call returned `STATUS_PENDING` (it is then waited
/// for, and its final status taken from the I/O status block).
fn flush_data_sync_only(h: HANDLE) -> io::Result<bool> {
    let mut iosb = IO_STATUS_BLOCK::default();
    // SAFETY: `h` is a live handle owned by the caller; no flush parameters
    // are passed (null, 0) as the ntifs contract allows; `iosb` outlives the
    // operation because a pending flush is waited for below before return.
    let status = unsafe {
        NtFlushBuffersFileEx(
            h,
            FLUSH_FLAGS_FILE_DATA_SYNC_ONLY,
            ptr::null(),
            0,
            &mut iosb,
        )
    };
    if status == STATUS_PENDING {
        // An asynchronous file object signals the file handle on completion.
        // SAFETY: `h` is a live waitable file handle.
        let _ = unsafe { WaitForSingleObject(h, INFINITE) };
        // SAFETY: the operation completed, so the kernel has written the
        // status union; reading the `Status` member is then defined.
        let fin = unsafe { iosb.Anonymous.Status };
        return if fin >= 0 {
            Ok(true)
        } else {
            Err(nt_error(fin))
        };
    }
    if status >= 0 {
        Ok(false)
    } else {
        Err(nt_error(status))
    }
}

/// A file opened `FILE_FLAG_NO_BUFFERING` on a synchronous handle.
pub struct RawFile {
    file: File,
}

impl RawFile {
    /// Creates (or truncates) `path`, sizes it to `len` (a multiple of
    /// 1 MiB), zero-fills it with unbuffered 1 MiB writes and flushes it
    /// (`FlushFileBuffers`). Returns the file and the time preparation took.
    ///
    /// # Errors
    ///
    /// Any I/O error.
    pub fn create_ready(path: &Path, len: u64) -> io::Result<(Self, Duration)> {
        let t = Instant::now();
        {
            let f = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(path)?;
            f.set_len(len)?;
        }
        let me = Self::open(path)?;
        let zero = AlignedBuf::zeroed(FILL_CHUNK);
        let mut at = 0u64;
        while at < len {
            let n = (len - at).min(FILL_CHUNK as u64) as usize;
            me.write_at(&zero.as_slice()[..n], at)?;
            at += n as u64;
        }
        me.flush_all()?;
        Ok((me, t.elapsed()))
    }

    /// Opens an existing file unbuffered for reading and writing.
    ///
    /// # Errors
    ///
    /// Any I/O error.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_NO_BUFFERING)
            .open(path)?;
        Ok(Self { file })
    }

    /// One positioned `WriteFile` of the whole of `buf` (sector aligned).
    ///
    /// # Errors
    ///
    /// The OS error, or `WriteZero` on a short write.
    pub fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        let n = self.file.seek_write(buf, offset)?;
        if n == buf.len() {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::WriteZero, "short write"))
        }
    }

    /// One positioned `ReadFile` into `buf` (sector aligned).
    ///
    /// # Errors
    ///
    /// The OS error.
    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.file.seek_read(buf, offset)
    }

    /// The data flush: `NtFlushBuffersFileEx(FLUSH_FLAGS_FILE_DATA_SYNC_ONLY)`.
    ///
    /// # Errors
    ///
    /// The failing NTSTATUS.
    pub fn flush_data(&self) -> io::Result<()> {
        flush_data_sync_only(handle(&self.file)).map(|_| ())
    }

    /// A full flush: `FlushFileBuffers`.
    ///
    /// # Errors
    ///
    /// The Win32 error.
    pub fn flush_all(&self) -> io::Result<()> {
        // SAFETY: the handle is live for the life of `self.file`.
        if unsafe { FlushFileBuffers(handle(&self.file)) } != FALSE {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// An owned event handle, closed on drop.
struct Event(HANDLE);

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateEventW and is closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// A `FILE_FLAG_NO_BUFFERING | FILE_FLAG_OVERLAPPED` handle that keeps a
/// whole batch of writes in flight (one `WriteFile` per write, each with its
/// own `OVERLAPPED` and event), then waits for all of them. This is the
/// queue-depth > 1 floor for page batches; store-io-win uses the same
/// overlapped calls through an I/O completion port.
pub struct OverlappedFile {
    file: File,
    events: Vec<Event>,
    ovs: Box<[OVERLAPPED]>,
    /// Data flushes that returned `STATUS_PENDING` on this asynchronous
    /// handle (each was then waited for).
    pub flush_pending: u64,
}

// SAFETY: the OVERLAPPED records and event handles are only touched through
// `&mut self`; handles are process-wide tokens usable from any thread.
unsafe impl Send for OverlappedFile {}

impl OverlappedFile {
    /// Opens `path` for overlapped unbuffered I/O with room for `depth`
    /// writes in flight.
    ///
    /// # Errors
    ///
    /// Any I/O error.
    pub fn open(path: &Path, depth: usize) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_NO_BUFFERING | FILE_FLAG_OVERLAPPED)
            .open(path)?;
        let mut events = Vec::with_capacity(depth);
        for _ in 0..depth {
            // SAFETY: null security attributes and name are allowed; manual
            // reset, initially non-signalled.
            let h = unsafe { CreateEventW(ptr::null(), TRUE, FALSE, ptr::null()) };
            if h.is_null() {
                return Err(io::Error::last_os_error());
            }
            events.push(Event(h));
        }
        // SAFETY: OVERLAPPED is plain old data; all-zero is its documented
        // initial state.
        let ovs = (0..depth)
            .map(|_| unsafe { std::mem::zeroed::<OVERLAPPED>() })
            .collect();
        Ok(Self {
            file,
            events,
            ovs,
            flush_pending: 0,
        })
    }

    /// Issues every write, then waits for all of them.
    ///
    /// # Errors
    ///
    /// The first failure (after every issued write has completed), or
    /// `InvalidInput` for more writes than the depth.
    pub fn write_all_at(&mut self, writes: &[(u64, &[u8])]) -> io::Result<()> {
        if writes.len() > self.ovs.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "batch deeper than the handle",
            ));
        }
        let h = handle(&self.file);
        let mut issued = 0usize;
        let mut first_err: Option<io::Error> = None;
        for (i, (offset, buf)) in writes.iter().enumerate() {
            let ov = &mut self.ovs[i];
            // SAFETY: zeroing a plain-old-data record no I/O is using.
            *ov = unsafe { std::mem::zeroed() };
            ov.Anonymous.Anonymous.Offset = *offset as u32;
            ov.Anonymous.Anonymous.OffsetHigh = (*offset >> 32) as u32;
            ov.hEvent = self.events[i].0;
            // SAFETY: `buf` and `ov` stay valid and unmoved until the
            // completion is collected below (both outlive this call: the
            // borrow of `writes` and the boxed slice of records); the byte
            // count pointer is null as overlapped calls require.
            let ok = unsafe { WriteFile(h, buf.as_ptr(), buf.len() as u32, ptr::null_mut(), ov) };
            // SAFETY: GetLastError has no preconditions.
            if ok == FALSE && unsafe { GetLastError() } != ERROR_IO_PENDING {
                first_err = Some(io::Error::last_os_error());
                break;
            }
            issued += 1;
        }
        for (i, (_, buf)) in writes.iter().enumerate().take(issued) {
            let mut n = 0u32;
            // SAFETY: `ovs[i]` is the record of an issued operation on `h`;
            // bWait = TRUE blocks until it completes.
            let ok = unsafe { GetOverlappedResult(h, &self.ovs[i], &mut n, TRUE) };
            if ok == FALSE {
                first_err.get_or_insert_with(io::Error::last_os_error);
            } else if n as usize != buf.len() {
                first_err.get_or_insert_with(|| {
                    io::Error::new(io::ErrorKind::WriteZero, "short overlapped write")
                });
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    /// The data flush on this asynchronous handle, waiting for it if it
    /// returns `STATUS_PENDING` (counted in [`Self::flush_pending`]).
    ///
    /// # Errors
    ///
    /// The failing NTSTATUS.
    pub fn flush_data(&mut self) -> io::Result<()> {
        if flush_data_sync_only(handle(&self.file))? {
            self.flush_pending += 1;
        }
        Ok(())
    }
}

/// The process's I/O counters.
#[must_use]
pub fn io_counters() -> Option<IoCounters> {
    let mut c = IO_COUNTERS::default();
    // SAFETY: the pseudo-handle of the current process is always valid and
    // `c` is writable for the call.
    let ok = unsafe { GetProcessIoCounters(GetCurrentProcess(), &mut c) };
    (ok != FALSE).then_some(IoCounters {
        write_calls: c.WriteOperationCount,
        write_bytes: c.WriteTransferCount,
        other_calls: Some(c.OtherOperationCount),
    })
}
