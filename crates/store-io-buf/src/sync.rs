//! Atomics that loom can model (`--cfg loom`) or the real ones.

#[cfg(loom)]
pub(crate) use loom::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

#[cfg(not(loom))]
pub(crate) use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
