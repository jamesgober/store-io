//! # store-io-buf
//!
//! Aligned I/O buffers for store-io.
//!
//! A [`BufPool`] maps one page-aligned arena at creation and carves it into
//! power-of-two size classes. [`BufPool::take`] pops a free slot from the
//! class's lock-free free list: no allocation, no lock, no system call, ever
//! blocking. An [`IoBuf`] owns its slot exclusively, moves into an I/O and
//! back out in the completion, and returns to its class when dropped.
//!
//! A pool configured as sensitive wipes every buffer on return ([`wipe`]),
//! and locks its arena in RAM and out of core dumps where the OS allows it.

#![deny(warnings)]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod arena;
mod pad;
pub mod pool;
mod queue;
mod sync;
mod wipe;

pub use pad::CachePadded;
pub use pool::{BufPool, ClassSpec, Exhausted, IoBuf, PoolConfig, PoolError};
pub use queue::IndexQueue;
pub use wipe::wipe;
