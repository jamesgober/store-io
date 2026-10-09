//! The block stack: the sysfs walk from a device number through partitions,
//! dm, md and loop devices to the leaf disk, the leaf's queue attributes, and
//! NVMe Identify, Get Features and SMART through the device node when it
//! can be opened. Unprivileged throughout; what cannot be read is listed in
//! `missing`.

use std::fs;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use store_io_core::class::{Missing, MissingSet};
use store_io_core::evidence::{AtomicFields, Bus, OsCacheMode, Privilege, StackLayer, Tri};

use crate::sys;

/// Most layers walked below a filesystem.
const MAX_DEPTH: usize = 16;
/// Most sysfs nodes visited in one walk.
const MAX_NODES: usize = 64;
/// Most `slaves/` entries considered per node.
const MAX_SLAVES: usize = 64;
/// Longest sysfs attribute read.
const SYSFS_READ_MAX: usize = 4096;
/// Longest string kept from a device (model, firmware).
const STRING_MAX: usize = 128;

/// Reads at most `max` bytes of a file, lossily as UTF-8.
pub(crate) fn read_bounded(path: &Path, max: usize) -> Option<String> {
    let mut f = fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    let _n = f.by_ref().take(max as u64).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Reads a sysfs attribute, trimmed.
pub(crate) fn read_sysfs(path: &Path) -> Option<String> {
    read_bounded(path, SYSFS_READ_MAX).map(|s| s.trim().to_owned())
}

// ----- the walk -------------------------------------------------------------

/// Result of a stack walk.
#[derive(Debug, Default)]
pub(crate) struct StackWalk {
    /// Layers in first-seen order, without duplicates.
    pub layers: Vec<StackLayer>,
    /// The first leaf disk reached (members of a mirror share one report).
    pub leaf: Option<PathBuf>,
    /// A node could not be classified with confidence.
    pub uncertain: bool,
}

/// Walks `/sys/dev/block/<major>:<minor>` down to the leaf devices.
pub(crate) fn walk(major: u32, minor: u32) -> Option<StackWalk> {
    let start = fs::canonicalize(format!("/sys/dev/block/{major}:{minor}")).ok()?;
    let mut w = StackWalk::default();
    let mut visited = 0;
    visit(&mut w, start, 0, &mut visited);
    Some(w)
}

fn push(w: &mut StackWalk, layer: StackLayer) {
    if !w.layers.contains(&layer) {
        w.layers.push(layer);
    }
}

fn visit(w: &mut StackWalk, node: PathBuf, depth: usize, visited: &mut usize) {
    if depth > MAX_DEPTH || *visited >= MAX_NODES {
        w.uncertain = true;
        return;
    }
    *visited += 1;
    // A partition is a linear window onto its parent disk.
    let node = if node.join("partition").is_file() {
        push(w, StackLayer::Linear);
        match node.parent() {
            Some(p) => p.to_path_buf(),
            None => {
                w.uncertain = true;
                return;
            }
        }
    } else {
        node
    };

    if let Some(uuid) = read_sysfs(&node.join("dm/uuid")) {
        let name = read_sysfs(&node.join("dm/name")).unwrap_or_default();
        match classify_dm(&name, &uuid) {
            Some(l) => push(w, l),
            None => w.uncertain = true,
        }
    } else if let Some(level) = read_sysfs(&node.join("md/level")) {
        let policy = read_sysfs(&node.join("md/consistency_policy"));
        let backlog = read_sysfs(&node.join("md/bitmap/backlog"));
        let c = classify_md(&level, policy.as_deref(), backlog.as_deref());
        if let Some(l) = c.layer {
            push(w, l);
        }
        if c.write_behind {
            push(w, StackLayer::WriteBehind);
        }
        if c.uncertain {
            w.uncertain = true;
        }
    } else if node.join("loop").is_dir() {
        push(w, StackLayer::Loop);
    }
    if let Some(mode) = read_sysfs(&node.join("bcache/cache_mode")) {
        if mode.contains("[writeback]") {
            push(w, StackLayer::WriteBackCache);
        }
    }

    let mut slaves: Vec<PathBuf> = match fs::read_dir(node.join("slaves")) {
        Ok(rd) => rd
            .take(MAX_SLAVES)
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect(),
        Err(_) => Vec::new(),
    };
    slaves.sort();
    if slaves.is_empty() {
        if w.leaf.is_none() {
            w.leaf = Some(node);
        }
        return;
    }
    for s in slaves {
        match fs::canonicalize(&s) {
            Ok(real) => visit(w, real, depth + 1, visited),
            Err(_) => w.uncertain = true,
        }
    }
}

/// Classifies a device-mapper node from its name and uuid (the target type
/// itself needs `dmsetup table`, which needs root).
pub(crate) fn classify_dm(name: &str, uuid: &str) -> Option<StackLayer> {
    let n = name.to_ascii_lowercase();
    let u = uuid.to_ascii_lowercase();
    if u.starts_with("crypt-") {
        return Some(StackLayer::Crypt);
    }
    if u.starts_with("vdo-") || n.starts_with("vdo") {
        return Some(StackLayer::Dedupe);
    }
    if u.starts_with("mpath-") || u.starts_with("part") {
        return Some(StackLayer::Linear);
    }
    if u.starts_with("lvm-") {
        let thin = u.ends_with("-tpool")
            || u.ends_with("-tdata")
            || u.ends_with("-tmeta")
            || n.ends_with("-tpool")
            || n.contains("_tdata")
            || n.contains("_tmeta")
            || u.ends_with("-cow")
            || u.ends_with("-real")
            || n.ends_with("-cow")
            || n.ends_with("-real");
        if thin {
            return Some(StackLayer::Thin);
        }
        let cache = u.ends_with("-cpool")
            || u.ends_with("-cdata")
            || u.ends_with("-cmeta")
            || n.contains("_cdata")
            || n.contains("_cmeta")
            || n.contains("_cvol")
            || n.contains("_wcorig");
        if cache {
            return Some(StackLayer::WriteBackCache);
        }
        return Some(StackLayer::Linear);
    }
    None
}

/// What an md array contributes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MdClass {
    pub layer: Option<StackLayer>,
    pub write_behind: bool,
    pub uncertain: bool,
}

/// Classifies an md array from `md/level`, `md/consistency_policy` and
/// `md/bitmap/backlog`.
pub(crate) fn classify_md(level: &str, policy: Option<&str>, backlog: Option<&str>) -> MdClass {
    let level = level.trim().to_ascii_lowercase();
    let write_behind = backlog
        .and_then(|b| b.trim().parse::<u64>().ok())
        .is_some_and(|b| b > 0);
    match level.as_str() {
        "raid0" | "raid1" | "raid10" | "0" | "1" | "10" => MdClass {
            layer: Some(StackLayer::Mirror),
            write_behind,
            uncertain: false,
        },
        "linear" => MdClass {
            layer: Some(StackLayer::Linear),
            write_behind,
            uncertain: false,
        },
        "raid4" | "raid5" | "raid6" | "4" | "5" | "6" => {
            let p = policy.map(|p| p.trim().to_ascii_lowercase());
            let protected = matches!(p.as_deref(), Some("ppl" | "journal"));
            MdClass {
                layer: Some(if protected {
                    StackLayer::ParityProtected
                } else {
                    StackLayer::ParityUnprotected
                }),
                write_behind,
                uncertain: p.is_none(),
            }
        }
        _ => MdClass {
            layer: None,
            write_behind,
            uncertain: true,
        },
    }
}

// ----- the leaf -------------------------------------------------------------

/// Facts read from the leaf disk's sysfs node.
#[derive(Debug, Default)]
pub(crate) struct LeafFacts {
    /// Kernel name (`sdd`, `nvme0n1`).
    pub name: String,
    /// `queue/write_cache`.
    pub cache_mode: OsCacheMode,
    /// Whether `queue/write_cache` could be read at all.
    pub cache_mode_read: bool,
    /// `queue/fua`.
    pub fua: Tri,
    /// `queue/logical_block_size`.
    pub logical: u32,
    /// `queue/physical_block_size`.
    pub physical: u32,
    /// `queue/minimum_io_size`.
    pub io_min: u32,
    /// `queue/max_sectors_kb` (falling back to `max_hw_sectors_kb`) in bytes.
    pub max_transfer: u32,
    /// `device/model`.
    pub model: Option<String>,
    /// `device/firmware_rev` or `device/rev`.
    pub firmware: Option<String>,
    /// `device/vendor`.
    pub vendor: Option<String>,
    /// Nearest `numa_node` up the device path.
    pub numa: Option<u16>,
    /// Interconnect.
    pub bus: Bus,
    /// NVMe namespace id (`nsid`).
    pub nsid: Option<u32>,
    /// NVMe controller name (`nvme0`), the char node to try second.
    pub ctrl: Option<String>,
}

/// Reads the leaf's attributes.
pub(crate) fn leaf_facts(leaf: &Path) -> LeafFacts {
    let q = leaf.join("queue");
    let name = leaf
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cache = read_sysfs(&q.join("write_cache"));
    let dev = leaf.join("device");
    let model = read_sysfs(&dev.join("model")).and_then(|s| clean(&s));
    let vendor = read_sysfs(&dev.join("vendor")).and_then(|s| clean(&s));
    let firmware = read_sysfs(&dev.join("firmware_rev"))
        .or_else(|| read_sysfs(&dev.join("rev")))
        .and_then(|s| clean(&s));
    let transport = read_sysfs(&dev.join("transport"));
    let real = fs::canonicalize(leaf).unwrap_or_else(|_| leaf.to_path_buf());
    let path = real.to_string_lossy().into_owned();
    let ctrl = fs::canonicalize(&dev)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
    LeafFacts {
        cache_mode: cache.as_deref().map(parse_cache_mode).unwrap_or_default(),
        cache_mode_read: cache.is_some(),
        fua: read_sysfs(&q.join("fua"))
            .as_deref()
            .map_or(Tri::Unknown, parse_flag01),
        logical: read_sysfs(&q.join("logical_block_size"))
            .as_deref()
            .and_then(parse_u32)
            .unwrap_or(0),
        physical: read_sysfs(&q.join("physical_block_size"))
            .as_deref()
            .and_then(parse_u32)
            .unwrap_or(0),
        io_min: read_sysfs(&q.join("minimum_io_size"))
            .as_deref()
            .and_then(parse_u32)
            .unwrap_or(0),
        max_transfer: read_sysfs(&q.join("max_sectors_kb"))
            .as_deref()
            .and_then(sectors_kb_to_bytes)
            .or_else(|| {
                read_sysfs(&q.join("max_hw_sectors_kb"))
                    .as_deref()
                    .and_then(sectors_kb_to_bytes)
            })
            .unwrap_or(0),
        numa: numa_node(&real),
        bus: bus_for(
            &name,
            &path,
            transport.as_deref(),
            vendor.as_deref(),
            model.as_deref(),
        ),
        nsid: read_sysfs(&leaf.join("nsid"))
            .as_deref()
            .and_then(parse_u32),
        ctrl: ctrl.filter(|c| c.starts_with("nvme")),
        name,
        model,
        firmware,
        vendor,
    }
}

/// `queue/write_cache` values.
pub(crate) fn parse_cache_mode(s: &str) -> OsCacheMode {
    match s.trim() {
        "write back" => OsCacheMode::WriteBack,
        "write through" => OsCacheMode::WriteThrough,
        _ => OsCacheMode::Unknown,
    }
}

/// A `0` / `1` attribute.
pub(crate) fn parse_flag01(s: &str) -> Tri {
    match s.trim() {
        "1" => Tri::Yes,
        "0" => Tri::No,
        _ => Tri::Unknown,
    }
}

/// A decimal attribute that fits a `u32`.
pub(crate) fn parse_u32(s: &str) -> Option<u32> {
    s.trim().parse().ok()
}

/// `*_sectors_kb` in bytes, saturating.
pub(crate) fn sectors_kb_to_bytes(s: &str) -> Option<u32> {
    parse_u32(s).map(|kb| kb.saturating_mul(1024))
}

/// A `numa_node` value; negative means none.
pub(crate) fn parse_numa(s: &str) -> Option<u16> {
    s.trim()
        .parse::<i32>()
        .ok()
        .filter(|n| *n >= 0)
        .and_then(|n| u16::try_from(n).ok())
}

fn numa_node(real: &Path) -> Option<u16> {
    let mut p = Some(real);
    for _ in 0..16 {
        let dir = p?;
        if let Some(v) = read_sysfs(&dir.join("numa_node")) {
            return parse_numa(&v);
        }
        p = dir.parent();
    }
    None
}

/// A printable ASCII string, trimmed, bounded, or `None` if empty.
pub(crate) fn clean(s: &str) -> Option<String> {
    let t: String = s
        .chars()
        .take(STRING_MAX)
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .collect();
    let t = t.trim().to_owned();
    if t.is_empty() { None } else { Some(t) }
}

/// The interconnect from the kernel name, the sysfs device path, the NVMe
/// transport, and the SCSI vendor and model.
pub(crate) fn bus_for(
    name: &str,
    path: &str,
    transport: Option<&str>,
    vendor: Option<&str>,
    model: Option<&str>,
) -> Bus {
    let p = path.to_ascii_lowercase();
    let v = vendor.unwrap_or("").to_ascii_lowercase();
    let m = model.unwrap_or("").to_ascii_lowercase();
    if name.starts_with("nvme") {
        let _fabrics = transport.is_some_and(|t| t.trim() != "pcie");
        return Bus::Nvme;
    }
    if name.starts_with("ram") || name.starts_with("zram") {
        return Bus::Ram;
    }
    if name.starts_with("vd") || name.starts_with("xvd") {
        return Bus::Virtual;
    }
    if p.contains("/usb") {
        return Bus::Usb;
    }
    if v.contains("msft")
        || v.contains("vmware")
        || v.contains("qemu")
        || m.contains("virtual")
        || m.contains("qemu")
        || p.contains("vmbus")
        || p.contains("virtio")
        || p.contains("/xen")
    {
        return Bus::Virtual;
    }
    if p.contains("/ata") {
        return Bus::Sata;
    }
    if name.starts_with("sd") || name.starts_with("sr") || p.contains("scsi") {
        return Bus::Scsi;
    }
    Bus::Unknown
}

// ----- NVMe -----------------------------------------------------------------

/// Device facts from NVMe admin commands, with what could not be read.
#[derive(Debug)]
pub(crate) struct NvmeFacts {
    pub cache_present: Tri,
    pub cache_enabled: Tri,
    pub atomic: AtomicFields,
    pub multi_controller: bool,
    pub model: Option<String>,
    pub firmware: Option<String>,
    pub backup_failed: Tri,
    pub missing: MissingSet,
    pub privilege: Privilege,
}

impl Default for NvmeFacts {
    fn default() -> Self {
        Self {
            cache_present: Tri::Unknown,
            cache_enabled: Tri::Unknown,
            atomic: AtomicFields::default(),
            multi_controller: false,
            model: None,
            firmware: None,
            backup_failed: Tri::Unknown,
            missing: MissingSet::new(),
            privilege: Privilege::Unprivileged,
        }
    }
}

const NVME_ADMIN_GET_LOG_PAGE: u8 = 0x02;
const NVME_ADMIN_IDENTIFY: u8 = 0x06;
const NVME_ADMIN_GET_FEATURES: u8 = 0x0A;
const NVME_FEAT_VOLATILE_WC: u32 = 0x06;
const NVME_LOG_SMART: u32 = 0x02;
const NVME_IDENTIFY_LEN: usize = 4096;
const NVME_SMART_LEN: usize = 512;

/// Identify, Get Features and SMART through `/dev/<ns>` or `/dev/<ctrl>`,
/// whichever opens. Not an NVMe leaf, or no openable node: everything is
/// listed as missing.
pub(crate) fn nvme_facts(leaf: &LeafFacts) -> NvmeFacts {
    let mut f = NvmeFacts::default();
    let all = [
        Missing::Identify,
        Missing::Smart,
        Missing::CachePresence,
        Missing::CacheEnabled,
        Missing::AtomicFields,
    ];
    if !leaf.name.starts_with("nvme") {
        for m in all {
            f.missing.insert(m);
        }
        return f;
    }
    let mut candidates = vec![format!("/dev/{}", leaf.name)];
    if let Some(c) = &leaf.ctrl {
        candidates.push(format!("/dev/{c}"));
    }
    let node = candidates
        .iter()
        .find_map(|p| fs::OpenOptions::new().read(true).open(p).ok());
    let Some(node) = node else {
        for m in all {
            f.missing.insert(m);
        }
        return f;
    };
    let fd = node.as_raw_fd();
    let mut data = vec![0u8; NVME_IDENTIFY_LEN];

    let mut cmd = sys::NvmeAdminCmd {
        opcode: NVME_ADMIN_IDENTIFY,
        nsid: 0,
        cdw10: 1,
        ..sys::NvmeAdminCmd::default()
    };
    match sys::nvme_admin(fd, &mut cmd, &mut data)
        .ok()
        .and_then(|()| parse_id_ctrl(&data))
    {
        Some(c) => {
            f.cache_present = if c.vwc & 1 != 0 { Tri::Yes } else { Tri::No };
            if c.vwc & 1 == 0 {
                f.cache_enabled = Tri::No;
            }
            f.atomic.awupf = Some(c.awupf);
            f.multi_controller = c.cmic & 0x2 != 0;
            f.model = c.model;
            f.firmware = c.firmware;
        }
        None => {
            f.missing.insert(Missing::Identify);
            f.missing.insert(Missing::CachePresence);
        }
    }

    match leaf.nsid {
        Some(nsid) => {
            data.fill(0);
            let mut cmd = sys::NvmeAdminCmd {
                opcode: NVME_ADMIN_IDENTIFY,
                nsid,
                cdw10: 0,
                ..sys::NvmeAdminCmd::default()
            };
            match sys::nvme_admin(fd, &mut cmd, &mut data)
                .ok()
                .and_then(|()| parse_id_ns(&data))
            {
                Some(n) => {
                    f.atomic.nsabp = n.nsfeat & 0x2 != 0;
                    f.atomic.nawupf = n.nawupf;
                    f.atomic.nabspf = n.nabspf;
                    f.atomic.nabo = n.nabo;
                }
                None => f.missing.insert(Missing::AtomicFields),
            }
        }
        None => f.missing.insert(Missing::AtomicFields),
    }

    if f.cache_enabled == Tri::Unknown {
        let mut cmd = sys::NvmeAdminCmd {
            opcode: NVME_ADMIN_GET_FEATURES,
            cdw10: NVME_FEAT_VOLATILE_WC,
            ..sys::NvmeAdminCmd::default()
        };
        match sys::nvme_admin(fd, &mut cmd, &mut []) {
            Ok(()) => {
                f.cache_enabled = if cmd.result & 1 != 0 {
                    Tri::Yes
                } else {
                    Tri::No
                }
            }
            Err(_) => f.missing.insert(Missing::CacheEnabled),
        }
    }

    let mut smart = vec![0u8; NVME_SMART_LEN];
    let mut cmd = sys::NvmeAdminCmd {
        opcode: NVME_ADMIN_GET_LOG_PAGE,
        nsid: 0xFFFF_FFFF,
        cdw10: ((NVME_SMART_LEN as u32 / 4 - 1) << 16) | NVME_LOG_SMART,
        ..sys::NvmeAdminCmd::default()
    };
    match sys::nvme_admin(fd, &mut cmd, &mut smart)
        .ok()
        .and_then(|()| parse_smart(&smart))
    {
        Some(cw) => {
            f.backup_failed = if cw & 0x10 != 0 { Tri::Yes } else { Tri::No };
            f.privilege = Privilege::Full;
        }
        None => f.missing.insert(Missing::Smart),
    }
    f
}

/// Identify Controller fields the probe uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IdCtrl {
    /// Byte 525: bit 0 VWCP.
    pub vwc: u8,
    /// Bytes 529:528, 0-based logical blocks.
    pub awupf: u16,
    /// Byte 76: bit 1 multiple controllers.
    pub cmic: u8,
    /// Byte 77.
    pub mdts: u8,
    /// Bytes 63:24.
    pub model: Option<String>,
    /// Bytes 71:64.
    pub firmware: Option<String>,
}

/// Parses a 4096-byte Identify Controller structure.
pub(crate) fn parse_id_ctrl(b: &[u8]) -> Option<IdCtrl> {
    if b.len() < NVME_IDENTIFY_LEN {
        return None;
    }
    Some(IdCtrl {
        vwc: b[525],
        awupf: u16::from_le_bytes([b[528], b[529]]),
        cmic: b[76],
        mdts: b[77],
        model: ascii(&b[24..64]),
        firmware: ascii(&b[64..72]),
    })
}

/// Identify Namespace fields the probe uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IdNs {
    /// Byte 24: bit 1 NSABP.
    pub nsfeat: u8,
    /// Bytes 37:36.
    pub nawupf: u16,
    /// Bytes 43:42 (an LBA, not 0-based).
    pub nabo: u16,
    /// Bytes 45:44.
    pub nabspf: u16,
    /// Bytes of the LBA format in use, 0 if the format index is out of range.
    pub lba_bytes: u32,
}

/// Parses a 4096-byte Identify Namespace structure.
pub(crate) fn parse_id_ns(b: &[u8]) -> Option<IdNs> {
    if b.len() < NVME_IDENTIFY_LEN {
        return None;
    }
    let nlbaf = usize::from(b[25]);
    let flbas = b[26];
    let index = usize::from(flbas & 0xF) | (usize::from((flbas >> 5) & 0x3) << 4);
    let lba_bytes = if index <= nlbaf && index < 64 {
        let lbads = b[128 + 4 * index + 2];
        if lbads < 32 { 1u32 << lbads } else { 0 }
    } else {
        0
    };
    Some(IdNs {
        nsfeat: b[24],
        nawupf: u16::from_le_bytes([b[36], b[37]]),
        nabo: u16::from_le_bytes([b[42], b[43]]),
        nabspf: u16::from_le_bytes([b[44], b[45]]),
        lba_bytes,
    })
}

/// The Critical Warning byte of a 512-byte SMART / Health log.
pub(crate) fn parse_smart(b: &[u8]) -> Option<u8> {
    if b.len() < NVME_SMART_LEN {
        return None;
    }
    Some(b[0])
}

fn ascii(b: &[u8]) -> Option<String> {
    let s: String = b
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| {
            if c.is_ascii_graphic() || c == b' ' {
                char::from(c)
            } else {
                '?'
            }
        })
        .collect();
    clean(&s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_dm_by_uuid_and_name() {
        assert_eq!(
            classify_dm("luks-x", "CRYPT-LUKS2-abc-luks-x"),
            Some(StackLayer::Crypt)
        );
        assert_eq!(classify_dm("vg-lv", "LVM-abcdef"), Some(StackLayer::Linear));
        assert_eq!(
            classify_dm("vg-pool-tpool", "LVM-abcdef-tpool"),
            Some(StackLayer::Thin)
        );
        assert_eq!(
            classify_dm("vg-pool_tmeta", "LVM-abcdef-tmeta"),
            Some(StackLayer::Thin)
        );
        assert_eq!(
            classify_dm("vg-snap-cow", "LVM-abcdef-cow"),
            Some(StackLayer::Thin)
        );
        assert_eq!(
            classify_dm("vg-lv_cdata", "LVM-abcdef-cdata"),
            Some(StackLayer::WriteBackCache)
        );
        assert_eq!(
            classify_dm("vg-lv_wcorig", "LVM-abcdef"),
            Some(StackLayer::WriteBackCache)
        );
        assert_eq!(
            classify_dm("mpatha", "mpath-3600"),
            Some(StackLayer::Linear)
        );
        assert_eq!(
            classify_dm("mpatha1", "part1-mpath-3600"),
            Some(StackLayer::Linear)
        );
        assert_eq!(classify_dm("vdo0", "VDO-abc"), Some(StackLayer::Dedupe));
        assert_eq!(classify_dm("mystery", ""), None);
        assert_eq!(classify_dm("", "garbage\u{0}\u{FFFD}"), None);
    }

    #[test]
    fn test_classify_md_levels_and_policies() {
        let c = classify_md("raid1", None, None);
        assert_eq!(
            (c.layer, c.write_behind, c.uncertain),
            (Some(StackLayer::Mirror), false, false)
        );
        let c = classify_md("raid1\n", Some("bitmap"), Some("64"));
        assert!(c.write_behind);
        let c = classify_md("raid5", Some("ppl"), None);
        assert_eq!(c.layer, Some(StackLayer::ParityProtected));
        let c = classify_md("raid6", Some("journal\n"), Some("x"));
        assert_eq!(c.layer, Some(StackLayer::ParityProtected));
        assert!(!c.write_behind);
        let c = classify_md("raid5", Some("resync"), None);
        assert_eq!(c.layer, Some(StackLayer::ParityUnprotected));
        assert!(!c.uncertain);
        let c = classify_md("raid4", None, None);
        assert_eq!(c.layer, Some(StackLayer::ParityUnprotected));
        assert!(c.uncertain);
        let c = classify_md("linear", None, None);
        assert_eq!(c.layer, Some(StackLayer::Linear));
        let c = classify_md("multipath", None, None);
        assert_eq!(c.layer, None);
        assert!(c.uncertain);
        assert!(classify_md("", None, None).uncertain);
    }

    #[test]
    fn test_sysfs_scalars_are_garbage_tolerant() {
        assert_eq!(parse_cache_mode("write back\n"), OsCacheMode::WriteBack);
        assert_eq!(parse_cache_mode("write through"), OsCacheMode::WriteThrough);
        assert_eq!(parse_cache_mode("writeback"), OsCacheMode::Unknown);
        assert_eq!(parse_cache_mode(""), OsCacheMode::Unknown);
        assert_eq!(parse_flag01("1\n"), Tri::Yes);
        assert_eq!(parse_flag01("0"), Tri::No);
        assert_eq!(parse_flag01("yes"), Tri::Unknown);
        assert_eq!(parse_u32(" 512 "), Some(512));
        assert_eq!(parse_u32("-1"), None);
        assert_eq!(parse_u32("99999999999"), None);
        assert_eq!(parse_u32("4k"), None);
        assert_eq!(sectors_kb_to_bytes("1280"), Some(1280 * 1024));
        assert_eq!(sectors_kb_to_bytes("4294967295"), Some(u32::MAX));
        assert_eq!(parse_numa("0"), Some(0));
        assert_eq!(parse_numa("-1"), None);
        assert_eq!(parse_numa("70000"), None);
        assert_eq!(parse_numa("abc"), None);
        assert_eq!(clean("  Virtual Disk    "), Some("Virtual Disk".to_owned()));
        assert_eq!(clean("\u{0}\u{1}\t"), None);
        assert_eq!(clean(&"x".repeat(1000)).map(|s| s.len()), Some(STRING_MAX));
    }

    #[test]
    fn test_bus_classification() {
        assert_eq!(
            bus_for(
                "nvme0n1",
                "/sys/devices/pci0000:00/0000:00:1d.0/nvme/nvme0/nvme0n1",
                Some("pcie"),
                None,
                None
            ),
            Bus::Nvme
        );
        assert_eq!(
            bus_for(
                "sdd",
                "/sys/devices/LNXSYSTM:00/.../0:0:0:3/block/sdd",
                None,
                Some("Msft"),
                Some("Virtual Disk")
            ),
            Bus::Virtual
        );
        assert_eq!(
            bus_for(
                "sda",
                "/sys/devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host4/target4:0:0/4:0:0:0/block/sda",
                None,
                Some("SanDisk"),
                Some("Extreme")
            ),
            Bus::Usb
        );
        assert_eq!(
            bus_for(
                "sda",
                "/sys/devices/pci0000:00/0000:00:17.0/ata1/host0/target0:0:0/0:0:0:0/block/sda",
                None,
                Some("ATA"),
                Some("Samsung SSD")
            ),
            Bus::Sata
        );
        assert_eq!(
            bus_for(
                "sdb",
                "/sys/devices/pci0000:00/0000:03:00.0/host0/target0:2:0/0:2:0:0/block/sdb",
                None,
                Some("DELL"),
                Some("PERC")
            ),
            Bus::Scsi
        );
        assert_eq!(
            bus_for(
                "vda",
                "/sys/devices/pci0000:00/0000:00:04.0/virtio1/block/vda",
                None,
                None,
                None
            ),
            Bus::Virtual
        );
        assert_eq!(
            bus_for(
                "xvda",
                "/sys/devices/vbd-51712/block/xvda",
                None,
                None,
                None
            ),
            Bus::Virtual
        );
        assert_eq!(
            bus_for("ram0", "/sys/devices/virtual/block/ram0", None, None, None),
            Bus::Ram
        );
        assert_eq!(
            bus_for(
                "zram0",
                "/sys/devices/virtual/block/zram0",
                None,
                None,
                None
            ),
            Bus::Ram
        );
        assert_eq!(
            bus_for(
                "mmcblk0",
                "/sys/devices/platform/mmc/mmcblk0",
                None,
                None,
                None
            ),
            Bus::Unknown
        );
    }

    fn id_ctrl_bytes() -> Vec<u8> {
        let mut b = vec![0u8; NVME_IDENTIFY_LEN];
        b[24..24 + 11].copy_from_slice(b"Samsung 990");
        for x in &mut b[35..64] {
            *x = b' ';
        }
        b[64..72].copy_from_slice(b"4B2QJXD7");
        b[76] = 0x02;
        b[77] = 7;
        b[525] = 0x07;
        b[528] = 0x03;
        b[529] = 0x00;
        b
    }

    #[test]
    fn test_parse_id_ctrl() {
        let c = parse_id_ctrl(&id_ctrl_bytes()).unwrap_or_else(|| panic!("parse"));
        assert_eq!(c.vwc, 7);
        assert_eq!(c.awupf, 3);
        assert_eq!(c.cmic, 2);
        assert_eq!(c.mdts, 7);
        assert_eq!(c.model.as_deref(), Some("Samsung 990"));
        assert_eq!(c.firmware.as_deref(), Some("4B2QJXD7"));
        assert!(parse_id_ctrl(&[0u8; 4095]).is_none());
        assert!(parse_id_ctrl(&[]).is_none());
        let garbage = vec![0xFFu8; NVME_IDENTIFY_LEN];
        let c = parse_id_ctrl(&garbage).unwrap_or_else(|| panic!("parse"));
        assert_eq!(c.awupf, 0xFFFF);
        assert_eq!(
            c.model.as_deref(),
            Some("????????????????????????????????????????")
        );
    }

    #[test]
    fn test_parse_id_ns() {
        let mut b = vec![0u8; NVME_IDENTIFY_LEN];
        b[24] = 0x02;
        b[25] = 1;
        b[26] = 1;
        b[36] = 0x07;
        b[42] = 0x08;
        b[44] = 0x1F;
        b[128 + 2] = 9;
        b[132 + 2] = 12;
        let n = parse_id_ns(&b).unwrap_or_else(|| panic!("parse"));
        assert_eq!(n.nsfeat & 2, 2);
        assert_eq!((n.nawupf, n.nabo, n.nabspf), (7, 8, 0x1F));
        assert_eq!(n.lba_bytes, 4096);
        b[26] = 0;
        assert_eq!(parse_id_ns(&b).map(|n| n.lba_bytes), Some(512));
        // Format index beyond NLBAF, or an absurd LBADS, gives 0 bytes.
        b[26] = 5;
        assert_eq!(parse_id_ns(&b).map(|n| n.lba_bytes), Some(0));
        b[26] = 0;
        b[128 + 2] = 63;
        assert_eq!(parse_id_ns(&b).map(|n| n.lba_bytes), Some(0));
        assert!(parse_id_ns(&b[..100]).is_none());
        assert!(parse_smart(&[0x10; NVME_SMART_LEN]).is_some_and(|cw| cw & 0x10 != 0));
        assert!(parse_smart(&[0; 511]).is_none());
    }
}
