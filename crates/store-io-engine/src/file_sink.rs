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
//! and the target file system's, through one staging buffer: each write
//! is admitted by the call's record on the target's device group and
//! completed before the call that made it returns, so the sink holds no
//! piece between calls (it registers no collector: its waits collect the
//! export stream's reads, R5.4). A region's last write is padded, then the
//! file cut to its length, flushed, and its directory synced: a region
//! counts as written only then. A failed write is reported to the export's
//! call with where it began, so the export's report never counts bytes the
//! sink did not write.

use std::marker::PhantomData;
use std::path::Path;
use std::rc::Rc;

use store_io_buf::{BufPool, IoBuf, PoolConfig};
use store_io_core::error::{Error, ErrorContext, Named, NotWrittenCause, Op, OsError, ReserveTag};
use store_io_format::crc32c::crc32c_append;
use store_io_format::exportinfo::{self, InfoHeader, RegionRecord, SPAN};
use store_io_format::meta::{RegionKind, RegionName};
use store_io_format::meta2::max_object;
use store_io_platform::{FileName, IoOp, Platform, QueueConfig};

use crate::call::{self, CallState};
use crate::exec::{QueueSet, Ran, run};
use crate::export::{ExportHeader, ExportSink, RegionInfo, SinkError};
use crate::io::not_written;
use crate::pace::Pace;
use crate::store::StoreOptions;

/// The record that makes a [`FileSink`]'s directory restorable.
pub const REGIONS_INFO: &str = "regions.info";

/// The largest staging write when the target is not paced.
const UNPACED_PIECE: usize = 256 << 10;

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
    queue: Option<QueueSet<P::Queue>>,
    pool: Option<BufPool>,
    /// The write block and the staging buffer's size (a multiple of it).
    block: u64,
    piece: usize,
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
            queue: None,
            pool: None,
            block: 0,
            piece: 0,
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

    /// Writes the staging buffer's first `len` bytes (whole blocks) at the
    /// current offset of `which` (the region's file, or `regions.info`),
    /// admitted by the call's record and completed before it returns.
    fn write_stage(&mut self, len: usize, info_file: bool) -> Result<(), Error> {
        let file = if info_file { &self.info } else { &self.file };
        let (Some(file), Some(queue)) = (file.as_ref(), self.queue.as_ref()) else {
            return Err(refusal());
        };
        // Admitted first (its wait collects the export stream's reads), so
        // a refusal keeps the staging buffer.
        let _held = match &self.pace {
            Some(p) => match p.admit() {
                Ok(h) => Some(h),
                Err(e) => {
                    self.admit_failed = true;
                    return Err(e);
                }
            },
            None => None,
        };
        let Some(mut buf) = self.stage.take() else {
            return Err(refusal());
        };
        if !buf.set_len(len) {
            self.stage = Some(buf);
            return Err(refusal());
        }
        let mut lane = queue.get(0);
        let ran = run(
            &mut lane,
            IoOp::Write {
                file,
                offset: self.at,
                buf,
                dsync: false,
            },
        );
        drop(lane);
        let (buf, result) = match ran {
            Ok(Ran::Done { buf, result }) => (buf, result.map_err(|raw| io(Op::Write, raw))),
            Ok(Ran::Abandoned(raw)) => (None, Err(io(Op::Write, raw))),
            // Refused before it reached the device: a full queue (none is
            // in flight here) or the OS's refusal.
            Err((buf, raw)) => (
                buf,
                Err(raw.map_or_else(
                    || not_written(NotWrittenCause::QueueFull, ErrorContext::default()),
                    |r| io(Op::Write, r),
                )),
            ),
        };
        // A buffer the queue kept (abandoned) is replaced from the pool.
        self.stage = match buf {
            Some(b) => Some(b),
            None => self.pool.as_ref().and_then(|p| p.take(self.piece).ok()),
        };
        match result {
            Ok(n) if n == len => {
                self.at += len as u64;
                Ok(())
            }
            Ok(_) => Err(not_written(
                NotWrittenCause::Interrupted,
                ErrorContext::default(),
            )),
            Err(e) => Err(e),
        }
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
        let pool = BufPool::new(&PoolConfig::uniform(
            align.min(self.piece),
            self.piece,
            self.piece,
            2,
        ))
        .map_err(|_| not_written(NotWrittenCause::PoolExhausted, ErrorContext::default()))?;
        self.stage =
            Some(pool.take(self.piece).map_err(|_| {
                not_written(NotWrittenCause::PoolExhausted, ErrorContext::default())
            })?);
        self.pool = Some(pool);
        let queue = self
            .platform
            .queue(QueueConfig {
                depth: 1,
                ..QueueConfig::default()
            })
            .map_err(|raw| io(Op::Queue, raw))?;
        self.queue = Some(QueueSet::new(vec![queue], 1));
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
        self.charged = charged;
        self.file = Some(file);
        self.cur = Some(record);
        self.fill = 0;
        self.at = 0;
        self.span_crc = 0;
        self.span_len = 0;
        self.spans = Vec::new();
        Ok(())
    }

    fn end_run(&mut self, declared: u64) -> Result<(), Error> {
        self.drain_stage(false)?;
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
        self.fill = 0;
        self.at = 0;
        self.stage_bytes(&bytes[..n], true)?;
        self.drain_stage(true)?;
        let Some(info) = self.info.take() else {
            return Err(refusal());
        };
        self.settle_file(&info, n as u64)?;
        self.sync_dir()
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
        let bytes = chunk.bytes();
        let n = bytes.len() as u64;
        // At the region's next byte, within its declared length.
        if chunk.offset() != written || n > declared - written {
            return Err(self.refuse(written));
        }
        self.crc_bytes(bytes);
        let staged = self.stage_bytes(bytes, false);
        // Its writes done (the staged bytes not yet written count none).
        if let Some(c) = &self.call {
            c.wrote(self.id, self.at.min(declared));
        }
        match staged {
            Ok(()) => {
                self.state = State::InRegion {
                    declared,
                    written: written + n,
                };
                Ok(())
            }
            // The failed write's start (`at` moves only past a write done).
            Err(e) => Err(self.fail(e, self.at)),
        }
    }

    fn end_region(&mut self) -> Result<(), SinkError> {
        let State::InRegion { declared, written } = self.state else {
            return Err(self.refuse(0));
        };
        if written != declared {
            return Err(self.refuse(written));
        }
        match self.end_run(declared) {
            Ok(()) => {
                self.state = State::Ended;
                if let Some(c) = &self.call {
                    c.region_ended(self.id);
                }
                Ok(())
            }
            Err(e) => Err(self.fail(e, self.at)),
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
