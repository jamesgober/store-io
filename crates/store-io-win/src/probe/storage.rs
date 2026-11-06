//! `IOCTL_STORAGE_QUERY_PROPERTY` and volume-to-disk mapping: query builders
//! and bounds-checked parsers of the descriptors a disk returns.
//!
//! Field offsets follow `ntddstor.h` / `winioctl.h`; each parser reads
//! through [`Reader`] so a short or hostile buffer yields `None`.
//!
//! The parsers compile on every platform (for fuzzing, which runs on
//! Linux): the few Windows constants they use are copied here, and on
//! Windows a compile-time assertion pins each copy to `windows-sys`.

#[cfg(windows)]
use core::mem::offset_of;

use store_io_core::evidence::{Bus, Hypervisor, StackLayer};
#[cfg(windows)]
use windows_sys::Win32::System::Ioctl::{
    NVMeDataTypeFeature, NVMeDataTypeIdentify, NVMeDataTypeLogPage, PropertyStandardQuery,
    ProtocolTypeNvme, STORAGE_PROPERTY_QUERY, STORAGE_PROTOCOL_SPECIFIC_DATA,
};

use crate::bytes::Reader;

/// Size of `STORAGE_PROTOCOL_SPECIFIC_DATA`.
const SPSD_LEN: usize = 40;
/// `STORAGE_BUS_TYPE` values (`ntddstor.h`).
const BUS_SCSI: i32 = 0x1;
const BUS_ATA: i32 = 0x3;
const BUS_FIBRE: i32 = 0x6;
const BUS_USB: i32 = 0x7;
const BUS_RAID: i32 = 0x8;
const BUS_ISCSI: i32 = 0x9;
const BUS_SAS: i32 = 0xA;
const BUS_SATA: i32 = 0xB;
const BUS_VIRTUAL: i32 = 0xE;
const BUS_FILE_BACKED_VIRTUAL: i32 = 0xF;
const BUS_SPACES: i32 = 0x10;
const BUS_NVME: i32 = 0x11;

// The copies above, pinned to the SDK's values where the SDK is available.
#[cfg(windows)]
const _: () = {
    use windows_sys::Win32::Storage::FileSystem as fs;
    assert!(SPSD_LEN == size_of::<STORAGE_PROTOCOL_SPECIFIC_DATA>());
    assert!(BUS_SCSI == fs::BusTypeScsi);
    assert!(BUS_ATA == fs::BusTypeAta);
    assert!(BUS_FIBRE == fs::BusTypeFibre);
    assert!(BUS_USB == fs::BusTypeUsb);
    assert!(BUS_RAID == fs::BusTypeRAID);
    assert!(BUS_ISCSI == fs::BusTypeiScsi);
    assert!(BUS_SAS == fs::BusTypeSas);
    assert!(BUS_SATA == fs::BusTypeSata);
    assert!(BUS_VIRTUAL == fs::BusTypeVirtual);
    assert!(BUS_FILE_BACKED_VIRTUAL == fs::BusTypeFileBackedVirtual);
    assert!(BUS_SPACES == fs::BusTypeSpaces);
    assert!(BUS_NVME == fs::BusTypeNvme);
};

/// Offset of `AdditionalParameters` (where protocol data begins) in the query.
#[cfg(windows)]
const QUERY_PARAMS: usize = offset_of!(STORAGE_PROPERTY_QUERY, AdditionalParameters);
/// Offset of `ProtocolSpecificData` in `STORAGE_PROTOCOL_DATA_DESCRIPTOR`.
const DESC_SPSD: usize = 8;

/// NVMe Identify CNS values (NVMe base specification, Identify command).
pub(crate) const NVME_CNS_NAMESPACE: u32 = 0;
pub(crate) const NVME_CNS_CONTROLLER: u32 = 1;
/// NVMe SMART / Health Information log page identifier.
pub(crate) const NVME_LOG_HEALTH: u32 = 0x02;
/// NVMe Volatile Write Cache feature identifier.
pub(crate) const NVME_FEATURE_VWC: u32 = 0x06;

/// A plain `STORAGE_PROPERTY_QUERY` for `id`.
#[cfg(windows)]
pub(crate) fn property_query(id: i32) -> Vec<u8> {
    let mut q = vec![0u8; size_of::<STORAGE_PROPERTY_QUERY>()];
    q[..4].copy_from_slice(&id.to_le_bytes());
    q[4..8].copy_from_slice(&PropertyStandardQuery.to_le_bytes());
    q
}

/// What an NVMe protocol-specific query asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NvmeQuery {
    /// Identify with the given CNS and NSID.
    Identify { cns: u32, nsid: u32 },
    /// A log page.
    LogPage { lid: u32 },
    /// Get Features.
    Feature { fid: u32 },
}

/// A `StorageDeviceProtocolSpecificProperty` query buffer, also used as the
/// output buffer, with room for `data_len` bytes of protocol data.
#[cfg(windows)]
pub(crate) fn nvme_query(property_id: i32, q: NvmeQuery, data_len: u32) -> Vec<u8> {
    let (data_type, value, sub) = match q {
        NvmeQuery::Identify { cns, nsid } => (NVMeDataTypeIdentify, cns, nsid),
        NvmeQuery::LogPage { lid } => (NVMeDataTypeLogPage, lid, 0),
        NvmeQuery::Feature { fid } => (NVMeDataTypeFeature, fid, 0),
    };
    let mut buf = vec![0u8; QUERY_PARAMS + SPSD_LEN + data_len as usize];
    buf[..4].copy_from_slice(&property_id.to_le_bytes());
    buf[4..8].copy_from_slice(&PropertyStandardQuery.to_le_bytes());
    let p = QUERY_PARAMS;
    let fields: [u32; 6] = [
        ProtocolTypeNvme as u32,
        data_type as u32,
        value,
        sub,
        SPSD_LEN as u32,
        data_len,
    ];
    for (i, f) in fields.iter().enumerate() {
        buf[p + i * 4..p + i * 4 + 4].copy_from_slice(&f.to_le_bytes());
    }
    buf
}

/// The protocol data payload of a `STORAGE_PROTOCOL_DATA_DESCRIPTOR` answer,
/// located through the descriptor's own offset and length fields.
pub(crate) fn protocol_data(out: &[u8]) -> Option<&[u8]> {
    let r = Reader::new(out);
    let off = r.u32(DESC_SPSD + 16)? as usize;
    let len = r.u32(DESC_SPSD + 20)? as usize;
    if off < SPSD_LEN || len == 0 {
        return None;
    }
    r.slice(DESC_SPSD.checked_add(off)?, len)
}

/// `FixedProtocolReturnData` (completion dword 0) of a protocol answer.
pub(crate) fn protocol_dword0(out: &[u8]) -> Option<u32> {
    Reader::new(out).u32(DESC_SPSD + 24)
}

/// `STORAGE_WRITE_CACHE_PROPERTY`, raw enum values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WriteCache {
    pub cache_type: u32,
    pub enabled: u32,
    pub write_through: u32,
    pub flush_supported: u8,
    pub user_power_protection: u8,
}

pub(crate) fn parse_write_cache(buf: &[u8]) -> Option<WriteCache> {
    let r = Reader::new(buf);
    Some(WriteCache {
        cache_type: r.u32(8)?,
        enabled: r.u32(12)?,
        write_through: r.u32(20)?,
        flush_supported: r.u8(24)?,
        user_power_protection: r.u8(25)?,
    })
}

/// `STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR`: `(logical, physical)` sector bytes.
pub(crate) fn parse_access_alignment(buf: &[u8]) -> Option<(u32, u32)> {
    let r = Reader::new(buf);
    Some((r.u32(16)?, r.u32(20)?))
}

/// `STORAGE_ADAPTER_DESCRIPTOR`: `(MaximumTransferLength, BusType)`.
pub(crate) fn parse_adapter(buf: &[u8]) -> Option<(u32, u8)> {
    let r = Reader::new(buf);
    Some((r.u32(8)?, r.u8(24)?))
}

/// `STORAGE_DEVICE_DESCRIPTOR` strings and bus.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct DeviceDesc {
    pub bus: i32,
    pub vendor: Option<String>,
    pub product: Option<String>,
    pub revision: Option<String>,
    pub serial: Option<String>,
}

pub(crate) fn parse_device_descriptor(buf: &[u8]) -> Option<DeviceDesc> {
    let r = Reader::new(buf);
    let string_at = |off: usize| -> Option<String> {
        let o = r.u32(off)? as usize;
        if o == 0 || o >= r.len() {
            return None;
        }
        r.ascii(o, r.len() - o)
    };
    Some(DeviceDesc {
        bus: r.u32(28)? as i32,
        vendor: string_at(12),
        product: string_at(16),
        revision: string_at(20),
        serial: string_at(24),
    })
}

/// `STORAGE_DEVICE_NUMA_PROPERTY`: the node, `None` when the device reports
/// no affinity (0xFFFFFFFF or a node that does not fit `u16`).
pub(crate) fn parse_numa(buf: &[u8]) -> Option<Option<u16>> {
    let node = Reader::new(buf).u32(8)?;
    Some(u16::try_from(node).ok())
}

/// `DISK_GEOMETRY_EX`: the disk's length in bytes (`DiskSize`, after the
/// 24-byte `DISK_GEOMETRY`).
pub(crate) fn parse_disk_size(buf: &[u8]) -> Option<u64> {
    Reader::new(buf).u64(24)
}

/// `STORAGE_DEVICE_NUMBER`: the disk number.
pub(crate) fn parse_device_number(buf: &[u8]) -> Option<u32> {
    Reader::new(buf).u32(4)
}

/// `VOLUME_DISK_EXTENTS`: every extent's disk number, in order, without
/// repeats, and whether the buffer held every extent the count names (24
/// bytes each from offset 8). `None` for no extent.
pub(crate) fn parse_all_disk_extents(buf: &[u8]) -> Option<(Vec<u32>, bool)> {
    let r = Reader::new(buf);
    let count = r.u32(0)? as usize;
    if count == 0 {
        return None;
    }
    let mut disks = Vec::new();
    for i in 0..count {
        let Some(d) = r.u32(8 + i * 24) else {
            return Some((disks, false));
        };
        if !disks.contains(&d) {
            disks.push(d);
        }
    }
    Some((disks, true))
}

/// `VOLUME_DISK_EXTENTS`: `(extent count, first extent's disk)`.
pub(crate) fn parse_disk_extents(buf: &[u8]) -> Option<(u32, u32)> {
    let r = Reader::new(buf);
    let count = r.u32(0)?;
    if count == 0 {
        return None;
    }
    Some((count, r.u32(8)?))
}

/// Maps a `STORAGE_BUS_TYPE` to the evidence vocabulary. A Storage Spaces
/// disk has no bus of its own (its pool's disks do): it is a stack layer
/// ([`layer_from_code`]), not a hypervisor's virtual disk.
pub(crate) fn bus_from_code(code: i32) -> Bus {
    match code {
        BUS_NVME => Bus::Nvme,
        BUS_SATA | BUS_ATA => Bus::Sata,
        BUS_SCSI | BUS_SAS | BUS_RAID => Bus::Scsi,
        BUS_ISCSI | BUS_FIBRE => Bus::Remote,
        BUS_USB => Bus::Usb,
        BUS_VIRTUAL | BUS_FILE_BACKED_VIRTUAL => Bus::Virtual,
        _ => Bus::Unknown,
    }
}

/// The stack layer a `STORAGE_BUS_TYPE` stands for, if any.
pub(crate) fn layer_from_code(code: i32) -> Option<StackLayer> {
    (code == BUS_SPACES).then_some(StackLayer::Pool)
}

/// Whether a raw SMBIOS table (`GetSystemFirmwareTable('RSMB')`: an 8-byte
/// header, then the structures) positively describes physical hardware: a
/// System Information (type 1) structure that gives both a manufacturer
/// and a product name, neither naming a virtual machine, and no BIOS Information (type 0) structure with
/// the "describes a virtual machine" bit (characteristics extension byte
/// 2, bit 4). A table without a type-1 structure, or malformed, describes
/// nothing: not physical.
pub(crate) fn smbios_says_physical(raw: &[u8]) -> bool {
    const GUESTS: [&str; 19] = [
        "vmware",
        "virtualbox",
        "innotek",
        "qemu",
        "kvm",
        "bochs",
        "xen",
        "parallels",
        "bhyve",
        "red hat",
        "nutanix",
        "amazon ec2",
        "google compute engine",
        "digitalocean",
        "hetzner",
        "alibaba",
        "openstack",
        "firecracker",
        "virtual machine",
    ];
    let Some(table) = raw.get(8..) else {
        return false;
    };
    let (mut system, mut guest) = (false, false);
    let mut at = 0usize;
    while let (Some(&kind), Some(&len)) = (table.get(at), table.get(at + 1)) {
        let len = usize::from(len);
        if len < 4 {
            return false;
        }
        let Some(formatted) = table.get(at..at + len) else {
            return false;
        };
        // The strings follow the formatted area, each NUL-terminated, the
        // set ending with an extra NUL.
        let strings_at = at + len;
        let mut end = strings_at;
        while table.get(end..end + 2).is_some_and(|w| w != [0, 0]) {
            end += 1;
        }
        if table.get(end..end + 2).is_none() {
            return false;
        }
        match kind {
            0 if formatted.get(0x13).is_some_and(|b| b & 0x10 != 0) => guest = true,
            1 => {
                let strings: Vec<&[u8]> = table[strings_at..end].split(|&b| b == 0).collect();
                let text = |index: Option<&u8>| {
                    index
                        .and_then(|&i| usize::from(i).checked_sub(1))
                        .and_then(|i| strings.get(i))
                        .map(|s| String::from_utf8_lossy(s).to_ascii_lowercase())
                        .unwrap_or_default()
                };
                let (maker, product) = (text(formatted.get(4)), text(formatted.get(5)));
                let name = format!("{maker} {product}");
                // A structure that names nothing describes nothing.
                if !maker.trim().is_empty() && !product.trim().is_empty() {
                    system = true;
                }
                if GUESTS.iter().any(|g| name.contains(g)) {
                    guest = true;
                }
            }
            127 => break,
            _ => {}
        }
        at = end + 2;
    }
    system && !guest
}

/// A hypervisor inferred from the disk's bus and model string. Windows runs
/// under Hyper-V as the root partition whenever virtualization-based
/// security is on, so a CPUID hypervisor bit would misreport most physical
/// Windows 11 machines; the virtual disk is the reliable signal.
pub(crate) fn hypervisor_from(bus: Bus, model: Option<&str>) -> Option<Hypervisor> {
    let m = model.unwrap_or("").to_ascii_lowercase();
    if m.contains("msft virtual") || m.contains("microsoft virtual") {
        return Some(Hypervisor::HyperV);
    }
    if m.contains("vmware") {
        return Some(Hypervisor::Vmware);
    }
    if m.contains("vbox")
        || m.contains("virtualbox")
        || m.contains("parallels")
        || m.contains("bhyve")
    {
        return Some(Hypervisor::Other);
    }
    if m.contains("qemu") || m.contains("virtio") {
        return Some(Hypervisor::Kvm);
    }
    if m.contains("xen") {
        return Some(Hypervisor::Xen);
    }
    if m.contains("amazon elastic block store") || m.contains("amazon ec2 nvme") {
        return Some(Hypervisor::Nitro);
    }
    if bus == Bus::Virtual {
        Some(Hypervisor::Other)
    } else {
        None
    }
}

/// The `\\.\PhysicalDriveN` name, NUL-terminated UTF-16.
#[cfg(windows)]
pub(crate) fn physical_drive_name(n: u32) -> Vec<u16> {
    crate::sys::wide_str(&format!("\\\\.\\PhysicalDrive{n}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le(v: &[u32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    #[test]
    fn test_property_query_and_nvme_query_layout() {
        let q = property_query(4);
        assert_eq!(q.len(), 12);
        assert_eq!(&q[..8], &le(&[4, 0]));
        let n = nvme_query(50, NvmeQuery::Identify { cns: 1, nsid: 0 }, 4096);
        assert_eq!(n.len(), 8 + 40 + 4096);
        assert_eq!(&n[..8], &le(&[50, 0]));
        assert_eq!(&n[8..32], &le(&[3, 1, 1, 0, 40, 4096]));
        let l = nvme_query(50, NvmeQuery::LogPage { lid: 2 }, 512);
        assert_eq!(&l[8..24], &le(&[3, 2, 2, 0]));
        let f = nvme_query(50, NvmeQuery::Feature { fid: 6 }, 4096);
        assert_eq!(&f[8..24], &le(&[3, 3, 6, 0]));
    }

    #[test]
    fn test_protocol_data_follows_descriptor_offsets_with_bounds() {
        let mut d = le(&[1, 48, 3, 1, 1, 0, 40, 4, 0xABCD, 0, 0, 0]);
        d.extend_from_slice(&[9, 8, 7, 6]);
        assert_eq!(protocol_data(&d), Some(&[9u8, 8, 7, 6][..]));
        assert_eq!(protocol_dword0(&d), Some(0xABCD));
        // Length past the buffer, offset inside the header, zero length.
        let mut long = d.clone();
        long[28..32].copy_from_slice(&5u32.to_le_bytes());
        assert_eq!(protocol_data(&long), None);
        let mut inside = d.clone();
        inside[24..28].copy_from_slice(&8u32.to_le_bytes());
        assert_eq!(protocol_data(&inside), None);
        let mut zero = d.clone();
        zero[28..32].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(protocol_data(&zero), None);
        let mut huge = d;
        huge[24..28].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(protocol_data(&huge), None);
        assert_eq!(protocol_data(&[0u8; 20]), None);
        assert_eq!(protocol_dword0(&[0u8; 10]), None);
    }

    #[test]
    fn test_parse_write_cache_and_truncation() {
        let mut b = le(&[1, 28, 2, 2, 1, 2]);
        b.extend_from_slice(&[1, 0, 0, 0]);
        let wc = parse_write_cache(&b);
        assert_eq!(
            wc,
            Some(WriteCache {
                cache_type: 2,
                enabled: 2,
                write_through: 2,
                flush_supported: 1,
                user_power_protection: 0,
            })
        );
        assert_eq!(parse_write_cache(&b[..25]), None);
        assert_eq!(parse_write_cache(&[]), None);
        // A BOOLEAN byte other than 0/1 is carried through, never trusted
        // as a Rust bool.
        b[25] = 0x7F;
        assert_eq!(
            parse_write_cache(&b).map(|w| w.user_power_protection),
            Some(0x7F)
        );
    }

    #[test]
    fn test_parse_alignment_adapter_numa_number_extents() {
        assert_eq!(
            parse_access_alignment(&le(&[1, 28, 64, 0, 512, 4096, 0])),
            Some((512, 4096))
        );
        assert_eq!(parse_access_alignment(&le(&[1, 28, 64, 0, 512])), None);
        let mut ad = le(&[1, 32, 524_288, 513, 3]);
        ad.extend_from_slice(&[0, 0, 1, 0, 17, 0, 1, 0, 0, 0, 1, 0]);
        assert_eq!(parse_adapter(&ad), Some((524_288, 17)));
        assert_eq!(parse_adapter(&ad[..24]), None);
        assert_eq!(parse_numa(&le(&[1, 12, 0])), Some(Some(0)));
        assert_eq!(parse_numa(&le(&[1, 12, 0xFFFF_FFFF])), Some(None));
        assert_eq!(parse_numa(&le(&[1, 12])), None);
        assert_eq!(parse_device_number(&le(&[7, 3, 1])), Some(3));
        assert_eq!(parse_device_number(&[1]), None);
        let mut ext = le(&[2, 0, 5, 0]);
        ext.extend_from_slice(&[0u8; 16]);
        assert_eq!(parse_disk_extents(&ext), Some((2, 5)));
        assert_eq!(parse_disk_extents(&le(&[0, 0, 5, 0])), None);
        assert_eq!(parse_disk_extents(&le(&[1])), None);
        // Two extents, disks 5 and 3; one disk twice; a count the buffer
        // cannot hold.
        let extent = |disk: u32| {
            let mut e = le(&[disk, 0]);
            e.extend_from_slice(&[0u8; 16]);
            e
        };
        let mut two = le(&[2, 0]);
        two.extend(extent(5));
        two.extend(extent(3));
        assert_eq!(parse_all_disk_extents(&two), Some((vec![5, 3], true)));
        let mut same = le(&[2, 0]);
        same.extend(extent(5));
        same.extend(extent(5));
        assert_eq!(parse_all_disk_extents(&same), Some((vec![5], true)));
        let mut short = le(&[3, 0]);
        short.extend(extent(5));
        assert_eq!(parse_all_disk_extents(&short), Some((vec![5], false)));
        assert_eq!(parse_all_disk_extents(&le(&[0, 0])), None);
        let mut geo = vec![0u8; 24];
        geo.extend_from_slice(&4_000_787_030_016u64.to_le_bytes());
        assert_eq!(parse_disk_size(&geo), Some(4_000_787_030_016));
        assert_eq!(parse_disk_size(&geo[..31]), None);
    }

    #[test]
    fn test_parse_device_descriptor_strings_and_hostile_offsets() {
        // Version, Size, type bytes, vendor/product/revision/serial offsets,
        // BusType, RawPropertiesLength; strings follow at 36.
        let header = le(&[1, 64, 0, 36, 44, 52, 0, 17, 20]);
        let mut b = header.clone();
        b.extend_from_slice(b"NVMe\0\0\0\0T-FORCE\0EIFM70.3\0  ");
        let d = parse_device_descriptor(&b).unwrap_or_default();
        assert_eq!(d.bus, 17);
        assert_eq!(d.serial, None);
        let mut with_serial = le(&[1, 80, 0, 36, 44, 52, 63, 17, 20]);
        with_serial.extend_from_slice(b"NVMe\0\0\0\0T-FORCE\0EIFM70.3\0  ");
        with_serial.extend_from_slice(b"S4EW_0001.\0\0\0\0\0\0");
        let d = parse_device_descriptor(&with_serial).unwrap_or_default();
        assert_eq!(d.serial.as_deref(), Some("S4EW_0001."));
        assert_eq!(d.vendor.as_deref(), Some("NVMe"));
        assert_eq!(d.product.as_deref(), Some("T-FORCE"));
        assert_eq!(d.revision.as_deref(), Some("EIFM70.3"));
        // Offsets beyond the buffer or zero give no string, not a panic.
        let mut bad = header;
        bad[12..16].copy_from_slice(&9999u32.to_le_bytes());
        bad[16..20].copy_from_slice(&0u32.to_le_bytes());
        bad[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        let d = parse_device_descriptor(&bad).unwrap_or_default();
        assert_eq!((d.vendor, d.product, d.revision), (None, None, None));
        assert_eq!(parse_device_descriptor(&[0u8; 31]), None);
    }

    /// An SMBIOS table with one type-1 structure naming `maker` and
    /// `product`, then the end marker.
    fn smbios(maker: &str, product: &str) -> Vec<u8> {
        let mut t = vec![0u8; 8];
        // Type 1, length 8: handle, then string indexes 1 (manufacturer)
        // and 2 (product), padded to the length.
        t.extend_from_slice(&[1, 8, 0, 0, 1, 2, 0, 0]);
        t.extend_from_slice(maker.as_bytes());
        t.push(0);
        t.extend_from_slice(product.as_bytes());
        t.push(0);
        t.push(0);
        t.extend_from_slice(&[127, 4, 0, 0, 0, 0]);
        t
    }

    #[test]
    fn test_smbios_says_physical_only_for_named_physical_hardware() {
        for (maker, product) in [
            ("innotek GmbH", "VirtualBox"),
            ("Microsoft Corporation", "Virtual Machine"),
            ("QEMU", "Standard PC (Q35 + ICH9, 2009)"),
            (
                "Parallels Software International Inc.",
                "Parallels Virtual Platform",
            ),
            ("Red Hat", "KVM"),
            ("Nutanix", "AHV"),
        ] {
            assert!(!smbios_says_physical(&smbios(maker, product)), "{maker}");
        }
        assert!(smbios_says_physical(&smbios(
            "Micro-Star International Co., Ltd.",
            "MS-7E12"
        )));
        assert!(smbios_says_physical(&smbios(
            "Microsoft Corporation",
            "Surface Laptop 7"
        )));
        // A type-0 structure with the "virtual machine" bit, whatever the
        // names say.
        let mut flagged = vec![0u8; 8];
        let mut bios = vec![0u8; 0x14];
        bios[1] = 0x14;
        bios[0x13] = 0x10;
        flagged.extend_from_slice(&bios);
        flagged.extend_from_slice(&[0, 0]);
        flagged.extend_from_slice(&smbios("Micro-Star International Co., Ltd.", "MS-7E12")[8..]);
        assert!(!smbios_says_physical(&flagged));
        // Malformed or empty tables, or none with a type-1 structure,
        // describe nothing: never physical, and never a panic.
        assert!(!smbios_says_physical(&[]));
        assert!(!smbios_says_physical(&[0u8; 8]));
        assert!(!smbios_says_physical(&[0, 0, 0, 0, 0, 0, 0, 0, 1, 2]));
        let mut cut = smbios("Micro-Star International Co., Ltd.", "MS-7E12");
        cut.truncate(14);
        assert!(!smbios_says_physical(&cut));
        let mut only_bios = vec![0u8; 8];
        only_bios.extend_from_slice(&[0, 4, 0, 0, 0, 0, 127, 4, 0, 0, 0, 0]);
        assert!(!smbios_says_physical(&only_bios));
        // A type-1 structure without its maker or its product.
        assert!(!smbios_says_physical(&smbios("", "MS-7E12")));
        assert!(!smbios_says_physical(&smbios(" ", "MS-7E12")));
        // Firmware without a maker string gives index 0.
        let mut no_maker = smbios("Micro-Star International Co., Ltd.", "MS-7E12");
        no_maker[8 + 4] = 0;
        assert!(!smbios_says_physical(&no_maker));
        assert!(!smbios_says_physical(&smbios(
            "Micro-Star International Co., Ltd.",
            " "
        )));
    }

    #[test]
    fn test_bus_and_hypervisor_mapping() {
        assert_eq!(bus_from_code(17), Bus::Nvme);
        assert_eq!(bus_from_code(11), Bus::Sata);
        assert_eq!(bus_from_code(3), Bus::Sata);
        assert_eq!(bus_from_code(10), Bus::Scsi);
        assert_eq!(bus_from_code(8), Bus::Scsi);
        assert_eq!(bus_from_code(9), Bus::Remote);
        assert_eq!(bus_from_code(6), Bus::Remote);
        assert_eq!(bus_from_code(7), Bus::Usb);
        assert_eq!(bus_from_code(14), Bus::Virtual);
        assert_eq!(bus_from_code(15), Bus::Virtual);
        // Storage Spaces: a pool layer over no known bus, no hypervisor.
        assert_eq!(bus_from_code(16), Bus::Unknown);
        assert_eq!(layer_from_code(16), Some(StackLayer::Pool));
        assert_eq!(
            hypervisor_from(bus_from_code(16), Some("Microsoft Storage Space Device")),
            None
        );
        for code in [-1, 1, 9, 14, 17, 99] {
            assert_eq!(layer_from_code(code), None, "{code}");
        }
        assert_eq!(bus_from_code(-1), Bus::Unknown);
        assert_eq!(bus_from_code(99), Bus::Unknown);
        assert_eq!(
            hypervisor_from(Bus::Virtual, Some("Msft Virtual Disk")),
            Some(Hypervisor::HyperV)
        );
        assert_eq!(
            hypervisor_from(Bus::Scsi, Some("VMware Virtual disk")),
            Some(Hypervisor::Vmware)
        );
        assert_eq!(
            hypervisor_from(Bus::Scsi, Some("QEMU HARDDISK")),
            Some(Hypervisor::Kvm)
        );
        assert_eq!(
            hypervisor_from(Bus::Nvme, Some("Amazon Elastic Block Store")),
            Some(Hypervisor::Nitro)
        );
        assert_eq!(
            hypervisor_from(Bus::Virtual, Some("Something")),
            Some(Hypervisor::Other)
        );
        assert_eq!(hypervisor_from(Bus::Nvme, Some("T-FORCE TM8FPZ004T")), None);
        assert_eq!(hypervisor_from(Bus::Nvme, None), None);
    }

    #[test]
    fn test_physical_drive_name_is_nul_terminated() {
        let n = physical_drive_name(3);
        assert_eq!(
            String::from_utf16_lossy(&n[..n.len() - 1]),
            "\\\\.\\PhysicalDrive3"
        );
        assert_eq!(n.last(), Some(&0));
    }
}
