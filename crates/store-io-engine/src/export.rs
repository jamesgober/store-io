//! Streaming export (PLANNING R5.6, D-148): a store's regions, their
//! attributes and their bytes, pushed in order to a caller's [`ExportSink`],
//! for backup, restore and replication shipping. How the stream travels is
//! the caller's: store-io does not frame it.
//!
//! **One plan, then each region at its turn.** Export shares `copy_store`'s
//! plan (`copy.rs`): every refusal and every volume fact is decided once,
//! before the sink is called, so the two cannot drift. Each region is then
//! looked up again at its turn: it must still be the planned (id,
//! generation), else the export fails (`StaleGeneration` if recycled,
//! `NotReady` if released), the region named. Sizes are fixed at the plan:
//! a page region grown since is exported at its planned size, a positioned
//! append region's durable prefix is clamped to its planned data size, and
//! a slot's object is read at its turn and refused `TooLarge` as the plan
//! refuses one.
//!
//! **What each kind exports.** A positioned append region its durable
//! prefix; an unpositioned one (any reopened store) its whole data area, as
//! read, with no durable length; a page region its data area, as read (a
//! writable store's pages may hold bytes not yet durable: the header says
//! the store was writable); a slot its committed object, as one chunk; a
//! region of a stale page (read-only opens only) its whole capacity and the
//! extra extent its header lists, by container offset.
//!
//! **Bytes.** One background read stream for the whole call, paced by the
//! call's record on the store's device group, its buffers taken before the
//! sink's `begin` (so the header carries its `lend`), each region added to
//! it at its turn. Chunks are owned: a sink may hold up to `lend - 1` of
//! them and drop them later. When the stream needs a buffer and every one
//! is lent, the sink's `settle` is called once; if it still holds them all,
//! the export fails `TooManyInFlight`. A media error fails the export at
//! once (a backup never has a silent hole).
//!
//! **What the report says.** The offset reached in the region being
//! exported counts only bytes the sink's `data` accepted; a region counts
//! as exported only once `end_region` returned `Ok`; the export is
//! `complete` only once `finish` returned `Ok`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use store_io_core::error::{Error, ErrorContext, NotWrittenCause, Op, OsError};
use store_io_core::id::RegionId;
use store_io_format::meta::{FillPattern, RegionKind, RegionName};
use store_io_format::meta2::max_object;
use store_io_platform::Platform;

use crate::copy::{CopyPlan, Facts};
use crate::io::{ctx_range, not_written};
use crate::lifecycle::retired_error;
use crate::slot_handle::committed_object;
use crate::store::{LiveRegion, Store, StoreOptions};
use crate::stream::{Chunk, Locator, ReadStream};

/// How [`Store::export_with`] exports.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ExportOptions {
    /// The regions to export, by name, each once (`None`: every region, in
    /// the store's order). A subset is exported in the store's order; the
    /// volume-wide refusals (a frozen store, lost or stale pages,
    /// unavailable entries) stand whatever it names.
    pub regions: Option<Vec<String>>,
    /// The export stream's reads in flight, 1 to
    /// [`MAX_DEPTH`](crate::MAX_DEPTH) (`None`: the stream's default, as
    /// [`StreamOptions`](crate::StreamOptions)).
    pub depth: Option<usize>,
    /// Export a store with stale or lost table pages or unavailable
    /// entries as far as it can be, as
    /// [`CopyOptions::accept_stale_pages`](crate::CopyOptions::accept_stale_pages):
    /// what is left out is in the header's [`VolumeFacts`]. A region of a
    /// stale page is exported in full, and only from a read-only open.
    pub accept_stale_pages: bool,
}

/// What the plan found about the volume as a whole (the facts
/// [`CopyReport`](crate::CopyReport) carries), so a restore never hides a
/// loss.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct VolumeFacts {
    /// Entries left out (unavailable, or of a kind this version does not
    /// know), in id order.
    pub omitted: Vec<RegionId>,
    /// Table pages lost: their regions are unknown.
    pub lost_pages: Vec<u32>,
    /// Table pages stale: their regions exported in full.
    pub stale_pages: Vec<u32>,
    /// Each region exported in full, with the data size its entry records
    /// (the bytes past it are its capacity's, as read).
    pub verified_up_to: Vec<(String, u64)>,
    /// Regions possibly superseded (another region's newer footprint may
    /// cover their bytes).
    pub superseded: Vec<String>,
}

/// What a sink's [`ExportSink::begin`] is told before the first region.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExportHeader {
    /// Regions that follow, exactly.
    pub region_count: u32,
    /// The store's block size: every region's data size is a multiple.
    pub block_size: u64,
    /// The volume's facts.
    pub facts: VolumeFacts,
    /// Whether the store was open for writing: a page region's bytes are
    /// then as read, possibly not yet durable.
    pub writable: bool,
    /// Chunks the export's stream lends at once: a sink holding `lend - 1`
    /// at most never meets `TooManyInFlight` by its own counting.
    pub lend: usize,
    /// An upper bound on the bytes a restore into a new store of the
    /// default geometry takes: its metadata, two header blocks per region,
    /// each region's data size (a capped region's cap when larger), each
    /// slot at the largest object a format-2 slot holds.
    pub total: u64,
}

/// One region, as a sink's [`ExportSink::region`] is told before its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RegionInfo {
    /// Its name.
    pub name: String,
    /// Its kind.
    pub kind: RegionKind,
    /// Its generation (a format-1 slot's: 1).
    pub generation: u64,
    /// Its space tag.
    pub tag: u32,
    /// Its growth cap, if capped.
    pub cap: Option<u64>,
    /// How its blocks were filled at provisioning.
    pub fill: FillPattern,
    /// The keyed fill's key.
    pub fill_key: [u8; 16],
    /// The store's block size.
    pub block_size: u64,
    /// Its data size as planned (a region exported in full: the length of
    /// what it exports).
    pub data_size: u64,
    /// A positioned append region's durable length as exported, at most
    /// `data_size` (`None`: an unpositioned append region's whole data
    /// area, as read, or another kind).
    pub durable_len: Option<u64>,
    /// A slot's committed object's length (`None`: none committed, or
    /// another kind).
    pub object_len: Option<u64>,
    /// Exported in full (a region of a stale page).
    pub full_copy: bool,
    /// Possibly superseded.
    pub superseded: bool,
}

impl RegionInfo {
    /// The bytes its region's data in the stream holds: a slot's object's
    /// length, an append region's durable length when it has one, else its
    /// data size.
    #[must_use]
    pub fn stream_len(&self) -> u64 {
        match self.kind {
            RegionKind::Slot => self.object_len.unwrap_or(0),
            _ => self.durable_len.unwrap_or(self.data_size),
        }
    }

    /// Whether a restore into a store of blocks of `block_size` can take
    /// it as it says, checked as untrusted input: a valid name, a non-zero
    /// generation, the block size the target's (a power of two from 512 to
    /// 65,536), a data size and cap in whole blocks with the cap at least
    /// the data size, a durable length only on an append region not
    /// exported in full and at most its data size, an object length only
    /// on a slot and at most what a slot holds, no slot exported in full,
    /// and its restored footprint (two header blocks and its data) within
    /// `u64`.
    ///
    /// # Errors
    ///
    /// `NotWritten(InvalidOptions)` at the first check that fails.
    pub fn validate(&self, block_size: u64) -> Result<(), Error> {
        let bad = || not_written(NotWrittenCause::InvalidOptions, ErrorContext::default());
        if RegionName::new(&self.name).is_err() || self.generation == 0 {
            return Err(bad());
        }
        let block = self.block_size;
        if block != block_size || !block.is_power_of_two() || !(512..=1 << 16).contains(&block) {
            return Err(bad());
        }
        if self.data_size % block != 0 {
            return Err(bad());
        }
        if let Some(cap) = self.cap {
            if cap < self.data_size || cap % block != 0 {
                return Err(bad());
            }
        }
        let slot = self.kind == RegionKind::Slot;
        if let Some(d) = self.durable_len {
            if self.kind != RegionKind::Append || self.full_copy || d > self.data_size {
                return Err(bad());
            }
        }
        if let Some(n) = self.object_len {
            let most = max_object(block.trailing_zeros() as u8) as u64;
            if !slot || n > most {
                return Err(bad());
            }
        }
        if self.full_copy && slot {
            return Err(bad());
        }
        let footprint = block
            .checked_mul(2)
            .and_then(|h| h.checked_add(self.cap.unwrap_or(0).max(self.data_size)));
        if footprint.is_none() {
            return Err(bad());
        }
        Ok(())
    }
}

/// How far an export got.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExportReport {
    /// Regions exported: each counted once its `end_region` returned `Ok`.
    pub regions: usize,
    /// Bytes the sink's `data` accepted.
    pub bytes: u64,
    /// The region being exported when the export stopped (`None` once
    /// every region ended, or before the first).
    pub region: Option<String>,
    /// The offset in that region's stream the sink accepted bytes up to.
    pub offset: u64,
    /// Whether the store was open for writing.
    pub writable: bool,
    /// Every region ended and the sink's `finish` returned `Ok`.
    pub complete: bool,
    /// The volume's facts (empty if the plan refused).
    pub facts: VolumeFacts,
}

/// A sink's refusal, without payload: an I/O error, a refusal, or the
/// caller's own code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SinkError {
    /// An I/O error, as the operating system reported it.
    Io(OsError),
    /// The sink refused the call (out of order, or past what it was told).
    Refused,
    /// The caller's own code.
    Other(u32),
}

impl core::fmt::Display for SinkError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(raw) => write!(f, "sink I/O error {}", raw.code),
            Self::Refused => f.write_str("sink refused the call"),
            Self::Other(code) => write!(f, "sink error {code}"),
        }
    }
}

impl std::error::Error for SinkError {}

/// An export that stopped: why, the sink's own error if it was the sink's
/// (`error` is then `Io` for [`SinkError::Io`], `NotWritten(Interrupted)`
/// otherwise), and how far it got (`complete` false).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ExportError {
    /// Why it stopped (the region in its context when one was being
    /// exported).
    pub error: Error,
    /// The sink's error, if the sink stopped it.
    pub sink: Option<SinkError>,
    /// How far it got.
    pub report: ExportReport,
}

impl core::fmt::Display for ExportError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "export stopped: {}", self.error)
    }
}

impl std::error::Error for ExportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Where an export pushes a store: its header, then each region's info,
/// its bytes in order and its end, then the finish. Pacing is invisible to
/// it: the export's reads are paced by its own call.
pub trait ExportSink {
    /// Before the first region.
    ///
    /// # Errors
    ///
    /// The sink's: the export stops.
    fn begin(&mut self, header: &ExportHeader) -> Result<(), SinkError>;

    /// Before a region's bytes.
    ///
    /// # Errors
    ///
    /// The sink's: the export stops.
    fn region(&mut self, info: &RegionInfo) -> Result<(), SinkError>;

    /// The region's next bytes, contiguous from offset 0 of its stream
    /// ([`Chunk::offset`]). The sink owns the chunk: it may keep up to the
    /// header's `lend - 1` and drop them later.
    ///
    /// # Errors
    ///
    /// The sink's: the export stops; the report's offset is where this
    /// chunk began.
    fn data(&mut self, chunk: Chunk) -> Result<(), SinkError>;

    /// Called when the export needs a buffer and the sink holds every one
    /// lent: drop the chunks no longer needed. If it still holds them all
    /// the export fails `TooManyInFlight`. By default, nothing.
    ///
    /// # Errors
    ///
    /// The sink's: the export stops.
    fn settle(&mut self) -> Result<(), SinkError> {
        Ok(())
    }

    /// After a region's last bytes: the region counts as exported once
    /// this returns `Ok`.
    ///
    /// # Errors
    ///
    /// The sink's: the export stops.
    fn end_region(&mut self) -> Result<(), SinkError>;

    /// After the last region (right after `begin` with none): the export is
    /// complete once this returns `Ok`.
    ///
    /// # Errors
    ///
    /// The sink's: the export stops, incomplete.
    fn finish(&mut self) -> Result<(), SinkError>;
}

/// Why an export stopped, before the report is attached.
struct Stop {
    error: Error,
    sink: Option<SinkError>,
}

impl From<Error> for Stop {
    fn from(error: Error) -> Self {
        Self { error, sink: None }
    }
}

/// A refusal naming `region`.
fn refused(cause: NotWrittenCause, region: Option<RegionId>) -> Error {
    Error::NotWritten {
        cause,
        ctx: ErrorContext {
            region,
            ..ErrorContext::default()
        },
    }
}

/// The sink's error as the export's, naming `region`.
fn sunk(region: Option<RegionId>) -> impl Fn(SinkError) -> Stop {
    move |e| {
        let ctx = ErrorContext {
            region,
            ..ErrorContext::default()
        };
        let error = match e {
            SinkError::Io(raw) => Error::Io {
                op: Op::Write,
                raw,
                ctx,
            },
            SinkError::Refused | SinkError::Other(_) => Error::NotWritten {
                cause: NotWrittenCause::Interrupted,
                ctx,
            },
        };
        Stop {
            error,
            sink: Some(e),
        }
    }
}

fn too_many(e: &Error) -> bool {
    matches!(
        e,
        Error::NotWritten {
            cause: NotWrittenCause::TooManyInFlight,
            ..
        }
    )
}

impl<P: Platform> Store<P>
where
    P::Queue: Send,
{
    /// Exports every region to `sink`, in the store's order: see the
    /// module docs and [`Self::export_with`].
    ///
    /// # Errors
    ///
    /// As [`Self::export_with`].
    pub fn export(&self, sink: &mut dyn ExportSink) -> Result<ExportReport, Box<ExportError>> {
        self.export_with(sink, ExportOptions::default())
    }

    /// Exports the regions `opts` names (all by default) to `sink`: its
    /// `begin` with the header, then for each region its `region`, its
    /// bytes through `data` and its `end_region`, then its `finish`.
    ///
    /// # Errors
    ///
    /// An [`ExportError`] (boxed: it holds the report) with how far it got. Before the sink is
    /// called: `NotWritten(InvalidOptions)` for a region named twice, a
    /// name no region has, a depth out of range, or on a writable open a
    /// region that only a read-only open exports (one of a stale page,
    /// possibly superseded, or at risk: open the store read-only to export
    /// it); `NotWritten(NotReady)` for an unavailable region or an entry
    /// that does not decode, `NotWritten(Frozen)` for a stale or lost table
    /// page (both unless [`ExportOptions::accept_stale_pages`]);
    /// `NotWritten(TooLarge)` for a slot object a format-2 slot cannot
    /// hold; `NotWritten(StaleGeneration)` for a region that changed while
    /// the plan read it; `NotWritten(PoolExhausted)` with too few buffers
    /// free for the stream; `Io` if its queue could not be made. At a
    /// region's turn: `NotWritten(StaleGeneration)` (recycled) or
    /// `NotWritten(NotReady)` (released) since the plan or while it
    /// streams, `NotWritten(TooLarge)` for a slot object committed since
    /// that a format-2 slot cannot hold; a read's errors as a
    /// [`ReadStream`]'s, a media error (`Corruption(MediaError)`, the
    /// range named) among them; `NotWritten(TooManyInFlight)` if the sink
    /// still holds every lent chunk after `settle`. Any error of the sink
    /// (in `sink`).
    pub fn export_with(
        &self,
        sink: &mut dyn ExportSink,
        opts: ExportOptions,
    ) -> Result<ExportReport, Box<ExportError>> {
        let mut report = ExportReport {
            writable: !self.inner.read_only,
            ..ExportReport::default()
        };
        match self.export_run(sink, &opts, &mut report) {
            Ok(()) => {
                report.region = None;
                report.offset = 0;
                report.complete = true;
                Ok(report)
            }
            Err(Stop { error, sink }) => Err(Box::new(ExportError {
                error,
                sink,
                report,
            })),
        }
    }

    /// The export's plan: the options checked, `copy_store`'s plan of the
    /// regions named, and on a writable open the regions only a read-only
    /// open exports refused.
    fn export_plan(&self, opts: &ExportOptions) -> Result<CopyPlan, Error> {
        // A name no region has is refused by the plan; the depth by the
        // stream, before the sink's `begin`.
        if let Some(names) = &opts.regions {
            if (1..names.len()).any(|i| names[..i].contains(&names[i])) {
                return Err(not_written(
                    NotWrittenCause::InvalidOptions,
                    ErrorContext::default(),
                ));
            }
        }
        let plan = self.plan_of(opts.accept_stale_pages, opts.regions.as_deref())?;
        // A region of a stale page is read by container offset: on a
        // writable open an accepted rollback may free its extra extent
        // under it. A superseded or at-risk region may be made writable
        // and stream newer bytes. Only a read-only open exports them.
        if !self.inner.read_only {
            if let Some(f) = plan.facts.iter().find(|f| f.full.is_some() || f.superseded) {
                return Err(refused(NotWrittenCause::InvalidOptions, Some(f.source.id)));
            }
        }
        Ok(plan)
    }

    /// An upper bound on what a restore of `facts` takes in a new store of
    /// the default geometry with blocks of `block` (saturating).
    fn export_total(facts: &[Facts], block: u64) -> u64 {
        let defaults = StoreOptions::default();
        let log2_block = block.trailing_zeros() as u8;
        let log2_table = defaults.log2_table_slot.clamp(log2_block, 16);
        let count = u32::try_from(facts.len()).unwrap_or(u32::MAX);
        let table =
            crate::v2::layout_for(log2_block, log2_table, defaults.region_capacity.max(count))
                .data_start();
        facts.iter().fold(table, |total, f| {
            let data = match f.kind {
                RegionKind::Slot => max_object(log2_block) as u64,
                _ => f.cap.unwrap_or(0).max(f.data_size),
            };
            total.saturating_add(2 * block).saturating_add(data)
        })
    }

    fn export_run(
        &self,
        sink: &mut dyn ExportSink,
        opts: &ExportOptions,
        report: &mut ExportReport,
    ) -> Result<(), Stop> {
        let plan = self.export_plan(opts)?;
        report.facts = VolumeFacts {
            omitted: plan.omitted.clone(),
            lost_pages: plan.lost_pages.clone(),
            stale_pages: plan.stale_pages.clone(),
            verified_up_to: plan.verified_up_to.clone(),
            superseded: plan.superseded.clone(),
        };
        // The call's record on this store's device group: the stream's
        // reads are its pieces.
        let pace = self.inner.domain.new_pace();
        let stream = self.export_stream(opts.depth, pace)?;
        let header = ExportHeader {
            region_count: u32::try_from(plan.facts.len()).unwrap_or(u32::MAX),
            block_size: plan.block,
            facts: report.facts.clone(),
            writable: report.writable,
            lend: stream.lend(),
            total: Self::export_total(&plan.facts, plan.block),
        };
        sink.begin(&header).map_err(sunk(None))?;
        for f in &plan.facts {
            report.region = Some(f.name.clone());
            report.offset = 0;
            self.export_region(f, plan.block, &stream, sink, report)?;
            report.regions += 1;
        }
        report.region = None;
        report.offset = 0;
        // Nothing is in flight: every part was read to its end.
        stream.cancel()?;
        sink.finish().map_err(sunk(None))
    }

    /// Slot `r`'s committed object at its turn, read under its header lock
    /// (a slot is never recycled or released: its gate stays live), refused
    /// as the plan refuses one a format-2 slot cannot hold.
    fn object_at_turn(&self, r: &LiveRegion, block: u64) -> Result<Option<Vec<u8>>, Error> {
        let header = r
            .header
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let object = committed_object(&header, &self.inner.layout);
        if object
            .as_ref()
            .is_some_and(|o| o.len() > max_object(block.trailing_zeros() as u8))
        {
            return Err(refused(NotWrittenCause::TooLarge, Some(r.id)));
        }
        Ok(object)
    }

    /// Exports region `f` (of the plan) at its turn.
    fn export_region(
        &self,
        f: &Facts,
        block: u64,
        stream: &ReadStream<P>,
        sink: &mut dyn ExportSink,
        report: &mut ExportReport,
    ) -> Result<(), Stop> {
        // Still the planned (id, generation): a recycle or release closes
        // the planned region's gate for good (a recycle's new generation is
        // a region of its own), so a gate still live is the plan's region.
        let r = &f.source;
        r.gate.live().map_err(|why| retired_error(why, r))?;
        let id = Some(r.id);
        let (durable_len, object, part) = match (f.kind, &f.full) {
            (RegionKind::Slot, _) => (None, self.object_at_turn(r, block)?, None),
            // By container offset: its capacity's runs, then its extra
            // extent.
            (_, Some((ranges, _))) => (
                None,
                None,
                Some((f.data_size, Locator::Ranges(ranges.clone()))),
            ),
            (kind, None) => {
                // A positioned append region's durable prefix, clamped to
                // its planned data size (still a valid prefix).
                let durable = (kind == RegionKind::Append && r.positioned.load(Ordering::Acquire))
                    .then(|| {
                        r.frontier
                            .as_ref()
                            .map_or(0, crate::frontier::AppendFrontier::durable)
                            .min(f.data_size)
                    });
                (
                    durable,
                    None,
                    Some((durable.unwrap_or(f.data_size), Locator::Map)),
                )
            }
        };
        let info = RegionInfo {
            name: f.name.clone(),
            kind: f.kind,
            generation: f.attrs.generation,
            tag: f.attrs.tag,
            cap: f.cap,
            fill: f.attrs.fill,
            fill_key: f.attrs.fill_key,
            block_size: block,
            data_size: f.data_size,
            durable_len,
            object_len: object.as_ref().map(|o| o.len() as u64),
            full_copy: f.full.is_some(),
            superseded: f.superseded,
        };
        sink.region(&info).map_err(sunk(id))?;
        if let Some(o) = object.filter(|o| !o.is_empty()) {
            let mut settled = false;
            let chunk = loop {
                match stream.lend_bytes(r, 0, &o) {
                    Ok(c) => break c,
                    Err(e) if too_many(&e) && !settled => {
                        sink.settle().map_err(sunk(id))?;
                        settled = true;
                    }
                    Err(e) => return Err(e.into()),
                }
            };
            sink.data(chunk).map_err(sunk(id))?;
            report.offset = o.len() as u64;
            report.bytes += o.len() as u64;
        }
        if let Some((len, loc)) = part {
            self.export_bytes(r, len, loc, stream, sink, report)?;
        }
        sink.end_region().map_err(sunk(id))
    }

    /// Streams `[0, len)` of `r` (read as `loc` places it) to the sink.
    fn export_bytes(
        &self,
        r: &Arc<LiveRegion>,
        len: u64,
        loc: Locator,
        stream: &ReadStream<P>,
        sink: &mut dyn ExportSink,
        report: &mut ExportReport,
    ) -> Result<(), Stop> {
        let id = Some(r.id);
        stream.add_part(r, 0..len, loc)?;
        let mut got = 0u64;
        let mut settled = false;
        while got < len {
            match stream.pull() {
                Some(Ok(c)) => {
                    let n = c.bytes().len() as u64;
                    // In order from the region's start, with no gap and
                    // nothing past its end.
                    if n == 0 || c.offset() != got || n > len - got {
                        let mut e = crate::store::corrupt();
                        if let Error::Corruption { ctx, .. } = &mut e {
                            *ctx = ctx_range(r, got, len);
                        }
                        return Err(e.into());
                    }
                    settled = false;
                    sink.data(c).map_err(sunk(id))?;
                    got += n;
                    report.offset = got;
                    report.bytes += n;
                }
                Some(Err(e)) if too_many(&e) && !settled => {
                    sink.settle().map_err(sunk(id))?;
                    settled = true;
                }
                Some(Err(e)) => return Err(e.into()),
                // Ended before the region's bytes did.
                None => {
                    return Err(
                        not_written(NotWrittenCause::Interrupted, ctx_range(r, got, len)).into(),
                    );
                }
            }
        }
        Ok(())
    }
}
