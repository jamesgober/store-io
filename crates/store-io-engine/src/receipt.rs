//! Positions, write tickets and durability receipts.
//!
//! These are the values a caller cannot forge: every constructor is private
//! to this crate. A position comes only from a region (checked against its
//! bounds and alignment), a ticket only from a submitted write, and a receipt
//! only from a completed durable write or a successful barrier.

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

    /// Whether this receipt proves the ticket's write durable.
    #[must_use]
    pub fn covers(&self, t: &WriteTicket) -> bool {
        self.volume == t.volume
            && self.region == t.region
            && self.generation == t.generation
            && self.start <= t.start
            && t.end <= self.through
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
