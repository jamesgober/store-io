//! The ownership lock: `LockFileEx(EXCLUSIVE | FAIL_IMMEDIATELY)` on one byte
//! at offset 2^63 − 2.
//!
//! `LockFileEx` is mandatory: locking data bytes would block the backend's
//! own second handle (the queue's) and any reader. The sentinel lies beyond
//! every possible data byte, so it conflicts with nothing but another owner.

use std::sync::Arc;

use store_io_platform::RawResult;

use crate::file::{FileInner, WinFile};
use crate::sys;

/// A held ownership lock, released by `UnlockFileEx` on drop. It keeps the
/// file handle alive, so the lock is always released before the handle
/// closes.
#[derive(Debug)]
pub struct WinLock {
    file: Arc<FileInner>,
}

impl WinLock {
    pub(crate) fn take(file: &WinFile) -> RawResult<Self> {
        sys::lock_sentinel(file.handle())?;
        Ok(Self {
            file: Arc::clone(&file.inner),
        })
    }
}

impl Drop for WinLock {
    fn drop(&mut self) {
        // Closing the handle would release the lock as well; an unlock
        // failure here leaves nothing to recover and nothing to report to.
        let _unlock_status = sys::unlock_sentinel(&self.file.handle);
    }
}
