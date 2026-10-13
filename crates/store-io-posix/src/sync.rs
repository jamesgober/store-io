//! Locks that loom can model (`--cfg loom`) or the real ones.

#[cfg(loom)]
pub(crate) use loom::sync::{Condvar, Mutex, MutexGuard};

#[cfg(not(loom))]
pub(crate) use std::sync::{Condvar, Mutex, MutexGuard};

/// Locks `m`. A panic while it was held cannot leave the protected state
/// half-written (every critical section is a push, a pop or a flag), so a
/// poisoned lock is used as it is.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Waits on `cv`, releasing and retaking the guard's lock. Never times out.
pub(crate) fn wait<'a, T>(cv: &Condvar, g: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    cv.wait(g)
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
