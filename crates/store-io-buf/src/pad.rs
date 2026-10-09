//! Cache-line padding to keep hot atomics from sharing a line.

use core::ops::Deref;

/// Aligns and pads `T` to 128 bytes: two 64-byte lines, because adjacent-line
/// prefetchers on x86-64 and 128-byte lines on Apple silicon otherwise still
/// cause false sharing.
#[derive(Debug, Default)]
#[repr(align(128))]
pub struct CachePadded<T>(T);

impl<T> CachePadded<T> {
    /// Wraps a value.
    pub const fn new(value: T) -> Self {
        Self(value)
    }
}

impl<T> Deref for CachePadded<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}
