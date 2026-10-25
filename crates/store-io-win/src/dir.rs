//! Directory handles: creation, exclusive file creation, rename by handle,
//! POSIX delete and directory flush.

use core::mem::offset_of;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use store_io_core::errno::is_unsupported;
use store_io_platform::{FileMode, FileName, RawResult};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_CANT_ACCESS_FILE, ERROR_DIRECTORY, ERROR_WRITE_PROTECT,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_RENAME_INFO,
    FileDispositionInfo, FileDispositionInfoEx, FileRenameInfo, FileRenameInfoEx, OPEN_EXISTING,
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
    /// Opens `path`; with `create`, creates it if missing, then flushes its
    /// parent so the entry is durable. Also when the directory exists: an
    /// earlier attempt that died, or one racing this, may have made it
    /// without the flush. The parent of an existing directory is left as it
    /// is where it cannot be opened for the flush (`ERROR_ACCESS_DENIED`:
    /// Windows has no volume-wide flush an unprivileged caller may make, so
    /// a directory made under such a parent by an attempt that died, or
    /// raced this one, keeps an entry that may not be durable), where it is
    /// write-protected (`ERROR_WRITE_PROTECT`), or where it cannot be
    /// flushed (unsupported: SMB and WSL shares; `sync_dir` and the probe
    /// class such a volume). A directory made here whose flush fails is
    /// removed again (if it is still the one made), so a retry never meets
    /// it as existing.
    pub(crate) fn open(path: &Path, create: bool) -> RawResult<Self> {
        let path = std::path::absolute(path)
            .map_err(|e| sys::win32(e.raw_os_error().unwrap_or(0) as u32))?;
        let created = create && sys::create_directory(&path)?;
        let handle = open_dir_handle(&path)?;
        if create {
            if let Some(parent) = path.parent() {
                match flush_parent(parent) {
                    Ok(()) => {}
                    Err(e)
                        if !created
                            && (e.code == ERROR_ACCESS_DENIED as i32
                                || e.code == ERROR_WRITE_PROTECT as i32
                                || is_unsupported(e)) => {}
                    Err(e) => {
                        if created {
                            let _removed = remove_made(&path, &handle);
                        }
                        return Err(e);
                    }
                }
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

    /// Renames the open `file` to `to` in this directory, refusing if `to`
    /// exists (`ERROR_ALREADY_EXISTS`).
    pub(crate) fn rename_noreplace(&self, to: &FileName, file: &WinFile) -> RawResult<()> {
        let target = self.child(to);
        let ex = rename_record(&target, FILE_RENAME_FLAG_POSIX_SEMANTICS);
        match sys::set_file_info(file.handle(), FileRenameInfoEx, &ex) {
            Ok(()) => Ok(()),
            Err(e) if sys::is_unsupported(e) => {
                self.legacy_rename.store(true, Ordering::Relaxed);
                // ReplaceIfExists FALSE.
                let legacy = rename_record(&target, 0);
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

/// Removes the directory `path` names if it is the one `h` is open on (the
/// same volume and file id): a second handle to it, with delete access,
/// marked for deletion with POSIX semantics (its name goes at once, so a
/// retry can make it again while another opener holds it; else the legacy
/// disposition, when its last handle closes); one that is not empty stays.
/// (`ReOpenFile` refuses a directory handle.) The handle is opened by path
/// after the create, so a directory swapped in between the two would be
/// the one checked against: an empty directory at worst.
fn remove_made(path: &Path, h: &Handle) -> RawResult<()> {
    let d = sys::create_file(
        path,
        DELETE | FILE_READ_ATTRIBUTES,
        sys::SHARE_ALL,
        OPEN_EXISTING,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
    )?;
    if sys::file_id(&d)? != sys::file_id(h)? {
        return Err(sys::win32(ERROR_CANT_ACCESS_FILE));
    }
    let flags: u32 = FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS;
    match sys::set_file_info(&d, FileDispositionInfoEx, &flags.to_le_bytes()) {
        Err(e) if sys::is_unsupported(e) => sys::set_file_info(&d, FileDispositionInfo, &[1u8]),
        r => r,
    }
}

/// Flushes the directory `parent`, opened with only the right a directory
/// needs to have been made in it (`FILE_ADD_SUBDIRECTORY`, which is
/// `FILE_APPEND_DATA`, enough for `FlushFileBuffers`), following reparse
/// points: the directory `path` names, which holds the entry.
fn flush_parent(parent: &Path) -> RawResult<()> {
    let h = sys::create_file(
        parent,
        FILE_ADD_SUBDIRECTORY,
        sys::SHARE_ALL,
        OPEN_EXISTING,
        FILE_FLAG_BACKUP_SEMANTICS,
    )?;
    sys::flush_file_buffers(&h)
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

    /// A made directory is removed through a handle checked to be on it; a
    /// different directory at the path is never removed.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn test_a_made_directory_is_removed_only_through_its_own_handle() {
        let base = std::env::temp_dir().join(format!("sio-win-remove-{}", std::process::id()));
        let _gone = std::fs::remove_dir_all(&base);
        let (a, b) = (base.join("a"), base.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let ha = open_dir_handle(&a).unwrap();
        assert!(remove_made(&b, &ha).is_err());
        assert!(b.is_dir());
        remove_made(&a, &ha).unwrap();
        // POSIX semantics: the name is gone while the handle is still open,
        // so a retry can make the directory again at once.
        std::fs::create_dir(&a).unwrap();
        drop(ha);
        std::fs::remove_dir_all(&base).unwrap();
    }
}
