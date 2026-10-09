//! Filesystem evidence: the statfs magic table, `/proc/self/mountinfo`, the
//! ext4 effective options, inode attribute flags and the statx direct-I/O
//! and atomic-write fields. Every parser here is pure and treats its input
//! as hostile: garbage yields "unknown", never a panic.

use store_io_core::evidence::FsKind;

use crate::sys::Statx;

// ----- statfs magic (include/uapi/linux/magic.h) ----------------------------

const EXT4_SUPER_MAGIC: u64 = 0xEF53;
const XFS_SUPER_MAGIC: u64 = 0x5846_5342;
const BTRFS_SUPER_MAGIC: u64 = 0x9123_683E;
const TMPFS_MAGIC: u64 = 0x0102_1994;
const RAMFS_MAGIC: u64 = 0x8584_58F6;
const HUGETLBFS_MAGIC: u64 = 0x9584_58F6;
const FUSE_SUPER_MAGIC: u64 = 0x6573_5546;
const NFS_SUPER_MAGIC: u64 = 0x6969;
const SMB_SUPER_MAGIC: u64 = 0x517B;
const SMB2_SUPER_MAGIC: u64 = 0xFE53_4D42;
const CIFS_SUPER_MAGIC: u64 = 0xFF53_4D42;
const CEPH_SUPER_MAGIC: u64 = 0x00C3_6400;
const AFS_SUPER_MAGIC: u64 = 0x5346_414F;
const AFS_FS_MAGIC: u64 = 0x6B41_4653;
const OVERLAYFS_SUPER_MAGIC: u64 = 0x794C_7630;
/// Reported by the out-of-tree ZFS driver; not in uapi.
const ZFS_SUPER_MAGIC: u64 = 0x2FC1_2FC1;
const F2FS_SUPER_MAGIC: u64 = 0xF2F5_2010;
const BCACHEFS_STATFS_MAGIC: u64 = 0xCA45_1A4E;
const EXFAT_SUPER_MAGIC: u64 = 0x2011_BAB0;
const MSDOS_SUPER_MAGIC: u64 = 0x4D44;
const V9FS_MAGIC: u64 = 0x0102_1997;
const NTFS_SB_MAGIC: u64 = 0x5346_544E;
const HFS_SUPER_MAGIC: u64 = 0x4244;
const HFSPLUS_SUPER_MAGIC: u64 = 0x482B;

/// Maps a statfs magic (low 32 bits) and, when known, the mountinfo
/// filesystem type to a kind. The type wins where it is more specific than
/// the magic (virtiofs shares FUSE's magic; ext2 shares ext4's).
pub(crate) fn fs_kind(magic: u64, fstype: Option<&str>) -> FsKind {
    if let Some(k) = fstype.and_then(kind_from_type) {
        return k;
    }
    kind_from_magic(magic & 0xFFFF_FFFF)
}

/// Kind from the mountinfo filesystem type.
pub(crate) fn kind_from_type(t: &str) -> Option<FsKind> {
    Some(match t {
        "ext4" | "ext3" => FsKind::Ext4,
        "xfs" => FsKind::Xfs,
        "btrfs" => FsKind::Btrfs,
        "zfs" => FsKind::Zfs,
        "bcachefs" => FsKind::Bcachefs,
        "f2fs" => FsKind::F2fs,
        "ntfs3" => FsKind::Ntfs3,
        "tmpfs" | "ramfs" | "hugetlbfs" | "devtmpfs" => FsKind::Tmpfs,
        "overlay" => FsKind::Overlay,
        "vfat" | "msdos" | "exfat" | "fat" => FsKind::Fat,
        "9p" | "virtiofs" | "drvfs" | "vboxsf" | "prl_fs" | "vmhgfs" => FsKind::Passthrough,
        "nfs" | "nfs4" | "cifs" | "smb3" | "smb2" | "ceph" | "afs" | "glusterfs" | "lustre"
        | "gfs2" | "ocfs2" => FsKind::Network,
        "fuse" | "fuseblk" => FsKind::Fuse,
        t if t.starts_with("fuse.") => FsKind::Fuse,
        "ext2" | "hfsplus" | "hfs" | "ntfs" => FsKind::Other,
        _ => return None,
    })
}

/// Kind from the statfs magic alone.
pub(crate) fn kind_from_magic(magic: u64) -> FsKind {
    match magic {
        EXT4_SUPER_MAGIC => FsKind::Ext4,
        XFS_SUPER_MAGIC => FsKind::Xfs,
        BTRFS_SUPER_MAGIC => FsKind::Btrfs,
        TMPFS_MAGIC | RAMFS_MAGIC | HUGETLBFS_MAGIC => FsKind::Tmpfs,
        FUSE_SUPER_MAGIC => FsKind::Fuse,
        NFS_SUPER_MAGIC | SMB_SUPER_MAGIC | SMB2_SUPER_MAGIC | CIFS_SUPER_MAGIC
        | CEPH_SUPER_MAGIC | AFS_SUPER_MAGIC | AFS_FS_MAGIC => FsKind::Network,
        OVERLAYFS_SUPER_MAGIC => FsKind::Overlay,
        ZFS_SUPER_MAGIC => FsKind::Zfs,
        F2FS_SUPER_MAGIC => FsKind::F2fs,
        BCACHEFS_STATFS_MAGIC => FsKind::Bcachefs,
        EXFAT_SUPER_MAGIC | MSDOS_SUPER_MAGIC => FsKind::Fat,
        V9FS_MAGIC => FsKind::Passthrough,
        NTFS_SB_MAGIC => FsKind::Ntfs3,
        HFS_SUPER_MAGIC | HFSPLUS_SUPER_MAGIC => FsKind::Hfs,
        _ => FsKind::Other,
    }
}

// ----- /proc/self/mountinfo -------------------------------------------------

/// Most lines the parser will look at.
const MOUNTINFO_MAX_LINES: usize = 1 << 16;

/// One `mountinfo` line (proc_pid_mountinfo(5)), fields unescaped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MountEntry {
    pub id: u64,
    pub major: u32,
    pub minor: u32,
    pub mount_point: String,
    pub options: String,
    pub fstype: String,
    pub source: String,
    pub super_options: String,
}

/// Parses one line: `ID PARENT MAJ:MIN ROOT MOUNT_POINT OPTS [tag...] - TYPE SOURCE SUPER_OPTS`.
pub(crate) fn parse_mountinfo_line(line: &str) -> Option<MountEntry> {
    let mut it = line.split(' ');
    let id = it.next()?.parse().ok()?;
    let _parent = it.next()?;
    let (maj, min) = it.next()?.split_once(':')?;
    let major = maj.parse().ok()?;
    let minor = min.parse().ok()?;
    let _root = it.next()?;
    let mount_point = unescape(it.next()?);
    let options = it.next()?.to_owned();
    // Optional tagged fields end at the lone separator "-".
    loop {
        if it.next()? == "-" {
            break;
        }
    }
    let fstype = it.next()?.to_owned();
    let source = unescape(it.next()?);
    let super_options = it.next().unwrap_or("").to_owned();
    Some(MountEntry {
        id,
        major,
        minor,
        mount_point,
        options,
        fstype,
        source,
        super_options,
    })
}

/// Undoes the `\ooo` octal escapes `mountinfo` uses for space, tab, newline
/// and backslash. Malformed escapes are kept literally.
pub(crate) fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            let oct = &b[i + 1..i + 4];
            if oct.iter().all(|c| (b'0'..=b'7').contains(c)) {
                let v = oct
                    .iter()
                    .fold(0u32, |acc, c| acc * 8 + u32::from(c - b'0'));
                if let Ok(byte) = u8::try_from(v) {
                    out.push(byte);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Finds the mount a file belongs to: by mount id when statx reported one,
/// else by device number, preferring the longest mount point that is a
/// prefix of `path` when several mounts share the device.
pub(crate) fn find_mount(
    text: &str,
    mnt_id: Option<u64>,
    dev: Option<(u32, u32)>,
    path: Option<&str>,
) -> Option<MountEntry> {
    let entries = text
        .lines()
        .take(MOUNTINFO_MAX_LINES)
        .filter_map(parse_mountinfo_line);
    let mut by_dev: Option<MountEntry> = None;
    for e in entries {
        if mnt_id == Some(e.id) {
            return Some(e);
        }
        if dev == Some((e.major, e.minor)) {
            let better = match (&by_dev, path) {
                (None, _) => true,
                (Some(cur), Some(p)) => {
                    let cur_ok = is_prefix(&cur.mount_point, p);
                    let new_ok = is_prefix(&e.mount_point, p);
                    (new_ok && !cur_ok)
                        || (new_ok == cur_ok && e.mount_point.len() > cur.mount_point.len())
                }
                (Some(_), None) => false,
            };
            if better {
                by_dev = Some(e);
            }
        }
    }
    by_dev
}

fn is_prefix(mount_point: &str, path: &str) -> bool {
    mount_point == "/"
        || path == mount_point
        || (path.starts_with(mount_point) && path[mount_point.len()..].starts_with('/'))
}

/// `DEVNAME=` from a sysfs `uevent` file.
pub(crate) fn uevent_devname(text: &str) -> Option<String> {
    text.lines()
        .take(64)
        .find_map(|l| l.strip_prefix("DEVNAME="))
        .filter(|n| {
            !n.is_empty()
                && n.len() <= 64
                && n.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'/')
        })
        .map(|n| n.rsplit('/').next().unwrap_or(n).to_owned())
}

/// Last component of a path-like string.
pub(crate) fn basename(s: &str) -> Option<&str> {
    s.rsplit('/').next().filter(|b| !b.is_empty())
}

// ----- mount options --------------------------------------------------------

/// Durability-relevant mount facts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MountFacts {
    /// `data=journal`.
    pub data_journal: bool,
    /// `nobarrier` / `barrier=0` / f2fs `fsync_mode=nobarrier`.
    pub no_barrier: bool,
    /// `errors=continue`, no journal, external journal, async commit.
    pub weak_error_mode: bool,
    /// overlay `volatile` (and a literal `sync=disabled`, should it appear).
    pub sync_disabled: bool,
    /// `lazytime`.
    pub lazytime: bool,
    /// Mount-level compression (btrfs `compress`).
    pub compressed: bool,
    /// btrfs `nodatacow`.
    pub nocow: bool,
}

impl MountFacts {
    /// Unions `other` into `self` (the ext4 options file adds to mountinfo).
    pub(crate) fn merge(&mut self, other: &Self) {
        self.data_journal |= other.data_journal;
        self.no_barrier |= other.no_barrier;
        self.weak_error_mode |= other.weak_error_mode;
        self.sync_disabled |= other.sync_disabled;
        self.lazytime |= other.lazytime;
        self.compressed |= other.compressed;
        self.nocow |= other.nocow;
    }
}

/// Facts from a mount's per-mount and per-superblock option strings.
pub(crate) fn mount_facts(fstype: &str, options: &str, super_options: &str) -> MountFacts {
    let mut f = MountFacts::default();
    for tok in options
        .split(',')
        .chain(super_options.split(','))
        .take(1024)
    {
        apply(&mut f, fstype, tok.trim());
    }
    f
}

/// Facts from `/proc/fs/ext4/<dev>/options`, one option per line (it prints
/// effective defaults that `mountinfo` omits, such as `barrier`).
pub(crate) fn ext4_options_facts(text: &str) -> MountFacts {
    let mut f = MountFacts::default();
    for line in text.lines().take(1024) {
        apply(&mut f, "ext4", line.trim());
    }
    f
}

fn apply(f: &mut MountFacts, fstype: &str, tok: &str) {
    match tok {
        "data=journal" => f.data_journal = true,
        "nobarrier" | "barrier=0" | "barrier=none" | "barrier=off" | "fsync_mode=nobarrier" => {
            f.no_barrier = true;
        }
        "errors=continue" | "journal_async_commit" | "noload" | "norecovery" => {
            f.weak_error_mode = true;
        }
        "lazytime" => f.lazytime = true,
        "volatile" if fstype == "overlay" => f.sync_disabled = true,
        "sync=disabled" => f.sync_disabled = true,
        "nodatacow" => f.nocow = true,
        "compress" | "compress-force" => f.compressed = true,
        t if t.starts_with("journal_dev=") || t.starts_with("journal_path=") => {
            f.weak_error_mode = true;
        }
        t if t.starts_with("compress=") || t.starts_with("compress-force=") => {
            f.compressed = true;
        }
        _ => {}
    }
}

// ----- inode flags (FS_IOC_GETFLAGS) ----------------------------------------

/// `FS_COMPR_FL`.
pub(crate) const FS_COMPR_FL: u64 = 0x4;
/// `FS_ENCRYPT_FL`.
pub(crate) const FS_ENCRYPT_FL: u64 = 0x800;
/// `FS_JOURNAL_DATA_FL` (ext4 `chattr +j`).
pub(crate) const FS_JOURNAL_DATA_FL: u64 = 0x4000;
/// `FS_NOCOW_FL` (btrfs).
pub(crate) const FS_NOCOW_FL: u64 = 0x80_0000;

/// Durability-relevant inode attributes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct InodeFacts {
    /// `+j`.
    pub data_journal: bool,
    /// Compressed or encrypted.
    pub transformed: bool,
    /// btrfs `+C`.
    pub nocow: bool,
}

/// Interprets `FS_IOC_GETFLAGS` bits.
pub(crate) fn inode_facts(flags: u64) -> InodeFacts {
    InodeFacts {
        data_journal: flags & FS_JOURNAL_DATA_FL != 0,
        transformed: flags & (FS_COMPR_FL | FS_ENCRYPT_FL) != 0,
        nocow: flags & FS_NOCOW_FL != 0,
    }
}

// ----- statx ----------------------------------------------------------------

/// Direct-I/O and atomic-write facts from `statx`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DioFacts {
    /// `STATX_DIOALIGN` was filled (6.1+ on a filesystem that reports it).
    pub reported: bool,
    /// Memory alignment; 0 with `reported` means direct I/O is unsupported.
    pub mem_align: u32,
    /// Offset alignment; 0 with `reported` means direct I/O is unsupported.
    pub offset_align: u32,
    /// `STATX_WRITE_ATOMIC` was filled.
    pub atomic_reported: bool,
    /// Largest untorn write in bytes (the optimal maximum where the
    /// filesystem distinguishes one, since above it XFS allocates).
    pub atomic_unit: u32,
    /// `STATX_ATTR_COMPRESSED` or `STATX_ATTR_ENCRYPTED` is set.
    pub transformed: bool,
}

/// Interprets the statx mask, alignment, atomic and attribute fields.
pub(crate) fn dio_facts(sx: &Statx) -> DioFacts {
    let reported = sx.stx_mask & libc::STATX_DIOALIGN != 0;
    let atomic_reported = sx.stx_mask & libc::STATX_WRITE_ATOMIC != 0;
    let atomic_unit = if atomic_reported {
        if sx.stx_atomic_write_unit_max_opt != 0 {
            sx.stx_atomic_write_unit_max_opt
                .min(sx.stx_atomic_write_unit_max)
        } else {
            sx.stx_atomic_write_unit_max
        }
    } else {
        0
    };
    let attrs = sx.stx_attributes & sx.stx_attributes_mask;
    let transformed =
        attrs & (libc::STATX_ATTR_COMPRESSED as u64 | libc::STATX_ATTR_ENCRYPTED as u64) != 0;
    DioFacts {
        reported,
        mem_align: if reported { sx.stx_dio_mem_align } else { 0 },
        offset_align: if reported { sx.stx_dio_offset_align } else { 0 },
        atomic_reported,
        atomic_unit,
        transformed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_magic_table() {
        assert_eq!(fs_kind(0xEF53, None), FsKind::Ext4);
        assert_eq!(fs_kind(0x5846_5342, None), FsKind::Xfs);
        assert_eq!(fs_kind(0x9123_683E, None), FsKind::Btrfs);
        assert_eq!(fs_kind(0x0102_1994, None), FsKind::Tmpfs);
        assert_eq!(fs_kind(0x6573_5546, None), FsKind::Fuse);
        assert_eq!(fs_kind(0x6969, None), FsKind::Network);
        assert_eq!(fs_kind(0xFE53_4D42, None), FsKind::Network);
        assert_eq!(fs_kind(0xFF53_4D42, None), FsKind::Network);
        assert_eq!(fs_kind(0x794C_7630, None), FsKind::Overlay);
        assert_eq!(fs_kind(0x2FC1_2FC1, None), FsKind::Zfs);
        assert_eq!(fs_kind(0xF2F5_2010, None), FsKind::F2fs);
        assert_eq!(fs_kind(0xCA45_1A4E, None), FsKind::Bcachefs);
        assert_eq!(fs_kind(0x2011_BAB0, None), FsKind::Fat);
        assert_eq!(fs_kind(0x4D44, None), FsKind::Fat);
        assert_eq!(fs_kind(0x0102_1997, None), FsKind::Passthrough);
        assert_eq!(fs_kind(0x5346_544E, None), FsKind::Ntfs3);
        assert_eq!(fs_kind(0x482B, None), FsKind::Hfs);
        assert_eq!(fs_kind(0xDEAD_BEEF, None), FsKind::Other);
        assert_eq!(fs_kind(0, None), FsKind::Other);
        // Sign-extended 32-bit magics from a `long` f_type still match.
        assert_eq!(fs_kind(0xFFFF_FFFF_FF53_4D42, None), FsKind::Network);
    }

    #[test]
    fn test_fstype_refines_magic() {
        assert_eq!(fs_kind(0x6573_5546, Some("virtiofs")), FsKind::Passthrough);
        assert_eq!(fs_kind(0x6573_5546, Some("fuse.sshfs")), FsKind::Fuse);
        assert_eq!(fs_kind(0xEF53, Some("ext2")), FsKind::Other);
        assert_eq!(fs_kind(0xEF53, Some("ext3")), FsKind::Ext4);
        assert_eq!(fs_kind(0x0102_1997, Some("9p")), FsKind::Passthrough);
        assert_eq!(fs_kind(0xEF53, Some("made-up")), FsKind::Ext4);
        assert_eq!(fs_kind(0xEF53, Some("")), FsKind::Ext4);
    }

    const WSL_LINE: &str =
        "80 65 8:48 / / rw,relatime - ext4 /dev/sdd rw,discard,errors=remount-ro,data=ordered";
    const NINEP_LINE: &str = "77 80 0:34 / /usr/lib/wsl/drivers ro,nosuid,nodev,noatime - 9p drivers ro,aname=drivers;fmask=222;dmask=222,cache=5,access=client,msize=65536,trans=fd,rfd=8,wfd=8";

    #[test]
    fn test_mountinfo_line_parses_with_and_without_tags() {
        let e = parse_mountinfo_line(WSL_LINE).unwrap_or_else(|| panic!("parse"));
        assert_eq!((e.id, e.major, e.minor), (80, 8, 48));
        assert_eq!(e.mount_point, "/");
        assert_eq!(e.fstype, "ext4");
        assert_eq!(e.source, "/dev/sdd");
        assert_eq!(e.super_options, "rw,discard,errors=remount-ro,data=ordered");
        let tagged = "76 80 0:32 / /mnt/wsl rw,relatime shared:1 master:2 - tmpfs none rw";
        let e = parse_mountinfo_line(tagged).unwrap_or_else(|| panic!("parse"));
        assert_eq!(e.fstype, "tmpfs");
        assert_eq!(e.mount_point, "/mnt/wsl");
        let escaped = "5 1 8:1 / /mnt/my\\040disk rw - ext4 /dev/sda1 rw";
        let e = parse_mountinfo_line(escaped).unwrap_or_else(|| panic!("parse"));
        assert_eq!(e.mount_point, "/mnt/my disk");
        // Missing super options are tolerated.
        let short = "5 1 8:1 / /x rw - ext4 /dev/sda1";
        assert_eq!(
            parse_mountinfo_line(short).map(|e| e.super_options),
            Some(String::new())
        );
    }

    #[test]
    fn test_mountinfo_garbage_is_rejected() {
        for bad in [
            "",
            "garbage",
            "a b c d e f - g h i",
            "1 2 8:x / / rw - ext4 /dev/sda1 rw",
            "1 2 8 / / rw - ext4 /dev/sda1 rw",
            "1 2 8:1 / / rw",
            "1 2 8:1 / / rw shared:1",
            "1 2 8:1 / / rw - ext4",
            "99999999999999999999999 2 8:1 / / rw - ext4 x y",
        ] {
            assert!(parse_mountinfo_line(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn test_unescape_handles_octal_and_malformed() {
        assert_eq!(unescape("a\\040b"), "a b");
        assert_eq!(unescape("\\011\\012\\134"), "\t\n\\");
        assert_eq!(unescape("x\\04"), "x\\04");
        assert_eq!(unescape("x\\"), "x\\");
        assert_eq!(unescape("\\999"), "\\999");
        // 0o777 does not fit a byte: kept literally.
        assert_eq!(unescape("\\777"), "\\777");
        assert_eq!(unescape("\\377"), "\u{FFFD}");
        assert_eq!(unescape(""), "");
    }

    #[test]
    fn test_find_mount_by_id_then_dev_and_longest_prefix() {
        let text = format!(
            "{WSL_LINE}\n{NINEP_LINE}\n90 80 8:48 /data /srv/data rw - ext4 /dev/sdd rw\nnot a line\n"
        );
        let e = find_mount(&text, Some(77), None, None).unwrap_or_else(|| panic!("by id"));
        assert_eq!(e.fstype, "9p");
        let e = find_mount(&text, None, Some((8, 48)), Some("/srv/data/x"))
            .unwrap_or_else(|| panic!("by dev"));
        assert_eq!(e.mount_point, "/srv/data");
        let e = find_mount(&text, None, Some((8, 48)), Some("/srv/datafile"))
            .unwrap_or_else(|| panic!("by dev"));
        assert_eq!(e.mount_point, "/");
        let e = find_mount(&text, None, Some((8, 48)), None).unwrap_or_else(|| panic!("first"));
        assert_eq!(e.id, 80);
        assert!(find_mount(&text, Some(1), Some((1, 1)), None).is_none());
        assert!(find_mount("", None, Some((8, 48)), None).is_none());
    }

    #[test]
    fn test_mount_facts_tokens() {
        let f = mount_facts(
            "ext4",
            "rw,relatime",
            "rw,data=journal,nobarrier,errors=continue,lazytime",
        );
        assert!(f.data_journal && f.no_barrier && f.weak_error_mode && f.lazytime);
        let f = mount_facts("ext4", "rw", "rw,barrier=0,journal_dev=0x0811");
        assert!(f.no_barrier && f.weak_error_mode && !f.data_journal);
        let f = mount_facts(
            "overlay",
            "rw",
            "rw,lowerdir=/a,upperdir=/b,workdir=/c,volatile",
        );
        assert!(f.sync_disabled);
        let f = mount_facts("ext4", "rw", "rw,volatile");
        assert!(!f.sync_disabled);
        let f = mount_facts("f2fs", "rw", "rw,fsync_mode=nobarrier");
        assert!(f.no_barrier);
        let f = mount_facts("btrfs", "rw", "rw,compress=zstd:3,nodatacow");
        assert!(f.compressed && f.nocow);
        let f = mount_facts(
            "ext4",
            "rw,relatime",
            "rw,discard,errors=remount-ro,data=ordered",
        );
        assert_eq!(f, MountFacts::default());
        let f = mount_facts("ext4", "", "");
        assert_eq!(f, MountFacts::default());
        let f = mount_facts("ext4", ",,,=,data=,journal_dev", "");
        assert_eq!(f, MountFacts::default());
    }

    #[test]
    fn test_ext4_options_file() {
        let text = "rw\nbsddf\nnogrpid\nblock_validity\ndioread_nolock\ndelalloc\njournal_checksum\nbarrier\nuser_xattr\nacl\nauto_da_alloc\ndata=ordered\ncommit=5\nmax_batch_time=15000\n";
        assert_eq!(ext4_options_facts(text), MountFacts::default());
        let text = "rw\nnobarrier\ndata=journal\njournal_async_commit\n";
        let f = ext4_options_facts(text);
        assert!(f.no_barrier && f.data_journal && f.weak_error_mode);
        let mut m = MountFacts::default();
        m.merge(&f);
        assert_eq!(m, f);
        assert_eq!(
            ext4_options_facts("\u{0}\u{0}garbage\n\n"),
            MountFacts::default()
        );
    }

    #[test]
    fn test_inode_facts() {
        assert_eq!(inode_facts(0x80000), InodeFacts::default());
        let f = inode_facts(FS_JOURNAL_DATA_FL | FS_COMPR_FL);
        assert!(f.data_journal && f.transformed && !f.nocow);
        let f = inode_facts(FS_ENCRYPT_FL | FS_NOCOW_FL);
        assert!(!f.data_journal && f.transformed && f.nocow);
        assert!(inode_facts(u64::MAX).nocow);
    }

    #[test]
    fn test_dio_facts_respects_mask_bits() {
        let mut sx = Statx::default();
        sx.stx_dio_mem_align = 4;
        sx.stx_dio_offset_align = 512;
        sx.stx_atomic_write_unit_max = 16384;
        // Without the mask bits nothing is believed.
        let d = dio_facts(&sx);
        assert_eq!(d, DioFacts::default());
        sx.stx_mask = libc::STATX_DIOALIGN | libc::STATX_WRITE_ATOMIC;
        let d = dio_facts(&sx);
        assert!(d.reported && d.atomic_reported);
        assert_eq!(
            (d.mem_align, d.offset_align, d.atomic_unit),
            (4, 512, 16384)
        );
        sx.stx_atomic_write_unit_max_opt = 4096;
        assert_eq!(dio_facts(&sx).atomic_unit, 4096);
        sx.stx_atomic_write_unit_max_opt = 1 << 20;
        assert_eq!(dio_facts(&sx).atomic_unit, 16384);
        sx.stx_attributes = libc::STATX_ATTR_ENCRYPTED as u64;
        assert!(
            !dio_facts(&sx).transformed,
            "attribute without its mask bit"
        );
        sx.stx_attributes_mask = libc::STATX_ATTR_ENCRYPTED as u64;
        assert!(dio_facts(&sx).transformed);
    }

    #[test]
    fn test_uevent_and_basename() {
        assert_eq!(
            uevent_devname("MAJOR=8\nMINOR=48\nDEVNAME=sdd\nDEVTYPE=disk\n"),
            Some("sdd".to_owned())
        );
        assert_eq!(
            uevent_devname("DEVNAME=mapper/vg-lv\n"),
            Some("vg-lv".to_owned())
        );
        assert_eq!(uevent_devname("DEVNAME=../etc\n"), None);
        assert_eq!(uevent_devname("DEVNAME=\n"), None);
        assert_eq!(uevent_devname("MAJOR=8\n"), None);
        assert_eq!(basename("/dev/sdd"), Some("sdd"));
        assert_eq!(basename("sdd"), Some("sdd"));
        assert_eq!(basename("/dev/"), None);
        assert_eq!(basename(""), None);
    }
}
