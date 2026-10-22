//! Directory mode: whole files replaced atomically.
//!
//! A [`Directory`] holds small objects as ordinary files (a manifest, a
//! `CURRENT` pointer, a configuration). [`Directory::replace`] swaps a file's
//! contents so that after any crash it holds exactly the old bytes or exactly
//! the new ones:
//!
//! 1. write the new bytes to a temporary file in the same directory (direct
//!    I/O), set its exact length, and flush it, data and metadata;
//! 2. rename it over the target (replace-if-exists, POSIX semantics);
//! 3. flush the directory, so the rename itself is durable.
//!
//! Success is reported only after all three. A failure before the rename
//! leaves the target untouched. Any failure from the rename on is
//! "durability unknown": the handle is poisoned and refuses every later
//! change, because the directory may now name either version. A directory
//! flush the platform cannot do is an error, never a success.
//!
//! The device is probed once, with the first file the handle opens, and the
//! durability class decides as it does for a store: unsafe or unverified
//! devices are refused unless the caller's [`Trust`] says otherwise.

use std::path::Path;
use std::sync::{Mutex, OnceLock, PoisonError};

use store_io_buf::{BufPool, PoolConfig};
use store_io_core::align::align_up;
use store_io_core::class::DurabilityClass;
use store_io_core::decide::{ClassDecision, Trust, decide};
use store_io_core::error::{
    CorruptionKind, Error, ErrorContext, FirstCause, Named, NotWrittenCause, Op, OsError,
};
use store_io_platform::{FileMode, FileName, IoOp, Platform, QueueConfig};

use crate::exec::{QueueSet, Ran, run};
use crate::io::not_written;

/// Block granularity of directory-mode transfers.
const BLOCK: u64 = 4096;
/// Largest single transfer.
const CHUNK: usize = 1 << 20;
/// Suffix of the temporary file a replace writes first.
const TEMP_SUFFIX: &str = ".sio-tmp";
/// The file whose exclusive lock a change holds, so one change at a time
/// runs in the directory across handles and processes (the temporary name
/// is then the changer's alone).
const LOCK_NAME: &str = ".sio-lock";
/// Longest object name: room for [`TEMP_SUFFIX`] within a file name.
const NAME_MAX: usize = 255 - TEMP_SUFFIX.len();

fn io(op: Op, raw: OsError) -> Error {
    Error::Io {
        op,
        raw,
        ctx: ErrorContext::default(),
    }
}

fn name_of(name: &str) -> Result<FileName, Error> {
    FileName::new(name)
        .map_err(|_| not_written(NotWrittenCause::InvalidName, ErrorContext::default()))
}

/// An object's name: a file name that leaves room for the temporary suffix
/// and never resolves to one of the directory's own files (a temporary
/// file or the lock, which a change would otherwise remove or overwrite),
/// to the directory itself, or to another file through a platform's
/// aliases (short names, devices, trimmed trailing characters). Names that
/// differ only in case, or in ignorable code points, may still be one file
/// on a case-insensitive file system, as `A` and `a` always were. Refused,
/// on every platform alike so names stay portable:
///
/// - longer than [`NAME_MAX`];
/// - empty or ending in `.` or a space once ignorable code points are
///   dropped (Windows drops trailing dots and spaces: `x.` is `x`, `...` is
///   the directory itself; and `.\u{200C}` is `.`, `\u{200C}` nothing, on
///   file systems that drop ignorables);
/// - shaped like a Windows short name (`SIO-LO~1`, `X~1.SIO`), which can
///   alias any long name;
/// - a Windows device name (`NUL`, `COM1`, `CON.txt`, ...);
/// - the lock file or a temporary file's name, compared as a
///   case-insensitive file system compares names: case folded (Unicode
///   included: `ſ` is `s` and `K` (Kelvin) is `k` there) and ignorable code
///   points dropped (HFS+, and Linux case-folding directories, skip them:
///   `.sio\u{200C}-lock` is `.sio-lock` there). Elsewhere in a name they
///   are kept: a word with a zero-width non-joiner is a name like any
///   other.
fn object_name(name: &str) -> Result<FileName, Error> {
    // Ignorable code points dropped, as case-insensitive file systems drop
    // them when comparing names.
    let folded: String = name
        .chars()
        .filter(|&c| !ignorable(c))
        .map(|c| match c {
            '\u{17F}' => 's',
            '\u{212A}' => 'k',
            c => c.to_ascii_lowercase(),
        })
        .collect();
    // Empty or ending in `.` or a space once folded (`x.`, `...`,
    // `.\u{200C}`): the directory itself, its parent, or a name Windows
    // trims.
    let trimmed = folded.is_empty() || folded.ends_with(['.', ' ']);
    let own = folded.ends_with(TEMP_SUFFIX) || folded == LOCK_NAME;
    let too_long = name.len() > NAME_MAX;
    let alias = short_name_shaped(name) || device_name(name);
    if too_long || alias || trimmed || own {
        return Err(not_written(
            NotWrittenCause::InvalidName,
            ErrorContext::default(),
        ));
    }
    name_of(name)
}

/// Whether `name` has the shape of a Windows 8.3 short name: a base of at
/// most 8 characters ending in `~` and digits (what comes before may hold
/// `~` itself: `A~BCDE~1`, `~$DOCU~1`), then optionally `.` and at most 3
/// characters.
fn short_name_shaped(name: &str) -> bool {
    let (base, ext) = name.split_once('.').unwrap_or((name, ""));
    let Some((_, digits)) = base.rsplit_once('~') else {
        return false;
    };
    !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && base.chars().count() <= 8
        && ext.chars().count() <= 3
        && !ext.contains('.')
}

/// Whether case-insensitive file systems ignore `c` when comparing names:
/// Unicode's Default_Ignorable_Code_Point (Linux case-folding directories
/// drop all of them; HFS+ a subset).
fn ignorable(c: char) -> bool {
    matches!(
        c,
        '\u{AD}'
            | '\u{34F}'
            | '\u{61C}'
            | '\u{115F}'..='\u{1160}'
            | '\u{17B4}'..='\u{17B5}'
            | '\u{180B}'..='\u{180F}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{3164}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{FFA0}'
            | '\u{FFF0}'..='\u{FFF8}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0000}'..='\u{E0FFF}'
    )
}

/// Whether `name` is a Windows device name, with or without an extension
/// (older Windows treats `NUL.txt` as the device too), in any case.
fn device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end();
    let upper = stem.to_ascii_uppercase();
    let numbered = |prefix: &str| {
        upper.strip_prefix(prefix).is_some_and(|n| {
            matches!(
                n,
                "0" | "1"
                    | "2"
                    | "3"
                    | "4"
                    | "5"
                    | "6"
                    | "7"
                    | "8"
                    | "9"
                    | "\u{B9}"
                    | "\u{B2}"
                    | "\u{B3}"
            )
        })
    };
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || numbered("COM")
        || numbered("LPT")
}

/// A directory of small objects, each replaced atomically.
pub struct Directory<P: Platform> {
    platform: P,
    dir: P::Dir,
    trust: Trust,
    decision: OnceLock<ClassDecision>,
    poisoned: OnceLock<FirstCause>,
    pool: BufPool,
    queue: QueueSet<P::Queue>,
    /// Serialises this handle's replaces and removes; across handles and
    /// processes, the lock on [`LOCK_NAME`] serialises replaces.
    change: Mutex<()>,
    /// This handle's own open of [`LOCK_NAME`] (opened on first change):
    /// each replace locks it, so the lock is this handle's open file
    /// description, and a lock taken there excludes every other handle.
    /// Kept for the handle's life: the lock file must not be deleted while
    /// a handle is open.
    lock_file: OnceLock<P::File>,
    /// The directory flush probed at open failed: every change is refused
    /// before any write, since its rename could never be made durable.
    dir_flush: Option<OsError>,
}

impl<P: Platform> core::fmt::Debug for Directory<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Directory")
            .field("class", &self.class())
            .field("poisoned", &self.poisoned.get().is_some())
            .finish()
    }
}

impl<P: Platform> Directory<P> {
    /// Opens the directory at `path`, creating it (and syncing its parent)
    /// when `create` is true. `trust` is applied when the device is probed.
    /// It also tries a directory flush: if that fails, the handle still
    /// reads, but every `replace`, `remove` and `sync` is refused before any
    /// write, since no change could be made durable.
    ///
    /// # Errors
    ///
    /// `NotFound` if it does not exist and `create` is false; `Io`.
    pub fn open(platform: P, path: &Path, create: bool, trust: Trust) -> Result<Self, Error> {
        let dir = platform.open_dir(path, create).map_err(|raw| {
            if store_io_core::errno::is_not_found(raw) {
                Error::NotFound { what: Named::Store }
            } else {
                io(Op::Open, raw)
            }
        })?;
        let queue = platform
            .queue(QueueConfig {
                depth: 8,
                ..QueueConfig::default()
            })
            .map_err(|raw| io(Op::Queue, raw))?;
        let pool = BufPool::new(&PoolConfig::uniform(4096, 4096, CHUNK, 4))
            .map_err(|_| not_written(NotWrittenCause::PoolExhausted, ErrorContext::default()))?;
        // Every change depends on a directory flush; learn now, before any
        // write, whether this directory can do one.
        let dir_flush = platform.sync_dir(&dir).err();
        Ok(Self {
            platform,
            dir,
            trust,
            decision: OnceLock::new(),
            poisoned: OnceLock::new(),
            pool,
            queue: QueueSet::new(vec![queue], 8),
            change: Mutex::new(()),
            lock_file: OnceLock::new(),
            dir_flush,
        })
    }

    /// The durability class of the directory's device, once a file has been
    /// opened through this handle.
    #[must_use]
    pub fn class(&self) -> Option<DurabilityClass> {
        self.decision.get().map(|d| d.class)
    }

    /// The class decision (evidence, reasons, label), once probed.
    #[must_use]
    pub fn decision(&self) -> Option<&ClassDecision> {
        self.decision.get()
    }

    /// Probes the device with `file` the first time, and refuses unsafe or
    /// unverified devices as the trust settings require.
    fn admit(&self, file: &P::File) -> Result<(), Error> {
        if self.decision.get().is_none() {
            let ev = self
                .platform
                .probe(&self.dir, file)
                .map_err(|raw| io(Op::Probe, raw))?;
            let _first = self.decision.set(decide(&ev, &self.trust));
        }
        match self.decision.get() {
            Some(d) => crate::store::refuse(d),
            None => Err(not_written(
                NotWrittenCause::NotReady,
                ErrorContext::default(),
            )),
        }
    }

    fn poison(&self, op: Op, raw: Option<OsError>) -> Error {
        let _first_kept = self.poisoned.set(FirstCause { op: Some(op), raw });
        Error::DurabilityUnknown {
            op,
            raw,
            ctx: ErrorContext::default(),
        }
    }

    fn writable(&self) -> Result<(), Error> {
        if let Some(first) = self.poisoned.get() {
            return Err(Error::Poisoned { first: *first });
        }
        match self.dir_flush {
            None => Ok(()),
            Some(raw) if store_io_core::errno::is_unsupported(raw) => Err(Error::Unsupported {
                what: store_io_core::error::Capability::DirectoryFlush,
            }),
            Some(raw) => Err(io(Op::SyncDir, raw)),
        }
    }

    /// Replaces the file `name` with `data`, atomically and durably: after
    /// any crash the file holds exactly the old or exactly the new bytes, and
    /// once this returns, exactly the new ones. One replace runs at a time in
    /// the directory: while another handle or process is replacing, this
    /// fails `Locked` with nothing written (the caller retries). Every
    /// writer must take the lock for that to hold: a process of a version
    /// before 0.5 does not. The lock is the file `.sio-lock` in the
    /// directory, created on first use and kept; delete it only while no
    /// `Directory` is open on the directory (an open handle keeps locking
    /// the file it opened, so a new one beside it splits the writers).
    ///
    /// # Errors
    ///
    /// `Locked` while another handle or process replaces in the directory;
    /// `Io(Lock)` if the file system cannot lock at all.
    /// `NotWritten(InvalidName)` for a name that could resolve to another
    /// file: longer than 247 bytes; shaped like a Windows short name (`~`
    /// and digits) or a Windows device name; and, compared as a
    /// case-insensitive file system compares names (case folded, ignorable
    /// code points dropped), empty, ending in `.` or a space, or the
    /// directory's own `.sio-lock` or a name ending in `.sio-tmp`.
    /// `Unsupported(DirectoryFlush)` or `Io(SyncDir)` when the directory
    /// cannot be flushed (found at open; nothing is written).
    /// `NotWritten`, `UnsafeDevice`, `Unverified`, `NoSpace` or `Io` before
    /// anything reached the device (the target is untouched).
    /// `DurabilityUnknown` if a write, the length update or the flush of the
    /// new bytes failed (the target is untouched) or anything from the rename
    /// on failed (the directory may name either version); the handle is then
    /// poisoned and later changes fail with `Poisoned`.
    pub fn replace(&self, name: &str, data: &[u8]) -> Result<(), Error> {
        self.writable()?;
        let target = object_name(name)?;
        let temp = name_of(&format!("{name}{TEMP_SUFFIX}"))?;
        let _one_at_a_time = self.change.lock().unwrap_or_else(PoisonError::into_inner);
        // Poisoned while this call waited for the handle's lock.
        self.writable()?;
        // One changer in the directory, across handles and processes: the
        // temporary name below is ours alone.
        let _owner = self.change_lock()?;
        // A leftover from an interrupted replace is never the live version.
        let _absent_or_removed = self.platform.unlink(&self.dir, &temp);
        let file = self
            .platform
            .create_file(&self.dir, &temp)
            .map_err(|raw| io(Op::Create, raw))?;
        if let Err(e) = self.admit(&file).and_then(|()| self.fill(&file, data)) {
            drop(file);
            let _cleanup = self.platform.unlink(&self.dir, &temp);
            return Err(e);
        }
        // The commit point: not after a poison seen here (a failed `sync`
        // on this handle poisons without the change lock, so one landing
        // after this check still commits; then the two calls overlapped,
        // and this replace's own directory flush decides its result).
        if let Some(first) = self.poisoned.get() {
            drop(file);
            let _cleanup = self.platform.unlink(&self.dir, &temp);
            return Err(Error::Poisoned { first: *first });
        }
        // From here on the directory may name either version.
        let renamed = self
            .platform
            .rename_replace(&self.dir, &temp, &target, &file);
        drop(file);
        renamed.map_err(|raw| self.poison(Op::Rename, Some(raw)))?;
        self.platform
            .sync_dir(&self.dir)
            .map_err(|raw| self.poison(Op::SyncDir, Some(raw)))
    }

    /// Takes the directory's change lock: an exclusive lock, through this
    /// handle's own open of [`LOCK_NAME`] (created on first use and kept,
    /// an empty file), held until the returned guard drops. `Locked` only
    /// when another holder has it.
    fn change_lock(&self) -> Result<P::Lock, Error> {
        let file = match self.lock_file.get() {
            Some(f) => f,
            None => {
                let name = name_of(LOCK_NAME)?;
                let open = || {
                    self.platform
                        .open_file(&self.dir, &name, FileMode::ReadWrite)
                };
                let f = match open() {
                    Ok(f) => f,
                    Err(raw) if store_io_core::errno::is_not_found(raw) => {
                        match self.platform.create_file(&self.dir, &name) {
                            Ok(f) => f,
                            // Created by another handle meanwhile.
                            Err(raw) if store_io_core::errno::is_already_exists(raw) => {
                                open().map_err(|raw| io(Op::Open, raw))?
                            }
                            Err(raw) => return Err(io(Op::Create, raw)),
                        }
                    }
                    Err(raw) => return Err(io(Op::Open, raw)),
                };
                // Only this handle's change mutex holder gets here.
                let _first = self.lock_file.set(f);
                self.lock_file.get().ok_or_else(|| {
                    not_written(NotWrittenCause::NotReady, ErrorContext::default())
                })?
            }
        };
        self.platform.lock_exclusive(file).map_err(|raw| {
            if store_io_core::errno::is_lock_held(raw) {
                Error::Locked
            } else {
                io(Op::Lock, raw)
            }
        })
    }

    /// Writes `data` to a fresh file, sets its exact length and flushes it.
    fn fill(&self, file: &P::File, data: &[u8]) -> Result<(), Error> {
        let c = ErrorContext::default();
        let len = data.len() as u64;
        let padded =
            align_up(len, BLOCK).ok_or_else(|| not_written(NotWrittenCause::TooLarge, c))?;
        self.platform
            .allocate(file, padded)
            .map_err(|raw| crate::store::space_err(Op::Allocate, raw, 0))?;
        let mut lane = self.queue.get(0);
        for (i, piece) in data.chunks(CHUNK).enumerate() {
            let piece_len = align_up(piece.len() as u64, BLOCK).unwrap_or(BLOCK) as usize;
            let mut buf = self
                .pool
                .take(piece_len)
                .map_err(|_| not_written(NotWrittenCause::PoolExhausted, c))?;
            let dst = buf.as_mut_slice();
            dst[..piece.len()].copy_from_slice(piece);
            dst[piece.len()..].fill(0);
            let ran = run(
                &mut lane,
                IoOp::Write {
                    file,
                    offset: (i * CHUNK) as u64,
                    buf,
                    dsync: false,
                },
            )
            .map_err(|(_b, raw)| {
                raw.map_or_else(
                    || not_written(NotWrittenCause::QueueFull, c),
                    |r| io(Op::Write, r),
                )
            })?;
            let result = match ran {
                Ran::Done { result, .. } => result,
                Ran::Abandoned(raw) => Err(raw),
            };
            // A failed or short write, or a failed flush, poisons the handle
            // (fail-stop), even though the target itself is untouched.
            match result {
                Ok(n) if n == piece_len => {}
                Ok(_) => return Err(self.poison(Op::Write, None)),
                Err(raw) => return Err(self.poison(Op::Write, Some(raw))),
            }
        }
        drop(lane);
        // Fail-stop: a metadata update the device refused after the writes
        // poisons the handle, as a failed write does.
        self.platform
            .set_len(file, len)
            .map_err(|raw| self.poison(Op::Allocate, Some(raw)))?;
        self.platform
            .flush_all(file)
            .map_err(|raw| self.poison(Op::FlushAll, Some(raw)))
    }

    /// Reads the whole file `name` (direct I/O: never the OS page cache).
    ///
    /// # Errors
    ///
    /// `NotFound`; `Corruption(MediaError)` for an unreadable range;
    /// `NotWritten(InvalidName)` as for [`Self::replace`]; `Io`.
    pub fn read(&self, name: &str) -> Result<Vec<u8>, Error> {
        let c = ErrorContext::default();
        let file = self
            .platform
            .open_file(&self.dir, &object_name(name)?, FileMode::ReadOnly)
            .map_err(|raw| {
                if store_io_core::errno::is_not_found(raw) {
                    Error::NotFound { what: Named::Slot }
                } else {
                    io(Op::Open, raw)
                }
            })?;
        let len = self.platform.size(&file).map_err(|raw| io(Op::Open, raw))?;
        let len = usize::try_from(len).map_err(|_| not_written(NotWrittenCause::TooLarge, c))?;
        let mut out = vec![0u8; len];
        let mut lane = self.queue.get(0);
        let mut at = 0usize;
        while at < len {
            let want = (len - at).min(CHUNK);
            let padded = align_up(want as u64, BLOCK).unwrap_or(BLOCK) as usize;
            let buf = self
                .pool
                .take(padded)
                .map_err(|_| not_written(NotWrittenCause::PoolExhausted, c))?;
            let ran = run(
                &mut lane,
                IoOp::Read {
                    file: &file,
                    offset: at as u64,
                    buf,
                },
            )
            .map_err(|(_b, raw)| {
                raw.map_or_else(
                    || not_written(NotWrittenCause::QueueFull, c),
                    |r| io(Op::Read, r),
                )
            })?;
            let (got, result) = match ran {
                Ran::Done { buf, result } => (buf, result),
                // A failed wait: a failure of the queue, not of the media.
                Ran::Abandoned(raw) => return Err(io(Op::Read, raw)),
            };
            let n = result.map_err(|raw| {
                if store_io_core::errno::is_media_error(raw) {
                    Error::Corruption {
                        kind: CorruptionKind::MediaError,
                        ctx: c,
                    }
                } else {
                    io(Op::Read, raw)
                }
            })?;
            let Some(buf) = got else {
                return Err(not_written(NotWrittenCause::QueueFull, c));
            };
            let got = n.min(want);
            out[at..at + got].copy_from_slice(&buf.as_slice()[..got]);
            if got < want {
                // The file shrank under us: report what is there.
                out.truncate(at + got);
                break;
            }
            at += got;
        }
        Ok(out)
    }

    /// Removes the file `name` durably (unlink, then flush the directory).
    ///
    /// # Errors
    ///
    /// `Unsupported(DirectoryFlush)` or `Io(SyncDir)` when the directory
    /// cannot be flushed (nothing changed); `NotFound` (nothing changed);
    /// `DurabilityUnknown` if the unlink or the directory flush failed (the
    /// handle is poisoned); `Poisoned`; `NotWritten(InvalidName)` as for
    /// [`Self::replace`]. A remove takes no lock: it touches no temporary
    /// file, and a replace and a remove of one name racing each other end
    /// in either order.
    pub fn remove(&self, name: &str) -> Result<(), Error> {
        self.writable()?;
        let target = object_name(name)?;
        let _one_at_a_time = self.change.lock().unwrap_or_else(PoisonError::into_inner);
        self.writable()?;
        match self.platform.unlink(&self.dir, &target) {
            Ok(()) => {}
            Err(raw) if store_io_core::errno::is_not_found(raw) => {
                return Err(Error::NotFound { what: Named::Slot });
            }
            Err(raw) => return Err(self.poison(Op::Unlink, Some(raw))),
        }
        self.platform
            .sync_dir(&self.dir)
            .map_err(|raw| self.poison(Op::SyncDir, Some(raw)))
    }

    /// Flushes the directory: every create, rename and unlink in it so far
    /// becomes durable.
    ///
    /// # Errors
    ///
    /// `Unsupported(DirectoryFlush)` or `Io(SyncDir)` when the directory
    /// cannot be flushed; `DurabilityUnknown` (the handle is poisoned);
    /// `Poisoned`.
    pub fn sync(&self) -> Result<(), Error> {
        self.writable()?;
        self.platform
            .sync_dir(&self.dir)
            .map_err(|raw| self.poison(Op::SyncDir, Some(raw)))
    }
}
