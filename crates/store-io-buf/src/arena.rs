//! Page-aligned memory for I/O buffers, mapped once and never reallocated.
//!
//! - **Linux and macOS**: an anonymous private `mmap`. On Linux the mapping is
//!   marked `MADV_DONTFORK`: if the process forks while a direct I/O is in
//!   flight, a shared copy-on-write page can receive the device's data in the
//!   wrong process (open(2), "O_DIRECT" notes).
//! - **Windows**: `VirtualAlloc(MEM_RESERVE | MEM_COMMIT)`.
//! - **Miri**: the global allocator with the same alignment, so the pool's
//!   pointer logic is checked under Miri.
//!
//! The arena hands out raw pointers into itself; the pool guarantees that each
//! slot is owned by at most one [`crate::IoBuf`] at a time.

use core::ptr::NonNull;

/// An owned, page-aligned, zero-initialised memory mapping.
pub struct Arena {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the arena is plain memory with no thread affinity. Access to its
// bytes is partitioned into slots by the pool, and each slot is reachable only
// through the single `IoBuf` that owns it.
unsafe impl Send for Arena {}
// SAFETY: as above; shared references to the arena only read its base pointer
// and length.
unsafe impl Sync for Arena {}

/// Protections applied to a sensitive arena.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Protection {
    /// Pages are locked in RAM (`mlock` / `VirtualLock`): never written to swap.
    pub locked: bool,
    /// Pages are excluded from kernel core dumps (`MADV_DONTDUMP`).
    pub excluded_from_dumps: bool,
}

/// Mapping the arena failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArenaError {
    /// Raw OS error code (errno or Win32), 0 if the size was invalid.
    pub code: i32,
}

impl Arena {
    /// Maps `len` bytes (rounded up to the page size by the OS), zero-filled
    /// and aligned to at least 4096 bytes.
    ///
    /// # Errors
    ///
    /// [`ArenaError`] if `len` is zero or the mapping fails.
    pub fn new(len: usize) -> Result<Self, ArenaError> {
        if len == 0 {
            return Err(ArenaError { code: 0 });
        }
        let ptr = sys::map(len)?;
        Ok(Self { ptr, len })
    }

    /// Base pointer of the mapping.
    #[inline]
    #[must_use]
    pub fn base(&self) -> NonNull<u8> {
        self.ptr
    }

    /// Length in bytes.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Locks the arena in RAM and excludes it from core dumps where the OS
    /// allows it (sensitive buffers: keys, plaintext). Best-effort: returns
    /// which protections were applied.
    #[must_use]
    pub fn protect_sensitive(&self) -> Protection {
        sys::protect(self.ptr, self.len)
    }

    /// Whether the arena is empty (never true for a constructed arena).
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` come from the matching `sys::map` call, the
        // arena owns the mapping, and every `IoBuf` holds an `Arc` to the pool
        // that owns this arena, so no slot is still in use.
        unsafe { sys::unmap(self.ptr, self.len) };
    }
}

#[cfg(miri)]
mod sys {
    use super::{ArenaError, NonNull};
    use std::alloc::{Layout, alloc_zeroed, dealloc};

    fn layout(len: usize) -> Result<Layout, ArenaError> {
        Layout::from_size_align(len, 4096).map_err(|_| ArenaError { code: 0 })
    }

    pub(super) fn map(len: usize) -> Result<NonNull<u8>, ArenaError> {
        let l = layout(len)?;
        // SAFETY: `l` has a non-zero size.
        NonNull::new(unsafe { alloc_zeroed(l) }).ok_or(ArenaError { code: 12 })
    }

    pub(super) fn protect(_ptr: NonNull<u8>, _len: usize) -> super::Protection {
        super::Protection::default()
    }

    /// # Safety
    ///
    /// `ptr` and `len` must come from `map`.
    pub(super) unsafe fn unmap(ptr: NonNull<u8>, len: usize) {
        if let Ok(l) = layout(len) {
            // SAFETY: allocated by `map` with the same layout (caller contract).
            unsafe { dealloc(ptr.as_ptr(), l) };
        }
    }
}

#[cfg(all(unix, not(miri)))]
mod sys {
    use super::{ArenaError, NonNull};

    pub(super) fn map(len: usize) -> Result<NonNull<u8>, ArenaError> {
        // SAFETY: a fresh anonymous private mapping; no existing memory is
        // touched. Arguments follow mmap(2).
        let p = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(ArenaError { code: errno() });
        }
        #[cfg(target_os = "linux")]
        {
            // SAFETY: `p`/`len` is the mapping just created. MADV_DONTFORK only
            // changes fork behaviour; failure is harmless and ignored by design
            // (it never affects this process's view of the memory).
            let _advice_status = unsafe { libc::madvise(p, len, libc::MADV_DONTFORK) };
        }
        NonNull::new(p.cast::<u8>()).ok_or(ArenaError { code: 0 })
    }

    pub(super) fn protect(ptr: NonNull<u8>, len: usize) -> super::Protection {
        // SAFETY: `ptr`/`len` is a live mapping owned by the arena; mlock and
        // madvise only change paging and dump behaviour.
        let locked = unsafe { libc::mlock(ptr.as_ptr().cast(), len) } == 0;
        #[cfg(target_os = "linux")]
        // SAFETY: as above.
        let excluded_from_dumps =
            unsafe { libc::madvise(ptr.as_ptr().cast(), len, libc::MADV_DONTDUMP) } == 0;
        #[cfg(not(target_os = "linux"))]
        let excluded_from_dumps = false;
        super::Protection {
            locked,
            excluded_from_dumps,
        }
    }

    /// # Safety
    ///
    /// `ptr` and `len` must come from `map`.
    pub(super) unsafe fn unmap(ptr: NonNull<u8>, len: usize) {
        // SAFETY: caller contract; the whole mapping is released once. A
        // failure leaks the mapping and cannot be reported from Drop.
        let _unmap_status = unsafe { libc::munmap(ptr.as_ptr().cast(), len) };
    }

    fn errno() -> i32 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }
}

#[cfg(all(windows, not(miri)))]
mod sys {
    use super::{ArenaError, NonNull};
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        VirtualLock,
    };

    pub(super) fn protect(ptr: NonNull<u8>, len: usize) -> super::Protection {
        // SAFETY: `ptr`/`len` is a live committed region owned by the arena;
        // VirtualLock only pins it in the working set. Windows has no
        // per-region dump exclusion outside WER; that gap is documented.
        let locked = unsafe { VirtualLock(ptr.as_ptr().cast(), len) } != 0;
        super::Protection {
            locked,
            excluded_from_dumps: false,
        }
    }

    pub(super) fn map(len: usize) -> Result<NonNull<u8>, ArenaError> {
        // SAFETY: reserves and commits a fresh region; no existing memory is
        // touched. Committed pages are zero-filled by the OS.
        let p = unsafe {
            VirtualAlloc(
                core::ptr::null(),
                len,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        };
        NonNull::new(p.cast::<u8>()).ok_or_else(|| ArenaError {
            code: std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
        })
    }

    /// # Safety
    ///
    /// `ptr` must come from `map`.
    pub(super) unsafe fn unmap(ptr: NonNull<u8>, _len: usize) {
        // SAFETY: caller contract; MEM_RELEASE with size 0 releases the whole
        // reservation made by `map`. A failure leaks the region and cannot be
        // reported from Drop.
        let _free_status = unsafe { VirtualFree(ptr.as_ptr().cast(), 0, MEM_RELEASE) };
    }
}
