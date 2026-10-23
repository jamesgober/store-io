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
//! - [`Submitter`]: many reads and writes in flight from one thread,
//!   without blocking; tickets from completions ([`completion`]).
//! - [`AppendRegion::scan`], [`PageRegion::scan`]: the raw ordered recovery
//!   read, which never stops at bad data.
//! - [`receipt`]: positions, write tickets and durability receipts, which
//!   only this crate can create.

#![deny(warnings)]
#![forbid(unsafe_code)]

mod batch;
pub mod completion;
mod create;
mod directory;
pub mod domain;
mod exec;
#[cfg(all(test, not(loom)))]
mod flush_slot_tests;
// The format-2 test helpers, shared with the in-crate tests, name the crate
// from outside.
#[cfg(all(test, not(loom)))]
extern crate self as store_io_engine;
mod flusher;
mod footprint;
mod force;
pub mod frontier;
mod gate;
mod io;
mod layout;
mod lifecycle;
mod lifecycle2;
mod mapped;
mod open;
mod open2;
mod optable;
mod provision;
mod provision2;
pub(crate) mod queue_core;
pub mod receipt;
mod region;
mod regionmap;
mod report;
mod scan;
mod settle;
mod slot_handle;
mod slots;
mod space;
mod stats;
mod store;
mod submitter;
mod sync;
mod v2;

pub use batch::{AppendBatch, PageBatch};
pub use completion::{SubmitterConfig, WriteMode};
pub use directory::Directory;
pub use domain::DomainStats;
pub use flusher::SubmitterWaker;
pub use receipt::{DurableReceipt, RegionPos, WriteTicket};
pub use region::{AppendRegion, PageRegion, Slot};
pub use scan::{ScanItem, ScanSummary};
pub use space::{Reservation, SpaceReport, TagSpace};
pub use stats::Outstanding;
pub use store::{
    CONTAINER, Healed, MetadataReport, Store, StoreOptions, StoreReport, Unavailable,
    UnavailableRegion,
};
pub use store_io_format::meta::RegionKind;
pub use submitter::{MAX_DEPTH, Submitter};
