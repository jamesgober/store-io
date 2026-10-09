//! Classifying raw operating-system errors.
//!
//! The rule depends first on *when* the error happened, then on the code:
//!
//! | Stage | Result |
//! |---|---|
//! | before the operation reached the kernel or device | [`ErrorKind::NotWritten`] |
//! | a submitted write, flush, full flush or directory flush | [`ErrorKind::DurabilityUnknown`] (poisons), whatever the code |
//! | anything after a rename was issued | [`ErrorKind::DurabilityUnknown`] (poisons) |
//! | reservation, allocation or provisioning | out of space or quota → [`ErrorKind::NoSpace`]; otherwise [`ErrorKind::Io`] |
//! | other metadata and setup | [`ErrorKind::Io`] |
//!
//! After submission every code is treated as "durability unknown": a device
//! may have persisted some, none or all of the bytes whatever the code says,
//! and fail-stop is the only safe response. The code still matters for
//! reporting and for the space checks.

use crate::error::{OsError, OsErrorSource};

/// When an operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Validation or setup before anything reached the kernel or device.
    BeforeSubmit,
    /// A write, flush, full flush or directory flush after submission.
    Submitted,
    /// After a rename was issued.
    AfterRename,
    /// Reservation, allocation or provisioning fill.
    Space,
    /// Other metadata or setup operations (open, lock, probe).
    Setup,
}

/// The class of an error, before context is attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Media untouched; retryable after fixing the cause.
    NotWritten,
    /// Durability unknown; the domain must be poisoned.
    DurabilityUnknown,
    /// Out of space or quota during reservation or provisioning.
    NoSpace,
    /// A setup or metadata failure with no effect on durable data.
    Io,
}

/// Classifies a raw error by stage and code.
#[must_use]
pub fn classify(stage: Stage, raw: OsError) -> ErrorKind {
    match stage {
        Stage::BeforeSubmit => ErrorKind::NotWritten,
        Stage::Submitted | Stage::AfterRename => ErrorKind::DurabilityUnknown,
        Stage::Space if is_no_space(raw) => ErrorKind::NoSpace,
        Stage::Space | Stage::Setup => ErrorKind::Io,
    }
}

/// Whether the code means out of space or out of quota.
#[must_use]
pub fn is_no_space(raw: OsError) -> bool {
    match raw.source {
        // ENOSPC = 28, EDQUOT = 122 (Linux) / 69 (macOS).
        OsErrorSource::Errno => matches!(raw.code, 28 | 122 | 69),
        // ERROR_HANDLE_DISK_FULL = 39, ERROR_DISK_FULL = 112,
        // ERROR_DISK_QUOTA_EXCEEDED = 1295.
        OsErrorSource::Win32 => matches!(raw.code, 39 | 112 | 1295),
        // STATUS_DISK_FULL = 0xC000007F, STATUS_QUOTA_EXCEEDED = 0xC0000044.
        OsErrorSource::NtStatus => matches!(raw.code as u32, 0xC000_007F | 0xC000_0044),
        OsErrorSource::Sim => raw.code == 28,
    }
}

/// Whether a failed read means the device could not read the range (a
/// latent sector error, a checksum failure, a device I/O error), as opposed
/// to a bad request or a lost handle. A scan reports such ranges as
/// unreadable and continues past them.
#[must_use]
pub fn is_media_error(raw: OsError) -> bool {
    match raw.source {
        // EIO = 5, ENODATA = 61, EBADMSG = 74 (checksum failures on some
        // file systems), EUCLEAN = 117 (structure needs cleaning).
        OsErrorSource::Errno => matches!(raw.code, 5 | 61 | 74 | 117),
        // ERROR_CRC = 23, ERROR_SECTOR_NOT_FOUND = 27, ERROR_READ_FAULT = 30,
        // ERROR_DATA_CHECKSUM_ERROR = 323, ERROR_DEVICE_HARDWARE_ERROR = 483,
        // ERROR_IO_DEVICE = 1117, ERROR_FILE_CORRUPT = 1392,
        // ERROR_DISK_CORRUPT = 1393.
        OsErrorSource::Win32 => {
            matches!(raw.code, 23 | 27 | 30 | 323 | 483 | 1117 | 1392 | 1393)
        }
        // STATUS_NONEXISTENT_SECTOR, STATUS_DATA_ERROR, STATUS_CRC_ERROR,
        // STATUS_DEVICE_DATA_ERROR, STATUS_FILE_CORRUPT_ERROR,
        // STATUS_IO_DEVICE_ERROR, STATUS_DATA_CHECKSUM_ERROR.
        OsErrorSource::NtStatus => matches!(
            raw.code as u32,
            0xC000_0015
                | 0xC000_003E
                | 0xC000_003F
                | 0xC000_009C
                | 0xC000_0102
                | 0xC000_0185
                | 0xC000_A002
        ),
        OsErrorSource::Sim => raw.code == 5,
    }
}

/// Whether the code means the call was interrupted before doing anything
/// (`EINTR`). Only meaningful for calls documented to fail with `EINTR`
/// before any I/O; callers retry those and count the retry.
#[must_use]
pub fn is_interrupted(raw: OsError) -> bool {
    raw.source == OsErrorSource::Errno && raw.code == 4
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn errno(code: i32) -> OsError {
        OsError {
            code,
            source: OsErrorSource::Errno,
        }
    }
    const fn win(code: i32) -> OsError {
        OsError {
            code,
            source: OsErrorSource::Win32,
        }
    }

    #[test]
    fn test_submitted_errors_are_always_durability_unknown() {
        for code in [5, 28, 122, 30, 117, 19, 6, 0] {
            assert_eq!(
                classify(Stage::Submitted, errno(code)),
                ErrorKind::DurabilityUnknown
            );
            assert_eq!(
                classify(Stage::AfterRename, errno(code)),
                ErrorKind::DurabilityUnknown
            );
        }
        for code in [112, 1117, 23, 21] {
            assert_eq!(
                classify(Stage::Submitted, win(code)),
                ErrorKind::DurabilityUnknown
            );
        }
    }

    #[test]
    fn test_space_stage_separates_no_space_from_other_errors() {
        assert_eq!(classify(Stage::Space, errno(28)), ErrorKind::NoSpace);
        assert_eq!(classify(Stage::Space, errno(122)), ErrorKind::NoSpace);
        assert_eq!(classify(Stage::Space, win(112)), ErrorKind::NoSpace);
        assert_eq!(classify(Stage::Space, win(1295)), ErrorKind::NoSpace);
        let nt = OsError {
            code: 0xC000_007Fu32 as i32,
            source: OsErrorSource::NtStatus,
        };
        assert_eq!(classify(Stage::Space, nt), ErrorKind::NoSpace);
        assert_eq!(classify(Stage::Space, errno(5)), ErrorKind::Io);
    }

    #[test]
    fn test_before_submit_is_not_written_and_setup_is_io() {
        assert_eq!(
            classify(Stage::BeforeSubmit, errno(5)),
            ErrorKind::NotWritten
        );
        assert_eq!(classify(Stage::Setup, errno(13)), ErrorKind::Io);
        assert!(is_interrupted(errno(4)));
        assert!(!is_interrupted(win(4)));
    }

    #[test]
    fn test_media_errors_are_told_apart_from_bad_requests() {
        assert!(is_media_error(errno(5)));
        assert!(is_media_error(errno(74)));
        assert!(!is_media_error(errno(22)));
        assert!(!is_media_error(errno(9)));
        let win = |code| OsError {
            code,
            source: OsErrorSource::Win32,
        };
        assert!(is_media_error(win(23)));
        assert!(is_media_error(win(1117)));
        assert!(!is_media_error(win(87)));
        let nt = |code: u32| OsError {
            code: code as i32,
            source: OsErrorSource::NtStatus,
        };
        assert!(is_media_error(nt(0xC000_009C)));
        assert!(!is_media_error(nt(0xC000_000D)));
    }
}
