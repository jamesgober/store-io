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
        OsErrorSource::Errno => errno_no_space(raw.code),
        // ERROR_HANDLE_DISK_FULL = 39, ERROR_DISK_FULL = 112,
        // ERROR_DISK_QUOTA_EXCEEDED = 1295.
        OsErrorSource::Win32 => matches!(raw.code, 39 | 112 | 1295),
        // STATUS_DISK_FULL = 0xC000007F, STATUS_QUOTA_EXCEEDED = 0xC0000044.
        OsErrorSource::NtStatus => matches!(raw.code as u32, 0xC000_007F | 0xC000_0044),
        OsErrorSource::Sim => raw.code == 28,
    }
}

/// Whether the code means the file to create already exists.
#[must_use]
pub fn is_already_exists(raw: OsError) -> bool {
    match raw.source {
        // EEXIST.
        OsErrorSource::Errno | OsErrorSource::Sim => raw.code == 17,
        // ERROR_FILE_EXISTS = 80, ERROR_ALREADY_EXISTS = 183.
        OsErrorSource::Win32 => matches!(raw.code, 80 | 183),
        // STATUS_OBJECT_NAME_COLLISION.
        OsErrorSource::NtStatus => raw.code as u32 == 0xC000_0035,
    }
}

/// Whether the code means the named file or directory does not exist.
#[must_use]
pub fn is_not_found(raw: OsError) -> bool {
    match raw.source {
        // ENOENT.
        OsErrorSource::Errno | OsErrorSource::Sim => raw.code == 2,
        // ERROR_FILE_NOT_FOUND = 2, ERROR_PATH_NOT_FOUND = 3.
        OsErrorSource::Win32 => matches!(raw.code, 2 | 3),
        // STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND.
        OsErrorSource::NtStatus => matches!(raw.code as u32, 0xC000_0034 | 0xC000_003A),
    }
}

/// Whether a failed read means the device could not read the range (a
/// latent sector error, a checksum failure, a device I/O error), as opposed
/// to a bad request or a lost handle. A scan reports such ranges as
/// unreadable and continues past them.
#[must_use]
pub fn is_media_error(raw: OsError) -> bool {
    match raw.source {
        OsErrorSource::Errno => errno_media(raw.code),
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

/// Whether the code means the object does not support the operation at all
/// (for example a directory flush on a file system that cannot do one), as
/// opposed to a failure while doing it.
///
/// Only meaningful for the result of a flush or sync call: `EINVAL` is also
/// what a misaligned or malformed request returns, so no other call may use
/// this to treat an error as "unsupported, skip". Errno numbers differ
/// between operating systems, so each is matched against its own.
#[must_use]
pub fn is_unsupported(raw: OsError) -> bool {
    match raw.source {
        OsErrorSource::Errno => errno_unsupported(raw.code),
        // ERROR_INVALID_FUNCTION = 1, ERROR_NOT_SUPPORTED = 50.
        OsErrorSource::Win32 => matches!(raw.code, 1 | 50),
        // STATUS_INVALID_DEVICE_REQUEST, STATUS_NOT_SUPPORTED.
        OsErrorSource::NtStatus => matches!(raw.code as u32, 0xC000_0010 | 0xC000_00BB),
        OsErrorSource::Sim => raw.code == 95,
    }
}

// Errno numbers differ between systems (and between Linux architectures:
// these are the asm-generic numbers of x86, Arm and RISC-V), so each table
// is compiled for its own system. Elsewhere only the numbers every Unix
// shares are matched, and anything else counts as a plain failure.

/// Linux: ENOSPC = 28, EDQUOT = 122.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn errno_no_space(code: i32) -> bool {
    matches!(code, 28 | 122)
}

/// macOS: ENOSPC = 28, EDQUOT = 69.
#[cfg(target_vendor = "apple")]
fn errno_no_space(code: i32) -> bool {
    matches!(code, 28 | 69)
}

/// Elsewhere: ENOSPC = 28.
#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn errno_no_space(code: i32) -> bool {
    code == 28
}

/// Linux: EIO = 5, ENODATA = 61, EBADMSG = 74 (checksum failures on some
/// file systems), EUCLEAN = 117 (structure needs cleaning).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn errno_media(code: i32) -> bool {
    matches!(code, 5 | 61 | 74 | 117)
}

/// macOS: EIO = 5, EBADMSG = 94. (61, 74 and 97 are ECONNREFUSED,
/// EPROGUNAVAIL and ENOLINK there; 117 does not exist.)
#[cfg(target_vendor = "apple")]
fn errno_media(code: i32) -> bool {
    matches!(code, 5 | 94)
}

/// Elsewhere: EIO = 5.
#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn errno_media(code: i32) -> bool {
    code == 5
}

/// Linux: EINVAL = 22 (fsync on an object without synchronisation), ENOSYS =
/// 38, EOPNOTSUPP / ENOTSUP = 95.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn errno_unsupported(code: i32) -> bool {
    matches!(code, 22 | 38 | 95)
}

/// Elsewhere: nothing counts as unsupported (every error is a failure).
#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn errno_unsupported(_code: i32) -> bool {
    false
}

/// macOS: EINVAL = 22, ENOTSUP = 45, ENOSYS = 78, EOPNOTSUPP = 102.
#[cfg(target_vendor = "apple")]
fn errno_unsupported(code: i32) -> bool {
    matches!(code, 22 | 45 | 78 | 102)
}

/// Whether a read failed because it started at or past the end of the file:
/// Windows reports `ERROR_HANDLE_EOF` / `STATUS_END_OF_FILE`, where POSIX
/// returns a short (zero-byte) read instead. Not a media error and not a
/// device fault: the bytes simply do not exist.
#[must_use]
pub fn is_end_of_file(raw: OsError) -> bool {
    match raw.source {
        // ERROR_HANDLE_EOF = 38.
        OsErrorSource::Win32 => raw.code == 38,
        // STATUS_END_OF_FILE.
        OsErrorSource::NtStatus => raw.code as u32 == 0xC000_0011,
        OsErrorSource::Errno | OsErrorSource::Sim => false,
    }
}

/// Whether a non-blocking lock attempt failed because another holder has
/// the lock (as opposed to the file system being unable to lock at all,
/// say `ENOLCK` or `EOPNOTSUPP`, which is a plain failure).
#[must_use]
pub fn is_lock_held(raw: OsError) -> bool {
    match raw.source {
        OsErrorSource::Errno => errno_lock_held(raw.code),
        // ERROR_LOCK_VIOLATION = 33.
        OsErrorSource::Win32 => raw.code == 33,
        // STATUS_LOCK_NOT_GRANTED.
        OsErrorSource::NtStatus => raw.code as u32 == 0xC000_0055,
        // The simulator's EWOULDBLOCK.
        OsErrorSource::Sim => raw.code == 11,
    }
}

/// Linux: EAGAIN (= EWOULDBLOCK) = 11, EACCES = 13.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn errno_lock_held(code: i32) -> bool {
    matches!(code, 11 | 13)
}

/// macOS: EAGAIN (= EWOULDBLOCK) = 35, EACCES = 13.
#[cfg(target_vendor = "apple")]
fn errno_lock_held(code: i32) -> bool {
    matches!(code, 35 | 13)
}

/// Elsewhere: EACCES = 13.
#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn errno_lock_held(code: i32) -> bool {
    code == 13
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
        if cfg!(target_vendor = "apple") {
            assert_eq!(classify(Stage::Space, errno(69)), ErrorKind::NoSpace);
            assert_eq!(classify(Stage::Space, errno(122)), ErrorKind::Io);
        } else if cfg!(any(target_os = "linux", target_os = "android")) {
            assert_eq!(classify(Stage::Space, errno(122)), ErrorKind::NoSpace);
            assert_eq!(
                classify(Stage::Space, errno(69)),
                ErrorKind::Io,
                "ESRMNT on Linux"
            );
        }
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
    fn test_already_exists_is_judged_per_numbering() {
        assert!(is_already_exists(errno(17)));
        assert!(!is_already_exists(errno(80)), "errno 80 is not EEXIST");
        let win = |code| OsError {
            code,
            source: OsErrorSource::Win32,
        };
        assert!(is_already_exists(win(80)) && is_already_exists(win(183)));
        assert!(!is_already_exists(win(17)));
    }

    #[test]
    fn test_not_found_is_judged_per_numbering() {
        assert!(is_not_found(errno(2)));
        assert!(!is_not_found(errno(3)), "ESRCH is not ENOENT");
        let win = |code| OsError {
            code,
            source: OsErrorSource::Win32,
        };
        assert!(is_not_found(win(2)) && is_not_found(win(3)));
        assert!(!is_not_found(win(5)));
    }

    #[test]
    fn test_unsupported_is_judged_per_operating_system() {
        let errno = |code| OsError {
            code,
            source: OsErrorSource::Errno,
        };
        if cfg!(any(
            target_os = "linux",
            target_os = "android",
            target_vendor = "apple"
        )) {
            assert!(is_unsupported(errno(22)));
        }
        assert!(
            !is_unsupported(errno(5)),
            "EIO is a failure, not unsupported"
        );
        assert!(
            !is_unsupported(errno(28)),
            "ENOSPC is a failure, not unsupported"
        );
        if cfg!(target_vendor = "apple") {
            assert!(is_unsupported(errno(45)) && is_unsupported(errno(78)));
            assert!(!is_unsupported(errno(38)), "ENOTSOCK on macOS");
        } else if cfg!(any(target_os = "linux", target_os = "android")) {
            assert!(is_unsupported(errno(38)) && is_unsupported(errno(95)));
            assert!(!is_unsupported(errno(45)), "EL2NSYNC on Linux");
            assert!(!is_unsupported(errno(102)), "ENETRESET on Linux");
        }
        let win = |code| OsError {
            code,
            source: OsErrorSource::Win32,
        };
        assert!(is_unsupported(win(1)) && is_unsupported(win(50)));
        assert!(!is_unsupported(win(5)));
    }

    #[test]
    fn test_end_of_file_is_only_the_windows_eof_codes() {
        assert!(is_end_of_file(OsError {
            code: 38,
            source: OsErrorSource::Win32
        }));
        assert!(is_end_of_file(OsError {
            code: 0xC000_0011_u32 as i32,
            source: OsErrorSource::NtStatus
        }));
        assert!(!is_end_of_file(OsError {
            code: 38,
            source: OsErrorSource::Errno
        }));
        assert!(!is_end_of_file(OsError {
            code: 23,
            source: OsErrorSource::Win32
        }));
    }

    #[test]
    fn test_media_errors_are_told_apart_from_bad_requests() {
        assert!(is_media_error(errno(5)));
        if cfg!(target_vendor = "apple") {
            assert!(is_media_error(errno(94)));
            for code in [61, 74, 97, 117] {
                assert!(
                    !is_media_error(errno(code)),
                    "{code} is not a media error on macOS"
                );
            }
        } else if cfg!(any(target_os = "linux", target_os = "android")) {
            for code in [61, 74, 117] {
                assert!(is_media_error(errno(code)));
            }
            assert!(!is_media_error(errno(94)), "ESOCKTNOSUPPORT on Linux");
            assert!(!is_media_error(errno(97)), "EAFNOSUPPORT on Linux");
        }
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

    #[test]
    fn test_a_held_lock_is_told_from_a_file_system_that_cannot_lock() {
        // EACCES everywhere; EAGAIN by each system's own number.
        assert!(is_lock_held(errno(13)));
        if cfg!(any(target_os = "linux", target_os = "android")) {
            assert!(is_lock_held(errno(11)));
            // ENOLCK and EOPNOTSUPP are failures, not a held lock.
            assert!(!is_lock_held(errno(37)));
            assert!(!is_lock_held(errno(95)));
        }
        if cfg!(target_vendor = "apple") {
            assert!(is_lock_held(errno(35)));
            assert!(!is_lock_held(errno(77)));
            assert!(!is_lock_held(errno(102)));
        }
        assert!(is_lock_held(OsError {
            code: 33,
            source: OsErrorSource::Win32,
        }));
        assert!(!is_lock_held(OsError {
            code: 1,
            source: OsErrorSource::Win32,
        }));
    }
}
