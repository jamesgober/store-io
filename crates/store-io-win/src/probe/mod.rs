//! Gathers [`Evidence`] for an open file: volume and file facts, then the
//! physical drive's storage properties and, on NVMe, the controller and
//! namespace identify data, the write-cache feature and the health log.
//!
//! Everything runs unprivileged: the drive is opened with desired access 0,
//! which `IOCTL_STORAGE_QUERY_PROPERTY` accepts for a standard user. The
//! write-cache property query makes the class driver send one device cache
//! flush. Whatever cannot be read stays `Unknown` and is listed in
//! `Evidence::missing`. This module never decides a class.

mod nvme;
mod storage;
mod volume;

use store_io_core::class::Missing;
use store_io_core::evidence::{Bus, Evidence, FsKind, OsCacheMode, PlatformKind, Privilege, Tri};
use store_io_platform::RawResult;
use windows_sys::Win32::Storage::FileSystem::{
    FileStorageInfo, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
};
use windows_sys::Win32::System::Ioctl::{
    IOCTL_STORAGE_GET_DEVICE_NUMBER, IOCTL_STORAGE_QUERY_PROPERTY, StorageAccessAlignmentProperty,
    StorageAdapterProperty, StorageDeviceNumaProperty, StorageDeviceProperty,
    StorageDeviceProtocolSpecificProperty, StorageDeviceWriteCacheProperty, WriteCacheDisabled,
    WriteCacheEnabled, WriteCacheTypeNone, WriteCacheTypeWriteBack, WriteCacheTypeWriteThrough,
};

use crate::dir::WinDir;
use crate::file::WinFile;
use crate::probe::storage::NvmeQuery;
use crate::sys::{self, Handle};

/// Page size assumed for direct-I/O buffer alignment: Windows documents
/// physical-sector alignment as the safe choice and the page covers every
/// physical sector size in use.
const PAGE: u32 = 4096;

/// Fills evidence for `file` in `dir`.
pub(crate) fn probe(dir: &WinDir, file: &WinFile) -> RawResult<Evidence> {
    let mut ev = Evidence::unknown(PlatformKind::Windows);
    ev.kernel = sys::kernel_version();
    ev.missing.insert(Missing::Stack);
    ev.missing.insert(Missing::PageCache);

    // Volume and file facts: if even these fail, nothing was learned.
    let (fs_name, _volume_flags) = sys::volume_info(file.handle())?;
    ev.fs.kind = volume::fs_kind(&fs_name);
    ev.fs.full_flush = Tri::Yes;
    ev.fs.direct_io = if ev.fs.kind == FsKind::Ntfs {
        Tri::Yes
    } else {
        Tri::Unknown
    };
    ev.fs.cow = match ev.fs.kind {
        FsKind::Ntfs => Tri::No,
        _ => Tri::Unknown,
    };
    ev.fs.dir_flush = match sys::flush_file_buffers(dir.handle()) {
        Ok(()) => Tri::Yes,
        Err(_) => Tri::No,
    };

    match sys::attribute_tag(file.handle()) {
        Ok((attrs, _tag)) => {
            let a = volume::attribute_facts(attrs);
            ev.fs.transformed = a.transformed;
            if a.may_allocate == Tri::Yes {
                ev.fs.cow = Tri::Yes;
            }
            if a.cloud_placeholder {
                ev.fs.kind = FsKind::Network;
            }
        }
        Err(_) => ev.missing.insert(Missing::InodeFlags),
    }

    let mut storage_logical = 0u32;
    let mut storage_physical = 0u32;
    match sys::file_info_bytes(file.handle(), FileStorageInfo, 28)
        .ok()
        .and_then(|b| volume::parse_storage_info(&b))
    {
        Some(si) => {
            ev.fs.os_atomic_unit = si.fs_effective_atomicity;
            ev.fs.dio_offset_align = si.logical_bytes_per_sector;
            ev.fs.dio_mem_align = PAGE.max(si.physical_bytes_for_performance);
            storage_logical = si.logical_bytes_per_sector;
            storage_physical = si.physical_bytes_for_atomicity;
        }
        None => ev.missing.insert(Missing::DioAlignment),
    }

    let mount_root = sys::volume_mount_root(dir.path()).ok();
    if let Some(root) = &mount_root {
        match volume::drive_class(sys::drive_type(root)) {
            volume::DriveClass::Network => ev.fs.kind = FsKind::Network,
            volume::DriveClass::Ram => ev.device.bus = Bus::Ram,
            volume::DriveClass::Local => {}
        }
    }

    let disk = mount_root.as_deref().and_then(disk_number);
    let drive = disk.and_then(|n| sys::open_device(&storage::physical_drive_name(n), 0).ok());
    match drive {
        Some(d) => probe_drive(&d, &mut ev),
        None => {
            for m in [
                Missing::Identify,
                Missing::Smart,
                Missing::CachePresence,
                Missing::CacheEnabled,
                Missing::OsCacheMode,
                Missing::AtomicFields,
            ] {
                ev.missing.insert(m);
            }
        }
    }
    if ev.device.logical_block == 0 {
        ev.device.logical_block = storage_logical;
        ev.device.physical_block = storage_physical;
    }
    Ok(ev)
}

/// The physical drive number behind a mount root, via the volume's device
/// number, falling back to its first disk extent.
fn disk_number(root: &[u16]) -> Option<u32> {
    let name = sys::volume_device_name(root).ok()?;
    let vol = sys::open_device(&name, 0).ok()?;
    let mut out = [0u8; 12];
    if let Ok(n) = sys::ioctl(&vol, IOCTL_STORAGE_GET_DEVICE_NUMBER, &[], &mut out) {
        if let Some(num) = storage::parse_device_number(&out[..n.min(out.len())]) {
            if num != u32::MAX {
                return Some(num);
            }
        }
    }
    let mut ext = [0u8; 8 + 24 * 8];
    let n = sys::ioctl(&vol, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], &mut ext).ok()?;
    storage::parse_disk_extents(&ext[..n.min(ext.len())]).map(|(_count, first)| first)
}

/// One `IOCTL_STORAGE_QUERY_PROPERTY` with a plain query.
fn query(drive: &Handle, id: i32, out_len: usize) -> Option<Vec<u8>> {
    let q = storage::property_query(id);
    let mut out = vec![0u8; out_len];
    let n = sys::ioctl(drive, IOCTL_STORAGE_QUERY_PROPERTY, &q, &mut out).ok()?;
    out.truncate(n.min(out_len));
    Some(out)
}

/// One NVMe protocol-specific query; returns the protocol payload and the
/// completion dword 0.
fn nvme(drive: &Handle, q: NvmeQuery, data_len: u32) -> Option<(Vec<u8>, u32)> {
    let mut buf = storage::nvme_query(StorageDeviceProtocolSpecificProperty, q, data_len);
    let input = buf.clone();
    let n = sys::ioctl(drive, IOCTL_STORAGE_QUERY_PROPERTY, &input, &mut buf).ok()?;
    buf.truncate(n.min(buf.len()));
    let dword0 = storage::protocol_dword0(&buf)?;
    let data = storage::protocol_data(&buf)?.to_vec();
    Some((data, dword0))
}

fn probe_drive(drive: &Handle, ev: &mut Evidence) {
    let d = &mut ev.device;

    match query(drive, StorageDeviceWriteCacheProperty, 64)
        .and_then(|b| storage::parse_write_cache(&b))
    {
        Some(wc) => {
            d.os_flush_supported = if wc.flush_supported != 0 {
                Tri::Yes
            } else {
                Tri::No
            };
            d.user_power_protection = if wc.user_power_protection != 0 {
                Tri::Yes
            } else {
                Tri::No
            };
            d.cache_enabled = match wc.enabled as i32 {
                e if e == WriteCacheEnabled => Tri::Yes,
                e if e == WriteCacheDisabled => Tri::No,
                _ => Tri::Unknown,
            };
            d.cache_present = match wc.cache_type as i32 {
                t if t == WriteCacheTypeWriteBack || t == WriteCacheTypeWriteThrough => Tri::Yes,
                t if t == WriteCacheTypeNone => Tri::No,
                _ => Tri::Unknown,
            };
            d.fua_supported = match wc.write_through {
                2 => Tri::Yes,
                1 => Tri::No,
                _ => Tri::Unknown,
            };
            // The OS issues flushes when the class driver reports flush
            // support and the user has not told it to drop them.
            d.os_cache_mode = if d.user_power_protection == Tri::Yes {
                OsCacheMode::WriteThrough
            } else if d.os_flush_supported == Tri::Yes {
                OsCacheMode::WriteBack
            } else {
                OsCacheMode::Unknown
            };
        }
        None => {
            ev.missing.insert(Missing::OsCacheMode);
            ev.missing.insert(Missing::CacheEnabled);
        }
    }

    match query(drive, StorageAccessAlignmentProperty, 32)
        .and_then(|b| storage::parse_access_alignment(&b))
    {
        Some((logical, physical)) => {
            d.logical_block = logical;
            d.physical_block = physical;
        }
        None => ev.missing.insert(Missing::DioAlignment),
    }

    if let Some((max_transfer, bus)) =
        query(drive, StorageAdapterProperty, 64).and_then(|b| storage::parse_adapter(&b))
    {
        d.max_transfer = max_transfer;
        d.bus = storage::bus_from_code(i32::from(bus));
    }

    if let Some(desc) =
        query(drive, StorageDeviceProperty, 1024).and_then(|b| storage::parse_device_descriptor(&b))
    {
        let bus = storage::bus_from_code(desc.bus);
        if bus != Bus::Unknown {
            d.bus = bus;
        }
        d.model = match (desc.vendor, desc.product) {
            (Some(v), Some(p)) => Some(format!("{v} {p}")),
            (v, p) => v.or(p),
        };
        d.firmware = desc.revision;
    }

    if let Some(node) =
        query(drive, StorageDeviceNumaProperty, 16).and_then(|b| storage::parse_numa(&b))
    {
        d.numa_node = node;
    }

    if d.bus == Bus::Nvme {
        probe_nvme(drive, ev);
    } else {
        ev.missing.insert(Missing::Identify);
        ev.missing.insert(Missing::Smart);
        ev.missing.insert(Missing::AtomicFields);
        if ev.device.cache_present == Tri::Unknown {
            ev.missing.insert(Missing::CachePresence);
        }
    }
    if ev.device.bus != Bus::Ram {
        ev.hypervisor = storage::hypervisor_from(ev.device.bus, ev.device.model.as_deref());
    }
}

fn probe_nvme(drive: &Handle, ev: &mut Evidence) {
    let mut identify_ok = false;
    let mut smart_ok = false;
    let d = &mut ev.device;

    match nvme(
        drive,
        NvmeQuery::Identify {
            cns: storage::NVME_CNS_CONTROLLER,
            nsid: 0,
        },
        4096,
    )
    .and_then(|(data, _)| nvme::parse_identify_controller(&data))
    {
        Some(c) => {
            identify_ok = true;
            // The device's own report overrides the class driver's view.
            d.cache_present = if c.vwc_present { Tri::Yes } else { Tri::No };
            d.atomic.awupf = Some(c.awupf);
            d.multi_controller = c.multi_controller;
            if c.model.is_some() {
                d.model = c.model;
            }
            if c.firmware.is_some() {
                d.firmware = c.firmware;
            }
        }
        None => {
            ev.missing.insert(Missing::Identify);
            if d.cache_present == Tri::Unknown {
                ev.missing.insert(Missing::CachePresence);
            }
        }
    }

    match nvme(
        drive,
        NvmeQuery::Identify {
            cns: storage::NVME_CNS_NAMESPACE,
            nsid: 1,
        },
        4096,
    )
    .and_then(|(data, _)| nvme::parse_identify_namespace(&data))
    {
        Some(ns) => {
            d.atomic.nsabp = ns.nsabp;
            d.atomic.nawupf = ns.nawupf;
            d.atomic.nabspf = ns.nabspf;
            d.atomic.nabo = ns.nabo;
            if ns.lba_bytes != 0 {
                d.logical_block = ns.lba_bytes;
            }
            if let Some(npwg) = ns.npwg {
                d.optimal_write = (u32::from(npwg) + 1).saturating_mul(ns.lba_bytes);
            }
        }
        None => ev.missing.insert(Missing::AtomicFields),
    }

    match nvme(
        drive,
        NvmeQuery::Feature {
            fid: storage::NVME_FEATURE_VWC,
        },
        4096,
    ) {
        Some((_, dword0)) => {
            d.cache_enabled = if nvme::vwc_enabled(dword0) {
                Tri::Yes
            } else {
                Tri::No
            };
        }
        None => {
            if d.cache_enabled == Tri::Unknown {
                ev.missing.insert(Missing::CacheEnabled);
            }
        }
    }

    match nvme(
        drive,
        NvmeQuery::LogPage {
            lid: storage::NVME_LOG_HEALTH,
        },
        512,
    )
    .and_then(|(data, _)| nvme::parse_health_critical_warning(&data))
    {
        Some(cw) => {
            smart_ok = true;
            d.backup_failed = if cw & 0x10 != 0 { Tri::Yes } else { Tri::No };
        }
        None => ev.missing.insert(Missing::Smart),
    }

    if identify_ok && smart_ok {
        ev.privilege = Privilege::Full;
    }
}
