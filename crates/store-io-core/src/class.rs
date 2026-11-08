//! Durability classes, receipt labels, and the reason and missing-evidence
//! vocabularies that explain them.

use core::fmt;

use crate::set::{BitSet, flag_enum};

/// The class of an I/O (PLANNING R5.4): what a device's pacing does with
/// it. Pacing changes when background work reaches the device, never what
/// is written or when it is durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum IoClass {
    /// Work a caller waits on: every blocking write, batch and slot commit,
    /// and a `Submitter`'s writes unless configured otherwise. Never paced;
    /// its flushes mark its device's commits active.
    #[default]
    Commit,
    /// A blocking read. Never paced, never counted as activity.
    Read,
    /// Background work: scans, read streams, export, import, `copy_store`,
    /// unshare, and `Submitter`s configured so. Always split into pieces;
    /// paced while the device's commits are active.
    Background,
}

impl IoClass {
    /// Whether this class is paced (only [`Self::Background`]).
    #[inline]
    #[must_use]
    pub const fn is_background(self) -> bool {
        matches!(self, Self::Background)
    }

    /// Whether this class's flushes and writes mark commits active (only
    /// [`Self::Commit`]).
    #[inline]
    #[must_use]
    pub const fn is_commit(self) -> bool {
        matches!(self, Self::Commit)
    }
}

impl fmt::Display for IoClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Commit => "commit",
            Self::Read => "read",
            Self::Background => "background",
        })
    }
}

/// What a completed durable operation means on this device.
///
/// Decided per device from evidence the device and operating system report
/// (never from operating-system cache toggles, vendor tables or timing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DurabilityClass {
    /// The device reports no volatile write cache (or an exact-model
    /// certificate says so) and its health is known to be good: a completed
    /// direct write into a ready region is durable; barriers need no flush.
    PowerSafe,
    /// The device has a volatile cache and the stack passes flushes: data is
    /// durable once a flush issued after the write completed has completed.
    FlushRequired,
    /// The evidence could hide suppressed flushes. Durable opens are refused
    /// unless attested or overridden.
    Unverified,
    /// The stack is known to drop or fake durability. Durable opens are
    /// refused unless overridden.
    Unsafe,
}

impl DurabilityClass {
    /// Whether a durable open is allowed without attestation or override.
    #[inline]
    #[must_use]
    pub const fn allows_durable(self) -> bool {
        matches!(self, Self::PowerSafe | Self::FlushRequired)
    }

    /// Whether barriers must issue a device flush.
    #[inline]
    #[must_use]
    pub const fn needs_flush(self) -> bool {
        !matches!(self, Self::PowerSafe)
    }
}

impl fmt::Display for DurabilityClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::PowerSafe => "power-safe",
            Self::FlushRequired => "flush-required",
            Self::Unverified => "unverified",
            Self::Unsafe => "unsafe",
        })
    }
}

flag_enum! {
    /// Why a device was classed below power-safe, or refused.
    pub enum Reason {
        /// filesystem cannot provide durable direct I/O (tmpfs, FUSE, network, 9p, FAT, RAM disk)
        FsUnsafe = 0,
        /// filesystem not yet qualified for durable writes
        FsUnqualified = 1,
        /// mounted with data journaling
        DataJournal = 2,
        /// mounted without write barriers
        NoBarrier = 3,
        /// filesystem syncs disabled (for example ZFS sync=disabled)
        SyncDisabled = 4,
        /// file has data journaling, compression or software encryption enabled
        InodeFlags = 5,
        /// errors=continue or no journal: metadata errors may be ignored
        WeakErrorMode = 6,
        /// operating system reports write-through over a device with a volatile cache
        FlushSuppressed = 7,
        /// operating system reports that cache flushes are not supported
        FlushUnsupported = 8,
        /// "turn off write-cache buffer flushing" is set: flushes never reach the device
        UserPowerProtection = 9,
        /// parity RAID without a write journal or partial parity log
        ParityWriteHole = 10,
        /// mirror member acknowledges before writing (write-behind)
        WriteBehind = 11,
        /// a write-back cache layer sits in the stack
        CacheLayer = 12,
        /// a thin, copy-on-write or virtual-disk layer may allocate on overwrite
        AllocatingLayer = 13,
        /// running under a hypervisor without a certificate for this volume type
        Hypervisor = 14,
        /// a filter driver or software encryption layer sits in the stack
        FilterDriver = 15,
        /// device bus (USB) commonly drops or reorders flushes
        UntrustedBus = 16,
        /// SMART reports the volatile-memory backup has failed
        BackupFailed = 17,
        /// volatile write cache state could not be read
        CacheStateUnknown = 18,
        /// the full-flush primitive (macOS F_FULLFSYNC) is not supported, or was not checked
        FullFlushUnsupported = 19,
        /// known kernel or driver bug affecting this configuration
        KnownBug = 20,
        /// device not known to be internal (an external drive, a disk image, or not identified)
        DeviceNotInternal = 21,
        /// device kept by another system: over a network or fabric (NVMe over fabrics, iSCSI, Fibre Channel, NBD, RBD) or a user-space server
        RemoteTarget = 22,
        /// a storage pool (Windows Storage Spaces) with its own resiliency and caching sits under the volume
        StoragePool = 23,
    }
}

flag_enum! {
    /// Evidence the probe could not obtain.
    pub enum Missing {
        /// device identify data (needs a readable device node)
        Identify = 0,
        /// SMART / health log (needs privilege on Linux)
        Smart = 1,
        /// volatile write cache presence
        CachePresence = 2,
        /// volatile write cache enabled state
        CacheEnabled = 3,
        /// operating-system cache mode
        OsCacheMode = 4,
        /// device atomic-write fields
        AtomicFields = 5,
        /// direct-I/O alignment
        DioAlignment = 6,
        /// storage stack layers below the filesystem
        Stack = 7,
        /// filesystem mount options
        MountOptions = 8,
        /// file attribute flags
        InodeFlags = 9,
        /// page-cache residency of a range (Linux `cachestat`)
        PageCache = 10,
    }
}

/// Set of [`Reason`]s.
pub type ReasonSet = BitSet<Reason>;
/// Set of [`Missing`] evidence items.
pub type MissingSet = BitSet<Missing>;

/// Opaque identity of a certificate (exact model, firmware, capacity and LBA
/// format, backed by a power-cut rig log) or of an operator attestation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CertId(pub u64);

/// Why the holder of a receipt may trust it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptLabel {
    /// The class comes from the device's own evidence.
    Evidence,
    /// The class comes from a certificate for this exact device model and firmware.
    Certified(CertId),
    /// The class was lifted from unverified by an operator attestation.
    Attested(CertId),
    /// The device is unverified or unsafe and the caller overrode the refusal;
    /// the receipt proves only that store-io issued the most conservative
    /// primitive available.
    Overridden(ReasonSet),
}

impl fmt::Display for ReceiptLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Evidence => f.write_str("evidence"),
            Self::Certified(id) => write!(f, "certified({:#x})", id.0),
            Self::Attested(id) => write!(f, "attested({:#x})", id.0),
            Self::Overridden(r) => {
                f.write_str("overridden(")?;
                for (i, reason) in r.iter().enumerate() {
                    if i > 0 {
                        f.write_str("; ")?;
                    }
                    write!(f, "{reason}")?;
                }
                f.write_str(")")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_io_class_defaults_to_commit_and_names_itself() {
        assert_eq!(IoClass::default(), IoClass::Commit);
        assert!(IoClass::Commit.is_commit() && !IoClass::Commit.is_background());
        assert!(!IoClass::Read.is_commit() && !IoClass::Read.is_background());
        assert!(IoClass::Background.is_background() && !IoClass::Background.is_commit());
        assert_eq!(IoClass::Background.to_string(), "background");
        assert_eq!(IoClass::Read.to_string(), "read");
        assert_eq!(IoClass::Commit.to_string(), "commit");
    }

    #[test]
    fn test_class_rules() {
        assert!(DurabilityClass::PowerSafe.allows_durable());
        assert!(!DurabilityClass::PowerSafe.needs_flush());
        assert!(DurabilityClass::FlushRequired.needs_flush());
        assert!(!DurabilityClass::Unverified.allows_durable());
        assert!(!DurabilityClass::Unsafe.allows_durable());
    }

    #[test]
    fn test_label_display_lists_reasons() {
        let l = ReceiptLabel::Overridden(ReasonSet::new().with(Reason::NoBarrier));
        assert_eq!(l.to_string(), "overridden(mounted without write barriers)");
    }
}
