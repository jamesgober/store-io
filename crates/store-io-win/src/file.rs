//! Open data files: `FILE_FLAG_NO_BUFFERING | FILE_FLAG_OVERLAPPED` handles
//! for I/O, plus one synchronous handle per writable file for flushes.
//!
//! Flushes never go through an overlapped handle. On one, the flush may
//! return `STATUS_PENDING` and finish later: the call would have to be
//! waited for (impossible on the queues' handles, which never signal), and
//! the kernel would write the status block after the caller returned. On a
//! synchronous handle the I/O manager waits inside the call, so a flush that
//! returns has finished.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use store_io_platform::{FileMode, RawResult};
use windows_sys::Win32::Foundation::{ERROR_CANT_ACCESS_FILE, GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_NO_BUFFERING, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
};

use crate::sys::{self, Handle};

/// Which primitive a data flush uses on this file, chosen once at open from
/// the filesystem and never weakened afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushMode {
    /// `NtFlushBuffersFileEx(FLUSH_FLAGS_FILE_DATA_SYNC_ONLY)`: NTFS.
    DataSyncOnly,
    /// `FlushFileBuffers`: every other filesystem (ReFS, FAT, exFAT).
    Full,
}

/// Process-unique ids for open files, so a queue's per-file handle cache
/// never confuses a closed file with a new one that reuses its handle value.
/// This is the crate's only global: an id source, never behaviour.
static NEXT_FILE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub(crate) struct FileInner {
    pub(crate) handle: Handle,
    /// Synchronous (non-overlapped) handle for flushes; writable files only.
    pub(crate) flush: Option<Arc<Handle>>,
    pub(crate) id: u64,
    pub(crate) writable: bool,
    pub(crate) flush_mode: FlushMode,
    pub(crate) path: PathBuf,
}

/// An open data file (closed when the last handle to it, including a held
/// [`crate::WinLock`], is dropped).
#[derive(Debug, Clone)]
pub struct WinFile {
    pub(crate) inner: Arc<FileInner>,
}

impl WinFile {
    /// Opens or creates `path` for unbuffered overlapped I/O.
    pub(crate) fn open(path: &Path, mode: FileMode, create: bool) -> RawResult<Self> {
        let access = match mode {
            FileMode::ReadWrite => GENERIC_READ | GENERIC_WRITE | DELETE,
            FileMode::ReadOnly => GENERIC_READ,
        };
        let disposition = if create { CREATE_NEW } else { OPEN_EXISTING };
        let flags = FILE_ATTRIBUTE_NORMAL
            | FILE_FLAG_NO_BUFFERING
            | FILE_FLAG_OVERLAPPED
            | FILE_FLAG_OPEN_REPARSE_POINT;
        let handle = sys::create_file(path, access, sys::SHARE_ALL, disposition, flags)?;
        let (attrs, _tag) = sys::attribute_tag(&handle)?;
        if attrs & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY) != 0 {
            // A symlink, junction or directory where a plain file was named.
            return Err(sys::win32(ERROR_CANT_ACCESS_FILE));
        }
        let (fs, _flags) = sys::volume_info(&handle)?;
        let flush_mode = if fs.eq_ignore_ascii_case("NTFS") {
            FlushMode::DataSyncOnly
        } else {
            FlushMode::Full
        };
        let flush = if mode == FileMode::ReadWrite {
            Some(Arc::new(sys::reopen(
                &handle,
                GENERIC_READ | GENERIC_WRITE,
                sys::SHARE_ALL,
                FILE_FLAG_NO_BUFFERING,
            )?))
        } else {
            None
        };
        Ok(Self {
            inner: Arc::new(FileInner {
                handle,
                flush,
                id: NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed),
                writable: mode == FileMode::ReadWrite,
                flush_mode,
                path: path.to_path_buf(),
            }),
        })
    }

    /// The data-flush primitive this file uses.
    #[must_use]
    pub fn flush_mode(&self) -> FlushMode {
        self.inner.flush_mode
    }

    /// Whether the handle has write rights.
    #[must_use]
    pub fn is_writable(&self) -> bool {
        self.inner.writable
    }

    /// The path the file was opened by.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    pub(crate) fn handle(&self) -> &Handle {
        &self.inner.handle
    }

    pub(crate) fn id(&self) -> u64 {
        self.inner.id
    }

    /// The data flush, synchronously on the calling thread: `DataSyncOnly`
    /// on NTFS, `FlushFileBuffers` elsewhere. The queue's `FlushData`
    /// operation performs exactly this.
    ///
    /// # Errors
    ///
    /// The raw error; a failed flush is never retried by this crate.
    pub fn flush_data(&self) -> RawResult<()> {
        flush_data(self.inner.flush.as_deref(), self.inner.flush_mode)
    }

    /// The synchronous flush handle (`None` for read-only files).
    pub(crate) fn flush_handle(&self) -> Option<&Arc<Handle>> {
        self.inner.flush.as_ref()
    }
}

/// Performs a data flush with the given primitive on a synchronous handle;
/// `None` (a read-only file) is refused.
pub(crate) fn flush_data(h: Option<&Handle>, mode: FlushMode) -> RawResult<()> {
    let Some(h) = h else {
        return Err(sys::win32(
            windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED,
        ));
    };
    match mode {
        FlushMode::DataSyncOnly => sys::flush_data_sync_only(h),
        FlushMode::Full => sys::flush_file_buffers(h),
    }
}
