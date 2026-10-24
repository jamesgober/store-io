//! Copying a store into a fresh format-2 one (v0.5 E5 C1; PLANNING r4 "The
//! copy tool", D-116): the way a format-1 store moves to format 2, and the
//! way out of a format-2 store that needs compacting.
//!
//! The source is opened read-only (it never heals and never writes its
//! volume record). A source with unavailable regions, table entries that do
//! not decode, or a stale or lost table page is refused, as is one holding
//! a slot object larger than a format-2 slot takes, before anything is
//! created; with [`CopyOptions::accept_stale_pages`] such a source is
//! copied as far as it can be (spec §12 "What `copy_store` copies"): the
//! regions of a stale page in full, what is left out or uncertain
//! reported. The target directory's copy lock ([`COPY_LOCK`], held for the
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
use store_io_format::judge::Standing;
use store_io_format::meta::{FillPattern, REGION_META_LEN, RegionKind, RegionMeta};
use store_io_format::meta2::{FLAG_CAPPED, decode_region_header, max_object};
use store_io_format::slot::FORMAT_2;
use store_io_platform::{FileMode, FileName, IoOp, Platform};

use crate::exec::{Ran, clear_stale, run};
use crate::settle::settle_read;

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
    /// Copy a source the open left frozen (a stale or lost table page) or
    /// with unavailable regions or entries this version cannot read,
    /// instead of refusing it (spec §12, "What `copy_store` copies"; the
    /// way out of a store no repair can mend in place):
    /// - every usable region, by its recorded data size;
    /// - a usable region of a stale page **in full**: its whole capacity,
    ///   plus the extra extent its header lists when that overlaps no other
    ///   region's footprint and no reserved span (a growth whose commit may
    ///   be the page's lost one); the target's data size covers all of it,
    ///   and its recorded data size is reported
    ///   ([`CopyReport::verified_up_to`]): the bytes past it may never
    ///   have been written, may be the newest, or (where no check can
    ///   tell: a growth another damaged header never listed, an uncommitted
    ///   one of a confirmed page's region) may never have been this
    ///   region's;
    /// - every stale page reported ([`CopyReport::stale_pages`]): its
    ///   newest commit is given up, so a region it created is missing and
    ///   one it released is back;
    /// - unavailable regions and unreadable entries left out, reported
    ///   ([`CopyReport::omitted`]); a lost page's regions are unknown,
    ///   reported by page ([`CopyReport::lost_pages`]);
    /// - a possibly superseded region copied and reported
    ///   ([`CopyReport::superseded`]): it may read another region's bytes.
    ///
    /// Default false: such a source is refused.
    pub accept_stale_pages: bool,
}

/// What [`copy_store`] copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyReport {
    /// Regions recreated in the target (slots included).
    pub regions: usize,
    /// Data bytes copied (regions' data sizes and slots' objects).
    pub bytes: u64,
    /// The source's container format.
    pub source_format: u16,
    /// With `accept_stale_pages`: the source's table entries left out
    /// (unavailable regions, entries of a kind or name this version cannot
    /// read), by id; the source's report says why.
    pub omitted: Vec<RegionId>,
    /// With `accept_stale_pages`: the source's lost table pages (no copy
    /// of them reads): their regions are unknown, none copied; a region
    /// they may have taken the bytes of is listed in
    /// [`Self::superseded`], and no extra extent is copied.
    pub lost_pages: Vec<u32>,
    /// With `accept_stale_pages`: the source's stale table pages (a newer
    /// copy damaged): each one's newest commit is given up, so a region
    /// that commit created is missing from the copy and one it released is
    /// in it, whatever regions the page holds.
    pub stale_pages: Vec<u32>,
    /// With `accept_stale_pages`: each region of a stale page, copied in
    /// full, with its recorded data size (name, bytes): only that much is
    /// known to be the region's acknowledged data (unless it is also listed
    /// in [`Self::superseded`]).
    pub verified_up_to: Vec<(String, u64)>,
    /// With `accept_stale_pages`: the regions copied that may hold another
    /// region's bytes, by name: possibly superseded (spec §9 step 4:
    /// another region's newer footprint may cover them), or of a stale or
    /// possibly accepted page while another page is stale or lost (D-130:
    /// a given-up commit may have released one, and another given-up
    /// commit handed its space to a region it created). The source held
    /// each read-only; the copy's is writable.
    pub superseded: Vec<String>,
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
    /// The target's data size (the source's, or a full copy's length).
    data_size: u64,
    /// Its cap (format 2's capped regions): the target is capped there too.
    cap: Option<u64>,
    attrs: CopyAttrs,
    /// A slot's committed object, if any.
    object: Option<Vec<u8>>,
    /// A region of a stale page, copied in full: the container ranges, in
    /// data order (its capacity, then its extra extent if taken), and its
    /// recorded data size.
    full: Option<(Vec<(u64, u64)>, u64)>,
    /// Possibly superseded (spec §9 step 4).
    superseded: bool,
    /// The source region (a full copy reads its runs and extra extent).
    source: std::sync::Arc<LiveRegion>,
}

/// Copies the store in `from` into a new format-2 store in `to`, whose
/// directory must hold no store. See the module docs.
///
/// # Errors
///
/// As [`Store::open_readonly`] for the source and [`Store::create`] for
/// the target; `Locked` while a writer holds `from` open or another copy
/// into `to` runs;
/// `AlreadyExists` if `to` holds a container file (any `store.sio`, even
/// one a crashed create left empty, which [`Store::create`] would take
/// over: remove it to copy there), or one appears there during the copy
/// (kept: the copy's temporary store is removed); `NotWritten(NotReady)`
/// for a source with unavailable regions or table entries that do not
/// decode (the first in the error's context; the source's report lists
/// them all), `NotWritten(Frozen)` for one with a stale or lost table
/// page (both unless [`CopyOptions::accept_stale_pages`]: then copied as
/// far as they can be; a full copy's read fails as a region read does:
/// `Corruption(MediaError)` naming the region and range, `Io`,
/// `NotWritten(Interrupted | QueueFull)`, or `Corruption(Metadata)` for a
/// read cut short, `NotWritten(PoolExhausted)`), `NotWritten(TooLarge)` for
/// a slot object larger than a format-2
/// slot holds (that region in the context), all before anything is
/// created; `Io` if whether `to` holds a store cannot be told (any failure
/// to look but "not found"), before anything but the target directory
/// (and, on the second look under the lock, the copy lock's file) is
/// created;
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
    let accept = opts.accept_stale_pages;
    let (mut omitted, mut lost_pages, mut stale_pages) = (Vec::new(), Vec::new(), Vec::new());
    if accept {
        // Every entry the open holds space for without serving it: the
        // unavailable regions and the entries of an unknown kind (format
        // 2's holds are the whole list; format 1 has only the former).
        omitted = src.omitted_for_copy();
        lost_pages.clone_from(&report.metadata.lost_pages);
        stale_pages.clone_from(&report.metadata.stale_pages);
    } else {
        // The source's state now (a read-only open: what its open found).
        if let Some(u) = src.unavailable_regions().first() {
            return Err(refused(NotWrittenCause::NotReady, Some(u.id)));
        }
        // An entry whose kind or name does not decode (a newer release's)
        // is a region the copy cannot recreate (the list may be truncated:
        // any).
        if let Some(&(p, s)) = report.metadata.invalid_entries.first() {
            let id = RegionId::from_raw((u64::from(p) << 32) | u64::from(s));
            return Err(refused(NotWrittenCause::NotReady, Some(id)));
        }
        if src.frozen() {
            return Err(refused(NotWrittenCause::Frozen, None));
        }
    }
    let block = report.block_size;
    let log2 = block.trailing_zeros() as u8;
    let source_format = report.format;
    let regions = src.regions_snapshot();
    let mut facts = Vec::with_capacity(regions.len());
    let (mut verified_up_to, mut superseded) = (Vec::new(), Vec::new());
    for r in &regions {
        let f = src.facts_for_copy(r, &regions)?;
        if let Some((_, recorded)) = &f.full {
            verified_up_to.push((f.name.clone(), *recorded));
        }
        if f.superseded {
            superseded.push(f.name.clone());
        }
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
        omitted,
        lost_pages,
        stale_pages,
        verified_up_to,
        superseded,
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
    /// The entries `copy_store` leaves out (`CopyReport::omitted`): format
    /// 2's holds (every unavailable or unknown-kind entry), else the
    /// unavailable regions.
    fn omitted_for_copy(&self) -> Vec<RegionId> {
        let meta = lock_meta(&self.inner.meta);
        match meta.v2.as_deref() {
            Some(v2) => {
                let mut ids: Vec<RegionId> = v2
                    .holds
                    .iter()
                    .map(|h| RegionId::from_raw(h.id()))
                    .collect();
                ids.sort_unstable_by_key(|id| id.get());
                ids
            }
            None => {
                let mut ids: Vec<RegionId> = meta.unavailable.iter().map(|u| u.id).collect();
                ids.sort_unstable_by_key(|id| id.get());
                ids
            }
        }
    }

    /// A region of a stale page, copied in full (format 2): its runs over
    /// the whole capacity, then its extra extent when that overlaps nothing
    /// any other bytes may belong to: the volume record and table, the
    /// region's own header blocks and runs, every other region's (header
    /// blocks and runs over the capacity, an extra extent), and every
    /// unavailable entry's reserved span and possible writes; never while a
    /// page is lost (its regions' footprints are unknown). `None` on a
    /// confirmed page.
    fn full_copy(
        &self,
        r: &LiveRegion,
        regions: &[std::sync::Arc<LiveRegion>],
    ) -> Option<(Vec<(u64, u64)>, u64)> {
        let meta = lock_meta(&self.inner.meta);
        let v2 = meta.v2.as_deref()?;
        let page = usize::try_from(r.id.get() >> 32).ok()?;
        if v2.pages.get(page)?.standing != Standing::Unconfirmed {
            return None;
        }
        let block = self.block();
        let mut ranges: Vec<(u64, u64)> = r.map.extents().collect();
        let overlaps = |(a, n): (u64, u64), (b, m): (u64, u64)| a < b + m && b < a + n;
        let extra = v2
            .stale_extra
            .iter()
            .find(|e| e.1 == r.id.get())
            .map(|e| e.2);
        let lost = v2
            .pages
            .iter()
            .any(|p| p.standing != Standing::Confirmed && p.standing != Standing::Unconfirmed);
        if let Some(extra) = extra.filter(|_| !lost) {
            let mut others = regions
                .iter()
                .flat_map(|o| {
                    o.map
                        .extents()
                        .chain(o.header_at.iter().map(|&h| (h, block)))
                        .collect::<Vec<_>>()
                })
                .chain(core::iter::once((0, self.inner.layout.data_start())))
                .chain(v2.holds.iter().filter_map(|h| h.span))
                .chain(v2.holds.iter().flat_map(|h| h.own.iter().copied()))
                .chain(
                    v2.stale_extra
                        .iter()
                        .filter(|e| e.1 != r.id.get())
                        .map(|e| e.2),
                );
            if !others.any(|o| overlaps(extra, o)) {
                ranges.push(extra);
            }
        }
        Some((ranges, r.map.len()))
    }

    /// Reads `out.len()` bytes of the container at `offset` (both whole
    /// blocks), whatever `r`'s data size says: a copy's full region, `at`
    /// the region offset they stand for (named in an error, as a region
    /// read's verdict is: `settle_read`).
    fn read_raw(
        &self,
        r: &LiveRegion,
        mut at: u64,
        mut offset: u64,
        out: &mut [u8],
    ) -> Result<(), Error> {
        let mut lane = self.inner.queues.get(0);
        clear_stale(&mut lane).map_err(|raw| io(Op::Read, raw))?;
        let chunk = self.chunk();
        for piece in out.chunks_mut(chunk) {
            let c = crate::io::ctx_range(r, at, at + piece.len() as u64);
            let buf = self.take(piece.len(), c)?;
            let op = IoOp::Read {
                file: &self.inner.file,
                offset,
                buf,
            };
            let (got, result) = match run(&mut lane, op) {
                Ok(Ran::Done { buf, result }) => (buf, result),
                // The wait failed with the read in flight: the queue's
                // failure, no verdict on the media.
                Ok(Ran::Abandoned(raw)) | Err((_, Some(raw))) => return Err(io(Op::Read, raw)),
                Err((_, None)) => {
                    return Err(crate::io::not_written(NotWrittenCause::QueueFull, c));
                }
            };
            let n = settle_read(self, r, at, at + piece.len() as u64, got.as_ref(), result)?;
            match got {
                Some(b) if n == piece.len() => piece.copy_from_slice(&b.as_slice()[..n]),
                // Short: bytes the region's runs say exist are not there.
                _ => return Err(crate::store::corrupt()),
            }
            offset += piece.len() as u64;
            at += piece.len() as u64;
        }
        Ok(())
    }

    /// Whether `r`, on a stale or possibly accepted page while another page
    /// is too (or lost), may hold another region's bytes (D-130): each
    /// such page's newest commit may be given up, one releasing `r`, the
    /// other handing its space to a region it created.
    fn at_risk(&self, r: &LiveRegion) -> bool {
        let meta = lock_meta(&self.inner.meta);
        let Some(v2) = meta.v2.as_deref() else {
            return false;
        };
        let page = usize::try_from(r.id.get() >> 32).unwrap_or(usize::MAX);
        v2.pages.get(page).is_some_and(|pg| {
            (pg.standing == Standing::Unconfirmed || pg.pair.possibly_accepted())
                && v2.pages.iter().enumerate().any(|(i, o)| {
                    i != page && (o.standing != Standing::Confirmed || o.pair.possibly_accepted())
                })
        })
    }

    /// What `copy_store` keeps of region `r` (either format); `regions` is
    /// every region of the store (a full copy's overlap test).
    fn facts_for_copy(
        &self,
        r: &std::sync::Arc<LiveRegion>,
        regions: &[std::sync::Arc<LiveRegion>],
    ) -> Result<Facts, Error> {
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
        // A slot's object is whole as committed: never copied "in full".
        let full = if r.kind == RegionKind::Slot {
            None
        } else {
            self.full_copy(r, regions)
        };
        let data_size = full
            .as_ref()
            .map_or(r.map.len(), |(ranges, _)| ranges.iter().map(|x| x.1).sum());
        Ok(Facts {
            name: r.name.as_str().to_owned(),
            kind: r.kind,
            data_size,
            // A capped region copied in full holds all of it.
            cap: capped.then(|| r.map.capacity().max(data_size)),
            attrs: CopyAttrs {
                generation,
                fill,
                fill_key,
                tag,
            },
            object,
            full,
            superseded: r.superseded || self.at_risk(r),
            source: std::sync::Arc::clone(r),
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
        if let Some((ranges, _)) = &f.full {
            let mut at = 0u64;
            for &(file, len) in ranges {
                let mut done = 0u64;
                while done < len {
                    let n = (len - done).min(CHUNK as u64) as usize;
                    src.read_raw(&f.source, at, file + done, &mut buf[..n])?;
                    match f.kind {
                        RegionKind::Page => {
                            let to = self.page_region(&f.name)?;
                            let _ticket = to.write(to.pos(at)?, &buf[..n])?;
                        }
                        _ => {
                            let _ticket = self.append_region(&f.name)?.append(&buf[..n])?;
                        }
                    }
                    done += n as u64;
                    at += n as u64;
                }
            }
            return Ok(f.data_size);
        }
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
