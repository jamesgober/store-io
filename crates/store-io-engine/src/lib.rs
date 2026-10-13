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
//! - [`AppendBatch`], [`PageBatch`]: many writes, one barrier.
//! - [`AppendRegion::scan`], [`PageRegion::scan`]: the raw ordered recovery
//!   read, which never stops at bad data.
//! - [`receipt`]: positions, write tickets and durability receipts, which
//!   only this crate can create.

#![deny(warnings)]
#![forbid(unsafe_code)]

mod batch;
mod create;
mod directory;
pub mod domain;
mod exec;
pub mod frontier;
mod gate;
mod io;
mod layout;
mod lifecycle;
mod open;
mod provision;
pub mod receipt;
mod region;
mod scan;
mod slots;
mod space;
mod stats;
mod store;
mod sync;

pub use batch::{AppendBatch, PageBatch};
pub use directory::Directory;
pub use domain::DomainStats;
pub use receipt::{DurableReceipt, RegionPos, WriteTicket};
pub use region::{AppendRegion, PageRegion, Slot};
pub use scan::{ScanItem, ScanSummary};
pub use space::{Reservation, SpaceReport, TagSpace};
pub use stats::Outstanding;
pub use store::{CONTAINER, Store, StoreOptions, StoreReport, Unavailable, UnavailableRegion};
pub use store_io_format::meta::RegionKind;
