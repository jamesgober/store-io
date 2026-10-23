//! Atomics that loom can model (`--cfg loom`) or the real ones.

#[cfg(loom)]
pub(crate) use loom::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

#[cfg(not(loom))]
pub(crate) use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

// The stand-in for a native wait (`wait`): under loom, and where the OS has
// no wait-on-address call store-io uses.
#[cfg(loom)]
pub(crate) use loom::sync::{Condvar, Mutex};
#[cfg(all(
    not(loom),
    not(any(target_os = "linux", target_os = "android", windows))
))]
pub(crate) use std::sync::{Condvar, Mutex};

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
