//! # store-io-platform
//!
//! The boundary between store-io's engine and the operating system.
//!
//! Everything above this crate (error classification, barriers, formats,
//! provisioning, slots) is one shared code path. Below it, each platform
//! backend (Linux, Windows, macOS) and the deterministic simulator implement
//! two traits:
//!
//! - [`Platform`]: metadata operations (open, create, rename, directory
//!   flush, lock, allocate, full flush, probe), synchronous and off the hot
//!   path, plus a factory for per-thread [`Queue`]s.
//! - [`Queue`]: data operations, completion-shaped. [`Queue::submit`] takes
//!   an [`IoOp`] that owns its buffer; the buffer comes back in a
//!   [`Completion`]. A synchronous backend performs the system call inside
//!   `submit` and completes immediately (no thread hop); the simulator and the
//!   io_uring / IOCP backends complete later, possibly out of order.
//!
//! Backends return raw operating-system codes ([`OsError`]) and never
//! classify them; the engine decides what an error means (see
//! `store_io_core::errno`).
//!
//! ## Contracts every implementation keeps
//!
//! 1. An operation's buffer is not touched by the caller between `submit` and
//!    the completion that returns it, and is never freed by the backend: it is
//!    returned in the completion, or in [`Rejected`] if `submit` refuses it.
//! 2. A file handle passed in an operation stays open until that operation's
//!    completion has been reaped. The engine guarantees this by keeping the
//!    handle alive while operations are in flight; a backend may copy the raw
//!    descriptor.
//! 3. `submit` returning `Err` means the operation never reached the kernel or
//!    device (the media is untouched).
//! 4. A completion's `result` is the raw outcome: `Ok(n)` bytes transferred,
//!    which may be short, or `Err(code)`.
//! 5. No call ever waits with a timeout. [`Queue::wait`] blocks until the
//!    requested completions exist.

#![deny(warnings)]
#![forbid(unsafe_code)]

use core::fmt;
use std::path::Path;

pub use store_io_buf::IoBuf;
pub use store_io_core::error::OsError;
pub use store_io_core::evidence::Evidence;

/// Result carrying a raw operating-system error.
pub type RawResult<T> = Result<T, OsError>;

/// A single path component: 1 to 255 bytes, no separators, not `.` or `..`,
/// no NUL. Validated once at construction so backends never see a path that
/// could escape its directory.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct FileName(String);

/// The name is not a valid single path component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidFileName;

impl fmt::Display for InvalidFileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a single path component (1-255 bytes, no separators, not . or .., no NUL)")
    }
}

impl std::error::Error for InvalidFileName {}

impl FileName {
    /// Validates a file name.
    ///
    /// # Errors
    ///
    /// [`InvalidFileName`] for an empty, overlong, `.`/`..`, or separator- or
    /// NUL-containing name.
    ///
    /// # Examples
    ///
    /// ```
    /// use store_io_platform::FileName;
    /// assert!(FileName::new("store.sio").is_ok());
    /// assert!(FileName::new("../etc").is_err());
    /// assert!(FileName::new("a/b").is_err());
    /// ```
    pub fn new(name: &str) -> Result<Self, InvalidFileName> {
        let bad = name.is_empty()
            || name.len() > 255
            || name == "."
            || name == ".."
            || name
                .bytes()
                .any(|b| b == b'/' || b == b'\\' || b == 0 || b == b':');
        if bad {
            Err(InvalidFileName)
        } else {
            Ok(Self(name.to_owned()))
        }
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for FileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

/// How a file is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMode {
    /// Direct (unbuffered) reads and writes.
    ReadWrite,
    /// Direct reads only; the handle has no write rights.
    ReadOnly,
}

/// How space is released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseHow {
    /// Deallocate (punch a hole / mark sparse and zero): space returns to the
    /// filesystem; the range must be provisioned again before use.
    Deallocate,
    /// Trim in place: allocation kept, contents undefined.
    TrimInPlace,
}

/// Facts about a byte range of a file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RangeState {
    /// Some extent in the range is allocated but unwritten.
    pub unwritten: store_io_core::evidence::Tri,
    /// Some extent in the range is shared with another file (reflink).
    pub shared: store_io_core::evidence::Tri,
    /// Pages of the range resident in the OS page cache, if knowable.
    pub cached_pages: Option<u64>,
    /// Valid data length covers the range (Windows), if knowable.
    pub valid_data: store_io_core::evidence::Tri,
}

/// Per-queue configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueConfig {
    /// Maximum operations in flight.
    pub depth: u32,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self { depth: 64 }
    }
}

/// A data operation. Owns its buffer until it completes.
#[derive(Debug)]
pub enum IoOp<'a, F> {
    /// Write the buffer's bytes at `offset`. `dsync` asks for durability at
    /// completion (Linux `RWF_DSYNC`; FUA in the simulator).
    Write {
        /// Target file.
        file: &'a F,
        /// Byte offset.
        offset: u64,
        /// The data; its length is the transfer size.
        buf: IoBuf,
        /// Durable at completion.
        dsync: bool,
    },
    /// Read `buf.len()` bytes at `offset` into the buffer.
    Read {
        /// Source file.
        file: &'a F,
        /// Byte offset.
        offset: u64,
        /// Destination.
        buf: IoBuf,
    },
    /// Flush the device cache for this file's data (`fdatasync`,
    /// `NtFlushBuffersFileEx(DATA_SYNC_ONLY)`, `F_FULLFSYNC`).
    FlushData {
        /// File whose data (and the device cache) is flushed.
        file: &'a F,
    },
}

/// A finished operation.
#[derive(Debug)]
pub struct Completion {
    /// The tag passed to [`Queue::submit`].
    pub tag: u64,
    /// The operation's buffer, returned (`None` for flushes).
    pub buf: Option<IoBuf>,
    /// Bytes transferred (possibly short), or the raw error.
    pub result: RawResult<usize>,
}

/// An operation `submit` refused; it never reached the kernel or device.
#[derive(Debug)]
pub struct Rejected {
    /// The operation's buffer, returned to the caller.
    pub buf: Option<IoBuf>,
    /// The raw error, if the refusal came from the OS (a full queue has none).
    pub raw: Option<OsError>,
}

/// Completions collected by [`Queue::reap`] / [`Queue::wait`].
///
/// Allocated once with a fixed capacity; reaping never grows it, so the hot
/// path never allocates.
#[derive(Debug)]
pub struct CompletionBuf {
    items: Vec<Completion>,
    cap: usize,
}

impl CompletionBuf {
    /// Creates a buffer for up to `cap` completions.
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            items: Vec::with_capacity(cap),
            cap,
        }
    }

    /// Room left before the buffer is full.
    #[must_use]
    pub fn room(&self) -> usize {
        self.cap - self.items.len()
    }

    /// Adds a completion; returns it back if the buffer is full.
    pub fn push(&mut self, c: Completion) -> Result<(), Completion> {
        if self.items.len() == self.cap {
            return Err(c);
        }
        self.items.push(c);
        Ok(())
    }

    /// Removes and returns every collected completion, in arrival order.
    pub fn drain(&mut self) -> std::vec::Drain<'_, Completion> {
        self.items.drain(..)
    }

    /// Number of collected completions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether nothing is collected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// A per-thread submission queue. Not `Sync`: one thread submits and reaps.
pub trait Queue {
    /// The platform's file handle type.
    type File;

    /// Submits an operation tagged with a caller value.
    ///
    /// # Errors
    ///
    /// [`Rejected`] when the operation was not submitted (queue full or an OS
    /// refusal before any I/O); its buffer is returned.
    fn submit(&mut self, op: IoOp<'_, Self::File>, tag: u64) -> Result<(), Rejected>;

    /// Moves finished completions into `out` without blocking; returns how
    /// many were added.
    fn reap(&mut self, out: &mut CompletionBuf) -> usize;

    /// Blocks until at least `min` completions are in `out` (or `out` is
    /// full); returns how many were added. Never times out.
    ///
    /// # Errors
    ///
    /// The raw error if waiting itself failed (not an operation error).
    fn wait(&mut self, min: usize, out: &mut CompletionBuf) -> RawResult<usize>;

    /// Operations submitted and not yet reaped.
    fn in_flight(&self) -> usize;
}

/// A platform backend.
pub trait Platform: Send + Sync + 'static {
    /// Open file handle (closed on drop).
    type File: Send + Sync;
    /// Open directory handle.
    type Dir: Send + Sync;
    /// Held ownership lock (released on drop, after in-flight I/O on the
    /// locked file has completed).
    type Lock: Send + Sync;
    /// Per-thread queue.
    type Queue: Queue<File = Self::File>;

    /// Opens a directory, creating it (and syncing its parent) if `create`.
    ///
    /// # Errors
    ///
    /// The raw error.
    fn open_dir(&self, path: &Path, create: bool) -> RawResult<Self::Dir>;

    /// Creates a new file exclusively (fails if it exists), mode 0600.
    ///
    /// # Errors
    ///
    /// The raw error; the file does not exist afterwards unless creation
    /// itself succeeded.
    fn create_file(&self, dir: &Self::Dir, name: &FileName) -> RawResult<Self::File>;

    /// Opens an existing file for direct I/O.
    ///
    /// # Errors
    ///
    /// The raw error.
    fn open_file(&self, dir: &Self::Dir, name: &FileName, mode: FileMode) -> RawResult<Self::File>;

    /// Atomically replaces `to` with `from` in the same directory. The file
    /// handle is the open `from` (Windows renames by handle).
    ///
    /// # Errors
    ///
    /// The raw error. After a failure the rename may or may not have
    /// happened; the engine treats it as durability-unknown.
    fn rename_replace(
        &self,
        dir: &Self::Dir,
        from: &FileName,
        to: &FileName,
        file: &Self::File,
    ) -> RawResult<()>;

    /// Removes a file.
    ///
    /// # Errors
    ///
    /// The raw error.
    fn unlink(&self, dir: &Self::Dir, name: &FileName) -> RawResult<()>;

    /// Makes the directory's entries durable. An unsupported directory flush
    /// is an error, never success.
    ///
    /// # Errors
    ///
    /// The raw error.
    fn sync_dir(&self, dir: &Self::Dir) -> RawResult<()>;

    /// Takes the exclusive ownership lock on an open file.
    ///
    /// # Errors
    ///
    /// The raw error (another holder reports the platform's "would block").
    fn lock_exclusive(&self, file: &Self::File) -> RawResult<Self::Lock>;

    /// Sets the file's size to exactly `len` bytes, truncating or extending
    /// with zeros. The data transfers stay block-aligned; only the size
    /// moves. A metadata change: durable after [`Self::flush_all`].
    ///
    /// # Errors
    ///
    /// The raw error.
    fn set_len(&self, file: &Self::File, len: u64) -> RawResult<()>;

    /// Allocates real blocks up to `len` bytes, so the file size becomes
    /// `len`. A file already at least `len` long is left as it is: this never
    /// shrinks a file (that is [`Self::set_len`]).
    /// Never leaves unwritten extents' contents readable as other files' data.
    ///
    /// # Errors
    ///
    /// The raw error (out of space reports the platform's ENOSPC equivalent).
    fn allocate(&self, file: &Self::File, len: u64) -> RawResult<()>;

    /// Current file size.
    ///
    /// # Errors
    ///
    /// The raw error.
    fn size(&self, file: &Self::File) -> RawResult<u64>;

    /// Releases a range.
    ///
    /// # Errors
    ///
    /// The raw error.
    fn release_range(
        &self,
        file: &Self::File,
        offset: u64,
        len: u64,
        how: ReleaseHow,
    ) -> RawResult<()>;

    /// Reports extent and cache state for a range.
    ///
    /// # Errors
    ///
    /// The raw error.
    fn range_state(&self, file: &Self::File, offset: u64, len: u64) -> RawResult<RangeState>;

    /// Makes data and metadata durable (fsync / FlushFileBuffers / F_FULLFSYNC).
    ///
    /// # Errors
    ///
    /// The raw error.
    fn flush_all(&self, file: &Self::File) -> RawResult<()>;

    /// Gathers raw evidence about the file's filesystem, stack and device.
    ///
    /// # Errors
    ///
    /// The raw error if nothing could be learned at all; partial evidence is
    /// returned with `missing` set.
    fn probe(&self, dir: &Self::Dir, file: &Self::File) -> RawResult<Evidence>;

    /// Creates a queue for the calling thread.
    ///
    /// # Errors
    ///
    /// The raw error.
    fn queue(&self, cfg: QueueConfig) -> RawResult<Self::Queue>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_name_rejects_escapes_and_separators() {
        for bad in ["", ".", "..", "a/b", "a\\b", "c:x", "nul\0"] {
            assert!(FileName::new(bad).is_err(), "{bad:?}");
        }
        assert!(FileName::new(&"x".repeat(256)).is_err());
        assert_eq!(
            FileName::new("store.sio").map(|n| n.as_str().to_owned()),
            Ok("store.sio".to_owned())
        );
    }

    #[test]
    fn test_completion_buf_never_grows() {
        let mut b = CompletionBuf::with_capacity(1);
        assert!(
            b.push(Completion {
                tag: 1,
                buf: None,
                result: Ok(0)
            })
            .is_ok()
        );
        assert!(
            b.push(Completion {
                tag: 2,
                buf: None,
                result: Ok(0)
            })
            .is_err()
        );
        assert_eq!(b.room(), 0);
        assert_eq!(b.drain().map(|c| c.tag).collect::<Vec<_>>(), vec![1]);
        assert!(b.is_empty());
    }
}
