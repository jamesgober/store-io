//! The macOS evidence probe (first, conservative form): the volume from
//! `fstatfs`, the release, the hypervisor flag, and the flush check on a
//! writable container and its directory. No device evidence yet (internal
//! or external, interconnect, model): every device fact is reported missing,
//! so a store is Unverified (`DeviceNotInternal`) until the device probe
//! lands. The probe never decides a class and never writes a file.

use std::os::fd::RawFd;

use store_io_core::class::Missing;
use store_io_core::error::OsError;
use store_io_core::evidence::{Evidence, FsKind, Hypervisor, PlatformKind, Tri};
use store_io_platform::RawResult;

use crate::dir::PosixDir;
use crate::file::PosixFile;
use crate::sys;

/// Gathers the evidence for `file` in `dir`.
///
/// The flush check, on a writable file of a volume not asked yet (a reader
/// never flushes): one `F_FULLFSYNC` on the file, then one on the
/// directory. Both accepted: the volume has accepted it. Either refused as
/// unsupported ([`sys::full_fsync_refused`]): the volume has refused it,
/// and every flush on it is `fsync` from now on, so a store opened despite
/// that (Unsafe, `FullFlushUnsupported`, refused unless overridden) never
/// meets the refusal again; on a volume that has accepted it, a refusal is
/// a failed flush (see [`FlushState`](crate::dir::FlushState)). Any other error is the probe's
/// failure: a flush that failed. `full_flush` and `dir_flush` report the
/// volume's state: `Yes` accepted, `No` refused, `Unknown` not asked (a
/// reader on a volume no writer has checked).
pub(crate) fn probe(dir: &PosixDir, file: &PosixFile) -> RawResult<Evidence> {
    let mut ev = Evidence::unknown(PlatformKind::MacOs);
    ev.kernel = sys::os_release();
    for m in [
        Missing::Identify,
        Missing::Smart,
        Missing::CachePresence,
        Missing::CacheEnabled,
        Missing::AtomicFields,
        Missing::OsCacheMode,
        Missing::Stack,
        Missing::PageCache,
        Missing::DioAlignment,
    ] {
        ev.missing.insert(m);
    }
    let volume = sys::volume(file.raw())?;
    ev.fs.kind = fs_kind(&volume);
    ev.fs.cow = match ev.fs.kind {
        // Copy-on-write metadata, clones and snapshots: an overwrite may
        // allocate.
        FsKind::Apfs => Tri::Yes,
        FsKind::Hfs => Tri::No,
        _ => Tri::Unknown,
    };
    // `F_NOCACHE` is a hint (uncached only at the kernel page's alignment),
    // and durability never rests on it: no direct I/O claim either way.
    ev.fs.direct_io = Tri::Unknown;
    let state = file.flush_state();
    if file.is_writable() && state.full() {
        let refusal = match flush_check(file.raw())? {
            None => flush_check(dir.raw())?,
            refused => refused,
        };
        match refusal {
            None => state.accept(),
            Some(e) if !state.refuse() => return Err(e),
            Some(_) => {}
        }
    }
    let full = state.answer();
    ev.fs.full_flush = full;
    ev.fs.dir_flush = full;
    if sys::under_hypervisor() == Some(true) {
        ev.hypervisor = Some(Hypervisor::Other);
    }
    Ok(ev)
}

/// One `F_FULLFSYNC` of `fd`: `None` accepted, the error if refused as
/// unsupported; any other error is the flush's failure.
fn flush_check(fd: RawFd) -> RawResult<Option<OsError>> {
    match sys::full_fsync(fd) {
        Ok(()) => Ok(None),
        Err(e) if sys::full_fsync_refused(e.code) => Ok(Some(e)),
        Err(e) => Err(e),
    }
}

/// The file system from the volume: APFS, HFS+, FAT or exFAT, FUSE (macFUSE,
/// older osxfuse), a network volume (not `MNT_LOCAL`), or another.
fn fs_kind(v: &sys::Volume) -> FsKind {
    match v.fs.as_str() {
        "apfs" => FsKind::Apfs,
        "hfs" => FsKind::Hfs,
        "msdos" | "exfat" => FsKind::Fat,
        "macfuse" | "osxfuse" | "fusefs" => FsKind::Fuse,
        _ if !v.local => FsKind::Network,
        _ => FsKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_volume_names_map_to_their_kinds() {
        let kind = |fs: &str, local| {
            fs_kind(&sys::Volume {
                fs: fs.to_owned(),
                local,
                from: String::new(),
            })
        };
        assert_eq!(kind("apfs", true), FsKind::Apfs);
        assert_eq!(kind("hfs", true), FsKind::Hfs);
        assert_eq!(kind("msdos", true), FsKind::Fat);
        assert_eq!(kind("exfat", true), FsKind::Fat);
        assert_eq!(kind("macfuse", true), FsKind::Fuse);
        assert_eq!(kind("osxfuse", true), FsKind::Fuse);
        assert_eq!(kind("fusefs", true), FsKind::Fuse);
        assert_eq!(kind("smbfs", false), FsKind::Network);
        assert_eq!(kind("nfs", false), FsKind::Network);
        assert_eq!(kind("udf", true), FsKind::Other);
    }
}
