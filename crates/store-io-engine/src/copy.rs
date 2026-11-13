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
//! copied (an append or page region's as its fill, durable before its
//! header, D-137; a stale page's full copies made durable by one barrier
//! after them all), closed, and only then renamed into
//! place by a rename that never replaces (a store created there meanwhile
//! is kept, the copy refused): a crash leaves no store under the target
//! name, or the whole copy.

use std::ops::ControlFlow;
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
use crate::pace::Pace;
use crate::qos::Qos;
use crate::settle::settle_read;

use crate::slot_handle::committed_object;
use crate::store::{CONTAINER, LiveRegion, Meta, Store, StoreOptions, io, lock_meta};
use crate::{RegionOptions, ScanItem};

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

/// A region's header facts, from a plan's header pass.
struct HeaderFacts {
    fill: FillPattern,
    fill_key: [u8; 16],
    capped: bool,
    object: Option<Vec<u8>>,
}

/// A copy's plan ([`Store::copy_plan`]): every region's facts, in the
/// store's order, and the volume's.
pub(crate) struct CopyPlan {
    facts: Vec<Facts>,
    omitted: Vec<RegionId>,
    lost_pages: Vec<u32>,
    stale_pages: Vec<u32>,
    verified_up_to: Vec<(String, u64)>,
    superseded: Vec<String>,
    block: u64,
    source_format: u16,
}

/// A refusal, its region named if it has one.
fn refused(cause: NotWrittenCause, region: Option<RegionId>) -> Error {
    Error::NotWritten {
        cause,
        ctx: ErrorContext {
            region,
            ..ErrorContext::default()
        },
    }
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

/// The largest piece a copy's reader hands the fill (a scan buffer is cut
/// into pieces this size or smaller), and how many it keeps queued (D-137):
/// at most `FEED_DEPTH + 2` pieces exist at once (queued, the one being
/// built, the one being consumed), 10 MiB whatever `max_io` is.
const FEED_PIECE: usize = 1 << 20;
/// See [`FEED_PIECE`].
const FEED_DEPTH: usize = 8;

/// A copied region's bytes, in order, from the source's scan on another
/// thread (D-137): the scan's reads stay in flight while the fill writes
/// (while commits are quiet; while they are active the call has one piece
/// in flight, a read or a write, when both stores are on one device).
struct Feed {
    pieces: std::sync::mpsc::Receiver<Result<(u64, Vec<u8>), Error>>,
    spent: std::sync::mpsc::Sender<Vec<u8>>,
    /// The piece being consumed and how much of it is.
    current: Vec<u8>,
    start: u64,
    used: usize,
    /// The source region, named in an error.
    region: RegionId,
}

impl Feed {
    /// Starts the reader of `r` (its region offsets `[0, len)`) on a
    /// thread of `s`. The thread ends when the scan does, on the first
    /// error (sent), or once the returned `Feed` is dropped.
    ///
    /// # Errors
    ///
    /// `Io { op: Queue }` if the thread cannot be started.
    fn start<'s, P: Platform>(
        s: &'s std::thread::Scope<'s, '_>,
        src: &'s Store<P>,
        r: &'s LiveRegion,
        len: u64,
        pace: Option<Pace>,
    ) -> Result<(std::thread::ScopedJoinHandle<'s, Result<(), Error>>, Self), Error>
    where
        P::Queue: Send,
    {
        let (put, pieces) = std::sync::mpsc::sync_channel(FEED_DEPTH);
        let (spent, back) = std::sync::mpsc::channel::<Vec<u8>>();
        let handle = std::thread::Builder::new()
            .name("store-io-copy".into())
            .spawn_scoped(s, move || {
                let mut fail = None;
                let scanned =
                    src.scan_region(r, 0, len, false, pace.as_ref(), |item, drain| match item {
                        ScanItem::Data { offset, bytes } => {
                            for (i, part) in bytes.chunks(FEED_PIECE).enumerate() {
                                let at = offset + (i * FEED_PIECE) as u64;
                                let mut v = back.try_recv().unwrap_or_default();
                                v.clear();
                                // Memory the pool does not account for: a failed
                                // allocation is reported, never an abort.
                                if v.try_reserve_exact(part.len()).is_err() {
                                    fail = Some(crate::io::not_written(
                                        NotWrittenCause::PoolExhausted,
                                        crate::io::ctx_range(r, at, at + part.len() as u64),
                                    ));
                                    return ControlFlow::Break(());
                                }
                                v.extend_from_slice(part);
                                // Never blocked on a full feed holding reads in
                                // flight (R5.4's progress rule): the fill may be
                                // waiting on the call's record for one of them,
                                // so the scan's reads are drained first.
                                let sent = match put.try_send(Ok((at, v))) {
                                    Ok(()) => true,
                                    Err(std::sync::mpsc::TrySendError::Full(piece)) => {
                                        drain() && {
                                            crate::test_hook::step(
                                                crate::test_hook::Step::FeedBlocking,
                                            );
                                            put.send(piece).is_ok()
                                        }
                                    }
                                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => false,
                                };
                                // The fill stopped (or the drain failed: the scan
                                // reports it): so does the scan.
                                if !sent {
                                    return ControlFlow::Break(());
                                }
                            }
                            ControlFlow::Continue(())
                        }
                        ScanItem::Unreadable { start, end } => {
                            fail = Some(Error::Corruption {
                                kind: store_io_core::error::CorruptionKind::MediaError,
                                ctx: crate::io::ctx_range(r, start, end),
                            });
                            ControlFlow::Break(())
                        }
                    });
                let failed = match (scanned, fail) {
                    (_, Some(e)) | (Err(e), None) => Some(e),
                    (Ok(_), None) => None,
                };
                if let Some(e) = failed {
                    // The fill may already have stopped: then nobody reads
                    // it, and its own error stands.
                    let _sent = put.send(Err(e.clone()));
                    return Err(e);
                }
                Ok(())
            })
            .map_err(|e| Error::Io {
                op: Op::Queue,
                raw: os_error(&e),
                ctx: ErrorContext {
                    region: Some(r.id),
                    ..ErrorContext::default()
                },
            })?;
        Ok((
            handle,
            Self {
                pieces,
                spent,
                current: Vec::new(),
                start: 0,
                used: 0,
                region: r.id,
            },
        ))
    }

    /// Fills `dst` with the region's bytes from offset `index` (the fill
    /// asks for them in order, with no gap).
    fn read(&mut self, index: u64, dst: &mut [u8]) -> Result<(), Error> {
        let mut done = 0usize;
        while done < dst.len() {
            if self.used == self.current.len() {
                let (start, piece) = match self.pieces.recv() {
                    Ok(Ok(p)) => p,
                    Ok(Err(e)) => return Err(e),
                    // The scan ended before the region did: bytes its map
                    // says exist are not there.
                    Err(_) => return Err(self.wrong(index + done as u64, index + dst.len() as u64)),
                };
                let old = std::mem::replace(&mut self.current, piece);
                let _recycled = self.spent.send(old);
                self.start = start;
                self.used = 0;
            }
            // In order and without a gap, or the copy is wrong.
            if self.start + self.used as u64 != index + done as u64 {
                return Err(self.wrong(index + done as u64, index + dst.len() as u64));
            }
            let n = (self.current.len() - self.used).min(dst.len() - done);
            dst[done..done + n].copy_from_slice(&self.current[self.used..self.used + n]);
            self.used += n;
            done += n;
        }
        Ok(())
    }

    /// The source's bytes `[start, end)` did not arrive as its map says.
    fn wrong(&self, start: u64, end: u64) -> Error {
        let mut e = crate::store::corrupt();
        if let Error::Corruption { ctx, .. } = &mut e {
            *ctx = ErrorContext {
                region: Some(self.region),
                range: Some(store_io_core::error::ByteRange { start, end }),
            };
        }
        e
    }
}

/// A thread's start failure as an [`OsError`](store_io_core::error::OsError).
fn os_error(e: &std::io::Error) -> store_io_core::error::OsError {
    use store_io_core::error::{OsError, OsErrorSource};
    OsError {
        code: e.raw_os_error().unwrap_or(0),
        source: if cfg!(windows) {
            OsErrorSource::Win32
        } else {
            OsErrorSource::Errno
        },
    }
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
/// than the source's, or a region's data size is not whole target blocks
/// (naming the source's region, before that region is provisioned);
/// `Io { op: Queue }` if a region's reader thread cannot be started; a
/// copied region's read errors as a scan's (`Corruption(MediaError)`
/// naming the source's region and range, also for a read cut short),
/// `NotWritten(PoolExhausted)` if the reader's pieces cannot be allocated;
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
    let CopyPlan {
        facts,
        omitted,
        lost_pages,
        stale_pages,
        verified_up_to,
        superseded,
        block,
        source_format,
    } = src.copy_plan(opts.accept_stale_pages)?;
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
        // The target is this call's own (R5.4): created, filled and closed
        // as background I/O, paced by its device's commits.
        let dst = Store::create_named(
            platform.clone(),
            to,
            target_opts,
            &temp,
            store_io_core::IoClass::Background,
        )?;
        // A device that needs larger blocks than the source's would round
        // every size up: refused (the temporary store is removed by the
        // next copy).
        if dst.report().block_size != block {
            return Err(refused(NotWrittenCause::Misaligned, None));
        }
        let reading = dst.reader_pace(&src);
        for f in &facts {
            bytes += dst.recreate(&src, f, reading.as_ref())?;
        }
        // One barrier for every byte not written as a fill (a stale page's
        // full copies; the clean close is best effort and reports nothing).
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
    /// unavailable regions (from the held metadata).
    fn omitted_for_copy(meta: &Meta) -> Vec<RegionId> {
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
        meta: &Meta,
        r: &LiveRegion,
        regions: &[std::sync::Arc<LiveRegion>],
    ) -> Option<(Vec<(u64, u64)>, u64)> {
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
    /// read's verdict is: `settle_read`). With `pace` (the copy's reader's
    /// call), in its pieces, each admitted before it is submitted and
    /// charged until it completes, their buffers under the pool's
    /// background cap.
    fn read_raw(
        &self,
        r: &LiveRegion,
        mut at: u64,
        mut offset: u64,
        out: &mut [u8],
        pace: Option<&Pace>,
    ) -> Result<(), Error> {
        let mut lane = self.inner.queues.get(0);
        clear_stale(&mut lane).map_err(|raw| io(Op::Read, raw))?;
        let chunk = pace.map_or(self.chunk(), |p| p.piece().min(self.chunk()));
        for piece in out.chunks_mut(chunk) {
            let c = crate::io::ctx_range(r, at, at + piece.len() as u64);
            let buf = match pace {
                Some(_) => self.take_capped(piece.len(), c)?,
                None => self.take(piece.len(), c)?,
            };
            let op = IoOp::Read {
                file: &self.inner.file,
                offset,
                buf,
            };
            let admitted = pace.map(Pace::admit).transpose()?;
            let ran = run(&mut lane, op);
            // Completed (or abandoned): its charge goes with it.
            drop(admitted);
            let (got, result) = match ran {
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

    /// The pacing of this copy's reads of `src` (R5.4: a call holds one
    /// issuer record per device group it uses): the target's own (the
    /// call's) when `src` is in its group, so the reader and the fill share
    /// one piece while commits are active and, while they are quiet, the
    /// reader leaves one of the call's pieces to the fill; else a record of
    /// the call on `src`'s group. `None` for a target that is no
    /// background call's.
    fn reader_pace(&self, src: &Store<P>) -> Option<Pace> {
        let own = self.inner.domain.pace()?;
        if Qos::same_group(&self.inner.qos, &src.inner.qos) {
            Some(own.second())
        } else {
            src.inner.domain.new_pace()
        }
    }

    /// Whether `r`, on a stale or possibly accepted page while another page
    /// is too (or lost), may hold another region's bytes (D-130): each
    /// such page's newest commit may be given up, one releasing `r`, the
    /// other handing its space to a region it created.
    fn at_risk(meta: &Meta, r: &LiveRegion) -> bool {
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

    /// What a region's header gives a plan, read with its header lock alone
    /// and dropped (the header pass, D-113's order: never under the
    /// metadata lock): its fill, fill key and capped flag, and a slot's
    /// committed object. The region's gate is checked after the header is
    /// read: a recycle or release retires it before it changes the header
    /// (and swaps the region list's entry only after), so a header read
    /// with the gate still live is the listed generation's, and one a retire
    /// may have changed fails `StaleGeneration`, before it is decoded.
    fn header_facts(&self, r: &LiveRegion) -> Result<HeaderFacts, Error> {
        let inner = &self.inner;
        let header = r
            .header
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if r.gate.live().is_err() {
            return Err(refused(NotWrittenCause::StaleGeneration, Some(r.id)));
        }
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
        let object = if r.kind == RegionKind::Slot {
            committed_object(&header, &inner.layout)
        } else {
            None
        };
        Ok(HeaderFacts {
            fill,
            fill_key,
            capped,
            object,
        })
    }

    /// Region `r`'s tag, from the held metadata's table entry.
    fn tag_of(meta: &Meta, r: &LiveRegion) -> Result<u32, Error> {
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
                    .ok_or_else(crate::store::corrupt)
            }
            None => meta
                .entries
                .iter()
                .find(|e| u64::from(e.region_id) == r.id.get())
                .map(|e| e.tag)
                .ok_or_else(crate::store::corrupt),
        }
    }

    /// What a copy keeps of region `r` (either format): `h` from the header
    /// pass, the rest from the held metadata; `regions` is every region of
    /// the store (a full copy's overlap test).
    fn facts_for_copy(
        &self,
        meta: &Meta,
        r: &std::sync::Arc<LiveRegion>,
        regions: &[std::sync::Arc<LiveRegion>],
        h: HeaderFacts,
    ) -> Result<Facts, Error> {
        let format_2 = self.inner.layout.format == FORMAT_2;
        // A format-1 slot's header-pair generation counts its commits; a
        // slot is never recycled, so its region generation is 1.
        let generation = if !format_2 && r.kind == RegionKind::Slot {
            1
        } else {
            r.generation.get()
        };
        let tag = Self::tag_of(meta, r)?;
        // A slot's object is whole as committed: never copied "in full".
        let full = if r.kind == RegionKind::Slot {
            None
        } else {
            self.full_copy(meta, r, regions)
        };
        let data_size = full
            .as_ref()
            .map_or(r.map.len(), |(ranges, _)| ranges.iter().map(|x| x.1).sum());
        Ok(Facts {
            name: r.name.as_str().to_owned(),
            kind: r.kind,
            data_size,
            // A capped region copied in full holds all of it.
            cap: h.capped.then(|| r.map.capacity().max(data_size)),
            attrs: CopyAttrs {
                generation,
                fill: h.fill,
                fill_key: h.fill_key,
                tag,
            },
            object: h.object,
            full,
            superseded: r.superseded || Self::at_risk(meta, r),
            source: std::sync::Arc::clone(r),
        })
    }

    /// A copy's plan of this store (V6's export shares it): every region's
    /// facts and the volume's, read in D-113's lock order. First the header
    /// pass, each region's header lock taken alone and dropped; then one
    /// metadata hold for the region list (its lock under the metadata
    /// lock, as provisioning takes it), the tags, the full copies, the
    /// regions at risk, and the unavailable and omitted lists. A region is
    /// its (id, generation): the hold's list must be the header pass's, or
    /// the plan fails `NotWritten(StaleGeneration)` naming the first region
    /// that changed between them (provisioned, released, recycled), never
    /// retried silently.
    ///
    /// # Errors
    ///
    /// `StaleGeneration` as above; without `accept`, `NotReady` for an
    /// unavailable region or an entry that does not decode, `Frozen` for a
    /// stale or lost page; `TooLarge` for a slot object a format-2 slot
    /// cannot hold; `Corruption(Metadata)` for a header or entry that does
    /// not decode.
    pub(crate) fn copy_plan(&self, accept: bool) -> Result<CopyPlan, Error> {
        let report = self.report();
        let block = report.block_size;
        let log2 = block.trailing_zeros() as u8;
        let regions = self.regions_snapshot();
        let mut headers = Vec::with_capacity(regions.len());
        for r in &regions {
            headers.push(self.header_facts(r)?);
        }
        crate::test_hook::step(crate::test_hook::Step::PlanHeaders);
        let meta = lock_meta(&self.inner.meta);
        let held = self.regions_snapshot();
        let same = |a: &LiveRegion, b: &LiveRegion| a.id == b.id && a.generation == b.generation;
        if let Some(i) = (0..regions.len().max(held.len()))
            .find(|&i| !matches!((regions.get(i), held.get(i)), (Some(a), Some(b)) if same(a, b)))
        {
            let changed = regions.get(i).or_else(|| held.get(i)).map(|r| r.id);
            return Err(refused(NotWrittenCause::StaleGeneration, changed));
        }
        let (mut omitted, mut lost_pages, mut stale_pages) = (Vec::new(), Vec::new(), Vec::new());
        if accept {
            // Every entry the open holds space for without serving it: the
            // unavailable regions and the entries of an unknown kind (format
            // 2's holds are the whole list; format 1 has only the former).
            omitted = Self::omitted_for_copy(&meta);
            // The pages as they stand now (an accepted rollback since the
            // open confirmed some).
            if let Some(v2) = meta.v2.as_deref() {
                for (i, p) in v2.pages.iter().enumerate() {
                    match p.standing {
                        Standing::Unconfirmed => stale_pages.push(i as u32),
                        Standing::Lost | Standing::Refused => lost_pages.push(i as u32),
                        Standing::Confirmed => {}
                    }
                }
            }
        } else {
            if let Some(u) = meta.unavailable.first() {
                return Err(refused(NotWrittenCause::NotReady, Some(u.id)));
            }
            // An entry whose kind or name does not decode (a newer release's)
            // is a region the copy cannot recreate (the list may be truncated:
            // any).
            // (The open's list, less those given up since: format 2's
            // holds are the entries as they stand.)
            let still = |id: u64| {
                meta.v2
                    .as_deref()
                    .is_none_or(|v2| v2.holds.iter().any(|h| h.id() == id))
            };
            if let Some(id) = report
                .metadata
                .invalid_entries
                .iter()
                .map(|&(p, s)| (u64::from(p) << 32) | u64::from(s))
                .find(|&id| still(id))
            {
                return Err(refused(
                    NotWrittenCause::NotReady,
                    Some(RegionId::from_raw(id)),
                ));
            }
            if self.frozen() {
                return Err(refused(NotWrittenCause::Frozen, None));
            }
        }
        let mut facts = Vec::with_capacity(held.len());
        let (mut verified_up_to, mut superseded) = (Vec::new(), Vec::new());
        for (r, h) in held.iter().zip(headers) {
            if h.object
                .as_ref()
                .is_some_and(|o| o.len() > max_object(log2))
            {
                return Err(refused(NotWrittenCause::TooLarge, Some(r.id)));
            }
            let f = self.facts_for_copy(&meta, r, &held, h)?;
            if let Some((_, recorded)) = &f.full {
                verified_up_to.push((f.name.clone(), *recorded));
            }
            if f.superseded {
                superseded.push(f.name.clone());
            }
            facts.push(f);
        }
        drop(meta);
        Ok(CopyPlan {
            facts,
            omitted,
            lost_pages,
            stale_pages,
            verified_up_to,
            superseded,
            block,
            source_format: report.format,
        })
    }

    /// Recreates `f` (a region of `src`) in this store and copies its data:
    /// an append or page region's as its provisioning's fill (durable and
    /// verified before its header, D-137), a full copy's after it (not yet
    /// durable: the caller's one barrier makes it so). Returns the bytes
    /// copied.
    fn recreate(&self, src: &Store<P>, f: &Facts, reading: Option<&Pace>) -> Result<u64, Error> {
        let mut opts = RegionOptions::default().size(f.data_size);
        if let Some(cap) = f.cap {
            opts = opts.max_size(cap);
        }
        // A data size the target's blocks cannot hold exactly would be
        // rounded up: refused before anything is read or written, naming
        // the source's region.
        if f.kind != RegionKind::Slot && f.data_size % self.block() != 0 {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::Misaligned,
                ctx: ErrorContext {
                    region: Some(f.source.id),
                    ..ErrorContext::default()
                },
            });
        }
        // An append or page region read whole through its own map is
        // copied as its provisioning's fill (D-137): one write per byte,
        // the source's reads in flight on a second thread.
        if f.full.is_none() && f.kind != RegionKind::Slot && f.data_size > 0 {
            let made = std::thread::scope(|s| {
                // The reader starts at the fill's first request, after the
                // region is placed: a region read in one piece is then read
                // while this thread waits for it, in a fixed order with this
                // thread's steps (the simulator's crash enumerations count
                // them). It may wait on its queue, or on the call's record
                // for the fill's piece, holding a lane of the source, and the
                // fill on it holding the target's queue 0: both stores are
                // this copy's own, opened by it, so nothing else waits for
                // either (D-136 forbids it in general).
                let mut feed = None;
                let mut content = |index: u64, dst: &mut [u8]| -> Result<(), Error> {
                    // A request for nothing (a first chunk all header
                    // zeros) starts nothing: the reader starts at the first
                    // request it must answer.
                    if dst.is_empty() {
                        return Ok(());
                    }
                    if feed.is_none() {
                        feed = Some(Feed::start(
                            s,
                            src,
                            &f.source,
                            f.data_size,
                            reading.cloned(),
                        )?);
                    }
                    match feed.as_mut() {
                        Some((_, reader)) => reader.read(index, dst),
                        None => Err(crate::store::corrupt()),
                    }
                };
                let made = self.provision2(
                    &f.name,
                    f.kind,
                    opts,
                    None,
                    Some(&f.attrs),
                    Some(&mut content),
                );
                // A failed provisioning stops the reader (its queue closed)
                // before it is joined. The provisioning's error comes first
                // (a reader's error reached it through the queue); else one
                // the reader met after the last byte was taken.
                let fed = match feed.take() {
                    None => Ok(()),
                    Some((handle, reader)) => {
                        drop(reader);
                        match handle.join() {
                            Ok(fed) => fed,
                            // A panic in the reader is the caller's, as it
                            // would be on this thread.
                            Err(panic) => std::panic::resume_unwind(panic),
                        }
                    }
                };
                match (made, fed) {
                    (Ok(made), Ok(())) => Ok(made),
                    (Err(e), _) | (Ok(_), Err(e)) => Err(e),
                }
            })?;
            debug_assert_eq!(made.generation.get(), f.attrs.generation, "{}", f.name);
            // Every byte of the data area was asked for: one the fill did
            // not cover would be committed unread (a backstop: the check
            // above makes the sizes equal).
            if made.map.len() != f.data_size {
                return Err(crate::store::corrupt());
            }
            return Ok(f.data_size);
        }
        let made = self.provision2(&f.name, f.kind, opts, None, Some(&f.attrs), None)?;
        debug_assert_eq!(made.generation.get(), f.attrs.generation, "{}", f.name);
        // The data size as the source's (checked above).
        debug_assert_eq!(made.map.len(), f.data_size, "{}", f.name);
        if let Some((ranges, _)) = &f.full {
            // This call's pieces (the target's `Domain::pace`), each
            // admitted before it is written and never across one of the
            // target's runs, so each charge covers one device I/O. Their
            // buffers are the region write path's own: the target's pool is
            // the call's, shared with no commit.
            let pace = self.inner.domain.pace();
            let piece = pace.map_or(CHUNK, |p| p.piece().min(CHUNK));
            let mut buf = vec![0u8; piece];
            let mut at = 0u64;
            for &(file, len) in ranges {
                let mut done = 0u64;
                while done < len {
                    let n = (len - done).min(piece as u64).min(made.map.run_left(at)) as usize;
                    src.read_raw(&f.source, at, file + done, &mut buf[..n], reading)?;
                    let charge = pace.map(crate::pace::Pace::admit).transpose()?;
                    match f.kind {
                        RegionKind::Page => {
                            let to = self.page_region(&f.name)?;
                            let _ticket = to.write(to.pos(at)?, &buf[..n])?;
                        }
                        _ => {
                            let _ticket = self.append_region(&f.name)?.append(&buf[..n])?;
                        }
                    }
                    drop(charge);
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
            // Empty: every other one was copied as its fill (D-137). A
            // region with data reaching here would be committed holding its
            // fill instead: refused (a backstop).
            RegionKind::Page | RegionKind::Append => {
                if f.data_size != 0 {
                    return Err(crate::store::corrupt());
                }
                Ok(0)
            }
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used, unused_results)]

    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use store_io_core::IoClass;
    use store_io_sim::world::TraceEvent;
    use store_io_sim::{SimConfig, SimPlatform};

    use crate::store::container_name;
    use crate::{Store, StoreOptions};

    fn background(p: &SimPlatform, dir: &str) -> Store<SimPlatform> {
        Store::create_named(
            p.clone(),
            Path::new(dir),
            StoreOptions::default(),
            &container_name().unwrap(),
            IoClass::Background,
        )
        .unwrap()
    }

    /// A copy's reader is a piece of its call: on the target's device group
    /// it shares the target's record (the call's), so reader and fill hold
    /// one piece between them while commits are active. A store that is no
    /// background call's has no reader pacing.
    #[test]
    fn test_a_copy_s_reader_shares_its_call_s_record_on_one_group() {
        let p = SimPlatform::new(SimConfig::volatile(54));
        let src = Store::create(p.clone(), Path::new("/a"), StoreOptions::default()).unwrap();
        let dst = background(&p, "/c");
        let own = dst.inner.domain.pace().unwrap();
        let reader = dst.reader_pace(&src).unwrap();
        assert!(reader.same_call(own));
        assert!(
            reader.is_second() && !own.is_second(),
            "it leaves the fill a piece"
        );
        assert!(src.reader_pace(&dst).is_none());
    }

    /// A copy's raw reads with its reader's pacing: `background_io`
    /// pieces, each charged when it is submitted, its buffer under the
    /// pool's background cap.
    #[test]
    fn test_a_paced_raw_read_reads_charged_capped_pieces() {
        let p = SimPlatform::new(SimConfig::volatile(55));
        let s = Store::create(p.clone(), Path::new("/s"), StoreOptions::default()).unwrap();
        let log = s.provision_append_region("log", 1 << 20).unwrap();
        let data: Vec<u8> = (0..1usize << 20)
            .map(|i| (i as u8) ^ ((i / 4096) as u8).wrapping_mul(37))
            .collect();
        let t = log.append(&data).unwrap();
        let _receipt = log.sync_through(&t).unwrap();
        let _hold = s.commit_hold();
        let r = Arc::clone(log.state());
        assert!(r.map.run_left(0) >= 1 << 20, "one run");
        let at = r.map.locate(0, 1 << 20);
        let pace = s.inner.domain.new_pace().unwrap();
        let (s2, seen) = (s.clone(), Arc::new(Mutex::new(Vec::new())));
        let out = Arc::clone(&seen);
        p.with_world(|w| {
            w.set_submit_probe(Some(Arc::new(move || {
                let held: u64 = s2
                    .inner
                    .pool
                    .background_holdings()
                    .iter()
                    .map(|c| u64::from(c.1))
                    .sum();
                out.lock().unwrap().push((s2.qos().in_flight, held));
            })))
        });
        let from = p.with_world(|w| w.trace().len());
        let mut back = vec![0u8; 1 << 20];
        s.read_raw(&r, 0, at, &mut back, Some(&pace)).unwrap();
        p.with_world(|w| w.set_submit_probe(None));
        assert!(back == data);
        let lens: Vec<u64> = p.with_world(|w| {
            w.trace()[from..]
                .iter()
                .filter_map(|e| match e {
                    TraceEvent::Read { len, .. } => Some(*len),
                    _ => None,
                })
                .collect()
        });
        assert_eq!(lens, vec![256 << 10; 4]);
        assert_eq!(*seen.lock().unwrap(), vec![(1, 1); 4]);
    }

    /// A reader with no pacing (a `Feed` given none: every store has a
    /// record today, but the type allows it) reads in the store's chunk,
    /// here 4 MiB (`max_io`), and still sends `FEED_PIECE` parts, each at
    /// its own offset (the reader's bound on the bytes it holds); the fill
    /// reads the region whole, in order.
    #[test]
    fn test_an_unpaced_feed_sends_pieces_at_their_offsets() {
        let p = SimPlatform::new(SimConfig::volatile(59));
        let opts = StoreOptions {
            max_io: 4 << 20,
            ..StoreOptions::default()
        };
        let s = Store::create(p.clone(), Path::new("/s"), opts).unwrap();
        assert!(s.chunk() > super::FEED_PIECE, "a chunk of several parts");
        let len = 8usize << 20;
        let log = s.provision_append_region("log", len as u64).unwrap();
        let data: Vec<u8> = (0..len)
            .map(|i| (i as u8) ^ ((i / 4096) as u8).wrapping_mul(37) ^ ((i >> 20) as u8))
            .collect();
        let t = log.append(&data).unwrap();
        let _receipt = log.sync_through(&t).unwrap();
        let r = Arc::clone(log.state());
        let back = std::thread::scope(|sc| {
            let (reader, mut feed) = super::Feed::start(sc, &s, &r, len as u64, None).unwrap();
            let mut back = vec![0u8; len];
            for (i, part) in back.chunks_mut(1 << 20).enumerate() {
                feed.read((i << 20) as u64, part).unwrap();
            }
            drop(feed);
            reader.join().unwrap().unwrap();
            back
        });
        assert!(back == data);
    }
}

/// The plan's lock order and its region identity (V6: export plans a live
/// store, so a region may change between the header pass and the hold).
#[cfg(all(test, not(loom)))]
mod plan_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, unused_results)]

    use std::path::Path;

    use store_io_core::error::{Error, NotWrittenCause};
    use store_io_format::slot::{FORMAT_1, FORMAT_2};
    use store_io_sim::{SimConfig, SimPlatform};

    use crate::test_hook::{Step, lib_tests};
    use crate::{Store, StoreOptions};

    type Action = Box<dyn FnOnce() + Send>;

    /// A writable store of `format` with an append region, a page region
    /// and a committed slot.
    fn store(seed: u64, format: u16) -> Store<SimPlatform> {
        let p = SimPlatform::new(SimConfig::volatile(seed));
        let opts = StoreOptions {
            container_format: format,
            ..StoreOptions::default()
        };
        let s = Store::create(p, Path::new("/s"), opts).unwrap();
        let _a = s.provision_append_region("a", 1 << 20).unwrap();
        let _p = s.provision_page_region("p", 1 << 20).unwrap();
        let slot = s.provision_slot("s").unwrap();
        let _r = slot.commit(b"object").unwrap();
        s
    }

    /// Plans `s` with `act` run between its passes.
    fn plan_with(s: &Store<SimPlatform>, act: Action) -> Result<super::CopyPlan, Error> {
        lib_tests::at(Step::PlanHeaders, act);
        let plan = s.copy_plan(false);
        lib_tests::clear();
        plan
    }

    /// The plan failed `StaleGeneration` naming `name`'s region (as it
    /// was listed when the test began).
    fn stale(r: &Result<super::CopyPlan, Error>, id: Option<store_io_core::id::RegionId>) -> bool {
        matches!(
            r,
            Err(Error::NotWritten {
                cause: NotWrittenCause::StaleGeneration,
                ctx,
            }) if ctx.region.is_some() && (id.is_none() || ctx.region == id)
        )
    }

    fn id_of(s: &Store<SimPlatform>, name: &str) -> Option<store_io_core::id::RegionId> {
        s.regions_snapshot()
            .iter()
            .find(|r| r.name.as_str() == name)
            .map(|r| r.id)
    }

    /// A plan made while a recycle has rewritten its region's header but
    /// not yet swapped the region list's entry (the list still names the
    /// old generation) fails `StaleGeneration`, never a plan pairing the
    /// new header with the old generation. Both formats.
    #[test]
    fn test_a_plan_inside_a_recycle_s_window_fails() {
        for (seed, format) in [(84, FORMAT_1), (85, FORMAT_2)] {
            let s = store(seed, format);
            let p_id = id_of(&s, "p");
            let s2 = s.clone();
            let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
            let out = std::sync::Arc::clone(&seen);
            lib_tests::at(
                Step::RecycleRewritten,
                Box::new(move || {
                    let plan = s2.copy_plan(false);
                    *out.lock().unwrap() = Some(stale(&plan, p_id));
                }),
            );
            let _q = s.page_region("p").unwrap().recycle().unwrap();
            lib_tests::clear();
            assert_eq!(*seen.lock().unwrap(), Some(true), "format {format}");
            assert!(s.copy_plan(false).is_ok());
        }
    }

    /// A region provisioned, released, recycled, or released and
    /// provisioned again under its name between the header pass and the
    /// hold fails the plan `StaleGeneration`, the region named; with
    /// nothing between, the plan holds every region, its slot's object
    /// from the header pass. Both formats.
    #[test]
    fn test_a_region_changed_between_the_passes_fails_the_plan() {
        for (seed, format) in [(80, FORMAT_1), (81, FORMAT_2)] {
            let s = store(seed, format);
            let plan = plan_with(&s, Box::new(|| {})).unwrap();
            assert_eq!(plan.facts.len(), 3, "format {format}");
            let slot = plan.facts.iter().find(|f| f.name == "s").unwrap();
            assert_eq!(slot.object.as_deref(), Some(&b"object"[..]));

            let s2 = s.clone();
            let r = plan_with(
                &s,
                Box::new(move || {
                    let _n = s2.provision_append_region("new", 1 << 20).unwrap();
                }),
            );
            assert!(
                stale(&r, None),
                "provisioned, format {format}: {:?}",
                r.err()
            );

            let s2 = s.clone();
            let r = plan_with(
                &s,
                Box::new(move || {
                    let _q = s2.page_region("p").unwrap().recycle().unwrap();
                }),
            );
            assert!(
                stale(&r, id_of(&s, "p")),
                "recycled, format {format}: {:?}",
                r.err()
            );

            let a_id = id_of(&s, "a");
            let s2 = s.clone();
            let r = plan_with(
                &s,
                Box::new(move || {
                    s2.append_region("a").unwrap().release().unwrap();
                    let _a = s2.provision_append_region("a", 1 << 20).unwrap();
                }),
            );
            assert!(
                stale(&r, a_id),
                "released and again, format {format}: {:?}",
                r.err()
            );

            let a_id = id_of(&s, "a");
            let s2 = s.clone();
            let r = plan_with(
                &s,
                Box::new(move || {
                    s2.append_region("a").unwrap().release().unwrap();
                }),
            );
            assert!(stale(&r, a_id), "released, format {format}: {:?}", r.err());
            assert!(s.copy_plan(false).is_ok(), "unchanged since");
        }
    }

    /// Plans racing provisioning, growth, recycling and release on another
    /// thread never deadlock (D-113's order: no header lock under the
    /// metadata lock) and fail only `StaleGeneration`. Both formats, under
    /// a watchdog.
    #[test]
    fn test_plans_racing_metadata_operations_never_deadlock() {
        let (tx, rx) = std::sync::mpsc::channel();
        let _w = std::thread::spawn(move || {
            for (seed, format) in [(82, FORMAT_1), (83, FORMAT_2)] {
                let s = store(seed, format);
                let s2 = s.clone();
                let ops = std::thread::spawn(move || {
                    for i in 0..60u64 {
                        let name = format!("r{}", i % 4);
                        if let Ok(r) = s2.provision_page_region(&name, 1 << 16) {
                            let _g = r.grow(1 << 17);
                            if i % 2 == 0 {
                                let _q = r.recycle();
                            }
                            let _r = r.release();
                        }
                        let _q = s2.page_region("p").and_then(|p| p.recycle());
                    }
                });
                let mut planned = 0;
                while !ops.is_finished() || planned == 0 {
                    match s.copy_plan(false) {
                        Ok(_) => planned += 1,
                        Err(e) => assert!(
                            matches!(
                                e,
                                Error::NotWritten {
                                    cause: NotWrittenCause::StaleGeneration,
                                    ..
                                }
                            ),
                            "format {format}: {e:?}"
                        ),
                    }
                }
                ops.join().unwrap();
            }
            let _sent = tx.send(());
        });
        rx.recv_timeout(std::time::Duration::from_secs(120))
            .expect("a plan deadlocked against a metadata operation (or panicked)");
    }
}
