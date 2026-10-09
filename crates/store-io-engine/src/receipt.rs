//! Positions, write tickets and durability receipts.
//!
//! These are the values a caller cannot forge: every constructor is private
//! to this crate. A position comes only from a region (checked against its
//! bounds and alignment), a ticket only from a submitted write, and a receipt
//! only from a completed durable write or a successful barrier.
//!
//! A ticket records the first device flush that can cover its write: the
//! flush issued after the write completed. A receipt records the flush its
//! barrier waited for. The receipt proves the ticket's write durable exactly
//! when the ticket's flush number is not later than the receipt's, which is
//! the join rule the flush domain enforces.
//!
//! # Compile-time guarantees
//!
//! The valid forms compile:
//!
//! ```
//! # use std::path::Path;
//! # use store_io_engine::{Store, StoreOptions};
//! # use store_io_sim::{SimConfig, SimPlatform};
//! # let store = Store::create(SimPlatform::new(SimConfig::volatile(1)), Path::new("/db"), StoreOptions::default()).unwrap();
//! # let pages = store.provision_page_region("p", 1 << 20).unwrap();
//! # let wal = store.provision_append_region("w", 1 << 20).unwrap();
//! let pos = pages.pos(4096)?;                       // a checked position
//! let ticket = pages.write(pos, &[0u8; 4096])?;     // a ticket per write
//! let receipt = pages.sync_through(&ticket)?;       // a receipt per barrier
//! assert!(receipt.covers(&ticket));
//! let appended = wal.append(b"x")?;
//! let _durable = wal.sync_through(&appended)?;
//! # Ok::<(), store_io_core::error::Error>(())
//! ```
//!
//! An integer is never a position:
//!
//! ```compile_fail,E0308
//! # use std::path::Path;
//! # use store_io_engine::{Store, StoreOptions};
//! # use store_io_sim::{SimConfig, SimPlatform};
//! # let store = Store::create(SimPlatform::new(SimConfig::volatile(1)), Path::new("/db"), StoreOptions::default()).unwrap();
//! # let pages = store.provision_page_region("p", 1 << 20).unwrap();
//! let _ticket = pages.write(4096, &[0u8; 4096]);
//! ```
//!
//! A position cannot be built by hand:
//!
//! ```compile_fail,E0451
//! # use store_io_engine::RegionPos;
//! let _pos = RegionPos { offset: 4096 };
//! ```
//!
//! A receipt cannot be forged:
//!
//! ```compile_fail,E0451
//! # use store_io_engine::DurableReceipt;
//! let _receipt = DurableReceipt { through: u64::MAX };
//! ```
//!
//! A ticket is not a position, and a receipt is not a ticket:
//!
//! ```compile_fail,E0308
//! # use std::path::Path;
//! # use store_io_engine::{Store, StoreOptions};
//! # use store_io_sim::{SimConfig, SimPlatform};
//! # let store = Store::create(SimPlatform::new(SimConfig::volatile(1)), Path::new("/db"), StoreOptions::default()).unwrap();
//! # let pages = store.provision_page_region("p", 1 << 20).unwrap();
//! let ticket = pages.write(pages.pos(0).unwrap(), &[0u8; 4096]).unwrap();
//! let _again = pages.write(ticket, &[0u8; 4096]);
//! ```
//!
//! ```compile_fail,E0308
//! # use std::path::Path;
//! # use store_io_engine::{Store, StoreOptions};
//! # use store_io_sim::{SimConfig, SimPlatform};
//! # let store = Store::create(SimPlatform::new(SimConfig::volatile(1)), Path::new("/db"), StoreOptions::default()).unwrap();
//! # let wal = store.provision_append_region("w", 1 << 20).unwrap();
//! let (_pos, receipt) = wal.append_durable(b"x").unwrap();
//! let _again = wal.sync_through(&receipt);
//! ```
//!
//! A ticket stands for one write and cannot be copied:
//!
//! ```compile_fail,E0599
//! # use std::path::Path;
//! # use store_io_engine::{Store, StoreOptions};
//! # use store_io_sim::{SimConfig, SimPlatform};
//! # let store = Store::create(SimPlatform::new(SimConfig::volatile(1)), Path::new("/db"), StoreOptions::default()).unwrap();
//! # let wal = store.provision_append_region("w", 1 << 20).unwrap();
//! let ticket = wal.append(b"x").unwrap();
//! let _copy = ticket.clone();
//! ```
//!
//! Dropping a ticket unused is flagged (here made an error):
//!
//! ```compile_fail
//! #![deny(unused_must_use)]
//! # use std::path::Path;
//! # use store_io_engine::{Store, StoreOptions};
//! # use store_io_sim::{SimConfig, SimPlatform};
//! # fn main() -> Result<(), store_io_core::error::Error> {
//! # let store = Store::create(SimPlatform::new(SimConfig::volatile(1)), Path::new("/db"), StoreOptions::default())?;
//! # let wal = store.provision_append_region("w", 1 << 20)?;
//! wal.append(b"x")?;
//! # Ok(())
//! # }
//! ```

use core::fmt;

use store_io_core::class::{DurabilityClass, ReceiptLabel};
use store_io_core::id::{Generation, RegionId, VolumeId};

/// A byte position inside one generation of one region.
///
/// Created by [`crate::PageRegion::pos`] or returned by appends. A position
/// from another region, or from a generation that has since been recycled,
/// is rejected before any I/O.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct RegionPos {
    pub(crate) volume: VolumeId,
    pub(crate) region: RegionId,
    pub(crate) generation: Generation,
    pub(crate) offset: u64,
}

impl RegionPos {
    /// Byte offset within the region's data area.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// The region.
    #[must_use]
    pub fn region(&self) -> RegionId {
        self.region
    }

    /// The region generation the position belongs to.
    #[must_use]
    pub fn generation(&self) -> Generation {
        self.generation
    }
}

impl fmt::Debug for RegionPos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "RegionPos({}/{}+{})",
            self.region, self.generation, self.offset
        )
    }
}

/// Proof that a write was submitted and completed. Barrier it to make it
/// durable. Not `Clone`: one ticket per write.
#[must_use = "a write ticket is how you make the write durable; drop it only if durability does not matter"]
#[derive(PartialEq, Eq)]
pub struct WriteTicket {
    pub(crate) volume: VolumeId,
    pub(crate) region: RegionId,
    pub(crate) generation: Generation,
    pub(crate) start: u64,
    pub(crate) end: u64,
    /// The first flush issued after the write completed.
    pub(crate) need: u64,
    /// The open store that issued it.
    pub(crate) epoch: u64,
}

impl WriteTicket {
    /// Where the write starts.
    #[must_use]
    pub fn pos(&self) -> RegionPos {
        RegionPos {
            volume: self.volume,
            region: self.region,
            generation: self.generation,
            offset: self.start,
        }
    }

    /// One past the last byte written (block-aligned for appends).
    #[must_use]
    pub fn end(&self) -> u64 {
        self.end
    }
}

impl fmt::Debug for WriteTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "WriteTicket({}/{} {}..{})",
            self.region, self.generation, self.start, self.end
        )
    }
}

/// Unforgeable proof that bytes are durable.
///
/// Its existence means every byte it covers is durable at [`Self::class`] on
/// this device, under the device's documented fault model. The label says
/// why that class is trusted.
#[must_use]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct DurableReceipt {
    pub(crate) volume: VolumeId,
    pub(crate) region: RegionId,
    pub(crate) generation: Generation,
    pub(crate) class: DurabilityClass,
    pub(crate) label: ReceiptLabel,
    pub(crate) start: u64,
    pub(crate) through: u64,
    pub(crate) untorn: bool,
    /// The flush the barrier waited for (`u64::MAX` when every completed
    /// write is durable at completion; 0 when no barrier ran).
    pub(crate) need: u64,
    /// `through` is a contiguous durable prefix of an append region.
    pub(crate) prefix: bool,
    /// The open store that issued it.
    pub(crate) epoch: u64,
}

impl DurableReceipt {
    /// The volume.
    #[must_use]
    pub fn volume(&self) -> VolumeId {
        self.volume
    }

    /// The region.
    #[must_use]
    pub fn region(&self) -> RegionId {
        self.region
    }

    /// The region generation.
    #[must_use]
    pub fn generation(&self) -> Generation {
        self.generation
    }

    /// The durability class the bytes are durable at.
    #[must_use]
    pub fn class(&self) -> DurabilityClass {
        self.class
    }

    /// Why the class is trusted.
    #[must_use]
    pub fn label(&self) -> ReceiptLabel {
        self.label
    }

    /// For append regions: every byte before this offset is durable. For page
    /// regions and slots: the end of the covered write.
    #[must_use]
    pub fn durable_through(&self) -> u64 {
        self.through
    }

    /// Whether the covered write was accepted as atomic (never torn).
    #[must_use]
    pub fn untorn(&self) -> bool {
        self.untorn
    }

    /// Whether this receipt proves the ticket's write durable: the write
    /// completed before the receipt's flush was issued, or (append regions)
    /// it lies inside the durable prefix. Tickets and receipts from different
    /// opens of a store never match: a reopened store vouches only for its
    /// own writes.
    #[must_use]
    pub fn covers(&self, t: &WriteTicket) -> bool {
        self.epoch == t.epoch
            && self.volume == t.volume
            && self.region == t.region
            && self.generation == t.generation
            && (t.need <= self.need || (self.prefix && t.end <= self.through))
    }
}

impl fmt::Debug for DurableReceipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "DurableReceipt({}/{} {}..{} {} {})",
            self.region, self.generation, self.start, self.through, self.class, self.label
        )
    }
}
