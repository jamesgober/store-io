//! How a [`StoreReport`] prints, and the unavailable-region report (D-090).

use store_io_core::RegionId;
use store_io_core::error::CorruptionKind;
use store_io_format::meta::RegionKind;

use crate::store::StoreReport;

impl core::fmt::Display for StoreReport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let d = &self.decision;
        writeln!(f, "volume           {}", self.volume)?;
        writeln!(
            f,
            "io               {} ({})",
            self.io.interface, self.io.reason
        )?;
        writeln!(f, "class            {}", d.class)?;
        writeln!(f, "label            {}", d.label)?;
        writeln!(f, "durable open     {:?}", d.durable_open)?;
        if d.power_safe_candidate {
            writeln!(
                f,
                "note             device reports no volatile cache; health unreadable (power-safe candidate)"
            )?;
        }
        for r in d.reasons.iter() {
            writeln!(f, "reason           {r}")?;
        }
        for m in d.missing.iter() {
            writeln!(f, "missing          {m}")?;
        }
        writeln!(f, "flush execution  inline (leader)")?;
        writeln!(f, "block size       {}", self.block_size)?;
        writeln!(f, "table capacity   {} regions", self.table_capacity)?;
        writeln!(f, "filesystem       {:?}", self.evidence.fs.kind)?;
        writeln!(f, "bus              {:?}", self.evidence.device.bus)?;
        if let Some(m) = &self.evidence.device.model {
            writeln!(f, "model            {m}")?;
        }
        for u in &self.unavailable {
            writeln!(
                f,
                "unavailable      region {:?} (id {}, {:?}): {}",
                u.name,
                u.id.get(),
                u.kind,
                u.reason
            )?;
        }
        writeln!(f, "format           {}", self.format)?;
        if self.format == store_io_format::slot::FORMAT_2 {
            write!(f, "{}", self.metadata)?;
        }
        write!(
            f,
            "mode             {}",
            if self.read_only {
                "read-only"
            } else {
                "read-write"
            }
        )
    }
}

/// Why a region the region table names could not be opened (D-090).
///
/// Such a region is never used as ready and its name stays reserved; a lookup
/// returns [`Error::Corruption`](store_io_core::error::Error::Corruption) naming the region, never `NotFound`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Unavailable {
    /// The container is shorter than the region's extent.
    ContainerShort {
        /// Bytes the container must hold for this region.
        needed: u64,
        /// Bytes it holds.
        actual: u64,
    },
    /// Neither copy of the region's header is valid.
    HeaderUnreadable,
    /// The header passes its checksum but holds impossible values.
    HeaderInvalid,
    /// The header belongs to another region or another kind of region.
    HeaderMismatch,
    /// The header says the region was released while the table says ready.
    HeaderReleased,
    /// Format 2: the table entry itself holds impossible values (its span
    /// stays reserved).
    EntryInvalid,
    /// Format 2: the header's extents do not match the entry (their sum,
    /// the span, the table area, or an overlap within the region).
    ExtentsInvalid,
    /// Format 2: a region with a newer (or equal) footprint overlaps it.
    Overlaps,
    /// Format 2: a region with a newer (or equal) footprint has its name.
    NameTaken,
}

impl Unavailable {
    /// The corruption kind a lookup of such a region reports.
    #[must_use]
    pub fn corruption_kind(&self) -> CorruptionKind {
        match self {
            Self::ContainerShort { .. } => CorruptionKind::Truncated,
            Self::HeaderUnreadable => CorruptionKind::HeaderCrc,
            Self::HeaderInvalid
            | Self::HeaderMismatch
            | Self::HeaderReleased
            | Self::EntryInvalid
            | Self::ExtentsInvalid
            | Self::Overlaps
            | Self::NameTaken => CorruptionKind::Metadata,
        }
    }
}

impl core::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ContainerShort { needed, actual } => write!(
                f,
                "the container holds {actual} bytes but the region needs {needed}"
            ),
            Self::HeaderUnreadable => f.write_str("neither copy of its header is valid"),
            Self::HeaderInvalid => f.write_str("its header holds impossible values"),
            Self::HeaderMismatch => f.write_str("its header belongs to another region or kind"),
            Self::HeaderReleased => {
                f.write_str("its header says released while the table says ready")
            }
            Self::EntryInvalid => f.write_str("its table entry holds impossible values"),
            Self::ExtentsInvalid => f.write_str("its extents do not match its table entry"),
            Self::Overlaps => f.write_str("a newer region overlaps it"),
            Self::NameTaken => f.write_str("a newer region has its name"),
        }
    }
}

/// A region the region table names that could not be opened (D-090).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnavailableRegion {
    /// The region's name.
    pub name: String,
    /// The region's id.
    pub id: RegionId,
    /// The region's kind.
    pub kind: RegionKind,
    /// Why it could not be opened.
    pub reason: Unavailable,
}
