//! Synchronisation primitives that loom can model (`--cfg loom`) or the real
//! ones.

#[cfg(loom)]
pub(crate) use loom::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[cfg(not(loom))]
pub(crate) use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
