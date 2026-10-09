//! # store-io-win
//!
//! The Windows backend behind the `store-io-platform` traits.
//!
//! - Data handles are opened `FILE_FLAG_NO_BUFFERING | FILE_FLAG_OVERLAPPED`
//!   with read, write and delete sharing; exclusivity comes from the
//!   ownership lock ([`WinLock`]), never from share modes.
//! - [`WinQueue`] submits overlapped `WriteFile` / `ReadFile` and reaps from
//!   its own I/O completion port with `GetQueuedCompletionStatusEx`, waiting
//!   `INFINITE` or polling with a zero timeout. No wait anywhere in this
//!   crate has a finite non-zero timeout.
//! - A data flush is `NtFlushBuffersFileEx(FLUSH_FLAGS_FILE_DATA_SYNC_ONLY)`
//!   on NTFS and `FlushFileBuffers` elsewhere; the choice is made once at
//!   open and recorded on the handle ([`WinFile::flush_mode`]).
//! - A `dsync` write performs the write and then that same data flush before
//!   it completes, so "durable at completion" never depends on
//!   `FILE_FLAG_WRITE_THROUGH`, whose FUA path is not certified on Windows.
//! - Rename replaces by handle with `FileRenameInfoEx(REPLACE_IF_EXISTS |
//!   POSIX_SEMANTICS)`; delete uses `FileDispositionInfoEx(DELETE |
//!   POSIX_SEMANTICS)`. Legacy fallbacks are recorded, never silent.
//! - [`Platform::probe`](store_io_platform::Platform::probe) fills
//!   `Evidence` from the volume, the file, the
//!   physical drive's storage properties and, on NVMe, the controller and
//!   namespace identify data, the write-cache feature and the health log.
//!   Device output is parsed as untrusted bytes with bounds checks.
//!
//! Every `unsafe` block cites the Win32 contract it relies on. All of them
//! live in the private `sys` module except the two calls in the queue that
//! hand a buffer and its completion record to `WriteFile` / `ReadFile`,
//! whose lifetime contract the queue's slot table upholds. On a non-Windows
//! target the crate compiles to an empty shell: none of the types below
//! exist there.

#![deny(warnings)]
// Off Windows the crate is an empty shell, so the links above have no target.
#![cfg_attr(not(windows), allow(rustdoc::broken_intra_doc_links))]

#[cfg(windows)]
mod alloc;
#[cfg(windows)]
mod bytes;
#[cfg(windows)]
mod dir;
#[cfg(windows)]
mod file;
#[cfg(windows)]
mod lock;
#[cfg(windows)]
mod platform;
#[cfg(windows)]
mod probe;
#[cfg(windows)]
mod queue;
#[cfg(windows)]
mod sys;

#[cfg(windows)]
pub use dir::WinDir;
#[cfg(windows)]
pub use file::{FlushMode, WinFile};
#[cfg(windows)]
pub use lock::WinLock;
#[cfg(windows)]
pub use platform::WinPlatform;
#[cfg(windows)]
pub use queue::WinQueue;
