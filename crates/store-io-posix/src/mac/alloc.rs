//! Space on macOS: `F_PREALLOCATE` provisioning, `F_PUNCHHOLE` release.
//! No extent map is read: a range's written state is unknown.

use store_io_core::evidence::Tri;
use store_io_platform::{RangeState, RawResult, ReleaseHow};

use crate::file::PosixFile;
use crate::sys;

/// Current size from `fstat`.
pub(crate) fn size(file: &PosixFile) -> RawResult<u64> {
    let st = sys::fstat(file.raw())?;
    u64::try_from(st.st_size).map_err(|_| sys::errno(libc::EINVAL))
}

/// Sets the file size to exactly `len` (`ftruncate`).
pub(crate) fn set_len(file: &PosixFile, len: u64) -> RawResult<()> {
    if !file.is_writable() {
        return Err(sys::errno(libc::EBADF));
    }
    let len = libc::off_t::try_from(len).map_err(|_| sys::errno(libc::EFBIG))?;
    sys::ftruncate(file.raw(), len)
}

/// Grows the file to `len` bytes: `F_PREALLOCATE` of the growth past the
/// physical end of file (the blocks the file holds, which a sparse tail
/// leaves short of its size), all or nothing (`F_ALLOCATEALL`), first
/// contiguous and, refused so, without `F_ALLOCATECONTIG`; then `ftruncate`
/// to `len`.
/// The new bytes read as zeros (macOS never exposes uninitialised blocks);
/// the engine fills them before a region is ready. A file already at least
/// `len` long is left as it is; this never truncates.
pub(crate) fn allocate(file: &PosixFile, len: u64) -> RawResult<()> {
    if !file.is_writable() {
        return Err(sys::errno(libc::EBADF));
    }
    let st = sys::fstat(file.raw())?;
    let cur = u64::try_from(st.st_size).map_err(|_| sys::errno(libc::EINVAL))?;
    if len <= cur {
        return Ok(());
    }
    let held = u64::try_from(st.st_blocks).unwrap_or(0).saturating_mul(512);
    let new_len = libc::off_t::try_from(len).map_err(|_| sys::errno(libc::EFBIG))?;
    if len > held {
        let grow = libc::off_t::try_from(len - held).map_err(|_| sys::errno(libc::EFBIG))?;
        if sys::preallocate(file.raw(), grow, true).is_err() {
            sys::preallocate(file.raw(), grow, false)?;
        }
    }
    sys::ftruncate(file.raw(), new_len)
}

/// Allocating a range inside the file without writing it: not available on
/// macOS (`F_PREALLOCATE` only extends past the end of file), `ENOTSUP`;
/// the engine writes a capped region's slack there instead. As the contract
/// asks, a range past the end is `EINVAL` and an empty one inside the file
/// or at its end does nothing.
pub(crate) fn allocate_range(file: &PosixFile, offset: u64, len: u64) -> RawResult<()> {
    if !file.is_writable() {
        return Err(sys::errno(libc::EBADF));
    }
    let end = offset
        .checked_add(len)
        .ok_or_else(|| sys::errno(libc::EINVAL))?;
    if end > size(file)? {
        return Err(sys::errno(libc::EINVAL));
    }
    if len == 0 {
        return Ok(());
    }
    Err(sys::errno(libc::ENOTSUP))
}

/// Releases a range. `Deallocate` punches a hole (`F_PUNCHHOLE`: the range
/// must be aligned to the file system's block); `TrimInPlace` is not
/// available and reports `EOPNOTSUPP`.
pub(crate) fn release(file: &PosixFile, offset: u64, len: u64, how: ReleaseHow) -> RawResult<()> {
    if !file.is_writable() {
        return Err(sys::errno(libc::EBADF));
    }
    match how {
        ReleaseHow::Deallocate => {
            let off = libc::off_t::try_from(offset).map_err(|_| sys::errno(libc::EFBIG))?;
            let l = libc::off_t::try_from(len).map_err(|_| sys::errno(libc::EFBIG))?;
            sys::punch_hole(file.raw(), off, l)
        }
        ReleaseHow::TrimInPlace => Err(sys::errno(libc::EOPNOTSUPP)),
    }
}

/// The state of `[offset, offset + len)`: nothing is known. macOS has no
/// unprivileged extent map, so whether the range is written or shared is
/// `Unknown`; page-cache residency is not reported (`None`: on macOS
/// 4 KiB-aligned writes stay cached by design, so it would decide nothing);
/// `valid_data` is a Windows notion.
pub(crate) fn range_state(_file: &PosixFile, _offset: u64, _len: u64) -> RawResult<RangeState> {
    Ok(RangeState {
        unwritten: Tri::Unknown,
        shared: Tri::Unknown,
        cached_pages: None,
        valid_data: Tri::Unknown,
    })
}
