//! # store-io
//!
//! Durable storage I/O for Rust databases.
//!
//! store-io is the layer between a storage engine and the files and devices it
//! writes to. It moves caller bytes to media and proves when they are durable.
//! It does not frame, parse or validate the bytes it moves, and it never decides
//! what a caller keeps after a crash.
//!
//! The design rests on four rules:
//!
//! - **Durability is decided per device, with evidence.** Each store is probed
//!   at open and classed as power-safe, flush-required, unverified or unsafe
//!   from what the device and operating system report. Operating-system cache
//!   toggles and vendor tables never promote a device.
//! - **A durable write produces an unforgeable receipt.** Positions, write
//!   tickets and receipts are distinct opaque types, so a sequence number can
//!   never be passed where a byte position is expected, and a barrier can never
//!   be a silent no-op.
//! - **Failure is fail-stop.** The first failed write or flush poisons the
//!   device's barrier domain. A flush is never retried and success is never
//!   reported after a failed one.
//! - **Nothing hides latency.** There are no timers, no group-commit windows and
//!   no background threads that wake an idle process.
//!
//! ## Quick start
//!
//! ```no_run
//! use store_io::{Store, StoreOptions};
//!
//! // First run: create. Every later run: `Store::open`.
//! let store = Store::create("data/orders", StoreOptions::default())?;
//!
//! // Regions are provisioned once; looking one up never creates it.
//! let wal = store.provision_append_region("wal", 256 << 20)?;
//!
//! // Append any number of bytes; durable when this returns.
//! let (pos, receipt) = wal.append_durable(b"order 1001: 3 widgets")?;
//! println!("durable through {} ({})", receipt.durable_through(), receipt.class());
//!
//! // Many records, one write and one flush.
//! let mut batch = wal.batch();
//! batch.append(b"order 1002")?;
//! batch.append(b"order 1003")?;
//! let (start, receipt) = batch.commit()?;
//!
//! // Small objects: old or new after any crash.
//! let manifest = store.provision_slot("manifest")?;
//! let _receipt = manifest.commit(b"wal@0")?;
//! # let _ = (pos, start, receipt);
//! # Ok::<(), store_io::Error>(())
//! ```
//!
//! `docs/GUIDE.md` in the repository walks through every part: append and
//! page regions, batches, many writers, small objects, recovery with
//! [`AppendRegion::scan`] and `resume_at`, the device report, errors and
//! space.
//!
//! ## Layers
//!
//! - **Simple:** [`Store`], [`AppendRegion`], [`PageRegion`], [`Slot`] and
//!   [`Directory`]; every call blocks until its I/O is done and copies the
//!   caller's bytes once into aligned buffers.
//! - **Batch:** [`AppendBatch`] and [`PageBatch`]: many writes, one barrier,
//!   with in-place encoding and no copy.
//! - **Engine:** the generic [`engine`] crate under all of it, which also runs
//!   on the deterministic simulator.
//!
//! ## Platforms
//!
//! Windows (unbuffered overlapped I/O on IOCP) and Linux (direct I/O,
//! `pwritev2`) have native backends ([`NativePlatform`]). On other targets the
//! types compile but there is no native backend yet; macOS arrives in 0.5.

#![deny(warnings)]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

/// The generic engine under the simple API (stores over any platform,
/// including the simulator).
pub use store_io_engine as engine;

pub use store_io_core::class::{DurabilityClass, ReceiptLabel};
pub use store_io_core::decide::{ClassDecision, DurableOpen, Trust};
pub use store_io_core::error::{
    ByteRange, Capability, CorruptionKind, Error, ErrorContext, FirstCause, Named, NotWrittenCause,
    Op, OsError, OsErrorSource, ReserveTag, Result,
};
pub use store_io_core::evidence::Evidence;
pub use store_io_core::id::{Generation, RegionId, VolumeId};
pub use store_io_engine::{
    CONTAINER, DurableReceipt, RegionPos, ScanItem, ScanSummary, SpaceReport, StoreOptions,
    StoreReport, TagSpace, WriteTicket,
};

/// The operating system's native backend.
#[cfg(windows)]
pub type NativePlatform = store_io_win::WinPlatform;

/// The operating system's native backend.
#[cfg(target_os = "linux")]
pub type NativePlatform = store_io_posix::PosixPlatform;

#[cfg(any(windows, target_os = "linux"))]
mod native;

#[cfg(any(windows, target_os = "linux"))]
pub use native::{
    AppendBatch, AppendRegion, Directory, PageBatch, PageRegion, Reservation, Slot, Store, probe,
};

/// What [`probe`] found out about a directory's device.
#[derive(Debug, Clone)]
pub struct Probe {
    /// The raw evidence.
    pub evidence: Evidence,
    /// The durability class decided from it with default trust.
    pub decision: ClassDecision,
}
