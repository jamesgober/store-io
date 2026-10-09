//! Space: allocation and end-of-file, size, range release, range state.
//!
//! Allocation never calls `SetFileValidData` (it needs a privilege and
//! exposes stale clusters); the engine fills sequentially afterwards, which
//! moves the valid data length with the writes.

use store_io_core::evidence::Tri;
use store_io_platform::{RangeState, RawResult, ReleaseHow};
use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED};
use windows_sys::Win32::Storage::FileSystem::{FileAllocationInfo, FileEndOfFileInfo};
use windows_sys::Win32::System::Ioctl::{
    FILE_REGION_USAGE_VALID_CACHED_DATA, FILE_REGION_USAGE_VALID_NONCACHED_DATA,
    FSCTL_FILE_LEVEL_TRIM, FSCTL_QUERY_FILE_REGIONS, FSCTL_SET_SPARSE, FSCTL_SET_ZERO_DATA,
};

use crate::bytes::Reader;
use crate::file::WinFile;
use crate::sys;

/// Region usage flags that mean "below the valid data length".
const VALID_USAGE: u32 =
    FILE_REGION_USAGE_VALID_CACHED_DATA | FILE_REGION_USAGE_VALID_NONCACHED_DATA;

/// Allocates `len` bytes of real clusters and sets the end of file to `len`.
pub(crate) fn allocate(file: &WinFile, len: u64) -> RawResult<()> {
    let Ok(signed) = i64::try_from(len) else {
        return Err(sys::win32(ERROR_INVALID_PARAMETER));
    };
    let rec = signed.to_le_bytes();
    sys::set_file_info(file.handle(), FileAllocationInfo, &rec)?;
    sys::set_file_info(file.handle(), FileEndOfFileInfo, &rec)
}

/// Sets the end of file to exactly `len` (allocation follows NTFS rules).
pub(crate) fn set_len(file: &WinFile, len: u64) -> RawResult<()> {
    let Ok(signed) = i64::try_from(len) else {
        return Err(sys::win32(ERROR_INVALID_PARAMETER));
    };
    sys::set_file_info(file.handle(), FileEndOfFileInfo, &signed.to_le_bytes())
}

/// Current end of file.
pub(crate) fn size(file: &WinFile) -> RawResult<u64> {
    sys::file_size(file.handle())
}

/// Releases `[offset, offset + len)`.
pub(crate) fn release_range(
    file: &WinFile,
    offset: u64,
    len: u64,
    how: ReleaseHow,
) -> RawResult<()> {
    let (Ok(start), Some(end_u)) = (i64::try_from(offset), offset.checked_add(len)) else {
        return Err(sys::win32(ERROR_INVALID_PARAMETER));
    };
    let Ok(end) = i64::try_from(end_u) else {
        return Err(sys::win32(ERROR_INVALID_PARAMETER));
    };
    let mut out = [0u8; 16];
    match how {
        ReleaseHow::Deallocate => {
            // FILE_SET_SPARSE_BUFFER { SetSparse: TRUE } then
            // FILE_ZERO_DATA_INFORMATION { FileOffset, BeyondFinalZero }.
            let _bytes = sys::ioctl(file.handle(), FSCTL_SET_SPARSE, &[1u8], &mut out)?;
            let mut zero = [0u8; 16];
            zero[..8].copy_from_slice(&start.to_le_bytes());
            zero[8..].copy_from_slice(&end.to_le_bytes());
            let _bytes = sys::ioctl(file.handle(), FSCTL_SET_ZERO_DATA, &zero, &mut out)?;
            Ok(())
        }
        ReleaseHow::TrimInPlace => {
            // FILE_LEVEL_TRIM { Key: 0, NumRanges: 1, Ranges: [{ Offset, Length }] }.
            let mut trim = [0u8; 24];
            trim[4..8].copy_from_slice(&1u32.to_le_bytes());
            trim[8..16].copy_from_slice(&offset.to_le_bytes());
            trim[16..24].copy_from_slice(&len.to_le_bytes());
            let n = sys::ioctl(file.handle(), FSCTL_FILE_LEVEL_TRIM, &trim, &mut out)?;
            // FILE_LEVEL_TRIM_OUTPUT { NumRangesProcessed }.
            match Reader::new(&out[..n.min(out.len())]).u32(0) {
                Some(processed) if processed >= 1 => Ok(()),
                _ => Err(sys::win32(ERROR_NOT_SUPPORTED)),
            }
        }
    }
}

/// Extent and cache state of `[offset, offset + len)`.
///
/// - `valid_data`: whether the whole range lies below the valid data length,
///   from `FSCTL_QUERY_FILE_REGIONS`. NTFS answers only a
///   `VALID_CACHED_DATA` query (`VALID_NONCACHED_DATA` is rejected with
///   `ERROR_INVALID_PARAMETER` on Windows 11 26200 [measured]) and reports
///   the valid data length as the cache manager sees it, which is the
///   on-disk length once the file has been flushed; this backend only ever
///   writes unbuffered, so the two never differ for its own writes.
///   `Unknown` when the filesystem rejects the query or truncates the answer.
/// - `cached_pages`: `Some(0)`: every handle this backend opens bypasses
///   the cache, so nothing it wrote is cached (foreign buffered openers are
///   outside this handle's knowledge).
/// - `unwritten`, `shared`: `Unknown` (no unprivileged NTFS query exists).
pub(crate) fn range_state(file: &WinFile, offset: u64, len: u64) -> RawResult<RangeState> {
    let (Ok(start), Some(end_u)) = (i64::try_from(offset), offset.checked_add(len)) else {
        return Err(sys::win32(ERROR_INVALID_PARAMETER));
    };
    let Ok(signed_len) = i64::try_from(len) else {
        return Err(sys::win32(ERROR_INVALID_PARAMETER));
    };
    // FILE_REGION_INPUT { FileOffset, Length, DesiredUsage }.
    let mut input = [0u8; 24];
    input[..8].copy_from_slice(&start.to_le_bytes());
    input[8..16].copy_from_slice(&signed_len.to_le_bytes());
    input[16..20].copy_from_slice(&FILE_REGION_USAGE_VALID_CACHED_DATA.to_le_bytes());
    let mut out = vec![0u8; 16 + 24 * 64];
    let valid_data = match sys::ioctl(file.handle(), FSCTL_QUERY_FILE_REGIONS, &input, &mut out) {
        Ok(n) => valid_covers(&out[..n.min(out.len())], offset, end_u),
        Err(_) => Tri::Unknown,
    };
    Ok(RangeState {
        unwritten: Tri::Unknown,
        shared: Tri::Unknown,
        cached_pages: Some(0),
        valid_data,
    })
}

/// Whether the regions in a `FILE_REGION_OUTPUT` buffer cover
/// `[start, end)` with valid data (either usage flag).
pub(crate) fn valid_covers(buf: &[u8], start: u64, end: u64) -> Tri {
    let r = Reader::new(buf);
    let (Some(total), Some(count)) = (r.u32(4), r.u32(8)) else {
        return Tri::Unknown;
    };
    if total != count {
        // Truncated answer: do not guess.
        return Tri::Unknown;
    }
    if start >= end {
        return Tri::Yes;
    }
    // Regions are returned in file order; walk them as a coverage frontier.
    let mut frontier = start;
    for i in 0..count as usize {
        let base = 16usize.saturating_add(i.saturating_mul(24));
        let (Some(off), Some(len), Some(usage)) = (r.i64(base), r.i64(base + 8), r.u32(base + 16))
        else {
            return Tri::Unknown;
        };
        if off < 0 || len < 0 || usage & VALID_USAGE == 0 {
            continue;
        }
        let (off, len) = (off as u64, len as u64);
        let Some(region_end) = off.checked_add(len) else {
            return Tri::Unknown;
        };
        if off > frontier {
            return Tri::No;
        }
        if region_end > frontier {
            frontier = region_end;
        }
        if frontier >= end {
            return Tri::Yes;
        }
    }
    Tri::No
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regions(entries: &[(i64, i64, u32)], total: u32) -> Vec<u8> {
        let mut b = vec![0u8; 16];
        b[4..8].copy_from_slice(&total.to_le_bytes());
        b[8..12].copy_from_slice(&(entries.len() as u32).to_le_bytes());
        for (off, len, usage) in entries {
            b.extend_from_slice(&off.to_le_bytes());
            b.extend_from_slice(&len.to_le_bytes());
            b.extend_from_slice(&usage.to_le_bytes());
            b.extend_from_slice(&0u32.to_le_bytes());
        }
        b
    }

    #[test]
    fn test_valid_covers_full_partial_and_gap() {
        let v = FILE_REGION_USAGE_VALID_CACHED_DATA;
        assert_eq!(
            valid_covers(&regions(&[(0, 8192, v)], 1), 0, 8192),
            Tri::Yes
        );
        let nc = FILE_REGION_USAGE_VALID_NONCACHED_DATA;
        assert_eq!(
            valid_covers(&regions(&[(0, 8192, nc)], 1), 0, 8192),
            Tri::Yes
        );
        assert_eq!(valid_covers(&regions(&[(0, 4096, v)], 1), 0, 8192), Tri::No);
        assert_eq!(
            valid_covers(&regions(&[(0, 4096, v), (4096, 4096, v)], 2), 0, 8192),
            Tri::Yes
        );
        assert_eq!(
            valid_covers(&regions(&[(0, 4096, v), (8192, 4096, v)], 2), 0, 12288),
            Tri::No
        );
        assert_eq!(
            valid_covers(&regions(&[(4096, 4096, v)], 1), 0, 8192),
            Tri::No
        );
        assert_eq!(valid_covers(&regions(&[(0, 8192, 0)], 1), 0, 8192), Tri::No);
        assert_eq!(valid_covers(&regions(&[], 0), 0, 4096), Tri::No);
        assert_eq!(valid_covers(&regions(&[], 0), 4096, 4096), Tri::Yes);
    }

    #[test]
    fn test_valid_covers_truncated_and_garbage_are_unknown() {
        let v = FILE_REGION_USAGE_VALID_CACHED_DATA;
        assert_eq!(valid_covers(&[], 0, 1), Tri::Unknown);
        assert_eq!(valid_covers(&[0u8; 8], 0, 1), Tri::Unknown);
        // Header says more regions exist than were returned.
        assert_eq!(
            valid_covers(&regions(&[(0, 8192, v)], 2), 0, 8192),
            Tri::Unknown
        );
        // Count claims an entry the buffer does not contain.
        let mut short = regions(&[(0, 8192, v)], 1);
        short.truncate(20);
        assert_eq!(valid_covers(&short, 0, 8192), Tri::Unknown);
        // Negative offsets are skipped; huge extents cannot overflow the
        // frontier arithmetic.
        assert_eq!(
            valid_covers(&regions(&[(-1, 8192, v)], 1), 0, 8192),
            Tri::No
        );
        assert_eq!(valid_covers(&regions(&[(0, -5, v)], 1), 0, 8192), Tri::No);
        assert_eq!(
            valid_covers(&regions(&[(0, i64::MAX, v), (1, i64::MAX, v)], 2), 0, 8192),
            Tri::Yes
        );
        let huge = regions(&[(i64::MAX, i64::MAX, v)], 1);
        assert_eq!(valid_covers(&huge, u64::MAX - 1, u64::MAX), Tri::No);
    }
}
