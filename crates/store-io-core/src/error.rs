//! The error model.
//!
//! One enum, [`Error`], whose variants tell the caller exactly what is known
//! about the bytes involved and what to do next. The two that matter most
//! are never conflated:
//!
//! - [`Error::NotWritten`]: the operation never reached the device; the media
//!   is provably untouched; fix the cause and retry.
//! - [`Error::DurabilityUnknown`]: the operation was submitted and failed; the
//!   bytes may or may not be on media, and the device's barrier domain is
//!   poisoned: every later write and barrier on it fails with
//!   [`Error::Poisoned`]. Recover by reopening and letting your recovery logic
//!   decide what survived.
//!
//! Errors carry the raw operating-system code, the operation, and the region
//! and byte range involved (the volume is the store the call was made on). They never carry payload bytes.

use core::fmt;

use crate::class::{MissingSet, ReasonSet};
use crate::id::{Generation, RegionId};

/// Raw operating-system error code, kept exactly as the OS reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OsError {
    /// The code: a Linux/macOS `errno`, a Win32 error, or an NTSTATUS.
    pub code: i32,
    /// Which numbering `code` uses.
    pub source: OsErrorSource,
}

/// Numbering space of an [`OsError`] code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsErrorSource {
    /// POSIX `errno`.
    Errno,
    /// Win32 `GetLastError()`.
    Win32,
    /// NTSTATUS from an `Nt*` call (stored as its bit pattern).
    NtStatus,
    /// Injected by the simulator.
    Sim,
}

impl fmt::Display for OsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.source {
            OsErrorSource::Errno => write!(f, "errno {}", self.code),
            OsErrorSource::Win32 => write!(f, "Win32 error {}", self.code),
            OsErrorSource::NtStatus => write!(f, "NTSTATUS {:#010x}", self.code as u32),
            OsErrorSource::Sim => write!(f, "simulated error {}", self.code),
        }
    }
}

impl std::error::Error for OsError {}

/// The operation that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Op {
    /// A data write.
    Write,
    /// A data read.
    Read,
    /// A data flush (barrier primitive).
    FlushData,
    /// A full flush (data and metadata).
    FlushAll,
    /// A directory flush.
    SyncDir,
    /// A rename.
    Rename,
    /// Creating a file or directory.
    Create,
    /// Opening a file or directory.
    Open,
    /// Allocating space.
    Allocate,
    /// Releasing space.
    Release,
    /// Taking the ownership lock.
    Lock,
    /// Probing the device.
    Probe,
    /// Removing a file.
    Unlink,
    /// Setting up an I/O queue (an io_uring ring, a completion port).
    Queue,
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Write => "write",
            Self::Read => "read",
            Self::FlushData => "data flush",
            Self::FlushAll => "full flush",
            Self::SyncDir => "directory flush",
            Self::Rename => "rename",
            Self::Create => "create",
            Self::Open => "open",
            Self::Allocate => "allocate",
            Self::Release => "release",
            Self::Lock => "lock",
            Self::Probe => "probe",
            Self::Unlink => "unlink",
            Self::Queue => "queue setup",
        })
    }
}

/// Byte range `[start, end)` within a region or file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    /// First byte.
    pub start: u64,
    /// One past the last byte.
    pub end: u64,
}

/// Where an error happened. The volume is the store the call was made on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ErrorContext {
    /// The region.
    pub region: Option<RegionId>,
    /// The byte range.
    pub range: Option<ByteRange>,
}

impl fmt::Display for ErrorContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(r) = self.region {
            write!(f, " region {r}")?;
        }
        if let Some(b) = self.range {
            write!(f, " bytes {}..{}", b.start, b.end)?;
        }
        Ok(())
    }
}

/// Why an operation was rejected before reaching the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NotWrittenCause {
    /// The offset or length is not aligned to the region's block size.
    Misaligned,
    /// The range extends past the end of the region.
    OutOfBounds,
    /// The position belongs to another region or volume.
    ForeignPosition,
    /// The position belongs to an older generation of the region.
    StaleGeneration,
    /// The write would overwrite bytes already covered by a receipt.
    OverwritesDurable,
    /// No buffer of the needed size is free in the pool.
    PoolExhausted,
    /// Too many writes are in flight on the region.
    TooManyInFlight,
    /// The region is not ready (released, shared, or failed verification).
    NotReady,
    /// The append would not fit in the region.
    RegionFull,
    /// The data is empty.
    Empty,
    /// The data exceeds the object's capacity (slots).
    TooLarge,
    /// The name is refused: empty, too long or holding NUL, or (for a
    /// directory's objects) one that could resolve to another file (see
    /// `Directory::replace`).
    InvalidName,
    /// The store is open read-only.
    ReadOnly,
    /// The device queue is full, or a `Submitter` holds as many operations
    /// (in flight or not yet taken back) as its depth allows.
    QueueFull,
    /// The operation was interrupted before any I/O was issued, or a read's
    /// bytes never came back (a backend breaking its contract). Nothing was
    /// written.
    Interrupted,
    /// The append region was reopened and its append position is not known
    /// yet: call `resume_at` with the end of your valid data first.
    NotPositioned,
    /// Two writes in one batch overlap.
    Overlap,
    /// The buffer does not come from this store's pool, so it may not meet
    /// the device's alignment (use the submitter's `take_buf`).
    ForeignBuffer,
    /// Allocation is frozen: a table page of a format-2 store is stale or
    /// lost, so no region may be provisioned, grown past its capacity or
    /// released until it is repaired (spec §9 step 2).
    Frozen,
    /// The options contradict each other (a region's cap below its size).
    InvalidOptions,
}

impl fmt::Display for NotWrittenCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Misaligned => "offset or length is not block-aligned",
            Self::OutOfBounds => "range extends past the end of the region",
            Self::ForeignPosition => "position belongs to another region or volume",
            Self::StaleGeneration => "position belongs to an older generation of the region",
            Self::OverwritesDurable => "write would overwrite durable bytes",
            Self::PoolExhausted => "no free I/O buffer of the needed size",
            Self::TooManyInFlight => "too many writes in flight on the region",
            Self::NotReady => "region is not ready",
            Self::RegionFull => "region has no room for the append",
            Self::Empty => "data is empty",
            Self::TooLarge => "data exceeds the object's capacity",
            Self::InvalidName => "name refused (empty, too long, holding NUL, or not allowed here)",
            Self::ReadOnly => "store is open read-only",
            Self::QueueFull => "device queue (or submitter) is full",
            Self::Interrupted => {
                "interrupted before any I/O was issued, or a read's bytes never came back"
            }
            Self::NotPositioned => "append position unknown after reopen; call resume_at first",
            Self::Overlap => "two writes in one batch overlap",
            Self::ForeignBuffer => "the buffer is not from this store's pool",
            Self::Frozen => "allocation is frozen: a table page is stale or lost",
            Self::InvalidOptions => "the options contradict each other",
        })
    }
}

/// What the domain was poisoned by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirstCause {
    /// The failed operation (or the caller's explicit poison).
    pub op: Option<Op>,
    /// The raw error, if any.
    pub raw: Option<OsError>,
}

/// Corruption store-io detected in its own metadata or on media.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CorruptionKind {
    /// The device reported an unreadable range.
    MediaError,
    /// A store-io header failed its checksum in both copies.
    HeaderCrc,
    /// The container belongs to another volume than expected.
    IdentityMismatch,
    /// The metadata is internally inconsistent (overlaps, impossible fields).
    Metadata,
    /// Both copies of a small object hold the same generation with different bytes.
    Fork,
    /// The container is shorter than its own metadata says it is.
    Truncated,
}

/// A capability the platform, filesystem or device does not offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Capability {
    /// A newer on-disk format or incompatible feature flags, or a format-1
    /// store opened for writing (format 1 is read-only from 0.5: copy it to
    /// format 2 with `copy_store`).
    FormatVersion,
    /// Directory flushes.
    DirectoryFlush,
    /// Trimming in place.
    TrimInPlace,
    /// Unsharing cloned extents.
    Unshare,
    /// Atomic (untorn) writes of the requested size.
    AtomicWrite,
    /// The operation on this platform.
    Platform,
}

/// What a lookup did not find, or found already existing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Named {
    /// A store at the given path.
    Store,
    /// A region with the given name.
    Region,
    /// A small-object slot with the given name.
    Slot,
}

/// What ran out: a space-accounting tag (tenant, class), store-io's own
/// table, or a region's own limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReserveTag {
    /// A caller-defined tag.
    Caller(u32),
    /// The region table is full.
    Table,
    /// The region is capped (its `max_size`): it cannot grow past its
    /// capacity.
    RegionLimit,
    /// The region's header lists as many extents as it can hold: it cannot
    /// grow past its capacity.
    Extents,
    /// The device or file system ran out of space during a growth.
    Growth,
}

/// Every error store-io returns.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// Rejected before reaching the device; the media is untouched. Fix the
    /// cause and retry.
    NotWritten {
        /// Why.
        cause: NotWrittenCause,
        /// Where.
        ctx: ErrorContext,
    },
    /// Submitted and failed; the bytes may or may not be durable. The barrier
    /// domain is now poisoned. Reopen and run recovery.
    DurabilityUnknown {
        /// The failed operation.
        op: Op,
        /// The raw error, if the OS returned one (a short transfer has none).
        raw: Option<OsError>,
        /// Where.
        ctx: ErrorContext,
    },
    /// The barrier domain failed earlier; no write or barrier will succeed on
    /// it. Reads still work. Reopen and run recovery.
    Poisoned {
        /// The failure that poisoned the domain.
        first: FirstCause,
    },
    /// Reservation or provisioning ran out of space or quota. Nothing was
    /// written and nothing is poisoned.
    NoSpace {
        /// The accounting tag that ran out.
        tag: ReserveTag,
        /// Where.
        ctx: ErrorContext,
    },
    /// store-io's own metadata or the media is damaged.
    Corruption {
        /// What kind.
        kind: CorruptionKind,
        /// Where.
        ctx: ErrorContext,
    },
    /// The device or stack is known to drop or fake durability. Use another
    /// device, or override explicitly (every receipt is then labelled).
    UnsafeDevice {
        /// Why.
        reasons: ReasonSet,
    },
    /// The evidence is incomplete; durability cannot be confirmed. Attest the
    /// device or override explicitly.
    Unverified {
        /// What could not be read.
        missing: MissingSet,
        /// What makes it unverifiable.
        reasons: ReasonSet,
    },
    /// The platform, filesystem or device does not support the operation.
    Unsupported {
        /// The missing capability.
        what: Capability,
    },
    /// The store was taken over by a newer owner. Never returned today:
    /// ownership is the operating system's lock with the acquire barrier
    /// (D-049), which no newer owner passes while a handle holds the store,
    /// so no handle can be fenced; reserved until the 1.0 API freeze
    /// decides whether it stays.
    Fenced {
        /// This handle's generation.
        stale: Generation,
    },
    /// Another holder has the lock: a store held by another handle (in
    /// this or another process; a writer and a reader exclude each other),
    /// or a directory another handle or process is replacing in.
    Locked,
    /// The named store, region or slot does not exist.
    NotFound {
        /// What was looked up.
        what: Named,
    },
    /// The named store, region or slot already exists.
    AlreadyExists {
        /// What was created.
        what: Named,
    },
    /// A setup or metadata operation failed before any data I/O.
    Io {
        /// The operation.
        op: Op,
        /// The raw error.
        raw: OsError,
        /// Where.
        ctx: ErrorContext,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotWritten { cause, ctx } => {
                write!(f, "not written ({cause}){ctx}; media untouched")
            }
            Self::DurabilityUnknown { op, raw, ctx } => {
                write!(f, "{op} failed after submission{ctx}")?;
                if let Some(r) = raw {
                    write!(f, " ({r})")?;
                }
                f.write_str("; durability unknown, device domain poisoned; reopen and recover")
            }
            Self::Poisoned { first } => {
                f.write_str("device domain poisoned by an earlier failure")?;
                if let Some(op) = first.op {
                    write!(f, " ({op}")?;
                    if let Some(r) = first.raw {
                        write!(f, ": {r}")?;
                    }
                    f.write_str(")")?;
                }
                f.write_str("; reopen and recover")
            }
            Self::NoSpace { tag, ctx } => write!(f, "no space for {tag:?}{ctx}; nothing committed"),
            Self::Corruption { kind, ctx } => write!(f, "corruption: {kind:?}{ctx}"),
            Self::UnsafeDevice { reasons } => {
                write_reasons(f, "device is unsafe for durable writes", reasons)
            }
            Self::Unverified { missing, reasons } => {
                write_reasons(f, "device durability is unverified", reasons)?;
                if !missing.is_empty() {
                    f.write_str("; missing:")?;
                    for m in missing.iter() {
                        write!(f, " {m};")?;
                    }
                }
                Ok(())
            }
            Self::Unsupported {
                what: Capability::FormatVersion,
            } => write!(
                f,
                "unsupported: FormatVersion (a newer on-disk format, or a format-1 store \
                 opened for writing: format 1 is read-only; copy it to format 2 with \
                 copy_store or `store-io copy`)"
            ),
            Self::Unsupported { what } => write!(f, "unsupported: {what:?}"),
            Self::Fenced { stale } => write!(
                f,
                "fenced: store taken over by a newer owner (this handle is {stale})"
            ),
            Self::Locked => {
                f.write_str("locked by another holder (a store's owner, or a directory's writer)")
            }
            Self::NotFound { what } => write!(f, "{what:?} not found"),
            Self::AlreadyExists { what } => write!(f, "{what:?} already exists"),
            Self::Io { op, raw, ctx } => write!(f, "{op} failed{ctx} ({raw})"),
        }
    }
}

fn write_reasons(f: &mut fmt::Formatter<'_>, head: &str, reasons: &ReasonSet) -> fmt::Result {
    f.write_str(head)?;
    for (i, r) in reasons.iter().enumerate() {
        f.write_str(if i == 0 { ": " } else { "; " })?;
        write!(f, "{r}")?;
    }
    Ok(())
}

impl std::error::Error for Error {}

/// `Result` with [`Error`].
pub type Result<T> = core::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::class::Reason;

    #[test]
    fn test_error_is_small() {
        assert!(
            core::mem::size_of::<Error>() <= 64,
            "{}",
            core::mem::size_of::<Error>()
        );
    }

    #[test]
    fn test_display_is_actionable() {
        let e = Error::DurabilityUnknown {
            op: Op::FlushData,
            raw: Some(OsError {
                code: 5,
                source: OsErrorSource::Errno,
            }),
            ctx: ErrorContext {
                region: Some(RegionId::from_raw(2)),
                ..ErrorContext::default()
            },
        };
        assert_eq!(
            e.to_string(),
            "data flush failed after submission region r2 (errno 5); durability unknown, device domain poisoned; reopen and recover"
        );
        let e = Error::UnsafeDevice {
            reasons: ReasonSet::new().with(Reason::NoBarrier),
        };
        assert_eq!(
            e.to_string(),
            "device is unsafe for durable writes: mounted without write barriers"
        );
    }
}
