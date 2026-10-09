//! Atomics that loom can model (`--cfg loom`) or the real ones.

#[cfg(loom)]
pub(crate) use loom::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

#[cfg(not(loom))]
pub(crate) use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// Hint inside a retry loop. Under loom it yields so the model checker can
/// schedule the thread the loop is waiting for.
#[inline(always)]
pub(crate) fn spin() {
    #[cfg(loom)]
    loom::thread::yield_now();
    #[cfg(not(loom))]
    core::hint::spin_loop();
}
