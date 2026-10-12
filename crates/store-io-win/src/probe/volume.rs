//! Filesystem facts: volume name and flags, `FILE_STORAGE_INFO`, file
//! attributes and drive type.

use store_io_core::evidence::{FsKind, Tri};

use crate::bytes::Reader;

// Copies of the SDK constants these parsers use, so they compile on every
// platform (for fuzzing); pinned to `windows-sys` on Windows below.
const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;
const FILE_ATTRIBUTE_COMPRESSED: u32 = 0x800;
const FILE_ATTRIBUTE_ENCRYPTED: u32 = 0x4000;
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x4_0000;
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
const DRIVE_REMOTE: u32 = 4;
const DRIVE_RAMDISK: u32 = 6;

#[cfg(windows)]
const _: () = {
    use windows_sys::Win32::Storage::FileSystem as fs;
    use windows_sys::Win32::System::WindowsProgramming as wp;
    assert!(FILE_ATTRIBUTE_SPARSE_FILE == fs::FILE_ATTRIBUTE_SPARSE_FILE);
    assert!(FILE_ATTRIBUTE_COMPRESSED == fs::FILE_ATTRIBUTE_COMPRESSED);
    assert!(FILE_ATTRIBUTE_ENCRYPTED == fs::FILE_ATTRIBUTE_ENCRYPTED);
    assert!(FILE_ATTRIBUTE_RECALL_ON_OPEN == fs::FILE_ATTRIBUTE_RECALL_ON_OPEN);
    assert!(FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS == fs::FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS);
    assert!(DRIVE_REMOTE == wp::DRIVE_REMOTE);
    assert!(DRIVE_RAMDISK == wp::DRIVE_RAMDISK);
};

/// Maps a filesystem name from `GetVolumeInformationByHandleW`.
pub(crate) fn fs_kind(name: &str) -> FsKind {
    let n = name.to_ascii_uppercase();
    match n.as_str() {
        "NTFS" => FsKind::Ntfs,
        "REFS" => FsKind::Refs,
        "FAT" | "FAT12" | "FAT16" | "FAT32" | "EXFAT" => FsKind::Fat,
        _ => FsKind::Other,
    }
}

/// What the drive type says about the volume: a network share or a RAM disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DriveClass {
    Local,
    Network,
    Ram,
}

pub(crate) fn drive_class(drive_type: u32) -> DriveClass {
    match drive_type {
        DRIVE_REMOTE => DriveClass::Network,
        DRIVE_RAMDISK => DriveClass::Ram,
        _ => DriveClass::Local,
    }
}

/// `FILE_STORAGE_INFO` fields store-io uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct StorageInfo {
    pub logical_bytes_per_sector: u32,
    pub physical_bytes_for_atomicity: u32,
    pub physical_bytes_for_performance: u32,
    pub fs_effective_atomicity: u32,
}

pub(crate) fn parse_storage_info(buf: &[u8]) -> Option<StorageInfo> {
    let r = Reader::new(buf);
    Some(StorageInfo {
        logical_bytes_per_sector: r.u32(0)?,
        physical_bytes_for_atomicity: r.u32(4)?,
        physical_bytes_for_performance: r.u32(8)?,
        fs_effective_atomicity: r.u32(12)?,
    })
}

/// What file attributes say about the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AttributeFacts {
    /// Compressed or EFS-encrypted: I/O is synchronous and transformed.
    pub transformed: bool,
    /// Sparse: overwrites may allocate.
    pub may_allocate: Tri,
    /// A cloud-files placeholder: data lives on a network service.
    pub cloud_placeholder: bool,
}

pub(crate) fn attribute_facts(attrs: u32) -> AttributeFacts {
    AttributeFacts {
        transformed: attrs & (FILE_ATTRIBUTE_COMPRESSED | FILE_ATTRIBUTE_ENCRYPTED) != 0,
        may_allocate: if attrs & FILE_ATTRIBUTE_SPARSE_FILE != 0 {
            Tri::Yes
        } else {
            Tri::No
        },
        cloud_placeholder: attrs
            & (FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_RECALL_ON_OPEN)
            != 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fs_kind_names() {
        assert_eq!(fs_kind("NTFS"), FsKind::Ntfs);
        assert_eq!(fs_kind("ntfs"), FsKind::Ntfs);
        assert_eq!(fs_kind("ReFS"), FsKind::Refs);
        assert_eq!(fs_kind("FAT32"), FsKind::Fat);
        assert_eq!(fs_kind("exFAT"), FsKind::Fat);
        assert_eq!(fs_kind("UDF"), FsKind::Other);
        assert_eq!(fs_kind(""), FsKind::Other);
    }

    #[test]
    fn test_drive_class() {
        assert_eq!(drive_class(DRIVE_REMOTE), DriveClass::Network);
        assert_eq!(drive_class(DRIVE_RAMDISK), DriveClass::Ram);
        assert_eq!(drive_class(3), DriveClass::Local);
        assert_eq!(drive_class(0), DriveClass::Local);
    }

    #[test]
    fn test_storage_info_parse_and_truncation() {
        let b: Vec<u8> = [512u32, 4096, 4096, 4096, 0xF, 0, 0]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        assert_eq!(
            parse_storage_info(&b),
            Some(StorageInfo {
                logical_bytes_per_sector: 512,
                physical_bytes_for_atomicity: 4096,
                physical_bytes_for_performance: 4096,
                fs_effective_atomicity: 4096,
            })
        );
        assert_eq!(parse_storage_info(&b[..15]), None);
        assert_eq!(parse_storage_info(&[]), None);
    }

    #[test]
    fn test_attribute_facts() {
        let plain = attribute_facts(0x20);
        assert!(!plain.transformed && !plain.cloud_placeholder);
        assert_eq!(plain.may_allocate, Tri::No);
        assert!(attribute_facts(FILE_ATTRIBUTE_COMPRESSED).transformed);
        assert!(attribute_facts(FILE_ATTRIBUTE_ENCRYPTED).transformed);
        assert_eq!(
            attribute_facts(FILE_ATTRIBUTE_SPARSE_FILE).may_allocate,
            Tri::Yes
        );
        assert!(attribute_facts(FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS).cloud_placeholder);
        assert!(attribute_facts(FILE_ATTRIBUTE_RECALL_ON_OPEN).cloud_placeholder);
    }
}
