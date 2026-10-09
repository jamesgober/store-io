//! Gathers [`Evidence`] about a file: filesystem, mount, inode, stack,
//! device and hypervisor. Fills facts in; never decides a class, never writes
//! a file (the direct-I/O canary is the engine's job through `range_state`).
//!
//! Every value read from sysfs, procfs or a device is bounds-checked and
//! treated as hostile: a missing or unparsable source leaves the fact
//! unknown and lists it in `missing`.

pub(crate) mod block;
pub(crate) mod fs;
pub(crate) mod virt;

use std::path::Path;

use store_io_core::class::Missing;
use store_io_core::evidence::{Evidence, FsKind, PlatformKind, Tri};
use store_io_platform::RawResult;

use crate::dir::PosixDir;
use crate::file::PosixFile;
use crate::sys;

/// Largest `/proc/self/mountinfo` the probe will read.
const MOUNTINFO_MAX: usize = 8 << 20;

/// Probes `file`, using `dir` only to locate the mount in `mountinfo`.
pub(crate) fn probe(dir: &PosixDir, file: &PosixFile) -> RawResult<Evidence> {
    let fd = file.raw();
    // If even fstat fails nothing can be learned: report the raw error.
    let st = sys::fstat(fd)?;
    let mut ev = Evidence::unknown(PlatformKind::Linux);

    let release = sys::kernel_release();
    ev.kernel = release.as_deref().and_then(virt::kernel_version);
    ev.hypervisor = virt::detect(release.as_deref());

    let sx = sys::statx(fd, sys::STATX_PROBE_MASK).ok();
    let bdev = file.is_block_device();
    let dev = device_number(&st, sx.as_ref(), bdev);

    if bdev {
        ev.fs.kind = FsKind::Raw;
        ev.fs.cow = Tri::No;
        ev.fs.full_flush = Tri::Yes;
        ev.fs.dir_flush = Tri::Yes;
    } else {
        filesystem(dir, fd, dev, sx.as_ref(), &mut ev);
    }

    match sx {
        Some(sx) => {
            let d = fs::dio_facts(&sx);
            if d.reported {
                ev.fs.dio_mem_align = d.mem_align;
                ev.fs.dio_offset_align = d.offset_align;
                if d.offset_align == 0 {
                    // The filesystem says direct I/O on this file would be
                    // served through the page cache (ext4 +j, verity, inline).
                    ev.fs.direct_io = Tri::No;
                }
            } else {
                ev.missing.insert(Missing::DioAlignment);
            }
            if d.atomic_reported {
                ev.fs.os_atomic_unit = d.atomic_unit;
            }
            if d.transformed {
                ev.fs.transformed = true;
            }
        }
        None => ev.missing.insert(Missing::DioAlignment),
    }

    stack_and_device(dev, &mut ev);

    let len = u64::try_from(st.st_size).unwrap_or(0).max(1);
    if sys::cachestat(fd, 0, len).is_err() {
        ev.missing.insert(Missing::PageCache);
    }
    Ok(ev)
}

/// The device the file lives on: for a block device its own number, else
/// the filesystem's. statx is preferred; `st_dev` is decoded otherwise.
fn device_number(st: &libc::stat, sx: Option<&sys::Statx>, bdev: bool) -> (u32, u32) {
    match sx {
        Some(sx) if bdev => (sx.stx_rdev_major, sx.stx_rdev_minor),
        Some(sx) => (sx.stx_dev_major, sx.stx_dev_minor),
        None if bdev => split_dev(st.st_rdev),
        None => split_dev(st.st_dev),
    }
}

/// Splits a glibc `dev_t` into `(major, minor)`.
pub(crate) fn split_dev(dev: u64) -> (u32, u32) {
    let major = ((dev >> 32) & 0xFFFF_F000) | ((dev >> 8) & 0xFFF);
    let minor = ((dev >> 12) & 0xFFFF_FF00) | (dev & 0xFF);
    (major as u32, minor as u32)
}

/// Filesystem kind, mount options, ext4 effective options and inode flags.
fn filesystem(
    dir: &PosixDir,
    fd: std::os::fd::RawFd,
    dev: (u32, u32),
    sx: Option<&sys::Statx>,
    ev: &mut Evidence,
) {
    let magic = sys::fstatfs(fd)
        .ok()
        .map(|s| (s.f_type as u64) & 0xFFFF_FFFF);
    let mnt_id = sx
        .filter(|s| s.stx_mask & libc::STATX_MNT_ID != 0)
        .map(|s| s.stx_mnt_id);
    let dir_path = std::fs::read_link(format!("/proc/self/fd/{}", dir.raw()))
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    let entry = block::read_bounded(Path::new("/proc/self/mountinfo"), MOUNTINFO_MAX)
        .and_then(|text| fs::find_mount(&text, mnt_id, Some(dev), dir_path.as_deref()));

    let kind = fs::fs_kind(
        magic.unwrap_or(0),
        entry.as_ref().map(|e| e.fstype.as_str()),
    );
    ev.fs.kind = kind;

    let mut m = match &entry {
        Some(e) => fs::mount_facts(&e.fstype, &e.options, &e.super_options),
        None => {
            ev.missing.insert(Missing::MountOptions);
            fs::MountFacts::default()
        }
    };
    if kind == FsKind::Ext4 {
        let devname = block::read_sysfs(
            &Path::new(&format!("/sys/dev/block/{}:{}", dev.0, dev.1)).join("uevent"),
        )
        .and_then(|u| fs::uevent_devname(&u))
        .or_else(|| {
            entry
                .as_ref()
                .and_then(|e| fs::basename(&e.source).map(str::to_owned))
        });
        match devname.and_then(|n| {
            block::read_bounded(Path::new(&format!("/proc/fs/ext4/{n}/options")), 1 << 16)
        }) {
            Some(text) => m.merge(&fs::ext4_options_facts(&text)),
            None => ev.missing.insert(Missing::MountOptions),
        }
    }

    let inode = match sys::inode_flags(fd) {
        Ok(flags) => fs::inode_facts(flags),
        Err(_) => {
            ev.missing.insert(Missing::InodeFlags);
            fs::InodeFacts::default()
        }
    };

    ev.fs.data_journal = m.data_journal || inode.data_journal;
    ev.fs.no_barrier = m.no_barrier;
    ev.fs.weak_error_mode = m.weak_error_mode;
    ev.fs.sync_disabled = m.sync_disabled;
    ev.fs.transformed = m.compressed || inode.transformed;
    ev.fs.cow = match kind {
        FsKind::Btrfs | FsKind::Zfs | FsKind::Bcachefs => Tri::Yes,
        FsKind::Ext4 | FsKind::Xfs | FsKind::F2fs | FsKind::Fat | FsKind::Tmpfs | FsKind::Ntfs3 => {
            Tri::No
        }
        _ => Tri::Unknown,
    };
    let local = matches!(
        kind,
        FsKind::Ext4
            | FsKind::Xfs
            | FsKind::Btrfs
            | FsKind::Zfs
            | FsKind::Bcachefs
            | FsKind::F2fs
            | FsKind::Ntfs3
            | FsKind::Tmpfs
            | FsKind::Fat
    );
    if local {
        ev.fs.full_flush = Tri::Yes;
        ev.fs.dir_flush = Tri::Yes;
    }
}

/// The sysfs stack walk, the leaf's queue attributes and NVMe Identify.
fn stack_and_device(dev: (u32, u32), ev: &mut Evidence) {
    let walk = if dev.0 == 0 {
        // Anonymous device: btrfs, overlay, network or FUSE. No block node to
        // walk from (btrfs's own device list is not read yet).
        None
    } else {
        block::walk(dev.0, dev.1)
    };
    let Some(w) = walk else {
        for m in [
            Missing::Stack,
            Missing::OsCacheMode,
            Missing::Identify,
            Missing::Smart,
            Missing::CachePresence,
            Missing::CacheEnabled,
            Missing::AtomicFields,
        ] {
            ev.missing.insert(m);
        }
        return;
    };
    ev.stack = w.layers;
    if w.uncertain {
        ev.missing.insert(Missing::Stack);
    }
    let Some(leaf) = w.leaf else {
        for m in [
            Missing::Stack,
            Missing::OsCacheMode,
            Missing::Identify,
            Missing::Smart,
            Missing::CachePresence,
            Missing::CacheEnabled,
            Missing::AtomicFields,
        ] {
            ev.missing.insert(m);
        }
        return;
    };

    let leaf = block::leaf_facts(&leaf);
    let d = &mut ev.device;
    d.os_cache_mode = leaf.cache_mode;
    if !leaf.cache_mode_read {
        ev.missing.insert(Missing::OsCacheMode);
    }
    d.os_flush_supported = match leaf.cache_mode {
        store_io_core::evidence::OsCacheMode::WriteBack => Tri::Yes,
        store_io_core::evidence::OsCacheMode::WriteThrough => Tri::No,
        store_io_core::evidence::OsCacheMode::Unknown => Tri::Unknown,
    };
    d.fua_supported = leaf.fua;
    d.logical_block = leaf.logical;
    d.physical_block = leaf.physical;
    d.optimal_write = leaf.io_min;
    d.max_transfer = leaf.max_transfer;
    d.numa_node = leaf.numa;
    d.bus = leaf.bus;
    // SCSI reports vendor and model separately ("Msft" + "Virtual Disk").
    d.model = match (&leaf.vendor, &leaf.model) {
        (Some(v), Some(m)) if !m.to_ascii_lowercase().starts_with(&v.to_ascii_lowercase()) => {
            Some(format!("{v} {m}"))
        }
        (Some(v), None) => Some(v.clone()),
        (_, m) => m.clone(),
    };
    d.firmware = leaf.firmware.clone();

    let nv = block::nvme_facts(&leaf);
    d.cache_present = nv.cache_present;
    d.cache_enabled = nv.cache_enabled;
    d.backup_failed = nv.backup_failed;
    d.atomic = nv.atomic;
    d.multi_controller = nv.multi_controller;
    if nv.model.is_some() {
        d.model = nv.model;
    }
    if nv.firmware.is_some() {
        d.firmware = nv.firmware;
    }
    ev.privilege = nv.privilege;
    ev.missing = ev.missing.union(nv.missing);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_dev_decodes_glibc_layout() {
        assert_eq!(split_dev(0x0830), (8, 48));
        // makedev(259, 3): major 0x103 in bits 8..20, minor 3 in bits 0..8.
        assert_eq!(split_dev(0x1_0303), (259, 3));
        assert_eq!(split_dev(0), (0, 0));
        // High bits: major 0x12345, minor 0xABCDEF
        let dev = ((0x1_2000u64) << 32) | (0x345u64 << 8) | ((0xAB_CD00u64) << 12) | 0xEF;
        assert_eq!(split_dev(dev), (0x1_2345, 0xAB_CDEF));
    }
}
