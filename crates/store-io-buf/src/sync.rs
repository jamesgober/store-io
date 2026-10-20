//! Atomics that loom can model (`--cfg loom`) or the real ones.

#[cfg(loom)]
pub(crate) use loom::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

#[cfg(not(loom))]
pub(crate) use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Hint inside a retry loop. Every retry in this crate follows another
/// thread's progress (a lost compare-and-swap, or a slot another thread has
/// already moved past), never a wait for one, so under loom it does nothing:
/// a yield there would hide bugs, because loom never again offers a thread a
/// value it read before yielding, so a stale read on the retry is never tried.
#[inline(always)]
pub(crate) fn spin() {
    #[cfg(not(loom))]
    core::hint::spin_loop();
}
