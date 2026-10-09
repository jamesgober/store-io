//! Page-aligned heap buffers for the raw unbuffered / `O_DIRECT` paths.
//!
//! store-io stages caller bytes into its own pooled aligned buffers; the raw
//! comparison paths need the same alignment to bypass the cache, so they use
//! this buffer and copy the payload into it inside the timed section (the
//! same copy store-io performs).

use std::alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error};
use std::ptr::NonNull;

/// Alignment of every buffer: the largest sector / page size the raw paths
/// meet on the measured devices.
pub const ALIGN: usize = 4096;

/// A zero-initialised, `ALIGN`-aligned byte buffer whose length is a
/// multiple of `ALIGN`.
pub struct AlignedBuf {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the buffer exclusively owns its allocation; sending it to another
// thread moves that ownership.
unsafe impl Send for AlignedBuf {}
// SAFETY: shared references only permit reads of initialised bytes.
unsafe impl Sync for AlignedBuf {}

impl AlignedBuf {
    /// Allocates `len` zeroed bytes. `len` is rounded up to `ALIGN`.
    #[must_use]
    pub fn zeroed(len: usize) -> Self {
        let len = len.max(1).div_ceil(ALIGN) * ALIGN;
        let Ok(layout) = Layout::from_size_align(len, ALIGN) else {
            panic!("invalid aligned-buffer layout for {len} bytes");
        };
        // SAFETY: `layout` has a non-zero size.
        let raw = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).unwrap_or_else(|| handle_alloc_error(layout));
        Self { ptr, len }
    }

    /// Length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// The bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` is valid for `len` initialised bytes for the life of
        // `self`, and no mutable borrow can coexist with `&self`.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// The bytes, mutably.
    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: `ptr` is valid for `len` initialised bytes and `&mut self`
        // guarantees exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: allocated in `zeroed` with exactly this size and alignment
        // (the layout was valid then, so it is valid now).
        unsafe {
            dealloc(
                self.ptr.as_ptr(),
                Layout::from_size_align_unchecked(self.len, ALIGN),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buffer_is_aligned_rounded_and_zeroed() {
        let mut b = AlignedBuf::zeroed(5000);
        assert_eq!(b.len(), 8192);
        assert_eq!(b.as_slice().as_ptr() as usize % ALIGN, 0);
        assert!(b.as_slice().iter().all(|&x| x == 0));
        b.as_mut_slice()[8191] = 7;
        assert_eq!(b.as_slice()[8191], 7);
    }
}
