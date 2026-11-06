//! Device identity on Linux (PLANNING R5.2): every whole disk under the
//! file system, with the identifiers sysfs reports for it, readable without
//! privilege.
//!
//! An identity is complete only when every leaf is a disk the probe can see
//! whole: a local NVMe disk with its controller's serial or subsystem name,
//! a USB disk with its bridge named, or a SATA disk; and only on bare metal,
//! on ext4 with its journal inside, XFS with no log or realtime device
//! elsewhere, or a raw block device; and with no member hidden from the
//! walk (bcache's cache device is never listed as a slave). A SCSI
//! or SAS disk may be a hardware RAID's logical drive (two of them can share
//! one array), a disk of unknown bus may be a ZFS volume or a
//! persistent-memory namespace sharing a pool, memory devices share the
//! machine's memory, a cloud volume (EBS, a persistent disk) is a remote
//! target behind a local NVMe controller, a loop device is a file on a
//! file system the walk does not judge, a virtual machine's disks may share
//! a host disk, and other file systems may keep data on other devices: all
//! of those are incomplete, never distinct.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use store_io_core::evidence::{Bus, FsKind};
use store_io_core::identity::{DeviceId, DeviceIdentity, DeviceNode};

use super::block::{self, read_sysfs};
use super::split_dev;

/// How many loop devices deep a backing file is followed.
const LOOP_DEPTH: usize = 4;

/// What decides completeness beyond the walk itself.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Context {
    /// A hypervisor was detected: the disks may be files on a host disk.
    pub hosted: bool,
    /// The file system keeps the file's data on the device `st_dev` names
    /// (ext4, XFS without a log or realtime device elsewhere, or a raw
    /// block device).
    pub single_device: bool,
}

/// The identity of the device numbered `dev` (a file system's `st_dev`, or a
/// block device's own number). Anonymous devices (major 0: btrfs, overlay,
/// network and FUSE file systems, tmpfs) are unknown.
pub(crate) fn of(dev: (u32, u32), cx: Context) -> DeviceIdentity {
    let mut id = DeviceIdentity {
        devices: Vec::new(),
        complete: cx.single_device,
    };
    add(&mut id, dev, cx.hosted, 0);
    if id.devices.is_empty() {
        id.complete = false;
    }
    id
}

/// Whether a leaf counts toward a complete identity: an NVMe disk with its
/// controller named and a USB disk with its bridge named (`named`: the
/// namespaces of one controller, the LUNs behind one bridge, are one
/// device), and a SATA disk, on bare metal only; nothing else (SCSI and SAS
/// may be a RAID's logical drives, an unknown bus may be a pooled volume,
/// memory devices share the machine's memory, a virtual or remote disk
/// hides its device).
pub(crate) fn leaf_counts(bus: Bus, hosted: bool, named: bool) -> bool {
    match bus {
        Bus::Nvme | Bus::Usb => !hosted && named,
        Bus::Sata => !hosted,
        _ => false,
    }
}

/// The USB bridge a disk hangs from, as the sysfs path of its USB device
/// (everything before the SCSI host the bridge presents): the LUNs of one
/// enclosure or dock share it.
pub(crate) fn usb_bridge(device_path: &str) -> Option<String> {
    let at = device_path.find("/host")?;
    let bridge = &device_path[..at];
    bridge
        .contains("/usb")
        .then(|| format!("usb-bridge:{bridge}"))
}

/// Whether ext4 keeps its journal inside the file system: jbd2 names an
/// internal journal `<device>-<inode>` and an external one by the journal
/// device's own name, so only the first form is on the store's device (an
/// ext4 without a journal has no entry: not counted either).
pub(crate) fn jbd2_internal(entries: &[String], devname: &str) -> bool {
    entries.iter().any(|e| {
        e.strip_prefix(devname)
            .and_then(|r| r.strip_prefix('-'))
            .is_some_and(|ino| !ino.is_empty() && ino.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// Whether an NVMe model names a cloud volume (a remote target behind a
/// local controller, even on a bare-metal instance).
pub(crate) fn cloud_volume(model: Option<&str>) -> bool {
    model.is_some_and(|m| {
        let m = m.to_ascii_lowercase();
        m.contains("amazon elastic block store") || m.contains("nvme_card-pd")
    })
}

/// Whether a file system's data stays on the one device `st_dev` names:
/// ext4 with its journal inside (`ext4_internal`, read only for ext4: its
/// mount options never show an external journal), or XFS whose mount
/// (`xfs_options`, read only for XFS) has no log or realtime device
/// elsewhere.
pub(crate) fn fs_counts(
    kind: FsKind,
    xfs_options: impl FnOnce() -> Option<String>,
    ext4_internal: impl FnOnce() -> bool,
) -> bool {
    match kind {
        FsKind::Ext4 => ext4_internal(),
        FsKind::Xfs => xfs_options().is_some_and(|o| !xfs_uses_other_devices(&o)),
        _ => false,
    }
}

/// Whether an XFS mount keeps part of the file system on another device
/// (`logdev=`, `rtdev=` in its super options).
pub(crate) fn xfs_uses_other_devices(super_options: &str) -> bool {
    super_options
        .split(',')
        .any(|o| o.starts_with("logdev=") || o.starts_with("rtdev="))
}

fn add(id: &mut DeviceIdentity, dev: (u32, u32), hosted: bool, depth: usize) {
    if dev.0 == 0 || depth > LOOP_DEPTH {
        id.complete = false;
        return;
    }
    let Some(walk) = block::walk(dev.0, dev.1) else {
        id.complete = false;
        return;
    };
    if walk.uncertain || walk.hidden_members || walk.leaves.is_empty() {
        id.complete = false;
    }
    for leaf in &walk.leaves {
        if leaf.join("loop").is_dir() {
            // A loop device is a file on another device: that device is
            // named (so two loop devices on one disk are one device), but
            // the backing file's own file system is not judged here, so the
            // identity is never complete.
            id.complete = false;
            if let Some(d) = backing_device(leaf) {
                add(id, d, hosted, depth + 1);
            }
            continue;
        }
        match leaf_id(leaf, hosted) {
            Some((d, counts)) => {
                if !counts {
                    id.complete = false;
                }
                if !id.devices.contains(&d) {
                    id.devices.push(d);
                }
            }
            None => id.complete = false,
        }
    }
}

/// The device holding a loop device's backing file. The path is the one
/// the kernel recorded at setup, read in this process's mount namespace: a
/// backing path that is now a link, or names nothing, gives no answer (a
/// file renamed or replaced since setup could be on another device). A
/// backing file renamed within its file system still resolves to that file
/// system's device, which is the one that matters.
fn backing_device(leaf: &Path) -> Option<(u32, u32)> {
    let file = read_sysfs(&leaf.join("loop/backing_file"))?;
    // The kernel appends " (deleted)" to a backing file that was unlinked:
    // its device is still the one it was on, but its path names nothing.
    if file.ends_with("(deleted)") {
        return None;
    }
    let md = std::fs::symlink_metadata(PathBuf::from(file)).ok()?;
    if md.file_type().is_symlink() {
        return None;
    }
    Some(split_dev(md.dev()))
}

/// A leaf disk's node and names, and whether it counts toward a complete
/// identity ([`leaf_counts`]); `None` when its node cannot be read.
fn leaf_id(leaf: &Path, hosted: bool) -> Option<(DeviceId, bool)> {
    let facts = block::leaf_facts(leaf);
    let (major, minor) = parse_dev(&read_sysfs(&leaf.join("dev"))?)?;
    let mut d = DeviceId::node(DeviceNode::Linux { major, minor });
    // The namespace's or LUN's world-wide id, then the controller's or
    // device's serial and, for NVMe, the subsystem's name: two namespaces of
    // one controller share the serial and the NQN.
    for attr in ["wwid", "device/wwid"] {
        if let Some(v) = read_sysfs(&leaf.join(attr)) {
            d.add_name(&v);
        }
    }
    let before = d.names.len();
    for attr in ["device/serial", "device/subsysnqn"] {
        if let Some(v) = read_sysfs(&leaf.join(attr)) {
            d.add_name(&v);
        }
    }
    if facts.bus == Bus::Usb {
        let path = std::fs::canonicalize(leaf.join("device")).ok();
        if let Some(b) = path.and_then(|p| usb_bridge(&p.to_string_lossy())) {
            d.add_name(&b);
        }
    }
    let named = d.names.len() > before;
    let counts = leaf_counts(facts.bus, hosted, named) && !cloud_volume(facts.model.as_deref());
    Some((d, counts))
}

/// Parses sysfs `dev` (`major:minor`).
pub(crate) fn parse_dev(s: &str) -> Option<(u32, u32)> {
    let (a, b) = s.trim().split_once(':')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dev_reads_major_and_minor() {
        assert_eq!(parse_dev("259:0\n"), Some((259, 0)));
        assert_eq!(parse_dev("8:16"), Some((8, 16)));
        assert_eq!(parse_dev("8"), None);
        assert_eq!(parse_dev("a:1"), None);
    }

    #[test]
    fn test_an_anonymous_device_is_unknown() {
        let cx = Context {
            hosted: false,
            single_device: true,
        };
        let id = of((0, 42), cx);
        assert!(!id.complete);
        assert!(id.devices.is_empty());
    }

    #[test]
    fn test_only_disks_seen_whole_count() {
        // Bare metal.
        assert!(leaf_counts(Bus::Nvme, false, true));
        assert!(!leaf_counts(Bus::Nvme, false, false), "no controller name");
        assert!(leaf_counts(Bus::Usb, false, true));
        assert!(!leaf_counts(Bus::Usb, false, false), "no bridge name");
        assert!(leaf_counts(Bus::Sata, false, false));
        for bus in [Bus::Scsi, Bus::Unknown, Bus::Virtual, Bus::Remote, Bus::Ram] {
            assert!(!leaf_counts(bus, false, true), "{bus:?}");
        }
        // Nothing counts in a guest.
        for bus in [Bus::Nvme, Bus::Sata, Bus::Usb, Bus::Ram] {
            assert!(!leaf_counts(bus, true, true), "{bus:?}");
        }
    }

    #[test]
    fn test_luns_behind_one_usb_bridge_share_its_name() {
        let a = "/sys/devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host6/target6:0:0/6:0:0:0";
        let b = "/sys/devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host6/target6:0:0/6:0:0:1";
        let c = "/sys/devices/pci0000:00/0000:00:14.0/usb2/2-2/2-2:1.0/host7/target7:0:0/7:0:0:0";
        assert_eq!(usb_bridge(a), usb_bridge(b));
        assert_ne!(usb_bridge(a), usb_bridge(c));
        assert!(usb_bridge(a).is_some());
        assert_eq!(
            usb_bridge("/sys/devices/pci0000:00/0000:00:1f.2/ata1/host0/target0:0:0/0:0:0:0"),
            None
        );
        assert_eq!(usb_bridge("/sys/devices/virtual/block/nullb0"), None);
    }

    #[test]
    fn test_only_an_internal_ext4_journal_counts() {
        let e = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert!(jbd2_internal(&e(&["sda1-8"]), "sda1"));
        assert!(jbd2_internal(&e(&["nvme0n1p2-8", "sdb1-8"]), "nvme0n1p2"));
        assert!(!jbd2_internal(&e(&["sdb2"]), "sda1"), "an external journal");
        assert!(!jbd2_internal(&e(&["sda10-8"]), "sda1"));
        assert!(!jbd2_internal(&e(&["sda1-"]), "sda1"));
        // A neighbour's name that only starts with this one (`sda18`).
        assert!(!jbd2_internal(&e(&["sda18"]), "sda1"));
        assert!(!jbd2_internal(&[], "sda1"), "no journal");
    }

    #[test]
    fn test_only_ext4_and_xfs_on_one_device_count() {
        let plain = || Some("rw,attr2".to_owned());
        assert!(fs_counts(FsKind::Ext4, || None, || true));
        assert!(
            !fs_counts(FsKind::Ext4, plain, || false),
            "journal elsewhere or none"
        );
        assert!(fs_counts(FsKind::Xfs, plain, || false));
        assert!(!fs_counts(
            FsKind::Xfs,
            || Some("rw,logdev=/dev/sdb1".to_owned()),
            || true
        ));
        assert!(!fs_counts(FsKind::Xfs, || None, || true), "mount not found");
        for kind in [FsKind::Btrfs, FsKind::Tmpfs, FsKind::Network, FsKind::Other] {
            assert!(!fs_counts(kind, plain, || true), "{kind:?}");
        }
    }

    #[test]
    fn test_a_cloud_volume_behind_a_local_controller_is_named() {
        assert!(cloud_volume(Some("Amazon Elastic Block Store")));
        assert!(cloud_volume(Some("nvme_card-pd")));
        assert!(!cloud_volume(Some("Amazon EC2 NVMe Instance Storage")));
        assert!(!cloud_volume(Some("Samsung SSD 990 PRO 2TB")));
        assert!(!cloud_volume(None));
    }

    #[test]
    fn test_an_xfs_log_or_realtime_device_elsewhere_is_seen() {
        assert!(xfs_uses_other_devices("rw,attr2,logdev=/dev/sdb1,noquota"));
        assert!(xfs_uses_other_devices("rw,rtdev=/dev/sdc"));
        assert!(!xfs_uses_other_devices(
            "rw,attr2,inode64,logbufs=8,noquota"
        ));
        assert!(!xfs_uses_other_devices(""));
    }
}
