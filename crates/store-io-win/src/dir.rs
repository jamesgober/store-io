//! Directory handles: creation, exclusive file creation, rename by handle,
//! POSIX delete and directory flush.

use core::mem::offset_of;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use store_io_platform::{FileMode, FileName, RawResult};
use windows_sys::Win32::Foundation::{ERROR_CANT_ACCESS_FILE, ERROR_DIRECTORY};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ADD_FILE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY,
    FILE_READ_ATTRIBUTES, FILE_RENAME_INFO, FileDispositionInfo, FileDispositionInfoEx,
    FileRenameInfo, FileRenameInfoEx, OPEN_EXISTING,
};
use windows_sys::Win32::System::WindowsProgramming::{
    FILE_RENAME_FLAG_POSIX_SEMANTICS, FILE_RENAME_FLAG_REPLACE_IF_EXISTS,
};

use crate::file::WinFile;
use crate::sys::{self, Handle};

/// An open directory (`FILE_FLAG_BACKUP_SEMANTICS`, add-file access so
/// `FlushFileBuffers` is accepted).
#[derive(Debug)]
pub struct WinDir {
    handle: Handle,
    path: PathBuf,
    legacy_rename: AtomicBool,
    legacy_delete: AtomicBool,
}

impl WinDir {
    /// Opens `path`, creating it (and flushing its parent) when `create`.
    pub(crate) fn open(path: &Path, create: bool) -> RawResult<Self> {
        let path = std::path::absolute(path)
            .map_err(|e| sys::win32(e.raw_os_error().unwrap_or(0) as u32))?;
        let mut created = false;
        if create {
            created = sys::create_directory(&path)?;
        }
        let handle = open_dir_handle(&path)?;
        if created {
            if let Some(parent) = path.parent() {
                let p = open_dir_handle(parent)?;
                sys::flush_file_buffers(&p)?;
            }
        }
        Ok(Self {
            handle,
            path,
            legacy_rename: AtomicBool::new(false),
            legacy_delete: AtomicBool::new(false),
        })
    }

    /// The absolute directory path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether a rename on this directory had to fall back from
    /// `FileRenameInfoEx(POSIX_SEMANTICS)` to legacy `FileRenameInfo`
    /// (which fails when the target has open handles). Reported, never silent.
    #[must_use]
    pub fn used_legacy_rename(&self) -> bool {
        self.legacy_rename.load(Ordering::Relaxed)
    }

    /// Whether an unlink had to fall back from POSIX delete to `DeleteFileW`.
    #[must_use]
    pub fn used_legacy_delete(&self) -> bool {
        self.legacy_delete.load(Ordering::Relaxed)
    }

    pub(crate) fn handle(&self) -> &Handle {
        &self.handle
    }

    pub(crate) fn child(&self, name: &FileName) -> PathBuf {
        self.path.join(name.as_str())
    }

    pub(crate) fn create_file(&self, name: &FileName) -> RawResult<WinFile> {
        WinFile::open(&self.child(name), FileMode::ReadWrite, true)
    }

    pub(crate) fn open_file(&self, name: &FileName, mode: FileMode) -> RawResult<WinFile> {
        WinFile::open(&self.child(name), mode, false)
    }

    /// Renames the open `file` over `to` in this directory.
    pub(crate) fn rename_replace(&self, to: &FileName, file: &WinFile) -> RawResult<()> {
        let target = self.child(to);
        let ex = rename_record(
            &target,
            FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS,
        );
        match sys::set_file_info(file.handle(), FileRenameInfoEx, &ex) {
            Ok(()) => Ok(()),
            Err(e) if sys::is_unsupported(e) => {
                self.legacy_rename.store(true, Ordering::Relaxed);
                let legacy = rename_record(&target, 1);
                sys::set_file_info(file.handle(), FileRenameInfo, &legacy)
            }
            Err(e) => Err(e),
        }
    }

    /// Removes `name` with POSIX semantics (open handles keep working).
    pub(crate) fn unlink(&self, name: &FileName) -> RawResult<()> {
        let path = self.child(name);
        let h = sys::create_file(
            &path,
            DELETE | FILE_READ_ATTRIBUTES,
            sys::SHARE_ALL,
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
        )?;
        let (attrs, _tag) = sys::attribute_tag(&h)?;
        if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(sys::win32(ERROR_CANT_ACCESS_FILE));
        }
        let flags: u32 = FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS;
        match sys::set_file_info(&h, FileDispositionInfoEx, &flags.to_le_bytes()) {
            Ok(()) => Ok(()),
            Err(e) if sys::is_unsupported(e) => {
                self.legacy_delete.store(true, Ordering::Relaxed);
                match sys::set_file_info(&h, FileDispositionInfo, &[1u8]) {
                    Ok(()) => Ok(()),
                    Err(_) => {
                        drop(h);
                        sys::delete_file(&path)
                    }
                }
            }
            Err(e) => Err(e),
        }
    }

    /// `FlushFileBuffers` on the directory handle.
    pub(crate) fn sync(&self) -> RawResult<()> {
        sys::flush_file_buffers(&self.handle)
    }
}

/// Opens a directory handle with add-file access, refusing reparse points.
fn open_dir_handle(path: &Path) -> RawResult<Handle> {
    let h = sys::create_file(
        path,
        FILE_LIST_DIRECTORY | FILE_ADD_FILE | FILE_READ_ATTRIBUTES,
        sys::SHARE_ALL,
        OPEN_EXISTING,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
    )?;
    let (attrs, _tag) = sys::attribute_tag(&h)?;
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(sys::win32(ERROR_CANT_ACCESS_FILE));
    }
    if attrs & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(sys::win32(ERROR_DIRECTORY));
    }
    Ok(h)
}

/// A `FILE_RENAME_INFO` record with a full target path, laid out by hand
/// because the structure ends in a flexible array.
fn rename_record(target: &Path, flags_or_replace: u32) -> Vec<u8> {
    let name = sys::wide(target);
    let name_bytes = (name.len() - 1) * 2;
    let name_off = offset_of!(FILE_RENAME_INFO, FileName);
    let mut rec = vec![0u8; name_off + name_bytes + 2];
    // The first field is a union of ReplaceIfExists (BOOLEAN) and Flags
    // (u32); writing the u32 covers both readings.
    rec[..4].copy_from_slice(&flags_or_replace.to_le_bytes());
    let len_off = offset_of!(FILE_RENAME_INFO, FileNameLength);
    rec[len_off..len_off + 4].copy_from_slice(&(name_bytes as u32).to_le_bytes());
    for (i, ch) in name.iter().enumerate() {
        let at = name_off + i * 2;
        rec[at..at + 2].copy_from_slice(&ch.to_le_bytes());
    }
    rec
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rename_record_layout_matches_the_c_structure() {
        let rec = rename_record(Path::new("C:\\x\\y"), 3);
        let name_off = offset_of!(FILE_RENAME_INFO, FileName);
        assert_eq!(&rec[..4], &3u32.to_le_bytes());
        let root_off = offset_of!(FILE_RENAME_INFO, RootDirectory);
        assert!(
            rec[root_off..root_off + size_of::<usize>()]
                .iter()
                .all(|&b| b == 0)
        );
        let len_off = offset_of!(FILE_RENAME_INFO, FileNameLength);
        assert_eq!(&rec[len_off..len_off + 4], &(6u32 * 2).to_le_bytes());
        assert_eq!(rec.len(), name_off + 12 + 2);
        assert_eq!(&rec[name_off..name_off + 2], &(b'C' as u16).to_le_bytes());
        assert_eq!(&rec[rec.len() - 2..], &[0, 0]);
    }
}
