//! The raw comparison path: the platform primitives store-io's backends call,
//! with nothing in between.
//!
//! - Windows: `WriteFile` / `ReadFile` on a `FILE_FLAG_NO_BUFFERING` handle
//!   (synchronous handle: one call per I/O), and
//!   `NtFlushBuffersFileEx(FLUSH_FLAGS_FILE_DATA_SYNC_ONLY)` as the data
//!   flush (the call store-io-win makes on NTFS). [`OverlappedFile`] adds
//!   the overlapped form for queue-depth > 1 batches.
//! - Linux: `pwritev2(flags = 0)` / `preadv2` on an `O_DIRECT` descriptor and
//!   `fdatasync` as the data flush (the calls store-io-posix makes for a
//!   flush-required store).
//!
//! Every raw file is made "ready" before it is measured, exactly as store-io
//! makes a region ready: sized, zero-filled with unbuffered writes, and
//! flushed, so every measured write is an in-place overwrite of written
//! blocks (no allocation, no valid-data-length or size update).

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod win;

#[cfg(target_os = "linux")]
pub use linux::{RawFile, io_counters};
#[cfg(windows)]
pub use win::{OverlappedFile, RawFile, io_counters};

/// Human-readable description of the raw durable-write primitive.
#[cfg(windows)]
pub const PRIMITIVE: &str = "WriteFile on a FILE_FLAG_NO_BUFFERING handle + NtFlushBuffersFileEx(FLUSH_FLAGS_FILE_DATA_SYNC_ONLY)";
/// Human-readable description of the raw durable-write primitive.
#[cfg(target_os = "linux")]
pub const PRIMITIVE: &str = "pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync";

/// Human-readable description of the raw read primitive.
#[cfg(windows)]
pub const READ_PRIMITIVE: &str =
    "ReadFile on a FILE_FLAG_NO_BUFFERING handle (one call per request)";
/// Human-readable description of the raw read primitive.
#[cfg(target_os = "linux")]
pub const READ_PRIMITIVE: &str = "preadv2 on an O_DIRECT descriptor (one call per request)";

/// Where the per-process I/O counters come from.
#[cfg(windows)]
pub const COUNTER_SOURCE: &str = "GetProcessIoCounters (WriteOperationCount: NtWriteFile calls; OtherOperationCount includes flushes)";
/// Where the per-process I/O counters come from.
#[cfg(target_os = "linux")]
pub const COUNTER_SOURCE: &str =
    "/proc/self/io (syscw: write-family system calls; fdatasync is not counted)";

/// Per-process I/O counters at one instant.
#[derive(Debug, Clone, Copy, Default)]
pub struct IoCounters {
    /// Write calls issued by the process.
    pub write_calls: u64,
    /// Bytes passed to write calls.
    pub write_bytes: u64,
    /// Other I/O calls (Windows: includes flushes); `None` where the OS has
    /// no such counter.
    pub other_calls: Option<u64>,
}

impl IoCounters {
    /// `self - before`, saturating.
    #[must_use]
    pub fn since(&self, before: &IoCounters) -> IoCounters {
        IoCounters {
            write_calls: self.write_calls.saturating_sub(before.write_calls),
            write_bytes: self.write_bytes.saturating_sub(before.write_bytes),
            other_calls: match (self.other_calls, before.other_calls) {
                (Some(a), Some(b)) => Some(a.saturating_sub(b)),
                _ => None,
            },
        }
    }
}

/// Size of the zero-fill writes that make a raw file ready.
pub(crate) const FILL_CHUNK: usize = 1 << 20;
