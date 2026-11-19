//! A new store's directory while a call builds it under a temporary name
//! (`copy_store`, `import`): the copy lock held from before the directory
//! is checked for a store until after the rename, a crashed build's
//! temporary removed under it, and the rename into place that never
//! replaces a store created meanwhile (D-116, R5.6).

use std::path::Path;

use store_io_core::errno::{is_already_exists, is_lock_held, is_not_found};
use store_io_core::error::{Error, ErrorContext, Named, NotWrittenCause, Op};
use store_io_platform::{FileMode, FileName, Platform};

use crate::copy::{COPY_LOCK, COPYING};
use crate::store::{CONTAINER, io};

fn name(s: &str) -> Result<FileName, Error> {
    FileName::new(s).map_err(|_| Error::NotWritten {
        cause: NotWrittenCause::InvalidName,
        ctx: ErrorContext::default(),
    })
}

/// The target directory of a call building a new store, its copy lock
/// held until it drops.
pub(crate) struct TargetDir<P: Platform> {
    pub(crate) dir: P::Dir,
    /// The container's name, and the temporary one the store is built
    /// under.
    pub(crate) final_name: FileName,
    pub(crate) temp: FileName,
    _lock_file: P::File,
    _lock: P::Lock,
}

impl<P: Platform> TargetDir<P> {
    /// Opens (creating) `to`, refuses it if it holds a container file,
    /// takes the copy lock, refuses again under it, and removes a crashed
    /// build's temporary.
    ///
    /// # Errors
    ///
    /// `Io` if the directory cannot be made or opened, or whether it holds
    /// a store cannot be told; `AlreadyExists` if it holds one; `Locked`
    /// while another copy or import into it runs.
    pub(crate) fn open(platform: &P, to: &Path) -> Result<Self, Error> {
        let dir = platform
            .open_dir(to, true)
            .map_err(|raw| io(Op::Create, raw))?;
        let final_name = name(CONTAINER)?;
        let temp = name(COPYING)?;
        let holds_store = || match platform.open_file(&dir, &final_name, FileMode::ReadOnly) {
            Ok(_) => Err(Error::AlreadyExists { what: Named::Store }),
            Err(raw) if is_not_found(raw) => Ok(()),
            Err(raw) => Err(io(Op::Open, raw)),
        };
        holds_store()?;
        // The copy lock, then (under it) no store there and a crashed
        // build's leftover removed: no other call can touch the temporary
        // name until this one's rename is done.
        let lock_name = name(COPY_LOCK)?;
        let lock_file = match platform.create_file(&dir, &lock_name) {
            Ok(f) => f,
            Err(raw) if is_already_exists(raw) => platform
                .open_file(&dir, &lock_name, FileMode::ReadWrite)
                .map_err(|raw| io(Op::Open, raw))?,
            Err(raw) => return Err(io(Op::Create, raw)),
        };
        let lock = platform.lock_exclusive(&lock_file).map_err(|raw| {
            if is_lock_held(raw) {
                Error::Locked
            } else {
                io(Op::Lock, raw)
            }
        })?;
        holds_store()?;
        match platform.unlink(&dir, &temp) {
            Ok(()) => {}
            Err(raw) if is_not_found(raw) => {}
            Err(raw) => return Err(io(Op::Unlink, raw)),
        }
        Ok(Self {
            dir,
            final_name,
            temp,
            _lock_file: lock_file,
            _lock: lock,
        })
    }

    /// Renames the closed temporary store into place (never replacing one
    /// another process created meanwhile) and syncs the directory: the
    /// commit of the call.
    ///
    /// # Errors
    ///
    /// `Io` if the temporary cannot be opened; `AlreadyExists` if a store
    /// appeared under the target name (the temporary removed, best effort);
    /// `DurabilityUnknown` if the rename or the directory sync after it
    /// failed (the store may or may not be in place).
    pub(crate) fn commit(&self, platform: &P) -> Result<(), Error> {
        let file = platform
            .open_file(&self.dir, &self.temp, FileMode::ReadWrite)
            .map_err(|raw| io(Op::Open, raw))?;
        match platform.rename_noreplace(&self.dir, &self.temp, &self.final_name, &file) {
            Ok(()) => {}
            Err(raw) if is_already_exists(raw) => {
                drop(file);
                // Best effort: the next copy or import removes it otherwise.
                if platform.unlink(&self.dir, &self.temp).is_ok() {
                    let _synced = platform.sync_dir(&self.dir);
                }
                return Err(Error::AlreadyExists { what: Named::Store });
            }
            Err(raw) => {
                return Err(Error::DurabilityUnknown {
                    op: Op::Rename,
                    raw: Some(raw),
                    ctx: ErrorContext::default(),
                });
            }
        }
        drop(file);
        platform
            .sync_dir(&self.dir)
            .map_err(|raw| Error::DurabilityUnknown {
                op: Op::SyncDir,
                raw: Some(raw),
                ctx: ErrorContext::default(),
            })
    }
}
