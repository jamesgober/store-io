//! The macOS evidence probe: the volume from `fstatfs`, its device from
//! DiskArbitration and IOKit (internal or not, interconnect, model,
//! removable media, Thunderbolt tunnels, RAID and multi-store lineage),
//! the release, the hypervisor flag, and the flush check on a writable
//! container and its directory. Only an internal device is qualified
//! (`DeviceNotInternal` otherwise); what cannot be read stays unknown, which
//! never raises the class. Nothing needs privilege. The probe never decides
//! a class and never writes a file.

use std::ffi::CString;
use std::os::fd::RawFd;

use store_io_core::class::Missing;
use store_io_core::error::OsError;
use store_io_core::evidence::{Bus, Evidence, FsKind, Hypervisor, PlatformKind, StackLayer, Tri};
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
    let device = bsd_name(&volume.from).and_then(|name| sys::device(&name));
    apply_device(&mut ev, device.as_ref(), volume.removable);
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

/// The BSD disk name of a volume mounted from `/dev/<disk>` (`disk3s5`), or
/// `None` (a share, `devfs`, anything else).
fn bsd_name(from: &str) -> Option<CString> {
    let name = from.strip_prefix("/dev/")?;
    let ok = name.starts_with("disk")
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric());
    ok.then(|| CString::new(name).ok()).flatten()
}

/// The device facts from its description (`None`: not a disk, or none to
/// read) and the mount's `MNT_REMOVABLE`.
fn apply_device(ev: &mut Evidence, device: Option<&sys::Device>, removable_mount: bool) {
    let protocol = device.and_then(|d| d.protocol.as_deref());
    let image = device.is_some_and(is_disk_image);
    // A disk image's "Virtual Interface" is not a hypervisor's disk.
    ev.device.bus = if image {
        Bus::Unknown
    } else {
        bus_of(protocol)
    };
    ev.device.model = device.and_then(|d| d.model.clone());
    if image {
        ev.stack.push(StackLayer::VirtualDisk);
    }
    ev.device.internal = internal_of(device, removable_mount);
}

/// Whether the device is a disk image (its blocks another file system's
/// file).
fn is_disk_image(d: &sys::Device) -> bool {
    d.protocol.as_deref() == Some("Disk Image")
        || d.model
            .as_deref()
            .is_some_and(|m| m.starts_with("Disk Image"))
}

/// The interconnects a built-in drive is on, on an Intel Mac: Apple's own
/// fabric (T2), PCIe, SATA. A Thunderbolt enclosure reports the same PCIe
/// or SATA, told apart by `IOPCITunnelled`, which IOPCIFamily sets on every
/// device below an ACPI Thunderbolt root.
const INTEL_PROTOCOLS: &[&str] = &["Apple Fabric", "PCI-Express", "SATA"];

/// The interconnect a built-in drive is on, on Apple silicon: only Apple's
/// own fabric. Its Thunderbolt ports' tunnels are not known to be marked
/// `IOPCITunnelled`, so a PCIe or SATA drive there (an enclosure; a Mac
/// Pro's PCIe card) is never taken for internal.
const APPLE_SILICON_PROTOCOLS: &[&str] = &["Apple Fabric"];

/// This machine's [`INTEL_PROTOCOLS`] or [`APPLE_SILICON_PROTOCOLS`], by
/// the machine, not the build ([`sys::apple_silicon`]: Rosetta).
fn internal_protocols() -> &'static [&'static str] {
    if sys::apple_silicon() {
        APPLE_SILICON_PROTOCOLS
    } else {
        INTEL_PROTOCOLS
    }
}

/// Whether the model names a virtual machine's disk (a file on the host).
fn is_vm_disk(d: &sys::Device) -> bool {
    d.model.as_deref().is_some_and(|m| {
        let m = m.to_ascii_lowercase();
        [
            "vmware",
            "vbox",
            "virtualbox",
            "parallels",
            "qemu",
            "virtio",
        ]
        .iter()
        .any(|v| m.contains(v))
    })
}

/// Whether the device is built into the machine. `Yes` only on all of:
/// the description calls it internal; its media is known to be neither
/// removable nor ejectable; the mount is not removable; its interconnect is
/// one a built-in drive is on ([`internal_protocols`]); it is not a disk
/// image nor a virtual machine's disk; IOKit shows its media on a single
/// line of parents (no RAID set or multi-store container, of which the
/// description names one member) hanging from no Thunderbolt-tunnelled PCIe
/// link (`IOPCITunnelled`); and its registry path names no Thunderbolt or
/// USB controller (an external enclosure's bridge, which may report itself
/// internal and may complete a flush early). `No` where any of those is
/// known false; `Unknown` where one cannot be read. An allowlist: an
/// interconnect not named (an SD card reader, FireWire) is never internal.
fn internal_of(device: Option<&sys::Device>, removable_mount: bool) -> Tri {
    internal_among(device, removable_mount, internal_protocols())
}

/// [`internal_of`] with the interconnects a built-in drive is on given.
fn internal_among(device: Option<&sys::Device>, removable_mount: bool, allowed: &[&str]) -> Tri {
    if removable_mount {
        return Tri::No;
    }
    let Some(d) = device else {
        return Tri::Unknown;
    };
    let bridged = d
        .path
        .as_deref()
        .is_some_and(|p| p.contains("Thunderbolt") || p.contains("USB"));
    let known_bus = d.protocol.as_deref().is_some_and(|p| allowed.contains(&p));
    let external = d.removable == Some(true)
        || d.ejectable == Some(true)
        || d.tunnelled == Some(true)
        || d.single == Some(false)
        || bridged
        || is_disk_image(d)
        || is_vm_disk(d)
        || !known_bus;
    let fixed = d.removable == Some(false) && d.ejectable == Some(false);
    // The path backstop needs a path to have looked at.
    let lineage = d.tunnelled == Some(false) && d.single == Some(true) && d.path.is_some();
    match (external, d.internal) {
        (true, _) | (false, Some(false)) => Tri::No,
        (false, Some(true)) if fixed && lineage => Tri::Yes,
        _ => Tri::Unknown,
    }
}

/// The interconnect from the description's protocol.
fn bus_of(protocol: Option<&str>) -> Bus {
    match protocol {
        Some("USB") => Bus::Usb,
        Some("SATA" | "ATA") => Bus::Sata,
        Some("PCI-Express" | "PCI" | "NVMe" | "Apple Fabric") => Bus::Nvme,
        Some("SAS" | "SCSI" | "Fibre Channel Interface") => Bus::Scsi,
        Some("Virtual Interface") => Bus::Virtual,
        _ => Bus::Unknown,
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
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn test_volume_names_map_to_their_kinds() {
        let kind = |fs: &str, local| {
            fs_kind(&sys::Volume {
                fs: fs.to_owned(),
                local,
                removable: false,
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

    #[test]
    fn test_only_a_dev_disk_names_a_disk() {
        let name = |s: &str| bsd_name(s).map(|c| c.into_string().unwrap());
        assert_eq!(name("/dev/disk3s5").as_deref(), Some("disk3s5"));
        assert_eq!(name("/dev/disk0").as_deref(), Some("disk0"));
        assert_eq!(name("disk3s5"), None);
        assert_eq!(name("/dev/rdisk3"), None);
        assert_eq!(name("/dev/disk3s5/x"), None);
        assert_eq!(name("//server/share"), None);
        assert_eq!(name("devfs"), None);
    }

    #[test]
    fn test_only_a_built_in_device_is_internal() {
        let dev = |internal, protocol: &str, removable, ejectable| sys::Device {
            internal,
            protocol: Some(protocol.to_owned()),
            model: None,
            removable,
            ejectable,
            path: Some("IOService:/AppleARMPE/arm-io/AppleT600xIO/ans@8F400000".to_owned()),
            tunnelled: Some(false),
            single: Some(true),
        };
        let no = Some(false);
        let yes = Some(true);
        let internal = |d: &sys::Device, mount| internal_among(Some(d), mount, INTEL_PROTOCOLS);
        assert_eq!(internal(&dev(yes, "Apple Fabric", no, no), false), Tri::Yes);
        assert_eq!(internal(&dev(yes, "PCI-Express", no, no), false), Tri::Yes);
        assert_eq!(internal(&dev(yes, "SATA", no, no), false), Tri::Yes);
        assert_eq!(internal(&dev(yes, "Apple Fabric", no, no), true), Tri::No);
        assert_eq!(internal(&dev(no, "PCI-Express", no, no), false), Tri::No);
        assert_eq!(internal(&dev(yes, "USB", no, no), false), Tri::No);
        assert_eq!(internal(&dev(yes, "Disk Image", no, no), false), Tri::No);
        assert_eq!(internal(&dev(yes, "SATA", yes, no), false), Tri::No);
        assert_eq!(internal(&dev(yes, "SATA", no, yes), false), Tri::No);
        // Not on the list: an SD card in a built-in reader, FireWire, none.
        assert_eq!(
            internal(&dev(yes, "Secure Digital", no, no), false),
            Tri::No
        );
        assert_eq!(internal(&dev(yes, "FireWire", no, no), false), Tri::No);
        let mut none = dev(yes, "SATA", no, no);
        none.protocol = None;
        assert_eq!(internal(&none, false), Tri::No);
        // A PCIe link tunnelled over Thunderbolt (an AHCI or NVMe
        // enclosure that says internal), or one whose tunnel is unknown.
        let mut tunnel = dev(yes, "SATA", no, no);
        tunnel.tunnelled = Some(true);
        assert_eq!(internal(&tunnel, false), Tri::No);
        tunnel.tunnelled = None;
        assert_eq!(internal(&tunnel, false), Tri::Unknown);
        // Removable and ejectable not described: not known to be fixed.
        let mut vague = dev(yes, "SATA", None, no);
        assert_eq!(internal(&vague, false), Tri::Unknown);
        vague.removable = no;
        vague.ejectable = None;
        assert_eq!(internal(&vague, false), Tri::Unknown);
        // No device path (or one too long to read): the backstop saw nothing.
        let mut pathless = dev(yes, "Apple Fabric", no, no);
        pathless.path = None;
        assert_eq!(internal(&pathless, false), Tri::Unknown);
        // A RAID set or multi-store container, or one IOKit cannot walk.
        let mut set = dev(yes, "PCI-Express", no, no);
        set.single = no;
        assert_eq!(internal(&set, false), Tri::No);
        set.single = None;
        assert_eq!(internal(&set, false), Tri::Unknown);
        // A disk image on an allowlisted protocol; a virtual machine's disk.
        let mut image = dev(yes, "SATA", no, no);
        image.model = Some("Disk Image".to_owned());
        assert_eq!(internal(&image, false), Tri::No);
        let mut vm = dev(yes, "SATA", no, no);
        vm.model = Some("VMware Virtual SATA Hard Drive".to_owned());
        assert_eq!(internal(&vm, false), Tri::No);
        vm.model = Some("Apple VirtIO Block Device".to_owned());
        assert_eq!(internal(&vm, false), Tri::No);
        assert_eq!(internal(&dev(yes, "NVMe", no, no), false), Tri::No);
        // This machine's list: Apple silicon takes only its own fabric.
        let pcie = internal_of(Some(&dev(yes, "PCI-Express", no, no)), false);
        let want = if sys::apple_silicon() {
            Tri::No
        } else {
            Tri::Yes
        };
        assert_eq!(pcie, want);
        // Apple silicon: only its own fabric is internal.
        let silicon = |d: &sys::Device| internal_among(Some(d), false, APPLE_SILICON_PROTOCOLS);
        assert_eq!(silicon(&dev(yes, "Apple Fabric", no, no)), Tri::Yes);
        assert_eq!(silicon(&dev(yes, "PCI-Express", no, no)), Tri::No);
        assert_eq!(silicon(&dev(yes, "SATA", no, no)), Tri::No);
        assert_eq!(
            internal_of(Some(&dev(yes, "Apple Fabric", no, no)), false),
            Tri::Yes
        );
        assert_eq!(internal(&dev(yes, "PCI", no, no), false), Tri::No);
        // An enclosure behind a Thunderbolt or USB bridge that says internal.
        let mut bridged = dev(yes, "PCI-Express", no, no);
        bridged.path =
            Some("IOService:/AppleACPIPlatformExpert/PCI0/IOThunderboltSwitch".to_owned());
        assert_eq!(internal(&bridged, false), Tri::No);
        bridged.path = Some("IOService:/.../AppleUSBXHCI/USB3.0 Hub".to_owned());
        assert_eq!(internal(&bridged, false), Tri::No);
        // A disk image known by its model.
        let mut image = dev(yes, "Virtual Interface", no, no);
        image.model = Some("Disk Image".to_owned());
        assert_eq!(internal(&image, false), Tri::No);
        assert!(is_disk_image(&image));
        assert_eq!(internal(&dev(None, "SATA", no, no), false), Tri::Unknown);
        assert_eq!(internal_of(None, false), Tri::Unknown);
        assert_eq!(internal_of(None, true), Tri::No);
        assert_eq!(bus_of(Some("USB")), Bus::Usb);
        assert_eq!(bus_of(Some("SATA")), Bus::Sata);
        assert_eq!(bus_of(Some("ATA")), Bus::Sata);
        assert_eq!(bus_of(Some("SAS")), Bus::Scsi);
        assert_eq!(bus_of(Some("PCI-Express")), Bus::Nvme);
        assert_eq!(bus_of(Some("Apple Fabric")), Bus::Nvme);
        assert_eq!(bus_of(Some("Virtual Interface")), Bus::Virtual);
        assert_eq!(bus_of(Some("Carrier Pigeon")), Bus::Unknown);
        assert_eq!(bus_of(None), Bus::Unknown);
    }

    /// The device behind the temporary directory's volume (a `/dev/disk`
    /// volume on every macOS runner) is described.
    #[test]
    fn test_the_temp_volumes_device_is_described() {
        let f = std::fs::File::open(std::env::temp_dir()).unwrap();
        let v = sys::volume(std::os::fd::AsRawFd::as_raw_fd(&f)).unwrap();
        let name = bsd_name(&v.from).unwrap_or_else(|| panic!("{v:?}"));
        // A CI runner's virtual disk describes no protocol or model (seen:
        // internal true, protocol none), so it is never classed internal.
        let d = sys::device(&name).unwrap_or_else(|| panic!("{v:?}: no description"));
        assert!(d.internal.is_some(), "{v:?} {d:?}");
        // IOKit walks the runner's disk: not tunnelled, its lineage read.
        assert!(
            d.tunnelled == Some(false) && d.single.is_some(),
            "{v:?} {d:?}"
        );
    }

    /// A RAM disk image (`hdiutil attach -nomount ram://`, no privilege) is
    /// a disk image: never internal, a virtual-disk layer.
    #[test]
    fn test_a_disk_image_is_not_internal() {
        let out = std::process::Command::new("hdiutil")
            .args(["attach", "-nomount", "ram://4096"])
            .output();
        let out = out.unwrap();
        assert!(out.status.success(), "hdiutil: {out:?}");
        let dev = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        // DiskArbitration may describe a just-attached disk a moment later.
        let d = bsd_name(&dev).and_then(|n| {
            (0..50).find_map(|_| {
                sys::device(&n).or_else(|| {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    None
                })
            })
        });
        let _detached = std::process::Command::new("hdiutil")
            .args(["detach", &dev])
            .output();
        let d = d.unwrap_or_else(|| panic!("{dev}: no description"));
        assert_eq!(internal_of(Some(&d), false), Tri::No, "{d:?}");
        assert!(is_disk_image(&d), "{d:?}");
    }
}
