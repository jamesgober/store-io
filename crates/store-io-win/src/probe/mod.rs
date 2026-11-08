//! Gathers [`Evidence`] for an open file: volume and file facts, then the
//! physical drive's storage properties and, on NVMe, the controller and
//! namespace identify data, the write-cache feature and the health log.
//!
//! Everything runs unprivileged: the drive is opened with desired access 0,
//! which `IOCTL_STORAGE_QUERY_PROPERTY` accepts for a standard user. The
//! write-cache property query makes the class driver send one device cache
//! flush. Whatever cannot be read stays `Unknown` and is listed in
//! `Evidence::missing`. This module never decides a class.

pub(crate) mod nvme;
pub(crate) mod storage;
pub(crate) mod volume;

use store_io_core::class::Missing;
use store_io_core::evidence::{Bus, Evidence, FsKind, OsCacheMode, PlatformKind, Privilege, Tri};
use store_io_core::identity::{DeviceId, DeviceIdentity, DeviceNode, normalize_name};
use store_io_platform::RawResult;
use windows_sys::Win32::Storage::FileSystem::{
    FileStorageInfo, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
};
use windows_sys::Win32::System::Ioctl::{
    IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_STORAGE_GET_DEVICE_NUMBER,
    IOCTL_STORAGE_QUERY_PROPERTY, StorageAccessAlignmentProperty, StorageAdapterProperty,
    StorageDeviceNumaProperty, StorageDeviceProperty, StorageDeviceProtocolSpecificProperty,
    StorageDeviceWriteCacheProperty, WriteCacheDisabled, WriteCacheEnabled, WriteCacheTypeNone,
    WriteCacheTypeWriteBack, WriteCacheTypeWriteThrough,
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

    // The file's own volume (from its handle, whatever junctions the path
    // went through), the directory path's only when the handle cannot name
    // one (a network file).
    let mount_root = sys::volume_guid_root(file.handle())
        .ok()
        .or_else(|| sys::volume_mount_root(dir.path()).ok());
    if let Some(root) = &mount_root {
        match volume::drive_class(sys::drive_type(root)) {
            volume::DriveClass::Network => ev.fs.kind = FsKind::Network,
            volume::DriveClass::Ram => ev.device.bus = Bus::Ram,
            volume::DriveClass::Local => {}
        }
    }

    ev.device.identity = if ev.fs.kind == FsKind::Network {
        DeviceIdentity::unknown()
    } else {
        mount_root
            .as_deref()
            .map_or_else(DeviceIdentity::unknown, |root| identity(root, file))
    };
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

/// The physical devices `file` lives on (the probe's identity part alone:
/// volume and storage-property queries and NVMe Identify Controller; never
/// the write-cache query, which sends a flush). Network volumes and cloud
/// placeholders are unknown, as in the probe.
pub(crate) fn device_identity(_dir: &WinDir, file: &WinFile) -> DeviceIdentity {
    let network = sys::volume_info(file.handle())
        .is_ok_and(|(fs, _)| volume::fs_kind(&fs) == FsKind::Network)
        || sys::attribute_tag(file.handle())
            .is_ok_and(|(attrs, _)| volume::attribute_facts(attrs).cloud_placeholder);
    if network {
        return DeviceIdentity::unknown();
    }
    // The file's own volume (`\\?\Volume{GUID}\`, cheap from its handle)
    // gives the drive type: no walk of the directory's path for its mount
    // root (`GetVolumePathNameW`, milliseconds, on a ditto's path), and no
    // junction on the way can name another volume. A network file has no
    // volume GUID: unknown.
    let Ok(volume) = sys::volume_guid_root(file.handle()) else {
        return DeviceIdentity::unknown();
    };
    if volume::drive_class(sys::drive_type(&volume)) == volume::DriveClass::Network {
        return DeviceIdentity::unknown();
    }
    identity_in(&volume)
}

/// Most extents a volume's identity reads (a spanned or striped volume
/// lists one per member disk and range).
const MAX_EXTENTS: usize = 64;

/// Every physical disk under a mount root, each with its serial (PLANNING
/// R5.2). Incomplete when a disk cannot be opened or queried, or is one the
/// probe cannot see whole ([`identity_of`] lists them), or the machine is a
/// guest.
pub(crate) fn identity(_root: &[u16], file: &WinFile) -> DeviceIdentity {
    // The volume comes from the file's own handle (`\\?\Volume{GUID}\`),
    // never from the directory's path text, which does not resolve a
    // junction or directory link on the way (review: a junction to another
    // drive named the wrong disk; volume serials can repeat on clones).
    let Ok(volume) = sys::volume_guid_root(file.handle()) else {
        return DeviceIdentity::unknown();
    };
    identity_in(&volume)
}

/// [`identity`] of the volume `root` (`\\?\Volume{GUID}\`).
fn identity_in(root: &[u16]) -> DeviceIdentity {
    let (disks, all) = match volume_disks(root) {
        Some(found) => found,
        // The extents could not be listed: the device number names one
        // disk, but not that it is the only one.
        None => match disk_number(root) {
            Some(d) => (vec![d], false),
            None => return DeviceIdentity::unknown(),
        },
    };
    let found: Vec<Disk> = disks
        .into_iter()
        .map(|number| {
            let drive = sys::open_device(&storage::physical_drive_name(number), 0).ok();
            let desc = drive.as_ref().and_then(|d| {
                query(d, StorageDeviceProperty, 1024)
                    .and_then(|b| storage::parse_device_descriptor(&b))
            });
            let nvme_bus = desc
                .as_ref()
                .is_some_and(|d| storage::bus_from_code(d.bus) == Bus::Nvme);
            let controller = drive.as_ref().filter(|_| nvme_bus).and_then(|d| {
                controller_of(number, desc.as_ref(), || {
                    nvme(
                        d,
                        NvmeQuery::Identify {
                            cns: storage::NVME_CNS_CONTROLLER,
                            nsid: 0,
                        },
                        4096,
                    )
                    .and_then(|(data, _)| nvme::parse_identify_controller(&data))
                })
            });
            Disk {
                number,
                desc,
                controller,
            }
        })
        .collect();
    identity_of(&found, all, smbios_guest() || cpuid_guest())
}

/// Disks whose NVMe Identify Controller [`controller_of`] keeps (a few
/// disks per machine; past it the oldest is forgotten).
const CONTROLLERS: usize = 64;

/// Each disk's NVMe Identify Controller, by its number and descriptor.
type Controllers = Vec<(u32, storage::DeviceDesc, nvme::IdentifyController)>;

static KNOWN_CONTROLLERS: std::sync::Mutex<Controllers> = std::sync::Mutex::new(Vec::new());

/// Disk `number`'s controller: asked of the device by `read` (an NVMe admin
/// command, hundreds of microseconds) only when the disk is not one already
/// asked with the same descriptor (vendor, product, revision and serial:
/// the namespace's EUI-64 on NVMe). A disk swapped or renumbered has
/// another descriptor and is asked afresh; which disks a volume lies on is
/// always read afresh (`volume_disks`). Nothing is kept without a
/// descriptor to check it by.
fn controller_of(
    number: u32,
    desc: Option<&storage::DeviceDesc>,
    read: impl FnOnce() -> Option<nvme::IdentifyController>,
) -> Option<nvme::IdentifyController> {
    let Some(desc) = desc else {
        return read();
    };
    let lock = || {
        KNOWN_CONTROLLERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    };
    if let Some((_, _, c)) = lock().iter().find(|(n, d, _)| *n == number && d == desc) {
        return Some(c.clone());
    }
    let c = read()?;
    let mut known = lock();
    known.retain(|(n, _, _)| *n != number);
    if known.len() >= CONTROLLERS {
        let _oldest = known.remove(0);
    }
    known.push((number, desc.clone(), c.clone()));
    Some(c)
}

/// Whether CPUID says this machine is a guest: a hypervisor is present
/// (leaf 1, ECX bit 31) and this machine is not the root partition of
/// Hyper-V running on hardware. Windows with virtualization-based security
/// runs as the root of its own Hyper-V on physical machines; the root is
/// told apart by the vendor `Microsoft Hv` (leaf 0x4000_0000), the `Hv#1`
/// interface (leaf 0x4000_0001), the `CpuManagement` privilege without
/// `Isolation` (leaf 0x4000_0003 EBX bits 12 and 22, bits 44 and 54 of the
/// privilege mask: Linux's test for the root partition, which ignores a
/// root claim made to an isolated, confidential guest) and no Hyper-V
/// below it (leaf 0x4000_0004 EAX bit 12, set only when this Hyper-V is
/// nested in a Hyper-V partition).
/// **Not caught by CPUID**: a guest running its own Hyper-V (VBS, the
/// Hyper-V role, WSL2) under another hypervisor (VMware, KVM, Xen,
/// VirtualBox) sees itself as a root with no Hyper-V parent; there SMBIOS
/// is the only signal. A second signal beside SMBIOS, never a replacement.
/// Not read on other architectures (Arm guests have SMBIOS only).
fn cpuid_guest() -> bool {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        #[cfg(target_arch = "x86")]
        use core::arch::x86::__cpuid;
        #[cfg(target_arch = "x86_64")]
        use core::arch::x86_64::__cpuid;
        // SAFETY: CPUID exists on every processor this target runs on and
        // never faults. A leaf beyond the hypervisor's maximum returns
        // unrelated data, never an error: the rule checks the maximum
        // before it trusts leaves 0x4000_0001 to 0x4000_0004.
        #[allow(unused_unsafe)]
        let c = unsafe {
            let hv = __cpuid(0x4000_0000);
            let mut vendor = [0u8; 12];
            vendor[..4].copy_from_slice(&hv.ebx.to_le_bytes());
            vendor[4..8].copy_from_slice(&hv.ecx.to_le_bytes());
            vendor[8..].copy_from_slice(&hv.edx.to_le_bytes());
            HvCpuid {
                leaf1_ecx: __cpuid(1).ecx,
                vendor,
                max_leaf: hv.eax,
                interface: __cpuid(0x4000_0001).eax,
                privileges_hi: __cpuid(0x4000_0003).ebx,
                recommendations: __cpuid(0x4000_0004).eax,
            }
        };
        cpuid_says_guest(&c)
    }
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    {
        false
    }
}

/// The CPUID registers [`cpuid_guest`] reads.
#[cfg_attr(
    not(any(target_arch = "x86", target_arch = "x86_64")),
    allow(dead_code)
)]
struct HvCpuid {
    leaf1_ecx: u32,
    vendor: [u8; 12],
    max_leaf: u32,
    interface: u32,
    privileges_hi: u32,
    recommendations: u32,
}

/// `Hv#1`, Hyper-V's interface signature (leaf 0x4000_0001 EAX).
#[cfg_attr(
    not(any(target_arch = "x86", target_arch = "x86_64")),
    allow(dead_code)
)]
const HV_INTERFACE: u32 = 0x3123_7648;

/// [`cpuid_guest`]'s rule on the raw registers.
#[cfg_attr(
    not(any(target_arch = "x86", target_arch = "x86_64")),
    allow(dead_code)
)]
fn cpuid_says_guest(c: &HvCpuid) -> bool {
    let present = c.leaf1_ecx & (1 << 31) != 0;
    let root = &c.vendor == b"Microsoft Hv"
        && c.max_leaf >= 0x4000_0004
        && c.interface == HV_INTERFACE
        && c.privileges_hi & (1 << 12) != 0
        && c.privileges_hi & (1 << 22) == 0
        && c.recommendations & (1 << 12) == 0;
    present && !root
}

/// Whether the machine's SMBIOS system information names a virtual machine
/// (read unprivileged; unlike the CPUID hypervisor bit, which Windows sets
/// on physical machines running virtualization-based security, SMBIOS
/// describes the hardware the OS was booted on).
fn smbios_guest() -> bool {
    // Only a table that positively describes physical hardware counts as
    // bare metal: an unreadable or malformed one is a guest. Read once: the
    // firmware's tables do not change while the process runs.
    static GUEST: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *GUEST.get_or_init(|| !sys::smbios_raw().is_some_and(|raw| storage::smbios_says_physical(&raw)))
}

/// What the identity reads of one disk.
#[derive(Debug, Clone, Default)]
pub(crate) struct Disk {
    /// The `PhysicalDrive` number.
    pub number: u32,
    /// `STORAGE_DEVICE_DESCRIPTOR`, if it could be read.
    pub desc: Option<storage::DeviceDesc>,
    /// NVMe Identify Controller, for an NVMe disk, if it could be read.
    pub controller: Option<nvme::IdentifyController>,
}

/// The identity of a volume's disks (`all`: every disk was listed).
/// Complete only when every disk is one the probe can see whole: an NVMe
/// disk with its controller's serial or subsystem name (the descriptor's
/// serial of an NVMe disk is the namespace's EUI-64: two namespaces of one
/// controller differ there), or a SATA disk; never a USB disk (its bridge
/// is not read), a virtual disk (a hypervisor's, whatever bus it
/// reports), a remote target, or a SCSI,
/// SAS or unknown bus, which may be a hardware RAID's logical drive
/// (`BusTypeRAID` maps to SCSI) or a Storage Spaces disk (its bus type maps
/// to unknown).
pub(crate) fn identity_of(disks: &[Disk], all: bool, guest: bool) -> DeviceIdentity {
    let mut id = DeviceIdentity {
        devices: Vec::new(),
        complete: all,
    };
    for disk in disks {
        let Some(desc) = &disk.desc else {
            id.complete = false;
            continue;
        };
        let bus = storage::bus_from_code(desc.bus);
        let mut d = DeviceId::node(DeviceNode::Windows { disk: disk.number });
        if let Some(s) = &desc.serial {
            d.add_name(s);
        }
        let mut controller_named = false;
        if let Some(c) = &disk.controller {
            for n in [&c.serial, &c.subnqn].into_iter().flatten() {
                // Named even when the name repeats the descriptor's serial.
                controller_named |= normalize_name(n).is_some();
                d.add_name(n);
            }
        }
        // A USB disk is not seen whole here: its bridge is not read, so the
        // LUNs of one enclosure would look distinct (R5.2).
        let seen_whole = match bus {
            Bus::Nvme => controller_named,
            Bus::Sata => true,
            _ => false,
        };
        let hosted = guest || storage::hypervisor_from(bus, desc.product.as_deref()).is_some();
        if !seen_whole || hosted {
            id.complete = false;
        }
        id.devices.push(d);
    }
    if id.devices.is_empty() {
        id.complete = false;
    }
    id
}

/// Every disk of a mount root's volume, and whether all were listed.
fn volume_disks(root: &[u16]) -> Option<(Vec<u32>, bool)> {
    let name = sys::volume_device_name(root).ok()?;
    let vol = sys::open_device(&name, 0).ok()?;
    let mut ext = vec![0u8; 8 + 24 * MAX_EXTENTS];
    match sys::ioctl(&vol, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], &mut ext) {
        Ok(n) => storage::parse_all_disk_extents(&ext[..n.min(ext.len())]),
        // More extents than the buffer holds: the first ones only, and
        // the identity incomplete.
        Err(_) => storage::parse_all_disk_extents(&ext).map(|(d, _)| (d, false)),
    }
}

/// The disk's length in bytes (`IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`, which
/// needs no access rights).
fn disk_size(drive: &Handle) -> Option<u64> {
    let mut out = [0u8; 40];
    let n = sys::ioctl(drive, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], &mut out).ok()?;
    storage::parse_disk_size(&out[..n.min(out.len())])
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
        // The device's geometry is unknown; the alignment is missing only if
        // the file system did not report it either (FileStorageInfo).
        None => {
            if ev.fs.dio_offset_align == 0 {
                ev.missing.insert(Missing::DioAlignment);
            }
        }
    }

    if let Some((max_transfer, bus)) =
        query(drive, StorageAdapterProperty, 64).and_then(|b| storage::parse_adapter(&b))
    {
        d.max_transfer = max_transfer;
        d.bus = storage::bus_from_code(i32::from(bus));
        if let Some(layer) = storage::layer_from_code(i32::from(bus)) {
            ev.stack.push(layer);
        }
    }

    if let Some(desc) =
        query(drive, StorageDeviceProperty, 1024).and_then(|b| storage::parse_device_descriptor(&b))
    {
        let bus = storage::bus_from_code(desc.bus);
        if bus != Bus::Unknown {
            d.bus = bus;
        }
        if let Some(layer) = storage::layer_from_code(desc.bus) {
            if !ev.stack.contains(&layer) {
                ev.stack.push(layer);
            }
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

    // Namespace 1, used only if it is this disk's (its size is the disk's):
    // otherwise its format and atomic fields are missing, not borrowed.
    let disk_bytes = disk_size(drive);
    match nvme(
        drive,
        NvmeQuery::Identify {
            cns: storage::NVME_CNS_NAMESPACE,
            nsid: 1,
        },
        4096,
    )
    .and_then(|(data, _)| nvme::parse_identify_namespace(&data))
    .filter(|ns| disk_bytes.is_some_and(|b| nvme::namespace_matches(ns, b)))
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
        // Not this disk's namespace (or unreadable): no atomic evidence at
        // all, AWUPF included (another namespace's NAWUPF may be smaller).
        None => {
            d.atomic = Default::default();
            ev.missing.insert(Missing::AtomicFields);
        }
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

#[cfg(test)]
mod identity_tests {
    use super::*;

    /// A disk's controller is asked once while its descriptor stays the
    /// same, and again when it changes (a disk swapped in under the same
    /// number) or with no descriptor to check it by.
    #[test]
    fn test_a_controller_is_asked_again_only_for_a_new_descriptor() {
        let desc = |serial: &str| storage::DeviceDesc {
            serial: Some(serial.to_owned()),
            ..storage::DeviceDesc::default()
        };
        let ctl = |serial: &str| nvme::IdentifyController {
            serial: Some(serial.to_owned()),
            ..nvme::IdentifyController::default()
        };
        // Disk numbers no other test uses: the cache is the process's.
        let (n, first, second) = (9_001, desc("eui-1"), desc("eui-2"));
        assert_eq!(
            controller_of(n, Some(&first), || Some(ctl("c1"))),
            Some(ctl("c1"))
        );
        assert_eq!(
            controller_of(n, Some(&first), || panic!("asked again")),
            Some(ctl("c1"))
        );
        assert_eq!(
            controller_of(n, Some(&second), || Some(ctl("c2"))),
            Some(ctl("c2"))
        );
        assert_eq!(controller_of(n, None, || Some(ctl("c3"))), Some(ctl("c3")));
        // Nothing kept without a descriptor: asked again. Another disk with
        // disk 9_001's descriptor is asked itself.
        assert_eq!(controller_of(n, None, || Some(ctl("c4"))), Some(ctl("c4")));
        assert_eq!(controller_of(9_002, Some(&second), || None), None);
    }

    /// The rule on bare metal.
    fn id(disks: &[Disk], all: bool) -> DeviceIdentity {
        identity_of(disks, all, false)
    }

    fn disk(number: u32, bus: i32, serial: &str, ctrl: Option<(&str, &str)>) -> Disk {
        Disk {
            number,
            desc: Some(storage::DeviceDesc {
                bus,
                product: Some("SSD 990".to_owned()),
                serial: Some(serial.to_owned()),
                ..storage::DeviceDesc::default()
            }),
            controller: ctrl.map(|(sn, nqn)| nvme::IdentifyController {
                serial: Some(sn.to_owned()),
                subnqn: Some(nqn.to_owned()),
                ..nvme::IdentifyController::default()
            }),
        }
    }

    const NVME: i32 = 17;
    const SATA: i32 = 11;
    const SAS: i32 = 10;
    const VIRTUAL: i32 = 14;

    #[test]
    fn test_two_namespaces_of_one_controller_are_one_device() {
        // The descriptor's serial is each namespace's EUI-64; the controller
        // serial is shared.
        let a = id(
            &[disk(
                0,
                NVME,
                "0025_3858_0000_0001.",
                Some(("S6B0NG0R1", "nqn.x")),
            )],
            true,
        );
        let b = id(
            &[disk(
                1,
                NVME,
                "0025_3858_0000_0002.",
                Some(("S6B0NG0R1", "nqn.x")),
            )],
            true,
        );
        assert!(a.complete && b.complete);
        assert!(!a.distinct(&b));
        let c = id(
            &[disk(
                2,
                NVME,
                "6479_A7A5_0A50_05F4.",
                Some(("TPBF2209", "nqn.y")),
            )],
            true,
        );
        assert!(a.distinct(&c));
    }

    #[test]
    fn test_any_disk_of_a_guest_machine_is_incomplete() {
        // A guest's SATA disk with a model no list knows (VirtualBox,
        // Parallels): SMBIOS says guest.
        let d = [disk(0, SATA, "VB1234-5678", None)];
        assert!(identity_of(&d, true, false).complete);
        assert!(!identity_of(&d, true, true).complete);
    }

    #[test]
    fn test_cpuid_names_a_guest_unless_it_is_hyper_v_s_root_on_hardware() {
        // The dev box: physical, VBS on (read 2026-11-06).
        let root = || HvCpuid {
            leaf1_ecx: 1 << 31,
            vendor: *b"Microsoft Hv",
            max_leaf: 0x4000_000c,
            interface: HV_INTERFACE,
            privileges_hi: 0x002b_b9ff,
            recommendations: 0x0094_0d1c,
        };
        assert!(!cpuid_says_guest(&root()), "the root partition");
        // Bare metal without VBS: no hypervisor bit (the hypervisor leaves
        // then hold whatever the processor returns past its maximum: zeros
        // on AMD; here arbitrary values, which the present bit overrides).
        let none = HvCpuid {
            leaf1_ecx: 0,
            vendor: [0x5a; 12],
            max_leaf: 0x0bb8,
            interface: 0x0e10,
            privileges_hi: 0x0064,
            recommendations: 0,
        };
        assert!(!cpuid_says_guest(&none), "no hypervisor");
        // The maximum leaf the rule needs, exactly.
        let at_max = HvCpuid {
            max_leaf: 0x4000_0004,
            ..root()
        };
        assert!(!cpuid_says_guest(&at_max), "a root at the minimum leaf");
        let cases = [
            (
                HvCpuid {
                    privileges_hi: 0x002b_a9ff,
                    ..root()
                },
                "a Hyper-V guest",
            ),
            (
                HvCpuid {
                    recommendations: 0x0094_0d1c | (1 << 12),
                    ..root()
                },
                "a root nested in a guest",
            ),
            (
                HvCpuid {
                    privileges_hi: 0x002b_b9ff | (1 << 22),
                    ..root()
                },
                "an isolated guest told it is a root",
            ),
            (
                HvCpuid {
                    vendor: *b"KVMKVMKVMKVM",
                    ..root()
                },
                "another hypervisor",
            ),
            (
                HvCpuid {
                    max_leaf: 0x4000_0001,
                    ..root()
                },
                "leaves past the maximum",
            ),
            (
                HvCpuid {
                    interface: 0,
                    ..root()
                },
                "not the Hyper-V interface",
            ),
        ];
        for (c, what) in cases {
            assert!(cpuid_says_guest(&c), "{what}");
        }
    }

    #[test]
    fn test_an_nvme_controller_with_only_meaningless_names_is_not_named() {
        let d = disk(0, NVME, "0025_3858_0000_0001.", Some(("    ", "")));
        assert!(!id(&[d], true).complete);
    }

    #[test]
    fn test_an_nvme_disk_named_by_its_controller_alone_is_complete() {
        // stornvme puts the controller serial in the descriptor when the
        // namespace has no EUI-64 or NGUID: the controller's name is then
        // a duplicate, and still names the controller.
        let d = disk(0, NVME, "S6B0NG0R1", Some(("S6B0NG0R1", "")));
        assert!(id(&[d], true).complete);
    }

    #[test]
    fn test_an_nvme_disk_without_its_controller_named_is_incomplete() {
        assert!(!id(&[disk(0, NVME, "0025_3858_0000_0001.", None)], true).complete);
    }

    #[test]
    fn test_only_disks_seen_whole_make_a_complete_identity() {
        assert!(id(&[disk(0, SATA, "WD-123", None)], true).complete);
        assert!(
            !id(&[disk(0, SATA, "WD-123", None)], false).complete,
            "not every disk listed"
        );
        // SAS, a hypervisor's disk, a USB disk (7, its bridge unread), a RAID
        // logical drive (8), Storage Spaces (16), unknown.
        for bus in [SAS, VIRTUAL, 7, 8, 16, 0] {
            assert!(!id(&[disk(0, bus, "X1", None)], true).complete, "bus {bus}");
        }
        let mut hyperv = disk(0, SATA, "X1", None);
        if let Some(d) = hyperv.desc.as_mut() {
            d.product = Some("Msft Virtual Disk".to_owned());
        }
        assert!(!identity_of(&[hyperv], true, false).complete);
        let unread = Disk {
            number: 3,
            ..Disk::default()
        };
        assert!(!id(&[disk(0, SATA, "WD-1", None), unread], true).complete);
        assert!(!identity_of(&[], true, false).complete);
    }
}
