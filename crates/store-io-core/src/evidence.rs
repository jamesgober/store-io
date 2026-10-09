//! Raw facts a platform backend reports about a path and its device.
//!
//! Backends fill this in; they never decide a class. [`crate::decide`] turns
//! evidence into a class, and the same evidence is printed in the device
//! report so every decision can be audited. Fields a backend could not read
//! stay `Unknown` / `None` and are listed in [`Evidence::missing`].

use crate::class::MissingSet;

/// A fact that may be true, false, or not knowable on this platform or with
/// this privilege.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tri {
    /// The fact holds.
    Yes,
    /// The fact does not hold.
    No,
    /// The fact could not be read.
    #[default]
    Unknown,
}

/// Operating system the evidence comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformKind {
    /// Linux.
    Linux,
    /// Windows.
    Windows,
    /// macOS.
    MacOs,
    /// The deterministic simulator.
    Sim,
}

/// Device interconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Bus {
    /// NVMe (PCIe).
    Nvme,
    /// SATA / ATA.
    Sata,
    /// SAS / SCSI.
    Scsi,
    /// USB-attached storage.
    Usb,
    /// A virtual device presented by a hypervisor or cloud.
    Virtual,
    /// RAM-backed block device.
    Ram,
    /// Not determined.
    #[default]
    Unknown,
}

/// What the operating system believes about the device write cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OsCacheMode {
    /// The OS issues flushes (Linux `write back`; Windows cache enabled).
    WriteBack,
    /// The OS issues no flushes (Linux `write through`).
    WriteThrough,
    /// Not readable.
    #[default]
    Unknown,
}

/// Facts about the device itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceFacts {
    /// Model string as reported by the device.
    pub model: Option<String>,
    /// Firmware revision.
    pub firmware: Option<String>,
    /// Interconnect.
    pub bus: Bus,
    /// The device reports a volatile write cache (NVMe Identify VWC bit 0,
    /// SCSI caching mode page, ATA identify).
    pub cache_present: Tri,
    /// The volatile write cache is currently enabled.
    pub cache_enabled: Tri,
    /// What the OS believes about the cache.
    pub os_cache_mode: OsCacheMode,
    /// The OS will pass flushes to the device (Windows `FlushCacheSupported`).
    pub os_flush_supported: Tri,
    /// Windows "turn off write-cache buffer flushing" is set
    /// (`UserDefinedPowerProtection`): flushes are completed without reaching
    /// the device.
    pub user_power_protection: Tri,
    /// SMART critical warning bit 4: volatile-memory backup has failed.
    pub backup_failed: Tri,
    /// The device supports FUA writes.
    pub fua_supported: Tri,
    /// Logical block size in bytes (0 = unknown).
    pub logical_block: u32,
    /// Physical block size in bytes (0 = unknown).
    pub physical_block: u32,
    /// Preferred write granularity (NVMe NPWG / Linux `io_min`), bytes.
    pub optimal_write: u32,
    /// Largest single transfer in bytes.
    pub max_transfer: u32,
    /// NVMe atomic-write fields, raw, 0-based as in the specification.
    pub atomic: AtomicFields,
    /// NUMA node of the device.
    pub numa_node: Option<u16>,
    /// The namespace is shared by several controllers (NVMe CMIC).
    pub multi_controller: bool,
}

/// NVMe atomic-write fields exactly as the specification defines them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AtomicFields {
    /// AWUPF, 0-based logical blocks (controller-wide).
    pub awupf: Option<u16>,
    /// NSFEAT.NSABP: namespace atomic fields are valid.
    pub nsabp: bool,
    /// NAWUPF, 0-based logical blocks (0h means "same as AWUPF").
    pub nawupf: u16,
    /// NABSPF, 0-based boundary size (0h means no boundary).
    pub nabspf: u16,
    /// NABO, boundary offset in logical blocks.
    pub nabo: u16,
}

/// Filesystem kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FsKind {
    /// A raw block device or owned namespace, no filesystem.
    Raw,
    /// ext4.
    Ext4,
    /// XFS.
    Xfs,
    /// NTFS.
    Ntfs,
    /// ReFS.
    Refs,
    /// APFS.
    Apfs,
    /// HFS+.
    Hfs,
    /// btrfs.
    Btrfs,
    /// ZFS.
    Zfs,
    /// bcachefs (out of the mainline kernel since 6.18).
    Bcachefs,
    /// F2FS.
    F2fs,
    /// Linux ntfs3 driver.
    Ntfs3,
    /// tmpfs / ramfs.
    Tmpfs,
    /// A FUSE filesystem.
    Fuse,
    /// NFS, SMB/CIFS or another network filesystem.
    Network,
    /// overlayfs.
    Overlay,
    /// FAT / exFAT.
    Fat,
    /// 9p, drvfs, virtiofs or another pass-through filesystem.
    Passthrough,
    /// The simulator's filesystem.
    Sim,
    /// Anything else.
    #[default]
    Other,
}

/// Mount and file attributes that change durability.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FsFacts {
    /// Filesystem kind.
    pub kind: FsKind,
    /// ext4 `data=journal` or the file's `+j` attribute.
    pub data_journal: bool,
    /// `nobarrier` / `barrier=0` / F2FS `fsync_mode=nobarrier`.
    pub no_barrier: bool,
    /// Filesystem-level syncs disabled (ZFS `sync=disabled`, overlay `volatile`).
    pub sync_disabled: bool,
    /// `errors=continue`, no journal, external or async journal commit.
    pub weak_error_mode: bool,
    /// File is compressed or uses software (non-inline) encryption.
    pub transformed: bool,
    /// Copy-on-write or snapshots may make overwrites allocate.
    pub cow: Tri,
    /// Direct I/O is honoured (the canary write stayed out of the page cache).
    pub direct_io: Tri,
    /// The full-flush primitive is supported (macOS F_FULLFSYNC).
    pub full_flush: Tri,
    /// Directory flushes are supported (Windows directory FlushFileBuffers).
    pub dir_flush: Tri,
    /// Filesystem-reported atomic write unit in bytes (statx, Windows
    /// `FileSystemEffectivePhysicalBytesPerSectorForAtomicity`), 0 if none.
    pub os_atomic_unit: u32,
    /// Direct-I/O memory and offset alignment in bytes (0 = unknown).
    pub dio_mem_align: u32,
    /// Direct-I/O offset alignment in bytes (0 = unknown).
    pub dio_offset_align: u32,
}

/// One layer of the storage stack between the filesystem and the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackLayer {
    /// A pass-through mapping (dm-linear, partitions).
    Linear,
    /// Disk encryption that forwards flushes (dm-crypt).
    Crypt,
    /// Thin provisioning (dm-thin, thin LUN, NVMe thin namespace).
    Thin,
    /// A write-back cache layer (dm-writecache, bcache writeback).
    WriteBackCache,
    /// RAID 0 / 1 / 10 (no parity).
    Mirror,
    /// RAID 4/5/6 with a write journal or partial parity log.
    ParityProtected,
    /// RAID 4/5/6 with neither (write hole).
    ParityUnprotected,
    /// A mirror member in write-behind mode.
    WriteBehind,
    /// A loop device over a file.
    Loop,
    /// A deduplicating or zero-detecting layer (VDO).
    Dedupe,
    /// A virtual-disk file (qcow2, dynamic VHDX).
    VirtualDisk,
    /// A filter driver or software encryption (BitLocker) on Windows.
    Filter,
}

/// A hypervisor or cloud environment detected under the OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hypervisor {
    /// KVM / QEMU.
    Kvm,
    /// Hyper-V (including WSL2).
    HyperV,
    /// VMware.
    Vmware,
    /// Xen.
    Xen,
    /// AWS Nitro.
    Nitro,
    /// Another or unidentified hypervisor.
    Other,
}

/// How much the probing process was allowed to see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Privilege {
    /// Could read device identify and SMART data.
    Full,
    /// Unprivileged; some device facts are missing.
    #[default]
    Unprivileged,
}

/// Advisory timings measured at volume creation or on explicit reprobe.
/// They never change the class.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Timings {
    /// Median flush time with nothing dirty, nanoseconds.
    pub flush_idle_ns: Option<u64>,
    /// Median flush time after ≥ 64 MiB of incompressible writes, nanoseconds.
    pub flush_loaded_ns: Option<u64>,
    /// Median extra cost of a FUA write over a plain write, nanoseconds.
    pub fua_extra_ns: Option<u64>,
    /// Write-latency inflation while a flush runs, in percent.
    pub flush_blocks_writes_pct: Option<u32>,
}

/// Everything a backend could learn about one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    /// Where the evidence comes from.
    pub platform: PlatformKind,
    /// Device facts.
    pub device: DeviceFacts,
    /// Filesystem facts.
    pub fs: FsFacts,
    /// Stack layers from the filesystem down to the device.
    pub stack: Vec<StackLayer>,
    /// Detected hypervisor, if any.
    pub hypervisor: Option<Hypervisor>,
    /// Probing privilege.
    pub privilege: Privilege,
    /// Kernel version `(major, minor, patch)` where meaningful.
    pub kernel: Option<(u16, u16, u16)>,
    /// Advisory timings.
    pub timings: Timings,
    /// Evidence that could not be obtained.
    pub missing: MissingSet,
}

impl Evidence {
    /// Evidence with every fact unknown, for the given platform.
    #[must_use]
    pub fn unknown(platform: PlatformKind) -> Self {
        Self {
            platform,
            device: DeviceFacts::default(),
            fs: FsFacts::default(),
            stack: Vec::new(),
            hypervisor: None,
            privilege: Privilege::Unprivileged,
            kernel: None,
            timings: Timings::default(),
            missing: MissingSet::new(),
        }
    }
}
