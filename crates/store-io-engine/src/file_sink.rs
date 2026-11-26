//! `FileSink` (PLANNING R5.6, D-168): an [`ExportSink`] writing an export
//! to a directory, one data file per region and a `regions.info` record
//! (format `exportinfo`) that makes the directory restorable.
//!
//! **The layout.** `region-00000.data`, `region-00001.data`, ... named by
//! the region's index in the stream, never by its name (which may hold
//! `/` or `..`), each exactly its region's stream length; and
//! `regions.info`, holding the export's header, the volume's facts, every
//! region's attributes and a CRC-32C of each 1 MiB span of each data file.
//! **The directory is restorable only once `finish` returned `Ok`**:
//! `regions.info` is created empty at `begin` and written, flushed and its
//! directory synced only at `finish`, so a directory without a whole
//! `regions.info` is never taken for a backup.
//!
//! **What it is pushed is checked**, as a public type a caller may drive
//! by hand or wrap: the calls in order (`begin`, then for each region
//! `region`, its `data`, `end_region`, then `finish`), each region's info
//! by [`RegionInfo::validate`] and its name once, each chunk at its
//! region's next byte and within its declared length, each region's
//! length whole at `end_region`, the regions' sizes within the header's
//! `total`, and exactly `region_count` regions. Any refusal or failure is
//! sticky: every later call is refused, so a later `finish` never makes the
//! directory restorable.
//!
//! **Writes.** Direct, in whole blocks of the larger of the source's block
//! and the target file system's, kept in flight together, up to [`DEPTH`]
//! (D-171: G21 measured one write at a time, D-168, far under the device's
//! rate). A chunk is written from its own buffer, with no copy (D-175),
//! when nothing is staged before it (or the stage ends on a block), its
//! bytes start at its buffer's start, its length is whole blocks and at
//! most the call's piece, its buffer is aligned for the target's direct
//! I/O, and the stream can spare it (counting every chunk it has lent, the
//! sink's and its caller's, it keeps one to deliver); anything else (a
//! slot's object, say, or a chunk the stream cannot spare) is staged in
//! pieces of the call's piece. A chunk written from goes back to its
//! stream when its write completes; one whose write's queue is lost (a
//! failed wait) stays with that queue, and the stream counts it no longer
//! lent (it has one buffer fewer) when the sink drops its record (at the
//! sink's drop). A sink the export stopped for its own reasons holds such
//! writes in flight, at most `lend - 1` of the source store's buffers,
//! until its next call or its drop. Each write is admitted
//! by the call's record on the target's device group before it is
//! submitted, and its charge held until it completes; the sink's writes in
//! flight are a collector of the call on this thread (R5.4), so a wait for
//! an admission anywhere in the call (the export stream's, or the sink's
//! own) reaps them first and never waits on itself. What the sink reports
//! written is only how far its writes done reach without a gap. A
//! region's writes are all completed before its last is padded and the
//! file cut to its length, flushed, and its directory synced: a region
//! counts as written only then. A failed write is reported to the export's
//! call with where it began (every write before it done), so the export's
//! report never counts bytes the sink did not write. A failed or refused
//! sink collects its writes in flight at once (their charges released on
//! the target's group); one the export stopped for its own reasons (a
//! media error, say) is not told, and holds its writes in flight, at most
//! [`DEPTH`], until its next call or its drop: drop it once the export
//! returns.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::marker::PhantomData;
use std::path::Path;
use std::rc::{Rc, Weak};
use std::sync::Arc;

use store_io_buf::{BufPool, IoBuf, PoolConfig};
use store_io_core::error::{Error, ErrorContext, Named, NotWrittenCause, Op, OsError, ReserveTag};
use store_io_format::crc32c::crc32c_append;
use store_io_format::exportinfo::{self, InfoHeader, RegionRecord, SPAN};
use store_io_format::meta::{RegionKind, RegionName};
use store_io_format::meta2::max_object;
use store_io_platform::{FileName, IoOp, Owner, Platform, Queue, QueueConfig, RawResult};

use crate::call::{self, CallState};
use crate::exec::{Lane, LaneMark, owned_lane};
use crate::export::{ExportHeader, ExportSink, RegionInfo, SinkError};
use crate::io::not_written;
use crate::pace::{Held, Pace, Registered};
use crate::qos::Collect;
use crate::store::StoreOptions;
use crate::stream::Chunk;

/// The record that makes a [`FileSink`]'s directory restorable.
pub const REGIONS_INFO: &str = "regions.info";

/// The largest staging write when the target is not paced.
const UNPACED_PIECE: usize = 256 << 10;

/// The sink's writes in flight at most (each admitted by the call's
/// record, which bounds the call's pieces in flight: R5.4), and its
/// staging buffers: one being filled and the others in flight.
pub(crate) const DEPTH: usize = 8;

/// One write in flight.
struct Pending {
    tag: u64,
    offset: u64,
    len: usize,
    /// Its result, once its completion was collected.
    result: Option<RawResult<usize>>,
    /// Its admission, released when its completion is collected.
    charge: Option<Held>,
    /// A write from a chunk's own buffer: the chunk, lent until the write's
    /// completion gives its buffer back (then the chunk drops at once).
    chunk: Option<Chunk>,
}

/// The sink's writes in flight, oldest first, and how far its writes done
/// reach: a collector of the export's call on the sink's thread (R5.4).
struct Writes<Q: Queue> {
    lane: Lane<Q>,
    pending: VecDeque<Pending>,
    /// The buffers of writes completed, for staging again.
    free: Vec<IoBuf>,
    /// The file being written: where its writes done reach without a gap.
    done_to: u64,
    /// The first failure: the error, and the start of the write it lost
    /// (every write before it done).
    failed: Option<(Error, u64)>,
    /// A failed wait, as [`Collect::failed`] reports it.
    wait_failed: Option<(OsError, u64)>,
    next_tag: u64,
}

impl<Q: Queue> Writes<Q> {
    /// Submits `buf` at `offset` of `file`, its admission `charge` held
    /// until it completes; `chunk`: the chunk `buf` is from (a write with
    /// no copy), else `buf` is a staging buffer.
    fn submit(
        &mut self,
        file: &Q::File,
        offset: u64,
        buf: IoBuf,
        charge: Option<Held>,
        mut chunk: Option<Chunk>,
    ) -> Result<(), Error> {
        let len = buf.len();
        let tag = self.next_tag;
        let op = IoOp::Write {
            file,
            offset,
            buf,
            dsync: false,
        };
        match self.lane.submit(op, tag) {
            Ok(()) => {
                self.next_tag += 1;
                self.pending.push_back(Pending {
                    tag,
                    offset,
                    len,
                    result: None,
                    charge,
                    chunk,
                });
                Ok(())
            }
            Err(rejected) => {
                if let Some(b) = rejected.buf {
                    match chunk.as_mut() {
                        Some(c) => c.restore(b),
                        None => self.free.push(b),
                    }
                }
                Err(rejected.raw.map_or_else(
                    || not_written(NotWrittenCause::QueueFull, ErrorContext::default()),
                    |r| io(Op::Write, r),
                ))
            }
        }
    }

    /// Files every completion the lane holds under its write (its charge
    /// released, its buffer free again), then moves `done_to` over the
    /// writes done in order: a write that failed or came back short stops
    /// it there, recorded as the failure.
    fn file(&mut self) {
        for comp in self.lane.done.drain() {
            let mut buf = comp.buf;
            if let Some(w) = self.pending.iter_mut().find(|w| w.tag == comp.tag) {
                w.result = Some(comp.result);
                w.charge = None;
                // A chunk's buffer goes back to its stream now (a chunk
                // whose completion has none is dropped when it is popped,
                // its buffer counted lost).
                if buf.is_some() {
                    if let Some(mut c) = w.chunk.take() {
                        if let Some(b) = buf.take() {
                            c.restore(b);
                        }
                    }
                }
            }
            if let Some(b) = buf {
                debug_assert!(
                    self.pending.iter().any(|w| w.tag == comp.tag),
                    "a completion of no write in flight"
                );
                self.free.push(b);
            }
        }
        while let Some(w) = self.pending.front() {
            let Some(result) = w.result else {
                break;
            };
            if self.failed.is_none() {
                match result {
                    Ok(n) if n == w.len => self.done_to = w.offset + w.len as u64,
                    Ok(_) => {
                        self.failed = Some((
                            not_written(NotWrittenCause::Interrupted, ErrorContext::default()),
                            w.offset,
                        ));
                    }
                    Err(raw) => self.failed = Some((io(Op::Write, raw), w.offset)),
                }
            }
            let _done = self.pending.pop_front();
        }
    }

    /// Collects what has completed, without waiting.
    fn reap(&mut self) {
        let _arrived = self.lane.reap();
        self.file();
    }

    /// Writes in flight from chunks' own buffers (the stream's buffers the
    /// sink holds).
    fn chunks(&self) -> usize {
        self.pending.iter().filter(|w| w.chunk.is_some()).count()
    }

    /// Waits for at least one write in flight: whether there was one. A
    /// failed wait is the sink's failure (sticky), at the oldest write's
    /// start, and every write's charge is dropped (the writes stay with the
    /// queue, never waited on again).
    fn wait_one(&mut self) -> Result<bool, OsError> {
        if let Some((raw, _)) = self.wait_failed {
            return Err(raw);
        }
        if self.pending.is_empty() {
            return Ok(false);
        }
        match self.lane.wait(1) {
            Ok(_) => {
                self.file();
                Ok(true)
            }
            Err(raw) => {
                self.file();
                let lost = self.pending.front().map_or(self.done_to, |w| w.offset);
                self.wait_failed = Some((raw, lost));
                if self.failed.is_none() {
                    self.failed = Some((io(Op::Write, raw), lost));
                }
                for w in &mut self.pending {
                    w.charge = None;
                }
                Err(raw)
            }
        }
    }

    /// Waits for every write in flight.
    fn wait_all(&mut self) -> Result<(), OsError> {
        while self.wait_one()? {}
        Ok(())
    }
}

impl<Q: Queue> Collect for RefCell<Writes<Q>> {
    fn collect_one(&self) -> Result<bool, OsError> {
        // Borrowed: the sink is the part asking, between its own calls
        // into its writes (it never holds the borrow while admitting).
        match self.try_borrow_mut() {
            Ok(mut w) => w.wait_one(),
            Err(_) => Ok(false),
        }
    }

    fn collect_all(&self) -> Result<(), OsError> {
        match self.try_borrow_mut() {
            Ok(mut w) => w.wait_all(),
            Err(_) => Ok(()),
        }
    }

    fn failed(&self) -> Option<(OsError, u64)> {
        self.try_borrow().ok()?.wait_failed
    }
}

/// How a [`FileSink`] writes.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct FileSinkOptions {
    /// Its pacing on the target's device group: the background fields
    /// ([`StoreOptions::background_io`], pacing and the rest), as a store
    /// on that device would take them.
    pub store: StoreOptions,
}

/// Where the sink is in its calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Fresh,
    Begun,
    InRegion { declared: u64, written: u64 },
    Ended,
    Finished,
    Failed,
}

/// The data file of region `index`.
fn data_name(index: u32) -> String {
    format!("region-{index:05}.data")
}

fn file_name(name: &str) -> Result<FileName, Error> {
    FileName::new(name).map_err(|_| refusal())
}

fn refusal() -> Error {
    not_written(NotWrittenCause::InvalidOptions, ErrorContext::default())
}

fn io(op: Op, raw: OsError) -> Error {
    Error::Io {
        op,
        raw,
        ctx: ErrorContext::default(),
    }
}

/// The sink error a failure shows its caller.
fn shown(e: &Error) -> SinkError {
    match e {
        Error::Io { raw, .. } => SinkError::Io(*raw),
        _ => SinkError::Refused,
    }
}

/// An export written to a directory: see the module docs. Not `Send`: it is
/// bound to the export's call on the thread that made it.
pub struct FileSink<P: Platform> {
    platform: P,
    dir: P::Dir,
    opts: FileSinkOptions,
    state: State,
    header: Option<ExportHeader>,
    /// The export's call, found at `begin` (none when driven by hand), and
    /// the sink's id on it.
    call: Option<Rc<CallState>>,
    id: usize,
    pace: Option<Pace>,
    /// Its writes in flight, registered as a collector of the call while
    /// the sink lives.
    writes: Option<Rc<RefCell<Writes<P::Queue>>>>,
    registered: Option<Registered>,
    pool: Option<BufPool>,
    /// The write block and the staging buffer's size (a multiple of it).
    block: u64,
    piece: usize,
    /// The target's direct-I/O memory alignment, and the chunks the
    /// export's stream lends at once (its header's `lend`).
    align: usize,
    lend: usize,
    /// Writes made from chunks' own buffers (no copy).
    direct_writes: u64,
    stage: Option<IoBuf>,
    /// Bytes in the staging buffer, and the file offset it starts at.
    fill: usize,
    at: u64,
    info: Option<P::File>,
    file: Option<P::File>,
    /// The region being written, its index, and its span CRCs so far.
    cur: Option<RegionRecord>,
    index: u32,
    span_crc: u32,
    span_len: u64,
    spans: Vec<u32>,
    /// Every region written, with its span CRCs.
    done: Vec<(RegionRecord, Vec<u32>)>,
    /// What the regions written so far charge against the header's total.
    charged: u64,
    /// An admission ended without one (another part's collection failed).
    admit_failed: bool,
    _thread: PhantomData<*const ()>,
}

impl<P: Platform> core::fmt::Debug for FileSink<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FileSink")
            .field("state", &self.state)
            .field("regions", &self.done.len())
            .finish_non_exhaustive()
    }
}

impl<P: Platform> FileSink<P> {
    /// A sink writing into the directory at `dir`, which must exist and be
    /// empty (it never creates one).
    ///
    /// # Errors
    ///
    /// `NotFound(Store)` if the directory does not exist,
    /// `NotWritten(InvalidOptions)` if it is not empty, `Io` if it cannot
    /// be opened or listed.
    pub fn create(platform: P, dir: &Path, opts: FileSinkOptions) -> Result<Self, Error> {
        let d = platform.open_dir(dir, false).map_err(|raw| {
            if store_io_core::errno::is_not_found(raw) {
                Error::NotFound { what: Named::Store }
            } else {
                io(Op::Open, raw)
            }
        })?;
        if !platform.dir_is_empty(&d).map_err(|raw| io(Op::Open, raw))? {
            return Err(refusal());
        }
        Ok(Self {
            platform,
            dir: d,
            opts,
            state: State::Fresh,
            header: None,
            call: None,
            id: 0,
            pace: None,
            writes: None,
            registered: None,
            pool: None,
            block: 0,
            piece: 0,
            align: 4096,
            lend: 0,
            direct_writes: 0,
            stage: None,
            fill: 0,
            at: 0,
            info: None,
            file: None,
            cur: None,
            index: 0,
            span_crc: 0,
            span_len: 0,
            spans: Vec::new(),
            done: Vec::new(),
            charged: 0,
            admit_failed: false,
            _thread: PhantomData,
        })
    }

    /// Fails the sink (sticky) with `e`, reporting it to the call at
    /// `offset` of the region being exported (a failed write's start, or
    /// the bytes accepted when a call is refused), and returns what its
    /// caller is shown. An admission that ended because collecting another
    /// part of the call failed is that part's failure, reported by it (the
    /// export asks it), never the sink's.
    fn fail(&mut self, e: Error, offset: u64) -> SinkError {
        self.state = State::Failed;
        // Its writes in flight collected now (best effort: a failed wait
        // drops their charges), so a failed sink holds no piece of the
        // target's group while its caller still holds it.
        if let Some(w) = &self.writes {
            if let Ok(mut ws) = w.try_borrow_mut() {
                let _waited = ws.wait_all();
            }
        }
        let shown = shown(&e);
        if let Some(c) = &self.call {
            if !self.admit_failed {
                c.part_failed(e, offset);
            }
        }
        shown
    }

    /// Refuses the call (sticky), at `offset` of the region being exported.
    fn refuse(&mut self, offset: u64) -> SinkError {
        self.fail(refusal(), offset)
    }

    /// Whether `info` may be the next region: a valid record for the
    /// header's block, a name not yet used, within the region count and
    /// the header's total.
    fn admits(&self, info: &RegionInfo, header: &ExportHeader) -> Option<u64> {
        if self.index >= header.region_count || info.validate(header.block_size).is_err() {
            return None;
        }
        if self.done.iter().any(|(r, _)| r.name.as_str() == info.name) {
            return None;
        }
        let block = header.block_size;
        let data = match info.kind {
            RegionKind::Slot => max_object(block.trailing_zeros() as u8) as u64,
            _ => info.cap.unwrap_or(0).max(info.data_size),
        };
        let charged = self
            .charged
            .checked_add(2 * block)
            .and_then(|c| c.checked_add(data))?;
        (charged <= header.total).then_some(charged)
    }

    /// Submits the staging buffer's first `len` bytes (whole blocks) at
    /// the current offset of `which` (the region's file, or
    /// `regions.info`), admitted by the call's record first; the write
    /// stays in flight (with up to [`DEPTH`] - 1 others) until a collection
    /// completes it, and the next staging buffer is a completed write's.
    fn write_stage(&mut self, len: usize, info_file: bool) -> Result<(), Error> {
        let Some(w) = self.writes.clone() else {
            return Err(refusal());
        };
        self.failure_of(&w)?;
        // A row of the lane free (writes from chunks share them).
        self.make_room(&w)?;
        // Admitted first (a refusal keeps the staging buffer).
        let charge = self.admit_write(&w)?;
        let Some(mut buf) = self.stage.take() else {
            return Err(refusal());
        };
        if !buf.set_len(len) {
            self.stage = Some(buf);
            return Err(refusal());
        }
        let file = if info_file { &self.info } else { &self.file };
        let Some(file) = file.as_ref() else {
            self.stage = Some(buf);
            return Err(refusal());
        };
        w.borrow_mut().submit(file, self.at, buf, charge, None)?;
        self.at += len as u64;
        // The next staging buffer: a completed write's, else one waited
        // for (the pool holds one per write in flight and one to fill).
        self.stage = loop {
            let mut ws = w.borrow_mut();
            ws.reap();
            if let Some(b) = ws.free.pop() {
                break Some(b);
            }
            if !ws.wait_one().map_err(|raw| io(Op::Write, raw))? {
                break None;
            }
        };
        self.failure_of(&w)
    }

    /// Admits one write by the call's record (none when unpaced), with no
    /// borrow of the writes held: its wait collects the call's pieces on
    /// this thread, the sink's own writes among them (R5.4).
    fn admit_write(&mut self, w: &Rc<RefCell<Writes<P::Queue>>>) -> Result<Option<Held>, Error> {
        let charge = match &self.pace {
            Some(p) => match p.admit() {
                Ok(h) => Some(h),
                Err(e) => {
                    // The admission's wait collected the sink's own writes
                    // and that collection failed (a write, or the wait
                    // itself): the sink's failure, recorded as its own.
                    self.failure_of(w)?;
                    self.admit_failed = true;
                    return Err(e);
                }
            },
            None => None,
        };
        // A write the admission's wait collected may have failed: that is
        // the failure, before anything more is submitted (the admission's
        // charge released).
        self.failure_of(w)?;
        Ok(charge)
    }

    /// Waits, collecting the sink's own writes, until a row of the lane is
    /// free.
    fn make_room(&self, w: &Rc<RefCell<Writes<P::Queue>>>) -> Result<(), Error> {
        loop {
            let mut ws = w.borrow_mut();
            ws.reap();
            let full = ws.pending.len() >= DEPTH;
            if !full {
                break;
            }
            if !ws.wait_one().map_err(|raw| io(Op::Write, raw))? {
                break;
            }
        }
        self.failure_of(w)
    }

    /// Whether `c` may be written from its own buffer (see the module's
    /// "Writes"): the stage empty or ending on a block, its bytes from its
    /// buffer's start, whole blocks, at most the call's piece, and its
    /// buffer aligned for the target. Whether the stream can spare it is
    /// [`Self::accept`]'s to check, after collecting what has completed.
    fn direct_ok(&self, c: &Chunk) -> bool {
        let bytes = c.bytes();
        let (n, block) = (bytes.len(), self.block as usize);
        block > 0
            && self.fill % block == 0
            && c.starts_its_buffer()
            && n > 0
            && n % block == 0
            && n <= self.piece
            && (bytes.as_ptr() as usize) % self.align == 0
    }

    /// Writes `chunk` from its own buffer, or stages its bytes: staged too
    /// when the stream cannot spare it (it lends `lend` chunks at once, this
    /// one and any its caller still holds counted), never waited for, so
    /// the sink's writes in flight are never fewer than staging allows.
    fn accept(&mut self, mut chunk: Chunk) -> Result<(), Error> {
        if !self.direct_ok(&chunk) {
            return self.stage_bytes(chunk.bytes(), false);
        }
        let Some(w) = self.writes.clone() else {
            return Err(refusal());
        };
        // The chunks of writes completed back to the stream first.
        w.borrow_mut().reap();
        if chunk.stream_lent() >= self.lend {
            return self.stage_bytes(chunk.bytes(), false);
        }
        // What is staged first (whole blocks: no padding written).
        self.drain_stage(false)?;
        self.failure_of(&w)?;
        self.make_room(&w)?;
        let charge = self.admit_write(&w)?;
        let len = chunk.bytes().len();
        let Some(buf) = chunk.take_buf() else {
            drop(charge);
            return self.stage_bytes(chunk.bytes(), false);
        };
        let Some(file) = self.file.as_ref() else {
            chunk.restore(buf);
            return Err(refusal());
        };
        w.borrow_mut()
            .submit(file, self.at, buf, charge, Some(chunk))?;
        self.at += len as u64;
        self.direct_writes += 1;
        self.failure_of(&w)
    }

    /// Writes made from chunks' own buffers so far.
    #[cfg(all(test, not(loom)))]
    pub(crate) fn direct_writes(&self) -> u64 {
        self.direct_writes
    }

    /// The writes' recorded failure, if any (sticky).
    fn failure_of(&self, w: &Rc<RefCell<Writes<P::Queue>>>) -> Result<(), Error> {
        match &w.borrow().failed {
            Some((e, _)) => Err(e.clone()),
            None => Ok(()),
        }
    }

    /// Collects every write in flight, then the writes' failure if any.
    fn settle_writes(&self) -> Result<(), Error> {
        let Some(w) = &self.writes else {
            return Err(refusal());
        };
        let waited = w.borrow_mut().wait_all();
        self.failure_of(w)?;
        waited.map_err(|raw| io(Op::Write, raw))
    }

    /// How far the writes done reach in the file being written without a
    /// gap (after collecting what has completed).
    fn written(&self) -> u64 {
        self.writes.as_ref().map_or(0, |w| {
            let mut ws = w.borrow_mut();
            ws.reap();
            ws.done_to
        })
    }

    /// Whether a write (or a wait for one) failed.
    fn writes_failed(&self) -> bool {
        self.writes
            .as_ref()
            .is_some_and(|w| w.borrow().failed.is_some())
    }

    /// Where a failure leaves the file being written: every write in
    /// flight collected (best effort), how far the writes done reach.
    fn failed_at(&self) -> u64 {
        self.writes.as_ref().map_or(0, |w| {
            let mut ws = w.borrow_mut();
            let _waited = ws.wait_all();
            ws.done_to
        })
    }

    /// Starts a file: its writes done counted from 0 (none in flight).
    fn start_file(&mut self) {
        if let Some(w) = &self.writes {
            w.borrow_mut().done_to = 0;
        }
        self.fill = 0;
        self.at = 0;
    }

    /// Writes what the staging buffer holds, its last block padded with
    /// zeros.
    fn drain_stage(&mut self, info_file: bool) -> Result<(), Error> {
        if self.fill == 0 {
            return Ok(());
        }
        let block = self.block as usize;
        let len = self.fill.div_ceil(block) * block;
        if let Some(b) = self.stage.as_mut() {
            b.as_mut_capacity()[self.fill..len].fill(0);
        }
        self.write_stage(len, info_file)?;
        self.fill = 0;
        Ok(())
    }

    /// Copies `bytes` into the staging buffer, writing it whenever full.
    fn stage_bytes(&mut self, mut bytes: &[u8], info_file: bool) -> Result<(), Error> {
        while !bytes.is_empty() {
            let Some(b) = self.stage.as_mut() else {
                return Err(not_written(
                    NotWrittenCause::PoolExhausted,
                    ErrorContext::default(),
                ));
            };
            let n = bytes.len().min(self.piece - self.fill);
            b.as_mut_capacity()[self.fill..self.fill + n].copy_from_slice(&bytes[..n]);
            self.fill += n;
            bytes = &bytes[n..];
            if self.fill == self.piece {
                self.write_stage(self.piece, info_file)?;
                self.fill = 0;
            }
        }
        Ok(())
    }

    /// Extends the region's span CRCs over `bytes`.
    fn crc_bytes(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let n = bytes.len().min((SPAN - self.span_len) as usize);
            self.span_crc = crc32c_append(self.span_crc, &bytes[..n]);
            self.span_len += n as u64;
            bytes = &bytes[n..];
            if self.span_len == SPAN {
                self.spans.push(self.span_crc);
                self.span_crc = 0;
                self.span_len = 0;
            }
        }
    }

    /// Cuts `file` to `len`, then flushes it (data and size).
    fn settle_file(&self, file: &P::File, len: u64) -> Result<(), Error> {
        self.platform
            .set_len(file, len)
            .map_err(|raw| io(Op::Allocate, raw))?;
        self.platform
            .flush_all(file)
            .map_err(|raw| io(Op::FlushAll, raw))
    }

    /// Syncs the directory: the files' names durable.
    fn sync_dir(&self) -> Result<(), Error> {
        self.platform
            .sync_dir(&self.dir)
            .map_err(|raw| io(Op::SyncDir, raw))
    }

    fn begin_run(&mut self, header: &ExportHeader) -> Result<(), Error> {
        // The export's call when bound to one (its failures are then the
        // export's from here on).
        self.call = call::current();
        if let Some(c) = &self.call {
            self.id = c.sink_bound();
        }
        let block = header.block_size;
        if !block.is_power_of_two() || !(512..=1 << 16).contains(&block) {
            return Err(refusal());
        }
        if header.region_count > crate::v2::MAX_REGION_CAPACITY {
            return Err(refusal());
        }
        // `regions.info` now, empty, exclusively: the sink's device group
        // is the one this file is on.
        let name = file_name(REGIONS_INFO)?;
        let info = self
            .platform
            .create_file(&self.dir, &name)
            .map_err(|raw| io(Op::Create, raw))?;
        let free = self
            .platform
            .free_space(&self.dir)
            .map_err(|raw| io(Op::Open, raw))?;
        // Short of the device's free space: the default tag's, as a copy's
        // or a provisioning's target reports running out (`Caller(0)`).
        if free < header.total {
            return Err(Error::NoSpace {
                tag: ReserveTag::Caller(0),
                ctx: ErrorContext::default(),
            });
        }
        let ev = self
            .platform
            .probe(&self.dir, &info)
            .map_err(|raw| io(Op::Probe, raw))?;
        let fs_block = 1u64 << crate::layout::choose_log2_block(&ev, None);
        self.block = fs_block.max(block);
        let qos = crate::store::join_group(&self.platform, &ev, &self.opts.store, self.block);
        let clock = self.platform.clock();
        // The export's record for this group when bound to one, else a
        // call of its own.
        self.pace = match &self.call {
            Some(c) => c.pace_for(&qos, &clock),
            None => Some(Pace::new(&qos, clock)),
        };
        let piece = self.pace.as_ref().map_or(UNPACED_PIECE, Pace::piece);
        self.piece = (piece as u64).max(self.block).next_power_of_two() as usize;
        let align = [
            4096,
            ev.fs.dio_mem_align as usize,
            ev.device.logical_block as usize,
        ]
        .into_iter()
        .max()
        .unwrap_or(4096)
        .next_power_of_two();
        self.align = align;
        self.lend = header.lend;
        let pool = BufPool::new(&PoolConfig::uniform(
            align.min(self.piece),
            self.piece,
            self.piece,
            DEPTH as u32,
        ))
        .map_err(|_| not_written(NotWrittenCause::PoolExhausted, ErrorContext::default()))?;
        // One buffer to fill and the others for the writes in flight: the
        // lane never holds more than it has rows.
        let mut free = Vec::with_capacity(DEPTH);
        for _ in 0..DEPTH {
            free.push(pool.take(self.piece).map_err(|_| {
                not_written(NotWrittenCause::PoolExhausted, ErrorContext::default())
            })?);
        }
        self.stage = free.pop();
        self.pool = Some(pool);
        let queue = self
            .platform
            .queue(QueueConfig {
                depth: DEPTH as u32,
                // Several writes in flight before a wait (the Linux
                // backend otherwise performs each inside `submit`), on
                // the sink's one thread.
                parallel: true,
                pool_threads: self.opts.store.t3_pool_threads,
                owner: Owner::Thread,
            })
            .map_err(|raw| io(Op::Queue, raw))?;
        let writes = Rc::new(RefCell::new(Writes {
            lane: owned_lane(queue, &Arc::new(LaneMark::new()), DEPTH),
            pending: VecDeque::with_capacity(DEPTH),
            free,
            done_to: 0,
            failed: None,
            wait_failed: None,
            next_tag: 0,
        }));
        if let Some(p) = &self.pace {
            let part: Rc<dyn Collect> = writes.clone();
            let weak: Weak<dyn Collect> = Rc::downgrade(&part);
            self.registered = Some(p.register(weak));
        }
        self.writes = Some(writes);
        self.info = Some(info);
        self.header = Some(header.clone());
        Ok(())
    }

    fn region_run(&mut self, info: &RegionInfo) -> Result<(), Error> {
        let Some(header) = self.header.as_ref() else {
            return Err(refusal());
        };
        let Some(charged) = self.admits(info, header) else {
            return Err(refusal());
        };
        let Some(record) = info.to_record() else {
            return Err(refusal());
        };
        let name = file_name(&data_name(self.index))?;
        let file = self
            .platform
            .create_file(&self.dir, &name)
            .map_err(|raw| io(Op::Create, raw))?;
        // Its blocks allocated before its writes (D-171), so a device
        // without the room fails here, before any byte is written: on ext4
        // a direct write that extends a file is serialised, so allocation
        // lets the writes in flight overlap (measured: 40% of G21's
        // baseline without it, about 80% with it). On NTFS it does not:
        // allocation moves the end of file but not the valid data length
        // (`SetFileValidData` is never called: it needs a privilege and
        // exposes stale clusters), and each overlapped write past it still
        // blocks inside `WriteFile` before it returns pending. The file is
        // cut to the region's length at its end.
        let len = info.stream_len().div_ceil(self.block) * self.block;
        if len > 0 {
            self.platform
                .allocate(&file, len)
                .map_err(|raw| io(Op::Allocate, raw))?;
        }
        self.charged = charged;
        self.file = Some(file);
        self.cur = Some(record);
        self.start_file();
        self.span_crc = 0;
        self.span_len = 0;
        self.spans = Vec::new();
        Ok(())
    }

    fn end_run(&mut self, declared: u64) -> Result<(), Error> {
        self.drain_stage(false)?;
        self.settle_writes()?;
        if self.span_len > 0 {
            self.spans.push(self.span_crc);
            self.span_len = 0;
        }
        let (Some(file), Some(record)) = (self.file.take(), self.cur.take()) else {
            return Err(refusal());
        };
        self.settle_file(&file, declared)?;
        // The region counts as exported once its name is durable too.
        self.sync_dir()?;
        self.done.push((record, core::mem::take(&mut self.spans)));
        self.index += 1;
        Ok(())
    }

    fn finish_run(&mut self) -> Result<(), Error> {
        let Some(header) = self.header.clone() else {
            return Err(refusal());
        };
        if self.index != header.region_count || self.charged > header.total {
            return Err(refusal());
        }
        let facts = &header.facts;
        let omitted: Vec<u64> = facts.omitted.iter().map(|id| id.get()).collect();
        let mut verified = Vec::with_capacity(facts.verified_up_to.len());
        for (name, len) in &facts.verified_up_to {
            verified.push((RegionName::new(name).map_err(|_| refusal())?, *len));
        }
        verified.sort_by_key(|(n, _)| n.as_padded());
        let mut omitted_sorted = omitted;
        omitted_sorted.sort_unstable();
        let regions: Vec<exportinfo::RegionItem<'_>> =
            self.done.iter().map(|(r, s)| (*r, s.as_slice())).collect();
        let h = InfoHeader {
            writable: header.writable,
            block_log2: header.block_size.trailing_zeros() as u8,
            region_count: header.region_count,
            lend: u32::try_from(header.lend).unwrap_or(u32::MAX),
            total: header.total,
            omitted_count: 0,
            lost_count: 0,
            stale_count: 0,
            verified_count: 0,
        };
        let len = exportinfo::encoded_len(
            omitted_sorted.len(),
            facts.lost_pages.len(),
            facts.stale_pages.len(),
            verified.len(),
            &regions,
        )
        .ok_or_else(refusal)?;
        let mut bytes = vec![0u8; len];
        let n = exportinfo::encode(
            &h,
            &omitted_sorted,
            &facts.lost_pages,
            &facts.stale_pages,
            &verified,
            &regions,
            &mut bytes,
        )
        .map_err(|_| refusal())?;
        drop(regions);
        self.start_file();
        self.stage_bytes(&bytes[..n], true)?;
        self.drain_stage(true)?;
        self.settle_writes()?;
        let Some(info) = self.info.take() else {
            return Err(refusal());
        };
        self.settle_file(&info, n as u64)?;
        self.sync_dir()
    }
}

impl<P: Platform> Drop for FileSink<P> {
    /// Collects every write still in flight before the files they write
    /// and their buffers' pool go (best effort: a failed wait leaves them
    /// with the queue, which keeps their buffers).
    fn drop(&mut self) {
        if let Some(w) = &self.writes {
            if let Ok(mut ws) = w.try_borrow_mut() {
                let _waited = ws.wait_all();
            }
        }
    }
}

impl<P: Platform> ExportSink for FileSink<P> {
    fn begin(&mut self, header: &ExportHeader) -> Result<(), SinkError> {
        if self.state != State::Fresh {
            return Err(self.refuse(0));
        }
        match self.begin_run(header) {
            Ok(()) => {
                self.state = State::Begun;
                Ok(())
            }
            Err(e) => Err(self.fail(e, 0)),
        }
    }

    fn region(&mut self, info: &RegionInfo) -> Result<(), SinkError> {
        if !matches!(self.state, State::Begun | State::Ended) {
            return Err(self.refuse(0));
        }
        match self.region_run(info) {
            Ok(()) => {
                self.state = State::InRegion {
                    declared: info.stream_len(),
                    written: 0,
                };
                Ok(())
            }
            Err(e) => Err(self.fail(e, 0)),
        }
    }

    fn data(&mut self, chunk: crate::stream::Chunk) -> Result<(), SinkError> {
        let State::InRegion { declared, written } = self.state else {
            return Err(self.refuse(0));
        };
        let n = chunk.bytes().len() as u64;
        // At the region's next byte, within its declared length.
        if chunk.offset() != written || n > declared - written {
            return Err(self.refuse(written));
        }
        self.crc_bytes(chunk.bytes());
        let staged = self.accept(chunk);
        match staged {
            Ok(()) => {
                // Its writes done without a gap (the bytes staged or in
                // flight count none).
                if let Some(c) = &self.call {
                    c.wrote(self.id, self.written().min(declared));
                }
                self.state = State::InRegion {
                    declared,
                    written: written + n,
                };
                Ok(())
            }
            // The failed write's start: every write before it done.
            Err(e) => {
                let at = self.failed_at();
                if let Some(c) = &self.call {
                    c.wrote(self.id, at.min(declared));
                }
                Err(self.fail(e, at))
            }
        }
    }

    /// Waits until a write from a chunk's own buffer completes (one of the
    /// stream's buffers back to it): the export calls this when its stream
    /// needs a buffer and every one is lent.
    fn settle(&mut self) -> Result<(), SinkError> {
        let State::InRegion { declared, .. } = self.state else {
            return Ok(());
        };
        let Some(w) = self.writes.clone() else {
            return Ok(());
        };
        let before = w.borrow().chunks();
        let mut waited = Ok(());
        while before > 0 && waited.is_ok() {
            let mut ws = w.borrow_mut();
            ws.reap();
            if ws.chunks() < before {
                break;
            }
            match ws.wait_one() {
                Ok(true) => {}
                Ok(false) => break,
                Err(raw) => {
                    waited = Err(io(Op::Write, raw));
                    break;
                }
            }
        }
        match waited.and_then(|()| self.failure_of(&w)) {
            Ok(()) => Ok(()),
            Err(e) => {
                let at = self.failed_at();
                if let Some(c) = &self.call {
                    c.wrote(self.id, at.min(declared));
                }
                Err(self.fail(e, at))
            }
        }
    }

    fn end_region(&mut self) -> Result<(), SinkError> {
        let State::InRegion { declared, written } = self.state else {
            return Err(self.refuse(0));
        };
        if written != declared {
            return Err(self.refuse(written));
        }
        // Where its writes done reach now: a failure from here that is not
        // a write's (the last piece's admission, the file's cut, flush or
        // its directory's sync) is reported there, never past it (StoreSink
        // reports such a failure the same way, D-170).
        let before = self.written().min(declared);
        match self.end_run(declared) {
            Ok(()) => {
                self.state = State::Ended;
                if let Some(c) = &self.call {
                    c.region_ended(self.id);
                }
                Ok(())
            }
            Err(e) => {
                // A write that failed: where it began, every write before
                // it done.
                let at = if self.writes_failed() {
                    self.failed_at().min(declared)
                } else {
                    before
                };
                if let Some(c) = &self.call {
                    c.wrote(self.id, at);
                }
                Err(self.fail(e, at))
            }
        }
    }

    fn finish(&mut self) -> Result<(), SinkError> {
        if !matches!(self.state, State::Begun | State::Ended) {
            return Err(self.refuse(0));
        }
        match self.finish_run() {
            Ok(()) => {
                self.state = State::Finished;
                if let Some(c) = &self.call {
                    c.sink_finished();
                }
                Ok(())
            }
            Err(e) => Err(self.fail(e, 0)),
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, unused_results)]

    use std::path::Path;
    use std::sync::Arc;

    use store_io_format::meta::FillPattern;
    use store_io_sim::{SimConfig, SimPlatform};

    use super::*;
    use crate::Store;
    use crate::export::VolumeFacts;

    /// What a test region is: a page region of its bytes, an append
    /// region's durable prefix of them (exported padded to whole blocks),
    /// or a slot's committed object.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Kind {
        Page,
        Append,
        Slot,
    }

    /// A store of `regions` (each of `len` patterned bytes, of its kind) on
    /// the simulator's chunk of 128 KiB.
    fn zero_copy_store(
        seed: u64,
        regions: &[(u64, Kind)],
    ) -> (SimPlatform, Store<SimPlatform>, Vec<Vec<u8>>) {
        let p = SimPlatform::new(SimConfig::volatile(seed));
        let o = StoreOptions {
            queues: 2,
            buffers_per_class: 64,
            max_io: 1 << 17,
            ..StoreOptions::default()
        };
        let s = Store::create(p.clone(), Path::new("/db"), o).unwrap();
        let mut datas = Vec::new();
        for (i, &(len, kind)) in regions.iter().enumerate() {
            let data: Vec<u8> = (0..len)
                .map(|j| (j as u8) ^ (j >> 9) as u8 ^ (i as u8).wrapping_mul(37))
                .collect();
            if kind == Kind::Slot {
                let _d = s
                    .provision_slot(&format!("s{i}"))
                    .unwrap()
                    .commit(&data)
                    .unwrap();
            } else if kind == Kind::Append {
                let r = s
                    .provision_append_region(
                        &format!("a{i}"),
                        len.next_multiple_of(4096) + (1 << 20),
                    )
                    .unwrap();
                let t = r.append(&data).unwrap();
                let _d = r.sync_through(&t).unwrap();
            } else {
                let r = s.provision_page_region(&format!("p{i}"), len).unwrap();
                let _d = r.write_durable(r.pos(0).unwrap(), &data).unwrap();
            }
            datas.push(data);
        }
        drop(p.open_dir(Path::new("/out"), true).unwrap());
        (p, s, datas)
    }

    /// Exports `s` into a new sink with the stream `depth` (`None`: the
    /// default), and checks each region's data file holds its bytes (an
    /// append region's then zeros to its last block): the report, and the
    /// writes made from chunks' own buffers.
    fn export_checked(
        p: &SimPlatform,
        s: &Store<SimPlatform>,
        datas: &[Vec<u8>],
        depth: Option<usize>,
    ) -> (crate::export::ExportReport, u64) {
        let mut sink =
            FileSink::create(p.clone(), Path::new("/out"), FileSinkOptions::default()).unwrap();
        let o = crate::export::ExportOptions {
            depth,
            ..crate::export::ExportOptions::default()
        };
        let report = s.export_with(&mut sink, o).unwrap();
        let direct = sink.direct_writes();
        drop(sink);
        for (i, data) in datas.iter().enumerate() {
            let got = p
                .with_world(|w| {
                    w.media_of(&std::path::PathBuf::from("/out"), &data_name(i as u32))
                        .map(<[u8]>::to_vec)
                })
                .unwrap();
            let whole = got.len() >= data.len()
                && got[..data.len()] == data[..]
                && got[data.len()..].iter().all(|&b| b == 0)
                && (got.len() as u64).div_ceil(4096) == (data.len() as u64).div_ceil(4096);
            assert!(
                whole,
                "region {i}'s file ({} of {} bytes)",
                got.len(),
                data.len()
            );
        }
        (report, direct)
    }

    /// The stream's whole chunks are written from their own buffers (D-175:
    /// no staging copy), every byte where it belongs.
    #[test]
    fn test_whole_chunks_are_written_from_their_own_buffers() {
        let (p, s, datas) = zero_copy_store(900, &[(1 << 20, Kind::Page), (3 << 17, Kind::Page)]);
        let (report, direct) = export_checked(&p, &s, &datas, None);
        assert!(report.complete);
        // 8 and 3 chunks of 128 KiB: every one from its own buffer.
        assert_eq!(direct, 11);
    }

    /// Bytes that are not whole blocks (a slot's object) are staged
    /// (padded, then the file cut to its length); a region after them is
    /// written from its buffers again. An append region's durable prefix
    /// is exported in whole blocks, so every one of its chunks is.
    #[test]
    fn test_a_slot_s_object_is_staged_and_the_next_region_goes_on_without_copies() {
        let regions = [
            ((1 << 20) + 1000, Kind::Append),
            (1000, Kind::Slot),
            (1 << 19, Kind::Page),
        ];
        let (p, s, datas) = zero_copy_store(901, &regions);
        let (report, direct) = export_checked(&p, &s, &datas, None);
        assert!(report.complete);
        // The append region's 8 chunks of 128 KiB and its last block, the
        // slot's object staged, the page region's 4 chunks.
        assert_eq!(direct, 13);
    }

    /// A stream lending one chunk at a time is staged whole (no write may
    /// hold the stream's only buffer); one lending two writes from chunks
    /// (one out at a time, a chunk it cannot spare staged); both complete
    /// and exact.
    #[test]
    fn test_lend_one_stages_everything_and_lend_two_writes_from_chunks() {
        let (p, s, datas) = zero_copy_store(902, &[(1 << 20, Kind::Page)]);
        let (report, direct) = export_checked(&p, &s, &datas, Some(1));
        assert!(report.complete);
        assert_eq!(direct, 0);
        let (p, s, datas) = zero_copy_store(903, &[(1 << 20, Kind::Page)]);
        let (report, direct) = export_checked(&p, &s, &datas, Some(2));
        assert!(report.complete);
        assert!((1..=8).contains(&direct), "{direct}");
    }

    /// A wait failing at any point of an export with writes held in
    /// flight: whatever the export's end, once it and its sink are gone
    /// every background buffer of the source store is back with its pool
    /// (a chunk whose write's buffer stayed with a lost queue is counted
    /// lost to its stream exactly once: a second count would trip the
    /// stream's debug assertion).
    #[test]
    fn test_a_failed_wait_anywhere_leaves_no_background_buffer_held() {
        let mut failed = 0;
        for k in 1..=24 {
            let (p, s, _datas) = zero_copy_store(
                904,
                &[
                    (1 << 20, Kind::Page),
                    (1 << 19, Kind::Append),
                    (1000, Kind::Slot),
                ],
            );
            let mut sink =
                FileSink::create(p.clone(), Path::new("/out"), FileSinkOptions::default()).unwrap();
            p.with_world(|w| {
                w.hold_reaps(1 << 40);
                let next = w.waits() + k;
                w.faults_mut().fail_wait = Some((next, 5));
            });
            failed += u32::from(s.export(&mut sink).is_err());
            drop(sink);
            p.with_world(|w| {
                w.hold_reaps(0);
                w.faults_mut().fail_wait = None;
            });
            for (size, held, _cap) in s.inner.pool.background_holdings() {
                assert_eq!(
                    held, 0,
                    "wait {k}: {held} background buffers of {size} B held"
                );
            }
        }
        assert!(failed > 0, "no wait failed");
    }

    /// A wait failing with writes from chunks in flight (commits quiet,
    /// reaps held): the sink fails; once it drops, its records of those
    /// writes go and the stream counts each chunk no longer lent, once.
    #[test]
    fn test_a_lost_write_s_chunk_is_counted_lost_once_the_sink_drops() {
        let (p, _s, log, stream) = by_hand(908);
        let data = vec![9u8; 3 * 4096];
        let mut sink = begun_by_hand(&p, stream.lend(), data.len() as u64);
        p.with_world(|w| {
            w.advance_clock_ns(10_000_000_000);
            w.hold_reaps(1 << 40);
        });
        for i in 0..3 {
            let at = i * 4096;
            let c = stream
                .lend_bytes(log.state(), at as u64, &data[at..at + 4096])
                .unwrap();
            sink.data(c).unwrap();
        }
        let out = sink.writes.as_ref().unwrap().borrow().chunks();
        assert_eq!(out, 3, "writes from chunks in flight");
        assert_eq!(stream.lent(), 3);
        p.with_world(|w| {
            let next = w.waits() + 1;
            w.faults_mut().fail_wait = Some((next, 5));
        });
        assert!(sink.settle().is_err());
        drop(sink);
        assert_eq!(stream.lent(), 0, "each chunk counted lost");
    }

    /// A store with an append region, an empty `/out`, and an export
    /// stream to lend chunks from by hand (`lend_bytes`).
    fn by_hand(
        seed: u64,
    ) -> (
        SimPlatform,
        Store<SimPlatform>,
        crate::AppendRegion<SimPlatform>,
        crate::stream::ReadStream<SimPlatform>,
    ) {
        let p = SimPlatform::new(SimConfig::volatile(seed));
        let s = Store::create(p.clone(), Path::new("/db"), StoreOptions::default()).unwrap();
        let log = s.provision_append_region("log", 1 << 20).unwrap();
        drop(p.open_dir(Path::new("/out"), true).unwrap());
        let stream = s.export_stream(None, None).unwrap();
        (p, s, log, stream)
    }

    /// A sink begun by hand for one append region whose stream length is
    /// `len` bytes.
    fn begun_by_hand(p: &SimPlatform, lend: usize, len: u64) -> FileSink<SimPlatform> {
        let mut sink =
            FileSink::create(p.clone(), Path::new("/out"), FileSinkOptions::default()).unwrap();
        let header = ExportHeader {
            region_count: 1,
            block_size: 4096,
            facts: VolumeFacts::default(),
            writable: true,
            lend,
            total: 1 << 30,
        };
        sink.begin(&header).unwrap();
        let info = RegionInfo {
            name: "log".to_owned(),
            kind: RegionKind::Append,
            generation: 1,
            tag: 0,
            cap: None,
            fill: FillPattern::Zeros,
            fill_key: [0; 16],
            block_size: 4096,
            data_size: len.next_multiple_of(4096),
            durable_len: Some(len),
            object_len: None,
            full_copy: false,
            superseded: false,
        };
        sink.region(&info).unwrap();
        sink
    }

    fn out_file(p: &SimPlatform) -> Vec<u8> {
        p.with_world(|w| {
            w.media_of(&std::path::PathBuf::from("/out"), &data_name(0))
                .map(<[u8]>::to_vec)
        })
        .unwrap()
    }

    /// Bytes that are not whole blocks, staged, then a chunk that could be
    /// written from its own buffer: it is staged behind them (the stage
    /// does not end on a block), so the file holds every byte in order.
    #[test]
    fn test_a_whole_chunk_after_a_partial_stage_is_staged_behind_it() {
        let (p, _s, log, stream) = by_hand(905);
        let data: Vec<u8> = (0..5096u32).map(|i| (i % 251) as u8).collect();
        let mut sink = begun_by_hand(&p, stream.lend(), data.len() as u64);
        let first = stream.lend_bytes(log.state(), 0, &data[..1000]).unwrap();
        sink.data(first).unwrap();
        let second = stream.lend_bytes(log.state(), 1000, &data[1000..]).unwrap();
        sink.data(second).unwrap();
        sink.end_region().unwrap();
        sink.finish().unwrap();
        assert_eq!(sink.direct_writes(), 0);
        assert!(out_file(&p) == data);
    }

    /// `settle` waits until a write from a chunk's own buffer completes:
    /// one of the stream's buffers is back when it returns.
    #[test]
    fn test_settle_waits_for_a_write_from_a_chunk() {
        let (p, _s, log, stream) = by_hand(906);
        let data: Vec<u8> = (0..3 * 4096u32).map(|i| (i % 241) as u8).collect();
        let mut sink = begun_by_hand(&p, stream.lend(), data.len() as u64);
        p.with_world(|w| w.hold_reaps(1 << 40));
        for i in 0..3 {
            let at = i * 4096;
            let c = stream
                .lend_bytes(log.state(), at as u64, &data[at..at + 4096])
                .unwrap();
            sink.data(c).unwrap();
        }
        let chunks = |sink: &FileSink<SimPlatform>| sink.writes.as_ref().unwrap().borrow().chunks();
        let before = chunks(&sink);
        assert!(before >= 1, "writes from chunks in flight");
        sink.settle().unwrap();
        assert!(chunks(&sink) < before, "one back");
        p.with_world(|w| w.hold_reaps(0));
        sink.end_region().unwrap();
        sink.finish().unwrap();
        assert_eq!(sink.direct_writes(), 3);
        assert!(out_file(&p) == data);
    }

    /// With commits quiet and its writes held in flight, the sink never
    /// holds more than `lend - 1` of the stream's buffers in writes from
    /// chunks (one the stream cannot spare is staged): the stream always
    /// has one to lend (a sink holding them all would leave it none, and
    /// the next chunk would be refused), and every byte arrives.
    #[test]
    fn test_a_sink_holds_at_most_lend_minus_one_chunks() {
        let (p, _s, log, stream) = by_hand(907);
        let lend = stream.lend();
        let n = lend + 2;
        let data: Vec<u8> = (0..(n * 4096) as u32).map(|i| (i % 239) as u8).collect();
        let mut sink = begun_by_hand(&p, lend, data.len() as u64);
        // Commits quiet (the provisioning's long past), so the sink's
        // writes are not paced to one piece in flight; reaps held back.
        p.with_world(|w| {
            w.advance_clock_ns(10_000_000_000);
            w.hold_reaps(1 << 40);
        });
        let mut most = 0;
        for i in 0..n {
            let at = i * 4096;
            let c = stream
                .lend_bytes(log.state(), at as u64, &data[at..at + 4096])
                .unwrap();
            sink.data(c).unwrap();
            most = most.max(sink.writes.as_ref().unwrap().borrow().chunks());
        }
        assert_eq!(most, lend - 1, "lend {lend}");
        p.with_world(|w| w.hold_reaps(0));
        sink.end_region().unwrap();
        sink.finish().unwrap();
        // The chunks it could not spare staged, the rest from their buffers.
        assert!(sink.direct_writes() >= (lend - 1) as u64);
        assert!(out_file(&p) == data);
    }

    /// A sink bound to an export's call, begun, in a 1 MiB page region,
    /// with reaps held back so its writes stay in flight until a wait.
    struct InFlight {
        p: SimPlatform,
        _s: Store<SimPlatform>,
        call: Rc<CallState>,
        pace: Pace,
        sink: FileSink<SimPlatform>,
        _bound: call::Bound,
    }

    fn begun_in_flight(seed: u64) -> InFlight {
        let p = SimPlatform::new(SimConfig::volatile(seed));
        let s = Store::create(p.clone(), Path::new("/db"), StoreOptions::default()).unwrap();
        drop(p.open_dir(Path::new("/out"), true).unwrap());
        let pace = s.inner.domain.new_pace();
        let call = CallState::new(Arc::clone(&s.inner.qos), pace.clone());
        let bound = call::bind(&call);
        let mut sink =
            FileSink::create(p.clone(), Path::new("/out"), FileSinkOptions::default()).unwrap();
        let header = ExportHeader {
            region_count: 1,
            block_size: 4096,
            facts: VolumeFacts::default(),
            writable: true,
            lend: 4,
            total: 1 << 30,
        };
        sink.begin(&header).unwrap();
        let info = RegionInfo {
            name: "r".to_owned(),
            kind: RegionKind::Page,
            generation: 1,
            tag: 0,
            cap: None,
            fill: FillPattern::Zeros,
            fill_key: [0; 16],
            block_size: 4096,
            data_size: 1 << 20,
            durable_len: None,
            object_len: None,
            full_copy: false,
            superseded: false,
        };
        sink.region(&info).unwrap();
        p.with_world(|w| w.hold_reaps(1 << 40));
        InFlight {
            p,
            _s: s,
            call,
            pace: pace.unwrap(),
            sink,
            _bound: bound,
        }
    }

    /// Every piece the call may still hold, held here.
    fn hold_the_rest(pace: &Pace) -> Vec<Held> {
        let mut held = Vec::new();
        while let Some(h) = pace.try_charge() {
            held.push(h);
            assert!(held.len() < 1024, "an admission that never refuses");
        }
        held
    }

    /// A wait for the sink's own writes failing inside its admission's
    /// collection is the sink's failure, recorded with its offset, never
    /// taken for another part's.
    #[test]
    fn test_a_failed_wait_inside_an_admission_is_the_sink_s_failure() {
        let mut t = begun_in_flight(3120);
        let piece = t.sink.piece;
        t.sink.stage_bytes(&vec![7u8; 2 * piece], false).unwrap();
        assert_eq!(t.sink.writes.as_ref().unwrap().borrow().pending.len(), 2);
        let held = hold_the_rest(&t.pace);
        t.p.with_world(|w| {
            let n = w.waits();
            w.faults_mut().fail_wait = Some((n + 1, 5));
        });
        let e = t.sink.stage_bytes(&vec![7u8; piece], false).unwrap_err();
        drop(held);
        assert!(matches!(e, Error::Io { op: Op::Write, .. }), "{e:?}");
        assert!(!t.sink.admit_failed);
        let at = t.sink.failed_at();
        let _shown = t.sink.fail(e, at);
        let f = t.call.failure().expect("recorded as the sink's");
        assert!(matches!(f.error, Error::Io { op: Op::Write, .. }));
        assert_eq!(f.offset, 0);
    }

    /// A write that failed, collected by the admission of the next one:
    /// that failure is returned before anything more is submitted (here the
    /// next submission would be refused with another code).
    #[test]
    fn test_a_write_failure_collected_by_an_admission_is_returned_first() {
        let mut t = begun_in_flight(3122);
        let piece = t.sink.piece;
        t.p.with_world(|w| {
            let n = w.writes();
            w.faults_mut().fail_write = Some(n + 1);
        });
        t.sink.stage_bytes(&vec![7u8; 2 * piece], false).unwrap();
        let held = hold_the_rest(&t.pace);
        t.p.with_world(|w| {
            let n = w.write_submits();
            w.faults_mut().fail_submit = Some((n + 1, 28));
        });
        let e = t.sink.stage_bytes(&vec![7u8; piece], false).unwrap_err();
        drop(held);
        match e {
            Error::Io {
                op: Op::Write, raw, ..
            } => assert_eq!(raw.code, 5, "{e:?}"),
            _ => panic!("{e:?}"),
        }
        assert_eq!(t.sink.failed_at(), 0);
    }

    /// A failed sink collects its writes in flight at once: it holds no
    /// piece of the target's group afterwards.
    #[test]
    fn test_a_failed_sink_holds_no_piece() {
        let mut t = begun_in_flight(3124);
        let piece = t.sink.piece;
        t.sink.stage_bytes(&vec![7u8; 2 * piece], false).unwrap();
        assert_eq!(t.pace.held_here(), 2);
        let _shown = t.sink.refuse(0);
        assert_eq!(t.pace.held_here(), 0);
        assert!(t.sink.writes.as_ref().unwrap().borrow().pending.is_empty());
    }

    /// A sink dropped with writes in flight completes them first.
    #[test]
    fn test_a_dropped_sink_completes_its_writes_in_flight() {
        let mut t = begun_in_flight(3126);
        let piece = t.sink.piece;
        t.sink.stage_bytes(&vec![7u8; 2 * piece], false).unwrap();
        let before = t.p.with_world(|w| w.writes());
        let InFlight { p, sink, .. } = t;
        drop(sink);
        assert_eq!(p.with_world(|w| w.writes()), before + 2);
    }

    /// Three writes in flight, the second failing: the writes done reach
    /// only the first's end and the failure is the second's start, though
    /// the third completed after it (`done_to` never moves past a failure).
    #[test]
    fn test_a_failed_write_stops_the_writes_done_at_its_start() {
        let p = SimPlatform::new(SimConfig::volatile(3110));
        let d = p.open_dir(Path::new("/w"), true).unwrap();
        let f = p.create_file(&d, &FileName::new("f").unwrap()).unwrap();
        let q = p
            .queue(QueueConfig {
                depth: DEPTH as u32,
                ..QueueConfig::default()
            })
            .unwrap();
        let pool = BufPool::new(&PoolConfig::uniform(4096, 4096, 4096, 4)).unwrap();
        let mut w = Writes {
            lane: owned_lane(q, &Arc::new(LaneMark::new()), DEPTH),
            pending: VecDeque::new(),
            free: Vec::new(),
            done_to: 0,
            failed: None,
            wait_failed: None,
            next_tag: 0,
        };
        p.with_world(|w| {
            let n = w.writes();
            w.faults_mut().fail_write = Some(n + 2);
        });
        for i in 0..3u64 {
            let mut b = pool.take(4096).unwrap();
            assert!(b.set_len(4096));
            w.submit(&f, i * 4096, b, None, None).unwrap();
        }
        w.wait_all().unwrap();
        assert_eq!(w.done_to, 4096);
        assert!(matches!(
            w.failed,
            Some((Error::Io { op: Op::Write, .. }, 4096))
        ));
        assert!(w.pending.is_empty());
        assert_eq!(w.free.len(), 3);
    }

    /// An admission that ends without one (here: every piece of the call
    /// held on this thread, which no collector can collect) fails the sink
    /// but is not recorded as the sink's failure: the part whose pieces
    /// they are reports its own, and the export asks it.
    #[test]
    fn test_an_admission_ended_by_another_part_is_not_the_sink_s_failure() {
        let p = SimPlatform::new(SimConfig::volatile(3100));
        let s = Store::create(p.clone(), Path::new("/db"), StoreOptions::default()).unwrap();
        drop(p.open_dir(Path::new("/out"), true).unwrap());
        let pace = s.inner.domain.new_pace();
        let call = CallState::new(Arc::clone(&s.inner.qos), pace.clone());
        let _bound = call::bind(&call);
        let mut sink =
            FileSink::create(p.clone(), Path::new("/out"), FileSinkOptions::default()).unwrap();
        let header = ExportHeader {
            region_count: 1,
            block_size: 4096,
            facts: VolumeFacts::default(),
            writable: true,
            lend: 4,
            total: 1 << 30,
        };
        sink.begin(&header).unwrap();
        let info = RegionInfo {
            name: "r".to_owned(),
            kind: RegionKind::Page,
            generation: 1,
            tag: 0,
            cap: None,
            fill: FillPattern::Zeros,
            fill_key: [0; 16],
            block_size: 4096,
            data_size: 1 << 20,
            durable_len: None,
            object_len: None,
            full_copy: false,
            superseded: false,
        };
        sink.region(&info).unwrap();
        // Every piece the call may hold, held here (another part's).
        let pace = pace.unwrap();
        let mut held = Vec::new();
        while let Some(h) = pace.try_charge() {
            held.push(h);
            assert!(held.len() < 1024, "an admission that never refuses");
        }
        let piece = sink.piece;
        let e = sink.stage_bytes(&vec![7u8; piece], false).unwrap_err();
        assert!(sink.admit_failed);
        let at = sink.at;
        let _shown = sink.fail(e, at);
        assert_eq!(sink.state, State::Failed);
        assert!(call.failure().is_none(), "recorded as the sink's");
        // A failure of the sink's own is recorded.
        sink.admit_failed = false;
        let _shown = sink.fail(refusal(), 0);
        assert!(call.failure().is_some());
    }
}
