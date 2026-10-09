//! Identities: which volume, which region, which generation.
//!
//! These are distinct types so that one can never be passed where another is
//! expected. They are plain values read from and written to disk; what must
//! be unforgeable (positions, tickets, receipts) is built from them inside the
//! engine.

use core::fmt;

/// 128-bit identity of a volume (one container or raw namespace).
///
/// Generated randomly at creation and stored in every slot header, so a slot
/// or region from another volume is recognised as foreign.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VolumeId([u8; 16]);

impl VolumeId {
    /// Wraps the 16 bytes stored on disk.
    #[inline]
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The 16 bytes as stored on disk.
    #[inline]
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for VolumeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, b) in self.0.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for VolumeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VolumeId({self})")
    }
}

/// Identity of a region within a volume. Never reused within a volume.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RegionId(u32);

impl RegionId {
    /// Wraps the id stored on disk.
    #[inline]
    #[must_use]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// The id as stored on disk.
    #[inline]
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for RegionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "r{}", self.0)
    }
}

impl fmt::Debug for RegionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RegionId({})", self.0)
    }
}

/// A generation: how many times an object or region has been rewritten,
/// recycled or re-owned. Starts at 1; 0 is never written.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Generation(u64);

impl Generation {
    /// The first generation.
    pub const FIRST: Self = Self(1);

    /// Wraps a generation read from disk. Returns `None` for 0, which is never
    /// written.
    #[inline]
    #[must_use]
    pub const fn from_raw(raw: u64) -> Option<Self> {
        if raw == 0 { None } else { Some(Self(raw)) }
    }

    /// The generation as stored on disk.
    #[inline]
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The following generation, or `None` at `u64::MAX` (never reached in
    /// practice: one increment per nanosecond would take 584 years).
    #[inline]
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }
}

impl fmt::Display for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "g{}", self.0)
    }
}

impl fmt::Debug for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Generation({})", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_volume_id_displays_as_uuid() {
        let v = VolumeId::from_bytes([0xAB; 16]);
        assert_eq!(v.to_string(), "abababab-abab-abab-abab-abababababab");
    }

    #[test]
    fn test_generation_zero_is_not_a_generation() {
        assert_eq!(Generation::from_raw(0), None);
        assert_eq!(Generation::from_raw(1), Some(Generation::FIRST));
        assert_eq!(Generation::FIRST.next().map(Generation::get), Some(2));
        assert_eq!(
            Generation::from_raw(u64::MAX).and_then(Generation::next),
            None
        );
    }
}
