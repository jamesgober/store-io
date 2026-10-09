//! Space: `fallocate` provisioning, hole punching, FIEMAP extent state and
//! `cachestat` page-cache residency.

use store_io_core::evidence::Tri;
use store_io_platform::{RangeState, RawResult, ReleaseHow};

use crate::file::PosixFile;
use crate::sys;

/// Upper bound on FIEMAP calls for one range: a hostile filesystem returning
/// one tiny extent per call cannot keep the walk going forever.
const FIEMAP_MAX_CALLS: u32 = 1 << 20;

/// Current size from `fstat`.
pub(crate) fn size(file: &PosixFile) -> RawResult<u64> {
    let st = sys::fstat(file.raw())?;
    u64::try_from(st.st_size).map_err(|_| sys::errno(libc::EINVAL))
}

/// Grows the file to `len` bytes with `fallocate` mode 0: real blocks are
/// allocated as unwritten extents (reading as zero, never another file's
/// data) and the size becomes `len`. The engine converts them to written
/// extents with direct writes. A file already at least `len` long is left as
/// it is; this never truncates. `KEEP_SIZE` is never used.
pub(crate) fn allocate(file: &PosixFile, len: u64) -> RawResult<()> {
    if !file.is_writable() {
        return Err(sys::errno(libc::EBADF));
    }
    let cur = size(file)?;
    if len <= cur {
        return Ok(());
    }
    let offset = libc::off_t::try_from(cur).map_err(|_| sys::errno(libc::EFBIG))?;
    let grow = libc::off_t::try_from(len - cur).map_err(|_| sys::errno(libc::EFBIG))?;
    sys::fallocate(file.raw(), 0, offset, grow)
}

/// Releases a range. `Deallocate` punches a hole (`PUNCH_HOLE|KEEP_SIZE`);
/// `TrimInPlace` is not available for Linux files and reports `EOPNOTSUPP`.
pub(crate) fn release(file: &PosixFile, offset: u64, len: u64, how: ReleaseHow) -> RawResult<()> {
    if !file.is_writable() {
        return Err(sys::errno(libc::EBADF));
    }
    match how {
        ReleaseHow::Deallocate => {
            let off = libc::off_t::try_from(offset).map_err(|_| sys::errno(libc::EFBIG))?;
            let l = libc::off_t::try_from(len).map_err(|_| sys::errno(libc::EFBIG))?;
            sys::fallocate(
                file.raw(),
                libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                off,
                l,
            )
        }
        ReleaseHow::TrimInPlace => Err(sys::errno(libc::EOPNOTSUPP)),
    }
}

/// Extent and page-cache state of `[offset, offset + len)`.
///
/// `unwritten` is `Yes` when any extent in the range is unwritten, delayed
/// or of unknown location, or when part of the range has no extent at all (a
/// hole, or past the end of the file): none of those bytes is written data
/// on media. `shared` is `Yes` when any extent is shared (reflink). Both are
/// `Unknown` where the filesystem has no extent map (`ENOTTY` /
/// `EOPNOTSUPP`). `cached_pages` is `None` where `cachestat(2)` is
/// unavailable or refused. `valid_data` is a Windows notion and stays
/// `Unknown`.
pub(crate) fn range_state(file: &PosixFile, offset: u64, len: u64) -> RawResult<RangeState> {
    let (unwritten, shared) = match extents(file, offset, len) {
        Ok(x) => x,
        Err(e) if e.code == libc::ENOTTY || e.code == libc::EOPNOTSUPP => {
            (Tri::Unknown, Tri::Unknown)
        }
        Err(e) => return Err(e),
    };
    let cached_pages = sys::cachestat(file.raw(), offset, len)
        .ok()
        .map(|c| c.nr_cache);
    Ok(RangeState {
        unwritten,
        shared,
        cached_pages,
        valid_data: Tri::Unknown,
    })
}

/// Walks the extents of a range with a fixed buffer.
fn extents(file: &PosixFile, offset: u64, len: u64) -> RawResult<(Tri, Tri)> {
    let end = offset.checked_add(len).ok_or(sys::errno(libc::EINVAL))?;
    if len == 0 {
        return Ok((Tri::No, Tri::No));
    }
    let mut unwritten = false;
    let mut shared = false;
    let mut covered_to = offset;
    let mut start = offset;
    let mut calls = 0u32;
    loop {
        calls += 1;
        if calls > FIEMAP_MAX_CALLS {
            return Ok((Tri::Unknown, Tri::Unknown));
        }
        let mut map = sys::FiemapBuf::new(start, end - start);
        sys::fiemap(file.raw(), &mut map)?;
        let mapped = map.mapped();
        if mapped.is_empty() {
            break;
        }
        let mut last = false;
        let mut next = start;
        for e in mapped {
            if e.fe_logical >= end {
                last = true;
                break;
            }
            let e_end = e.fe_logical.saturating_add(e.fe_length);
            if e.fe_logical > covered_to {
                // A gap before this extent: a hole inside the range.
                unwritten = true;
            }
            covered_to = covered_to.max(e_end);
            if e.fe_flags
                & (sys::FIEMAP_EXTENT_UNWRITTEN
                    | sys::FIEMAP_EXTENT_DELALLOC
                    | sys::FIEMAP_EXTENT_UNKNOWN)
                != 0
            {
                unwritten = true;
            }
            if e.fe_flags & sys::FIEMAP_EXTENT_SHARED != 0 {
                shared = true;
            }
            if e.fe_flags & sys::FIEMAP_EXTENT_LAST != 0 {
                last = true;
            }
            next = next.max(e_end);
        }
        if last || next <= start || next >= end {
            break;
        }
        start = next;
    }
    if covered_to < end {
        unwritten = true;
    }
    Ok((tri(unwritten), tri(shared)))
}

fn tri(b: bool) -> Tri {
    if b { Tri::Yes } else { Tri::No }
}
