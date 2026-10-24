//! Copying a store into a fresh format-2 one (v0.5 E5 C1; PLANNING r4 "The
//! copy tool", D-116): the way a format-1 store moves to format 2, and the
//! way out of a format-2 store that needs compacting.
//!
//! The source is opened read-only (it never heals and never writes its
//! volume record). A source with unavailable regions, table entries that do
//! not decode, or a stale or lost table page is refused, as is one holding
//! a slot object larger than a format-2 slot takes, before anything is
//! created. The target directory's copy lock ([`COPY_LOCK`], held for the
//! whole copy) keeps two copies into one directory apart. The target is
//! created there under a temporary container name, every region recreated
//! with its name, kind, data size, cap, tag, region generation, fill
//! pattern and fill key (laid out anew, one extent each) and its data
//! copied, made durable by one barrier, closed, and only then renamed into
//! place by a rename that never replaces (a store created there meanwhile
//! is kept, the copy refused): a crash leaves no store under the target
//! name, or the whole copy.

use std::path::Path;

use store_io_core::errno::{is_already_exists, is_lock_held, is_not_found};
use store_io_core::error::{Error, ErrorContext, Named, NotWrittenCause, Op};
use store_io_core::id::RegionId;
use store_io_format::meta::{FillPattern, REGION_META_LEN, RegionKind, RegionMeta};
use store_io_format::meta2::{FLAG_CAPPED, decode_region_header, max_object};
use store_io_format::slot::FORMAT_2;
use store_io_platform::{FileMode, FileName, Platform};

use crate::RegionOptions;
use crate::store::{CONTAINER, LiveRegion, Store, StoreOptions, io, lock_meta};

/// The temporary name a copy's target is built under (no live store ever
/// has it: one left by a crashed copy is removed by the next).
pub const COPYING: &str = "store.sio.copying";

/// The file in a copy's target directory whose lock a copy holds from
/// before it looks for a store there until after its rename (created by
/// the first copy and kept: removing a lock file races its next user).
pub const COPY_LOCK: &str = "store.sio.copy-lock";

/// How [`copy_store`] builds its target.
#[derive(Debug, Clone, Default)]
pub struct CopyOptions {
    /// The target store's options (trust, buffers, queues...). Its block
    /// size is the source's whatever this says, and its table holds at
    /// least the source's regions; the container format is 2.
    pub store: StoreOptions,
}

/// What [`copy_store`] copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyReport {
    /// Regions recreated in the target (slots included).
    pub regions: usize,
    /// Data bytes copied (regions' data sizes and slots' objects).
    pub bytes: u64,
    /// The source's container format.
    pub source_format: u16,
}

/// What a recreated region keeps of its source (internal: for the
/// target's provisioning).
#[derive(Debug, Clone, Copy)]
pub(crate) struct CopyAttrs {
    pub(crate) generation: u64,
    pub(crate) fill: FillPattern,
    pub(crate) fill_key: [u8; 16],
    pub(crate) tag: u32,
}

/// A source region, as the copy recreates it.
struct Facts {
    name: String,
    kind: RegionKind,
    data_size: u64,
    /// Its cap (format 2's capped regions): the target is capped there too.
    cap: Option<u64>,
    attrs: CopyAttrs,
    /// A slot's committed object, if any.
    object: Option<Vec<u8>>,
}

/// Copies the store in `from` into a new format-2 store in `to`, whose
/// directory must hold no store. See the module docs.
///
/// # Errors
///
/// As [`Store::open_readonly`] for the source and [`Store::create`] for
/// the target; `Locked` while another copy into `to` runs;
/// `AlreadyExists` if `to` holds a container file (any `store.sio`, even
/// one a crashed create left empty, which [`Store::create`] would take
/// over: remove it to copy there), or one appears there during the copy
/// (kept: the copy's temporary store is removed); `NotWritten(NotReady)`
/// for a source with unavailable regions or table entries that do not
/// decode (the first in the error's context; the source's report lists
/// them all), `NotWritten(Frozen)` for one with a stale or lost table
/// page, `NotWritten(TooLarge)` for a slot object larger than a format-2
/// slot holds (that region in the context), all before anything is
/// created (but the target directory); `Io` if whether `to` holds a store
/// cannot be told (any failure to look but "not found"), likewise;
/// `NotWritten(Misaligned)` if the target's device needs larger blocks
/// than the source's, or a region's data size is not whole target blocks;
/// then any error of provisioning, reading or writing (the target left
/// under its temporary name, removed by the next copy into `to`);
/// `DurabilityUnknown` if the rename or the directory flush after it
/// failed: the copy, whole, may or may not be in place under the target
/// name (open it, or copy again: `AlreadyExists` means it is).
pub fn copy_store<P: Platform + Clone>(
    platform: P,
    from: &Path,
    to: &Path,
    opts: CopyOptions,
) -> Result<CopyReport, Error>
where
    P::Queue: Send,
{
    let src = Store::open_readonly(platform.clone(), from, opts.store.clone())?;
    let report = src.report();
    let refused = |cause, region| Error::NotWritten {
        cause,
        ctx: ErrorContext {
            region,
            ..ErrorContext::default()
        },
    };
    if let Some(u) = report.unavailable.first() {
        return Err(refused(NotWrittenCause::NotReady, Some(u.id)));
    }
    // An entry whose kind or name does not decode (a newer release's) is
    // a region the copy cannot recreate (the list may be truncated: any).
    if let Some(&(p, s)) = report.metadata.invalid_entries.first() {
        let id = RegionId::from_raw((u64::from(p) << 32) | u64::from(s));
        return Err(refused(NotWrittenCause::NotReady, Some(id)));
    }
    if report.metadata.frozen {
        return Err(refused(NotWrittenCause::Frozen, None));
    }
    let block = report.block_size;
    let log2 = block.trailing_zeros() as u8;
    let source_format = report.format;
    let regions = src.regions_snapshot();
    let mut facts = Vec::with_capacity(regions.len());
    for r in &regions {
        let f = src.facts_for_copy(r)?;
        if f.object
            .as_ref()
            .is_some_and(|o| o.len() > max_object(log2))
        {
            return Err(refused(NotWrittenCause::TooLarge, Some(r.id)));
        }
        facts.push(f);
    }
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
    // The copy lock, then (under it) no store there and a crashed copy's
    // leftover removed: no other copy can touch the temporary name until
    // this one's rename is done.
    let lock_name = name(COPY_LOCK)?;
    let lock_file = match platform.create_file(&dir, &lock_name) {
        Ok(f) => f,
        Err(raw) if is_already_exists(raw) => platform
            .open_file(&dir, &lock_name, FileMode::ReadWrite)
            .map_err(|raw| io(Op::Open, raw))?,
        Err(raw) => return Err(io(Op::Create, raw)),
    };
    let _lock = platform.lock_exclusive(&lock_file).map_err(|raw| {
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
    let mut target_opts = opts.store.clone();
    target_opts.container_format = FORMAT_2;
    target_opts.block_size = Some(block as u32);
    target_opts.region_capacity = target_opts
        .region_capacity
        .max(u32::try_from(facts.len()).unwrap_or(u32::MAX));
    let mut bytes = 0u64;
    {
        let dst = Store::create_named(platform.clone(), to, target_opts, &temp)?;
        // A device that needs larger blocks than the source's would round
        // every size up: refused (the temporary store is removed by the
        // next copy).
        if dst.report().block_size != block {
            return Err(refused(NotWrittenCause::Misaligned, None));
        }
        for f in &facts {
            bytes += dst.recreate(&src, f)?;
        }
        // One barrier for every byte written (the clean close is best
        // effort and reports nothing).
        dst.flush_all_accounted()?;
        // Closed here (the drop writes the clean-close record).
    }
    // Into place: the rename is the commit of the copy, and never replaces
    // a store another process created meanwhile.
    let file = platform
        .open_file(&dir, &temp, FileMode::ReadWrite)
        .map_err(|raw| io(Op::Open, raw))?;
    match platform.rename_noreplace(&dir, &temp, &final_name, &file) {
        Ok(()) => {}
        Err(raw) if is_already_exists(raw) => {
            drop(file);
            // Best effort: the next copy removes it otherwise.
            if platform.unlink(&dir, &temp).is_ok() {
                let _synced = platform.sync_dir(&dir);
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
        .sync_dir(&dir)
        .map_err(|raw| Error::DurabilityUnknown {
            op: Op::SyncDir,
            raw: Some(raw),
            ctx: ErrorContext::default(),
        })?;
    Ok(CopyReport {
        regions: facts.len(),
        bytes,
        source_format,
    })
}

fn name(s: &str) -> Result<FileName, Error> {
    FileName::new(s).map_err(|_| Error::NotWritten {
        cause: NotWrittenCause::InvalidName,
        ctx: ErrorContext::default(),
    })
}

/// The copy's chunk: 1 MiB.
const CHUNK: usize = 1 << 20;

impl<P: Platform> Store<P>
where
    P::Queue: Send,
{
    /// What `copy_store` keeps of region `r` (either format).
    fn facts_for_copy(&self, r: &LiveRegion) -> Result<Facts, Error> {
        let inner = &self.inner;
        let header = r
            .header
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let payload = header.payload.as_deref().unwrap_or(&[]);
        let format_2 = inner.layout.format == FORMAT_2;
        let (fill, fill_key, capped) = if format_2 {
            let v = decode_region_header(payload, inner.layout.log2_block, u64::MAX)
                .map_err(|_| crate::store::corrupt())?;
            (
                v.header.fill,
                v.header.fill_key,
                v.header.flags & FLAG_CAPPED != 0,
            )
        } else {
            // Format 1: the region meta (a slot's object follows it).
            let m = payload
                .get(..REGION_META_LEN)
                .and_then(|meta| RegionMeta::decode(meta).ok())
                .ok_or_else(crate::store::corrupt)?;
            (m.fill, m.fill_key, false)
        };
        // A format-1 slot's header-pair generation counts its commits; a
        // slot is never recycled, so its region generation is 1.
        let generation = if !format_2 && r.kind == RegionKind::Slot {
            1
        } else {
            r.generation.get()
        };
        let tag = {
            let meta = lock_meta(&inner.meta);
            match meta.v2.as_deref() {
                Some(v2) => {
                    let id = r.id.get();
                    v2.pages
                        .get(usize::try_from(id >> 32).unwrap_or(usize::MAX))
                        .and_then(|p| {
                            p.entries
                                .iter()
                                .find(|e| e.seq == (id & 0xFFFF_FFFF) as u32)
                        })
                        .map(|e| e.tag)
                        .ok_or_else(crate::store::corrupt)?
                }
                None => meta
                    .entries
                    .iter()
                    .find(|e| u64::from(e.region_id) == r.id.get())
                    .map(|e| e.tag)
                    .ok_or_else(crate::store::corrupt)?,
            }
        };
        let object = if r.kind == RegionKind::Slot {
            self.slot(r.name.as_str())?.read()
        } else {
            None
        };
        Ok(Facts {
            name: r.name.as_str().to_owned(),
            kind: r.kind,
            data_size: r.map.len(),
            cap: capped.then(|| r.map.capacity()),
            attrs: CopyAttrs {
                generation,
                fill,
                fill_key,
                tag,
            },
            object,
        })
    }

    /// Recreates `f` (a region of `src`) in this store and copies its data
    /// (not yet durable: the caller's one barrier makes every copy so).
    /// Returns the bytes copied.
    fn recreate(&self, src: &Store<P>, f: &Facts) -> Result<u64, Error> {
        let mut opts = RegionOptions::default().size(f.data_size);
        if let Some(cap) = f.cap {
            opts = opts.max_size(cap);
        }
        let made = self.provision2(&f.name, f.kind, opts, None, Some(&f.attrs))?;
        debug_assert_eq!(made.generation.get(), f.attrs.generation, "{}", f.name);
        // The data size as the source's (one not of whole target blocks
        // would have been rounded up).
        if made.map.len() != f.data_size {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::Misaligned,
                ctx: ErrorContext {
                    region: Some(made.id),
                    ..ErrorContext::default()
                },
            });
        }
        let mut buf = vec![0u8; CHUNK];
        match f.kind {
            RegionKind::Slot => {
                let Some(object) = &f.object else {
                    return Ok(0);
                };
                let _receipt = self.slot(&f.name)?.commit(object)?;
                Ok(object.len() as u64)
            }
            RegionKind::Page => {
                let (from, to) = (src.page_region(&f.name)?, self.page_region(&f.name)?);
                let mut at = 0u64;
                while at < f.data_size {
                    let n = (f.data_size - at).min(CHUNK as u64) as usize;
                    let got = from.read(from.pos(at)?, &mut buf[..n])?;
                    if got == 0 {
                        return Err(crate::store::corrupt());
                    }
                    let _ticket = to.write(to.pos(at)?, &buf[..got])?;
                    at += got as u64;
                }
                Ok(f.data_size)
            }
            RegionKind::Append => {
                let (from, to) = (src.append_region(&f.name)?, self.append_region(&f.name)?);
                let mut at = 0u64;
                while at < f.data_size {
                    let n = (f.data_size - at).min(CHUNK as u64) as usize;
                    let got = from.read(at, &mut buf[..n])?;
                    if got == 0 {
                        return Err(crate::store::corrupt());
                    }
                    let _ticket = to.append(&buf[..got])?;
                    at += got as u64;
                }
                Ok(f.data_size)
            }
        }
    }
}
