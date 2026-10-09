//! Secure wipe: zeroing that the compiler is not allowed to remove.
//!
//! An ordinary `fill(0)` on memory that is about to be freed or reused is a
//! dead store an optimiser may delete. Volatile writes are never removed, and
//! the compiler fence keeps later reuse of the memory from being reordered
//! before the wipe.

use core::sync::atomic::{Ordering, compiler_fence};

/// Overwrites `bytes` with zeros in a way the compiler cannot elide.
///
/// # Examples
///
/// ```
/// let mut secret = *b"hunter2";
/// store_io_buf::wipe(&mut secret);
/// assert_eq!(secret, [0; 7]);
/// ```
pub fn wipe(bytes: &mut [u8]) {
    for b in bytes.iter_mut() {
        // SAFETY: `b` is a valid, aligned, exclusive reference to a `u8`.
        unsafe { core::ptr::write_volatile(b, 0) };
    }
    compiler_fence(Ordering::SeqCst);
}
