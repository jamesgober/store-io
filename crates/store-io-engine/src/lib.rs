//! # store-io-engine
//!
//! The engine of store-io, generic over any `store-io-platform` backend.
//!
//! - [`domain`]: flush domains and the join rule: barriers share device
//!   flushes with no timer, and a failed flush poisons the device forever.
//! - [`frontier`]: lock-free append frontiers: atomic space reservation,
//!   out-of-order completion, contiguous completed and durable prefixes.
//! - [`Store`]: one container: create, open, read-only open, provisioning,
//!   the device report, poisoning.
//! - [`AppendRegion`], [`PageRegion`], [`Slot`]: the data surfaces.
//! - [`receipt`]: positions, write tickets and durability receipts, which
//!   only this crate can create.

#![deny(warnings)]
#![forbid(unsafe_code)]

pub mod domain;
mod exec;
pub mod frontier;
mod layout;
pub mod receipt;
mod region;
mod slots;
mod store;
mod sync;

pub use receipt::{DurableReceipt, RegionPos, WriteTicket};
pub use region::{AppendRegion, PageRegion, Slot};
pub use store::{CONTAINER, Store, StoreOptions, StoreReport};
