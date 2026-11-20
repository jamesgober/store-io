//! `StoreSink` (PLANNING R5.6, D-170): an export restored straight into a
//! new store, as an import restores one; what it is pushed checked, every
//! refusal sticky and leaving nothing under the target name; its writes one
//! call with the export's reads in every pacing state.

#![allow(clippy::unwrap_used, clippy::expect_used)]

// The format-2 audit (its helpers keep results they do not use).
#[allow(unused_results)]
mod format2_util;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use format2_util::audit::audit_in;

use store_io_core::error::CorruptionKind;
use store_io_core::error::{Error, Named, NotWrittenCause, ReserveTag};
use store_io_engine::{
    CONTAINER, COPYING, Chunk, ExportError, ExportHeader, ExportOptions, ExportReport, ExportSink,
    FileSink, FileSinkOptions, FillPattern, PageRegion, REGIONS_INFO, RegionInfo, RegionOptions,
    SinkError, Store, StoreOptions, StoreSink, StoreSinkOptions,
};
use store_io_format::fill::fill_keyed_v1;
use store_io_platform::Platform;
use store_io_sim::FaultPlan;
use store_io_sim::{CrashMode, DeviceKind, SimConfig, SimPlatform};

const DIR: &str = "/db";
const TO: &str = "/restored";
const CHUNK: usize = 1 << 17;
const B: u64 = 4096;
const SPAN: u64 = 1 << 20;
const SECOND: u64 = 1_000_000_000;

fn opts() -> StoreOptions {
    StoreOptions {
        queues: 2,
        buffers_per_class: 64,
        max_io: CHUNK,
        keyed_fill: true,
        ..StoreOptions::default()
    }
}

fn sink_opts() -> StoreSinkOptions {
    let mut o = StoreSinkOptions::default();
    o.store = opts();
    o
}

fn sink(p: &SimPlatform) -> StoreSink<SimPlatform> {
    StoreSink::create(p.clone(), Path::new(TO), sink_opts()).unwrap()
}

/// Each 8-byte word holds its own offset plus `salt`.
fn stamped(salt: u64, len: usize) -> Vec<u8> {
    (0..len.div_ceil(8) as u64)
        .flat_map(|w| (w * 8 + salt).to_le_bytes())
        .take(len)
        .collect()
}

fn cause(e: &Error) -> Option<NotWrittenCause> {
    match e {
        Error::NotWritten { cause, .. } => Some(*cause),
        _ => None,
    }
}

/// A store of every kind: a page region of two spans and a bit, a capped
/// page region, a page region recycled once (generation 2), a positioned
/// append region with a durable prefix short of its data size, a slot, a
/// slot committed empty and a slot never committed.
fn every_kind(p: &SimPlatform) -> Store<SimPlatform> {
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let pages = s.provision_page_region("pages", 2 * SPAN + 3 * B).unwrap();
    let _d = pages
        .write_durable(
            pages.pos(0).unwrap(),
            &stamped(1, (2 * SPAN + 3 * B) as usize),
        )
        .unwrap();
    let capped = s
        .provision_page_region_with(
            "capped",
            RegionOptions::default().size(4 * B).max_size(16 * B),
        )
        .unwrap();
    let _d = capped
        .write_durable(capped.pos(0).unwrap(), &stamped(2, 4 * B as usize))
        .unwrap();
    let again = s.provision_page_region("again", 2 * B).unwrap();
    let again = again.recycle().unwrap();
    let _d = again
        .write_durable(again.pos(0).unwrap(), &stamped(4, 2 * B as usize))
        .unwrap();
    let log = s.provision_append_region("log", 4 * CHUNK as u64).unwrap();
    let t = log.append(&stamped(3, CHUNK + 1000)).unwrap();
    let _d = log.sync_through(&t).unwrap();
    let _d = s
        .provision_slot("slot")
        .unwrap()
        .commit(b"the object")
        .unwrap();
    let _d = s.provision_slot("blank").unwrap().commit(b"").unwrap();
    let _e = s.provision_slot("empty").unwrap();
    s
}

/// A small store for the enumerations: a page region, an append region
/// with a durable prefix, a slot.
fn small(p: &SimPlatform) -> Store<SimPlatform> {
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let pages = s.provision_page_region("pages", 2 * B).unwrap();
    let _d = pages
        .write_durable(pages.pos(0).unwrap(), &stamped(1, 2 * B as usize))
        .unwrap();
    let log = s.provision_append_region("log", 4 * B).unwrap();
    let t = log.append(&stamped(3, 100)).unwrap();
    let _d = log.sync_through(&t).unwrap();
    let _d = s.provision_slot("slot").unwrap().commit(b"x").unwrap();
    s
}

/// Each region's info and bytes, as an export pushes them.
#[derive(Default)]
struct Keep {
    header: Option<ExportHeader>,
    regions: Vec<(RegionInfo, Vec<u8>)>,
}

impl ExportSink for Keep {
    fn begin(&mut self, header: &ExportHeader) -> Result<(), SinkError> {
        self.header = Some(header.clone());
        Ok(())
    }
    fn region(&mut self, info: &RegionInfo) -> Result<(), SinkError> {
        self.regions.push((info.clone(), Vec::new()));
        Ok(())
    }
    fn data(&mut self, chunk: Chunk) -> Result<(), SinkError> {
        let i = self.regions.len() - 1;
        self.regions[i].1.extend_from_slice(chunk.bytes());
        Ok(())
    }
    fn end_region(&mut self) -> Result<(), SinkError> {
        Ok(())
    }
    fn finish(&mut self) -> Result<(), SinkError> {
        Ok(())
    }
}

fn kept(s: &Store<SimPlatform>) -> Keep {
    let mut k = Keep::default();
    let r = s.export(&mut k).unwrap();
    assert!(r.complete);
    k
}

/// No store under `TO`.
fn none_restored(p: &SimPlatform) {
    let e = Store::open(p.clone(), Path::new(TO), opts()).unwrap_err();
    assert!(
        matches!(e, Error::NotFound { what: Named::Store }),
        "a store under the target name: {e:?}"
    );
}

/// The restored store matches `want` (the source's export): each region's
/// attributes and bytes; an append region restored unpositioned, its whole
/// data area its source's durable prefix then its pattern.
fn restored_matches(p: &SimPlatform, want: &Keep) {
    let s = Store::open(p.clone(), Path::new(TO), opts()).unwrap();
    let got = kept(&s);
    assert_eq!(got.regions.len(), want.regions.len());
    for ((gi, gb), (wi, wb)) in got.regions.iter().zip(&want.regions) {
        assert_eq!(gi.name, wi.name);
        assert_eq!(gi.kind, wi.kind, "{}", wi.name);
        assert_eq!(gi.generation, wi.generation, "{}", wi.name);
        assert_eq!(gi.tag, wi.tag, "{}", wi.name);
        assert_eq!(gi.cap, wi.cap, "{}", wi.name);
        assert_eq!(gi.fill, wi.fill, "{}", wi.name);
        assert_eq!(gi.fill_key, wi.fill_key, "{}", wi.name);
        assert_eq!(gi.data_size, wi.data_size, "{}", wi.name);
        assert_eq!(gi.object_len, wi.object_len, "{}", wi.name);
        match wi.durable_len {
            Some(d) => {
                assert_eq!(gi.durable_len, None, "{} restored unpositioned", wi.name);
                assert_eq!(gb.len() as u64, wi.data_size);
                assert!(gb[..d as usize] == wb[..], "{}'s prefix", wi.name);
                let mut tail = vec![0u8; (wi.data_size - d) as usize];
                match wi.fill {
                    FillPattern::KeyedV1 => fill_keyed_v1(&wi.fill_key, d, &mut tail),
                    FillPattern::Zeros => {}
                }
                assert!(gb[d as usize..] == tail[..], "{}'s pattern", wi.name);
            }
            None => assert!(gb == wb, "{}'s bytes", wi.name),
        }
    }
}

/// The restored store's slot committed empty reads back committed and
/// empty, and the slot never committed reads back none.
fn slots_restored(p: &SimPlatform) {
    let s = Store::open(p.clone(), Path::new(TO), opts()).unwrap();
    assert_eq!(s.slot("blank").unwrap().read(), Some(Vec::new()));
    assert_eq!(s.slot("empty").unwrap().read(), None);
    assert_eq!(s.slot("slot").unwrap().read(), Some(b"the object".to_vec()));
}

/// Whether the temporary the sink builds under is in `TO`.
fn temp_there(p: &SimPlatform) -> bool {
    p.with_world(|w| w.file_id(&PathBuf::from(TO), COPYING).is_some())
}

fn export_into(
    s: &Store<SimPlatform>,
    sink: &mut dyn ExportSink,
) -> Result<ExportReport, Box<ExportError>> {
    s.export(sink)
}

/// Every kind straight into a new store: every region with its
/// generation, tag, cap, fill and key, and its bytes; the sink lists each
/// region as exported (the append region's durable length its resume).
#[test]
fn test_every_kind_round_trips_into_a_store() {
    let p = SimPlatform::new(SimConfig::volatile(5000));
    let s = every_kind(&p);
    let want = kept(&s);
    let mut ss = sink(&p);
    assert!(!ss.is_complete());
    let r = export_into(&s, &mut ss).unwrap();
    assert!(r.complete && r.regions == 7);
    assert!(ss.is_complete());
    assert_eq!(ss.restored().len(), 7);
    let log = ss.restored().iter().find(|i| i.name == "log").unwrap();
    let was = want.regions.iter().find(|(i, _)| i.name == "log").unwrap();
    assert!(was.0.durable_len.is_some());
    assert_eq!(log.durable_len, was.0.durable_len);
    drop(ss);
    assert!(!temp_there(&p));
    restored_matches(&p, &want);
    slots_restored(&p);
}

/// A store with no region restores to one with none.
#[test]
fn test_no_region_round_trips() {
    let p = SimPlatform::new(SimConfig::volatile(5001));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let mut ss = sink(&p);
    let r = export_into(&s, &mut ss).unwrap();
    assert!(r.complete && ss.restored().is_empty());
    drop(ss);
    let t = Store::open(p.clone(), Path::new(TO), opts()).unwrap();
    assert!(kept(&t).regions.is_empty());
}

/// A subset restores to a store of exactly those regions.
#[test]
fn test_a_subset_restores_only_its_regions() {
    let p = SimPlatform::new(SimConfig::volatile(5002));
    let s = every_kind(&p);
    let all = kept(&s);
    let mut o = ExportOptions::default();
    o.regions = Some(vec!["log".to_owned(), "blank".to_owned()]);
    let mut ss = sink(&p);
    let r = s.export_with(&mut ss, o).unwrap();
    assert!(r.complete && r.regions == 2);
    drop(ss);
    let want = Keep {
        header: None,
        regions: all
            .regions
            .into_iter()
            .filter(|(i, _)| i.name == "log" || i.name == "blank")
            .collect(),
    };
    restored_matches(&p, &want);
}

/// A store already in the target directory: refused at `create`, nothing
/// written; the store there untouched.
#[test]
fn test_a_store_already_there_is_refused() {
    let p = SimPlatform::new(SimConfig::volatile(5003));
    let s = small(&p);
    drop(Store::create(p.clone(), Path::new(TO), opts()).unwrap());
    let before = p.with_world(|w| w.write_submits());
    let e = StoreSink::create(p.clone(), Path::new(TO), sink_opts()).unwrap_err();
    assert!(
        matches!(e, Error::AlreadyExists { what: Named::Store }),
        "{e:?}"
    );
    assert_eq!(p.with_world(|w| w.write_submits()), before, "it wrote");
    assert!(!temp_there(&p));
    let t = Store::open(p.clone(), Path::new(TO), opts()).unwrap();
    assert!(kept(&t).regions.is_empty());
    drop(s);
}

/// While one sink holds the directory, another is refused `Locked`.
#[test]
fn test_a_second_sink_into_the_directory_is_locked_out() {
    let p = SimPlatform::new(SimConfig::volatile(5004));
    let first = sink(&p);
    let e = StoreSink::create(p.clone(), Path::new(TO), sink_opts()).unwrap_err();
    assert!(matches!(e, Error::Locked), "{e:?}");
    drop(first);
    drop(sink(&p));
}

/// Too little free space for the header's total: `NoSpace` at `begin`,
/// before the store is created.
#[test]
fn test_too_little_space_is_refused_at_begin() {
    let p = SimPlatform::new(SimConfig::volatile(5005));
    let s = every_kind(&p);
    let total = kept(&s).header.unwrap().total;
    let mut ss = sink(&p);
    let used = p.with_world(|w| w.allocated_bytes());
    p.with_world(|w| w.faults_mut().capacity = Some(used + total - 4 * B));
    let e = export_into(&s, &mut ss).unwrap_err();
    assert!(
        matches!(
            e.error,
            Error::NoSpace {
                tag: ReserveTag::Caller(0),
                ..
            }
        ),
        "{:?}",
        e.error
    );
    assert!(!e.report.complete && e.report.regions == 0);
    assert!(!temp_there(&p), "the store was created");
    drop(ss);
    none_restored(&p);
}

/// The export's block size, a power of two from 512 to 65,536, and its
/// region count within the table: checked at `begin`; a device that needs
/// larger blocks than the export's is `Misaligned`.
#[test]
fn test_a_header_the_store_cannot_take_is_refused() {
    for (seed, wrong) in [
        (5006, Misuse::SmallBlock),
        (5007, Misuse::BadBlock),
        (5008, Misuse::HugeCount),
    ] {
        let p = SimPlatform::new(SimConfig::volatile(seed));
        let s = small(&p);
        let mut w = Wrap::new(&p, wrong);
        let e = export_into(&s, &mut w).unwrap_err();
        let want = match wrong {
            Misuse::SmallBlock => NotWrittenCause::Misaligned,
            _ => NotWrittenCause::InvalidOptions,
        };
        assert_eq!(cause(&e.error), Some(want), "{wrong:?}: {:?}", e.error);
        assert!(!e.report.complete);
        assert!(!w.inner.is_complete());
        drop(w);
        none_restored(&p);
        if wrong == Misuse::SmallBlock {
            // Its store was made under the temporary name, then refused.
            assert!(temp_there(&p));
            drop(sink(&p));
            assert!(!temp_there(&p), "the next sink removed it");
        }
    }
}

/// What a wrapper does to the calls it forwards to its `StoreSink`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Misuse {
    /// `begin` forwarded twice.
    BeginTwice,
    /// Region 0's `end_region` not forwarded.
    SkipFirstEnd,
    /// The first chunk not forwarded (the next is not at its next byte).
    DropFirstChunk,
    /// `end_region` forwarded before the first chunk.
    EndEarly,
    /// Region 1's calls not forwarded (fewer regions than the header says).
    SkipSecondRegion,
    /// `finish` forwarded twice.
    FinishTwice,
    /// Region 0's info forwarded with generation 0.
    BadInfo,
    /// The header forwarded with a total of one block.
    SmallTotal,
    /// Region 1 forwarded under region 0's name.
    SameName,
    /// The header forwarded with a region count of 1.
    FewerCount,
    /// The header forwarded with 512-byte blocks.
    SmallBlock,
    /// The header forwarded with a block size that is no power of two.
    BadBlock,
    /// The header forwarded with a region count past the format's table.
    HugeCount,
    /// `finish` not forwarded: the sink never finishes.
    NoFinish,
    /// A commit hold taken on the source store at the first chunk, while
    /// the wrapper holds it (the call turns paced mid-export).
    HoldAtFirstChunk,
    /// The first region's first two chunks forwarded in swapped order.
    SwapChunks,
    /// `finish` forwarded inside the first region, before its end.
    FinishInRegion,
    /// The slot `slot`'s one chunk (its object) not forwarded: its
    /// `end_region` comes with none of its bytes and nothing after it.
    DropSlotChunk,
}

/// A caller's wrapper around a `StoreSink` that misuses it as told and
/// swallows every error the sink returns (so only the call can tell the
/// export the sink failed). Its `settle` is the default.
struct Wrap {
    inner: StoreSink<SimPlatform>,
    misuse: Misuse,
    regions: usize,
    chunks: usize,
    first: String,
    errors: Vec<(&'static str, SinkError)>,
    hold_on: Option<Store<SimPlatform>>,
    hold: Option<store_io_engine::CommitHold>,
    held: Option<Chunk>,
    name: String,
}

impl Wrap {
    fn new(p: &SimPlatform, misuse: Misuse) -> Self {
        Self {
            inner: sink(p),
            misuse,
            regions: 0,
            chunks: 0,
            first: String::new(),
            errors: Vec::new(),
            hold_on: None,
            hold: None,
            held: None,
            name: String::new(),
        }
    }

    fn eat(&mut self, call: &'static str, r: Result<(), SinkError>) -> Result<(), SinkError> {
        if let Err(e) = r {
            self.errors.push((call, e));
        }
        Ok(())
    }

    fn skipping(&self) -> bool {
        self.misuse == Misuse::SkipSecondRegion && self.regions == 2
    }
}

impl ExportSink for Wrap {
    fn begin(&mut self, header: &ExportHeader) -> Result<(), SinkError> {
        let mut h = header.clone();
        match self.misuse {
            Misuse::SmallTotal => h.total = B,
            Misuse::FewerCount => h.region_count = 1,
            Misuse::SmallBlock => h.block_size = 512,
            Misuse::BadBlock => h.block_size = 3 * B,
            Misuse::HugeCount => h.region_count = u32::MAX,
            _ => {}
        }
        let r = self.inner.begin(&h);
        self.eat("begin", r)?;
        if self.misuse == Misuse::BeginTwice {
            let r = self.inner.begin(header);
            self.eat("begin", r)?;
        }
        Ok(())
    }

    fn region(&mut self, info: &RegionInfo) -> Result<(), SinkError> {
        self.regions += 1;
        self.chunks = 0;
        self.name = info.name.clone();
        if self.skipping() {
            return Ok(());
        }
        let mut i = info.clone();
        if self.regions == 1 {
            self.first = info.name.clone();
            if self.misuse == Misuse::BadInfo {
                i.generation = 0;
            }
        }
        if self.misuse == Misuse::SameName && self.regions == 2 {
            i.name = self.first.clone();
        }
        let r = self.inner.region(&i);
        self.eat("region", r)
    }

    fn data(&mut self, chunk: Chunk) -> Result<(), SinkError> {
        self.chunks += 1;
        if self.skipping() {
            return Ok(());
        }
        if self.misuse == Misuse::DropSlotChunk && self.name == "slot" {
            return Ok(());
        }
        if self.chunks == 1 {
            match self.misuse {
                Misuse::DropFirstChunk => return Ok(()),
                Misuse::EndEarly => {
                    let r = self.inner.end_region();
                    self.eat("end_region", r)?;
                }
                Misuse::HoldAtFirstChunk if self.hold.is_none() => {
                    self.hold = self.hold_on.as_ref().map(Store::commit_hold);
                }
                Misuse::SwapChunks if self.regions == 1 => {
                    self.held = Some(chunk);
                    return Ok(());
                }
                Misuse::FinishInRegion if self.regions == 1 => {
                    let r = self.inner.finish();
                    self.eat("finish", r)?;
                }
                _ => {}
            }
        }
        let r = self.inner.data(chunk);
        self.eat("data", r)?;
        if let Some(first) = self.held.take() {
            let r = self.inner.data(first);
            self.eat("data", r)?;
        }
        Ok(())
    }

    fn end_region(&mut self) -> Result<(), SinkError> {
        if self.skipping() || (self.misuse == Misuse::SkipFirstEnd && self.regions == 1) {
            return Ok(());
        }
        let r = self.inner.end_region();
        self.eat("end_region", r)
    }

    fn finish(&mut self) -> Result<(), SinkError> {
        if self.misuse == Misuse::NoFinish {
            return Ok(());
        }
        let r = self.inner.finish();
        self.eat("finish", r)?;
        if self.misuse == Misuse::FinishTwice {
            let r = self.inner.finish();
            self.eat("finish", r)?;
        }
        Ok(())
    }
}

/// Each misuse is refused at the call that misuses it, the refusal sticky
/// (so `finish` never renames the store into place), and the export is not
/// complete although the wrapper swallowed every error: the call knows its
/// sink failed. `finish` twice is refused, the store already in place.
#[test]
fn test_each_misuse_is_refused_and_nothing_is_restored() {
    for (i, misuse) in [
        Misuse::BeginTwice,
        Misuse::SkipFirstEnd,
        Misuse::DropFirstChunk,
        Misuse::EndEarly,
        Misuse::SkipSecondRegion,
        Misuse::BadInfo,
        Misuse::SmallTotal,
        Misuse::SameName,
        Misuse::FewerCount,
        Misuse::NoFinish,
        Misuse::FinishTwice,
        Misuse::SwapChunks,
        Misuse::DropSlotChunk,
    ]
    .into_iter()
    .enumerate()
    {
        let p = SimPlatform::new(SimConfig::volatile(5100 + i as u64));
        let s = every_kind(&p);
        let mut w = Wrap::new(&p, misuse);
        let r = export_into(&s, &mut w);
        match misuse {
            Misuse::FinishTwice => {
                let e = r.unwrap_err();
                assert!(!e.report.complete, "{misuse:?}");
                assert_eq!(cause(&e.error), Some(NotWrittenCause::InvalidOptions));
                assert_eq!(w.errors.len(), 1, "{misuse:?}: {:?}", w.errors);
                assert_eq!(w.errors[0].0, "finish");
                // The first finish renamed it into place.
                assert!(w.inner.is_complete());
                drop(w);
                assert!(Store::open(p.clone(), Path::new(TO), opts()).is_ok());
            }
            _ => {
                let e = r.unwrap_err();
                assert!(!e.report.complete, "{misuse:?}");
                // The first refusal is the export's error (the sink's own,
                // recorded on the call), at the region it refused.
                let want = match misuse {
                    Misuse::NoFinish => NotWrittenCause::Interrupted,
                    _ => NotWrittenCause::InvalidOptions,
                };
                assert_eq!(cause(&e.error), Some(want), "{misuse:?}: {:?}", e.error);
                if misuse == Misuse::FewerCount {
                    assert_eq!(e.report.regions, 1, "refused at the second region");
                }
                // A region the sink never ended: the report stands in it,
                // at its writes done (the first region's every piece when
                // its end never came; none of the second, never begun), and
                // nothing past it.
                let r = &e.report;
                let first = 2 * SPAN + 3 * B;
                match misuse {
                    Misuse::SkipFirstEnd => {
                        assert_eq!(r.regions, 0);
                        assert_eq!(r.region.as_deref(), Some("pages"));
                        assert_eq!((r.offset, r.bytes), (first, first));
                    }
                    Misuse::SkipSecondRegion => {
                        assert_eq!(r.regions, 1);
                        assert_eq!(r.region.as_deref(), Some("capped"));
                        assert_eq!((r.offset, r.bytes), (0, first));
                    }
                    _ => {}
                }
                if misuse != Misuse::NoFinish {
                    assert!(!w.errors.is_empty(), "{misuse:?}: nothing refused");
                    assert!(
                        w.errors.iter().all(|(_, e)| *e == SinkError::Refused),
                        "{misuse:?}: {:?}",
                        w.errors
                    );
                }
                assert!(!w.inner.is_complete(), "{misuse:?}");
                drop(w);
                none_restored(&p);
            }
        }
    }
}

/// Arms a write failure as the regions begin: the `k`-th write from the
/// first region's `region` call on (none: count them), or the write the
/// piece holding `pages`' second chunk makes.
struct Arm {
    inner: StoreSink<SimPlatform>,
    p: SimPlatform,
    k: Option<u64>,
    in_pages: bool,
    swallow: bool,
    regions: usize,
    chunks: usize,
    start: u64,
    errors: Vec<SinkError>,
    /// At the named region's `end_region`: its next write fails (or, with
    /// `true`, its next write submission is refused).
    end_fault: Option<(&'static str, bool)>,
    /// At the first region's n-th chunk: the k-th write from there fails.
    chunk_fault: Option<(usize, u64)>,
    /// At the named region's `end_region`: this region released.
    release: Option<(&'static str, PageRegion<SimPlatform>)>,
    /// At the first region's n-th chunk: a commit hold taken on this store.
    hold_at: Option<(usize, Store<SimPlatform>)>,
    hold: Option<store_io_engine::CommitHold>,
    /// The trace's length when the hold was taken.
    hold_trace: usize,
    /// The offset of the chunk whose `data` call failed.
    failed_chunk: Option<u64>,
    name: String,
}

impl Arm {
    fn new(p: &SimPlatform, k: Option<u64>, in_pages: bool, swallow: bool) -> Self {
        Self::with(p, sink_opts(), k, in_pages, swallow)
    }

    fn with(
        p: &SimPlatform,
        so: StoreSinkOptions,
        k: Option<u64>,
        in_pages: bool,
        swallow: bool,
    ) -> Self {
        Self {
            inner: StoreSink::create(p.clone(), Path::new(TO), so).unwrap(),
            p: p.clone(),
            k,
            in_pages,
            swallow,
            regions: 0,
            chunks: 0,
            start: 0,
            errors: Vec::new(),
            end_fault: None,
            chunk_fault: None,
            release: None,
            hold_at: None,
            hold: None,
            hold_trace: 0,
            failed_chunk: None,
            name: String::new(),
        }
    }

    fn pass(&mut self, r: Result<(), SinkError>) -> Result<(), SinkError> {
        match r {
            Err(e) if self.swallow => {
                self.errors.push(e);
                Ok(())
            }
            r => r,
        }
    }

    /// Writes since the first region began.
    fn writes(&self) -> u64 {
        self.p.with_world(|w| w.writes()) - self.start
    }
}

impl ExportSink for Arm {
    fn begin(&mut self, header: &ExportHeader) -> Result<(), SinkError> {
        let r = self.inner.begin(header);
        self.pass(r)
    }

    fn region(&mut self, info: &RegionInfo) -> Result<(), SinkError> {
        self.regions += 1;
        self.chunks = 0;
        if self.regions == 1 {
            self.start = self.p.with_world(|w| w.writes());
            if let Some(k) = self.k {
                let at = self.start + k;
                self.p.with_world(|w| w.faults_mut().fail_write = Some(at));
            }
        }
        if info.name != "pages" {
            self.in_pages = false;
        }
        self.name = info.name.clone();
        let r = self.inner.region(info);
        self.pass(r)
    }

    fn data(&mut self, chunk: Chunk) -> Result<(), SinkError> {
        self.chunks += 1;
        if self.in_pages && self.chunks == 2 {
            self.p.with_world(|w| {
                let n = w.writes();
                w.faults_mut().fail_write = Some(n + 1);
            });
        }
        if self.regions == 1 {
            if let Some((n, k)) = self.chunk_fault {
                if self.chunks == n {
                    self.p.with_world(|w| {
                        let at = w.writes() + k;
                        w.faults_mut().fail_write = Some(at);
                    });
                }
            }
            if self
                .hold_at
                .as_ref()
                .is_some_and(|(n, _)| *n == self.chunks)
            {
                self.hold_trace = self.p.with_world(|w| w.trace().len());
                self.hold = self.hold_at.as_ref().map(|(_, s)| s.commit_hold());
            }
        }
        let at = chunk.offset();
        let r = self.inner.data(chunk);
        if r.is_err() && self.failed_chunk.is_none() {
            self.failed_chunk = Some(at);
        }
        self.pass(r)
    }

    fn end_region(&mut self) -> Result<(), SinkError> {
        if let Some((name, submit)) = self.end_fault {
            if name == self.name {
                self.p.with_world(|w| {
                    if submit {
                        let at = w.write_submits() + 1;
                        w.faults_mut().fail_submit = Some((at, 5));
                    } else {
                        let at = w.writes() + 1;
                        w.faults_mut().fail_write = Some(at);
                    }
                });
            }
        }
        if self.release.as_ref().is_some_and(|(n, _)| *n == self.name) {
            if let Some((_, r)) = self.release.take() {
                r.release().unwrap();
            }
        }
        let r = self.inner.end_region();
        self.pass(r)
    }

    fn finish(&mut self) -> Result<(), SinkError> {
        let r = self.inner.finish();
        self.pass(r)
    }
}

/// `finish` inside a store's only region, before its end: refused (the
/// region was never committed), so nothing is renamed into place.
#[test]
fn test_finish_inside_a_region_is_refused() {
    let p = SimPlatform::new(SimConfig::volatile(5150));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let r = s.provision_page_region("pages", 4 * CHUNK as u64).unwrap();
    let _d = r
        .write_durable(r.pos(0).unwrap(), &stamped(7, 4 * CHUNK))
        .unwrap();
    let mut w = Wrap::new(&p, Misuse::FinishInRegion);
    let e = export_into(&s, &mut w).unwrap_err();
    assert!(!e.report.complete);
    assert_eq!(w.errors.first().map(|(c, _)| *c), Some("finish"));
    assert!(!w.inner.is_complete());
    drop(w);
    none_restored(&p);
}

/// The export fails at a write of the new store that fails mid-region,
/// also through a wrapper that swallowed it: the report clamped to the
/// region and offset where that write began, no store under the target
/// name; a sink into the same directory then restores it whole (the
/// temporary removed).
#[test]
fn test_a_failed_write_mid_region_leaves_no_store() {
    for swallow in [false, true] {
        let p = SimPlatform::new(SimConfig::volatile(5200 + u64::from(swallow)));
        let s = every_kind(&p);
        let want = kept(&s);
        let mut a = Arm::new(&p, None, true, swallow);
        let e = export_into(&s, &mut a).unwrap_err();
        p.with_world(|w| w.faults_mut().fail_write = None);
        if swallow {
            assert!(
                matches!(a.errors.first(), Some(SinkError::Io(_))),
                "{:?}",
                a.errors
            );
        } else {
            assert!(matches!(e.sink, Some(SinkError::Io(_))), "{:?}", e.sink);
        }
        assert!(
            matches!(e.error, Error::DurabilityUnknown { .. }),
            "{:?}",
            e.error
        );
        let r = &e.report;
        assert!(!r.complete);
        assert_eq!(r.region.as_deref(), Some("pages"));
        assert_eq!(r.regions, 0);
        // Nothing past the failed write's start counted.
        assert_eq!(r.bytes, r.offset);
        // The second piece: 128 KiB pieces, the first holding the two
        // header blocks and 120 KiB of the region.
        assert_eq!(r.offset, 120 << 10);
        assert!(!a.inner.is_complete());
        drop(a);
        none_restored(&p);
        assert!(temp_there(&p), "the failed store's temporary");
        let mut ss = sink(&p);
        assert!(export_into(&s, &mut ss).unwrap().complete);
        drop(ss);
        assert!(!temp_there(&p));
        restored_matches(&p, &want);
    }
}

/// Every write a small store's restore makes from its first region on
/// failed in turn: each but the last fails the export, no store under the
/// target name, the next sink into the directory starting afresh (the
/// temporary removed); the last, the clean-close record the store's drop
/// writes after every byte was flushed, is best effort (the next open
/// judges the store not closed cleanly, which is safe): the store renamed
/// into place is whole.
#[test]
fn test_any_failed_write_leaves_no_store_and_a_retry_restores() {
    let p = SimPlatform::new(SimConfig::volatile(5210));
    let s = small(&p);
    let want = kept(&s);
    let all = {
        let q = SimPlatform::new(SimConfig::volatile(5210));
        let t = small(&q);
        let mut a = Arm::new(&q, None, false, false);
        assert!(t.export(&mut a).unwrap().complete);
        a.writes()
    };
    assert!(all > 5, "{all} writes");
    for k in 1..all {
        let mut a = Arm::new(&p, Some(k), false, false);
        let e = export_into(&s, &mut a).unwrap_err();
        p.with_world(|w| w.faults_mut().fail_write = None);
        assert!(!e.report.complete, "write {k}");
        assert!(!a.inner.is_complete());
        drop(a);
        none_restored(&p);
    }
    let mut a = Arm::new(&p, Some(all), false, false);
    assert!(export_into(&s, &mut a).unwrap().complete);
    p.with_world(|w| w.faults_mut().fail_write = None);
    drop(a);
    assert!(!temp_there(&p));
    restored_matches(&p, &want);
}

/// The power cut at every step of a restore, then a crash that keeps,
/// drops or tears what was not flushed: a store under the target name is
/// whole, and where there is none a sink into the same directory restores
/// it.
#[test]
fn test_a_power_cut_at_any_step_leaves_no_store_or_the_whole_one() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    // Built once (the source store, closed); each run on a fork of it,
    // the store reopened with its append region resumed at its durable
    // length, so it exports what the store that made it did.
    let base = {
        let p = SimPlatform::new(SimConfig::volatile(5220));
        let s = small(&p);
        let want = kept(&s);
        let durable = s.append_region("log").unwrap().durable_through();
        drop(s);
        (p, want, durable)
    };
    // `cut`: the cut the fixture is for (`None`: the step count's), named
    // by a failure here.
    let fixture = |cut: Option<u64>| {
        let p = base.0.fork();
        let s = Store::open(p.clone(), Path::new(DIR), opts())
            .unwrap_or_else(|e| panic!("cut {cut:?}: reopen the source: {e:?}"));
        let _at = s
            .append_region("log")
            .and_then(|r| r.resume_at(base.2))
            .unwrap_or_else(|e| panic!("cut {cut:?}: resume the log: {e:?}"));
        (p, s, &base.1)
    };
    let probe = |p: &SimPlatform, cut: u64| {
        let steps = Arc::new(AtomicU64::new(0));
        let n = Arc::clone(&steps);
        p.with_world(|w| {
            w.set_step_probe(Some(Arc::new(move || {
                n.fetch_add(1, Ordering::Relaxed) >= cut
            })))
        });
        steps
    };
    let all = {
        let (p, s, _w) = fixture(None);
        let steps = probe(&p, u64::MAX);
        let mut ss = sink(&p);
        assert!(export_into(&s, &mut ss).unwrap().complete);
        steps.load(Ordering::Relaxed)
    };
    assert!(all > 10, "{all} steps");
    each_cut(all, |cut| {
        let (p, s, want) = fixture(Some(cut));
        let _steps = probe(&p, cut);
        let done = match StoreSink::create(p.clone(), Path::new(TO), sink_opts()) {
            Ok(mut ss) => export_into(&s, &mut ss).is_ok(),
            Err(_) => false,
        };
        drop(s);
        let mode = if cut % 2 == 0 {
            CrashMode::Random
        } else {
            CrashMode::LoseAll
        };
        p.with_world(|w| {
            w.set_step_probe(None);
            w.crash(&mode);
        });
        match Store::open(p.clone(), Path::new(TO), opts()) {
            Ok(t) => {
                drop(t);
                restored_matches(&p, want);
            }
            Err(e) => {
                assert!(!done, "cut {cut}: a restore that returned Ok is not there");
                assert!(matches!(e, Error::NotFound { .. }), "cut {cut}: {e:?}");
                let s = Store::open(p.clone(), Path::new(DIR), opts()).unwrap();
                let mut ss = sink(&p);
                assert!(export_into(&s, &mut ss).unwrap().complete, "cut {cut}");
                drop(ss);
                restored_matches(&p, want);
            }
        }
    });
}

/// Runs `f` for every cut in `0..=all`, each exactly once, spread over up
/// to four threads (each cut on a world of its own: a fork). A cut's
/// failure fails the test with that cut's message.
fn each_cut(all: u64, f: impl Fn(u64) + Sync) {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(4)) as u64;
    std::thread::scope(|scope| {
        for first in 0..threads {
            let f = &f;
            let _worker = scope.spawn(move || {
                let mut cut = first;
                while cut <= all {
                    f(cut);
                    cut += threads;
                }
            });
        }
    });
}

/// With commits held active (the group paced), an export into a store on
/// the source's own device completes: the sink's writes and the stream's
/// reads are one call, and each admission collects the other's pieces
/// first.
#[test]
fn test_a_paced_restore_on_the_same_device_completes() {
    let p = SimPlatform::new(SimConfig::volatile(5300));
    let s = every_kind(&p);
    let want = kept(&s);
    let hold = s.commit_hold();
    assert!(s.qos().active);
    let mut o = ExportOptions::default();
    o.depth = Some(4);
    let mut ss = sink(&p);
    let r = s.export_with(&mut ss, o).unwrap();
    assert!(r.complete);
    drop(hold);
    drop(ss);
    restored_matches(&p, &want);
}

/// With commits quiet (unpaced, several reads in flight at the call's
/// shared cap), the same completes.
#[test]
fn test_an_unpaced_restore_on_the_same_device_completes() {
    let p = SimPlatform::new(SimConfig::volatile(5301));
    let s = every_kind(&p);
    let want = kept(&s);
    p.with_world(|w| w.advance_clock_ns(10 * SECOND));
    assert!(!s.qos().active);
    let mut o = ExportOptions::default();
    o.depth = Some(8);
    let mut ss = sink(&p);
    let r = s.export_with(&mut ss, o).unwrap();
    assert!(r.complete);
    drop(ss);
    restored_matches(&p, &want);
}

/// A commit hold taken while a caller's wrapper (its `settle` the
/// default) holds a chunk, the call unpaced until then: the sink's next
/// admission is refused by the shrunken cap, collects the stream's reads in
/// flight, and is admitted; the call completes.
#[test]
fn test_a_hold_taken_while_a_chunk_is_held_completes() {
    let p = SimPlatform::new(SimConfig::volatile(5302));
    let s = every_kind(&p);
    let want = kept(&s);
    p.with_world(|w| w.advance_clock_ns(10 * SECOND));
    assert!(!s.qos().active);
    let mut w = Wrap::new(&p, Misuse::HoldAtFirstChunk);
    w.hold_on = Some(s.clone());
    let mut o = ExportOptions::default();
    o.depth = Some(8);
    let r = s.export_with(&mut w, o).unwrap();
    assert!(r.complete && w.errors.is_empty(), "{:?}", w.errors);
    assert!(w.hold.is_some(), "the hold was never taken");
    assert!(s.qos().active);
    drop(w);
    restored_matches(&p, &want);
}

/// The new store on another device, the source's commits held: each
/// part charged to its own device's record, and the call completes.
#[test]
fn test_a_restore_across_devices_completes_with_commits_held() {
    let p = SimPlatform::new(SimConfig::volatile(5303).with_device(DeviceKind::VolatileCache));
    p.with_world(|w| w.place(TO, 1));
    let s = every_kind(&p);
    let want = kept(&s);
    let hold = s.commit_hold();
    let mut ss = sink(&p);
    assert!(export_into(&s, &mut ss).unwrap().complete);
    drop(hold);
    drop(ss);
    restored_matches(&p, &want);
}

/// An export of 64 KiB blocks from a source paced at 20 KiB pieces into
/// a store of smaller buffers: every write to the new store in whole
/// blocks, every byte restored.
#[test]
fn test_a_restore_of_large_blocks_with_small_source_pieces() {
    blocks_against_pieces(5400, 1 << 16, 1 << 16);
}

/// The same with 8 KiB blocks, smaller than the source's 20 KiB piece, and
/// a new store of 16 KiB buffers.
#[test]
fn test_a_restore_of_blocks_under_the_source_s_piece() {
    blocks_against_pieces(5401, 8 << 10, 16 << 10);
}

/// An export of `block`-byte blocks from a store paced at 20 KiB pieces
/// into a store of the same blocks and `max_io` buffers on the same device
/// (so in its group, whose piece is its first member's), commits held:
/// every write to the new store's container in whole blocks.
fn blocks_against_pieces(seed: u64, block: u64, max_io: usize) {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    let big = StoreOptions {
        block_size: Some(block as u32),
        background_io: 20 << 10,
        ..opts()
    };
    let s = Store::create(p.clone(), Path::new(DIR), big.clone()).unwrap();
    assert_eq!(s.report().block_size, block);
    let r = s.provision_page_region("p", 64 * block).unwrap();
    let _d = r
        .write_durable(r.pos(0).unwrap(), &stamped(5, (64 * block) as usize))
        .unwrap();
    let log = s.provision_append_region("log", 8 * block).unwrap();
    let t = log.append(&stamped(6, 3 * block as usize + 100)).unwrap();
    let _d = log.sync_through(&t).unwrap();
    let want = kept(&s);
    let mut so = sink_opts();
    so.store.block_size = Some(block as u32);
    so.store.max_io = max_io;
    let hold = s.commit_hold();
    let mut ss = StoreSink::create(p.clone(), Path::new(TO), so).unwrap();
    assert!(export_into(&s, &mut ss).unwrap().complete);
    drop(hold);
    drop(ss);
    let t = Store::open(p.clone(), Path::new(TO), big).unwrap();
    let got = kept(&t);
    assert!(got.regions[0].1 == want.regions[0].1);
    drop(t);
    let odd: Vec<(u64, u64)> = p.with_world(|w| {
        let id = w
            .file_id(&PathBuf::from(TO), store_io_engine::CONTAINER)
            .unwrap();
        w.trace()
            .iter()
            .filter_map(|e| match e {
                store_io_sim::world::TraceEvent::Write {
                    file, offset, len, ..
                } if *file == id && (offset % block != 0 || len % block != 0) => {
                    Some((*offset, *len))
                }
                _ => None,
            })
            .collect()
    });
    assert!(odd.is_empty(), "writes not in whole blocks: {odd:?}");
}

/// The new store's own region header write refused at submission (the
/// sink's own failure, not an ended admission): recorded on the call at
/// that region, so the region is not counted, also through a wrapper that
/// swallows the error. A refused submission writes nothing: the error is
/// that region's `NotWritten(Interrupted)` (the slot write's own), never a
/// later call's refusal.
#[test]
fn test_a_refused_header_write_is_the_sink_s_failure() {
    for swallow in [false, true] {
        let p = SimPlatform::new(SimConfig::volatile(5600 + u64::from(swallow)));
        let s = every_kind(&p);
        let mut a = Arm::new(&p, None, false, swallow);
        a.end_fault = Some(("pages", true));
        let e = export_into(&s, &mut a).unwrap_err();
        p.with_world(|w| w.faults_mut().fail_submit = None);
        assert_eq!(
            cause(&e.error),
            Some(NotWrittenCause::Interrupted),
            "{:?}",
            e.error
        );
        let r = &e.report;
        assert!(!r.complete);
        assert_eq!(r.regions, 0, "the region was counted");
        assert_eq!(r.region.as_deref(), Some("pages"));
        if swallow {
            assert_eq!(a.errors.first(), Some(&SinkError::Refused));
        } else {
            assert_eq!(e.sink, Some(SinkError::Refused));
        }
        drop(a);
        none_restored(&p);
    }
}

/// A write failing inside one `data` call, past the start of the chunk
/// it carries (the first chunk, of 256 KiB, filling two 128 KiB pieces of
/// the new store, whose buffers are 128 KiB: the smallest `max_io` a store
/// takes; the second piece failing, at 120 KiB of the region): the report
/// never counts past the chunk's start.
#[test]
fn test_a_write_failing_inside_a_chunk_counts_none_of_it() {
    let p = SimPlatform::new(SimConfig::volatile(5610));
    let src = StoreOptions {
        max_io: 256 << 10,
        ..opts()
    };
    let s = Store::create(p.clone(), Path::new(DIR), src).unwrap();
    let r = s.provision_page_region("pages", 1 << 20).unwrap();
    let _d = r
        .write_durable(r.pos(0).unwrap(), &stamped(8, 1 << 20))
        .unwrap();
    let mut so = sink_opts();
    so.store.max_io = 128 << 10;
    let mut a = Arm::with(&p, so, None, false, false);
    a.chunk_fault = Some((1, 2));
    let e = export_into(&s, &mut a).unwrap_err();
    p.with_world(|w| w.faults_mut().fail_write = None);
    assert!(
        matches!(e.error, Error::DurabilityUnknown { .. }),
        "{:?}",
        e.error
    );
    assert_eq!(e.report.region.as_deref(), Some("pages"));
    assert_eq!(a.failed_chunk, Some(0), "not the first chunk's call");
    assert_eq!(e.report.offset, 0, "past the chunk's start");
    assert_eq!(e.report.bytes, 0);
    drop(a);
    none_restored(&p);
}

/// The last piece of an append region (its durable prefix's tail and the
/// pattern) failing at `end_region`: the report at that piece's start.
#[test]
fn test_a_last_piece_failing_at_the_region_s_end_is_where_it_began() {
    let p = SimPlatform::new(SimConfig::volatile(5611));
    let s = every_kind(&p);
    let mut a = Arm::new(&p, None, false, false);
    a.end_fault = Some(("log", false));
    let e = export_into(&s, &mut a).unwrap_err();
    p.with_world(|w| w.faults_mut().fail_write = None);
    assert!(
        matches!(e.error, Error::DurabilityUnknown { .. }),
        "{:?}",
        e.error
    );
    assert_eq!(e.report.region.as_deref(), Some("log"));
    assert_eq!(e.report.offset, 120 << 10);
    drop(a);
    none_restored(&p);
}

/// A store of one page region of four chunks, and where its first bytes
/// lie in the container.
fn four_chunks(p: &SimPlatform) -> (Store<SimPlatform>, u64) {
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let r = s.provision_page_region("pages", 4 * CHUNK as u64).unwrap();
    let _d = r
        .write_durable(r.pos(0).unwrap(), &stamped(0, 4 * CHUNK))
        .unwrap();
    let first = stamped(0, 64);
    let base = p.with_world(|w| {
        let media = w.media_of(&PathBuf::from(DIR), CONTAINER).unwrap();
        media.windows(64).position(|win| win == &first[..]).unwrap() as u64
    });
    (s, base)
}

/// A media error mid-region stops the export with the sink holding
/// accepted bytes it has not written: the report counts only its writes
/// done (`StoreSink`: up to its second piece's start; `FileSink`, staging
/// 128 KiB pieces, the group's: its first piece). The store sink restores
/// nothing, and a later
/// `end_region` or `finish` by hand is refused.
#[test]
fn test_an_export_side_stop_counts_only_the_sink_s_writes() {
    let bad = CHUNK as u64 + 8192..CHUNK as u64 + 12288;
    let arm = |p: &SimPlatform, base: u64| {
        p.with_world(|w| {
            let file = w.file_id(&PathBuf::from(DIR), CONTAINER).unwrap();
            *w.faults_mut() = FaultPlan {
                bad_ranges: vec![(file, base + bad.start, base + bad.end)],
                ..FaultPlan::default()
            };
        });
    };
    let media = |e: &ExportError| {
        matches!(
            e.error,
            Error::Corruption {
                kind: CorruptionKind::MediaError,
                ..
            }
        )
    };
    // StoreSink.
    let p = SimPlatform::new(SimConfig::volatile(5620));
    let (s, base) = four_chunks(&p);
    arm(&p, base);
    let mut ss = sink(&p);
    let e = export_into(&s, &mut ss).unwrap_err();
    assert!(media(&e), "{:?}", e.error);
    assert_eq!(e.report.region.as_deref(), Some("pages"));
    assert_eq!(e.report.offset, 120 << 10, "accepted {}", bad.start);
    assert_eq!(e.report.bytes, 120 << 10);
    assert_eq!(ss.end_region(), Err(SinkError::Refused));
    assert_eq!(ss.finish(), Err(SinkError::Refused));
    assert!(!ss.is_complete());
    drop(ss);
    none_restored(&p);
    // FileSink.
    let p = SimPlatform::new(SimConfig::volatile(5621));
    let (s, base) = four_chunks(&p);
    arm(&p, base);
    drop(p.open_dir(Path::new("/backup"), true).unwrap());
    let mut fs =
        FileSink::create(p.clone(), Path::new("/backup"), FileSinkOptions::default()).unwrap();
    let e = export_into(&s, &mut fs).unwrap_err();
    assert!(media(&e), "{:?}", e.error);
    assert_eq!(e.report.region.as_deref(), Some("pages"));
    assert_eq!(e.report.offset, CHUNK as u64, "accepted {}", bad.start);
    assert_eq!(e.report.bytes, CHUNK as u64);
    let info = p.with_world(|w| {
        w.media_of(&PathBuf::from("/backup"), REGIONS_INFO)
            .map(<[u8]>::to_vec)
    });
    assert!(info.unwrap_or_default().is_empty());
}

/// A region released at its turn stops the export between regions: the
/// sink restores nothing, and a later `finish` by hand is refused (fewer
/// regions than the header counts).
#[test]
fn test_a_region_released_at_its_turn_restores_nothing() {
    let p = SimPlatform::new(SimConfig::volatile(5622));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    for (name, salt) in [("a", 1), ("b", 2)] {
        let r = s.provision_page_region(name, 2 * B).unwrap();
        let _d = r
            .write_durable(r.pos(0).unwrap(), &stamped(salt, 2 * B as usize))
            .unwrap();
    }
    let mut a = Arm::new(&p, None, false, false);
    a.release = Some(("a", s.page_region("b").unwrap()));
    let e = export_into(&s, &mut a).unwrap_err();
    assert_eq!(cause(&e.error), Some(NotWrittenCause::NotReady), "{e:?}");
    assert_eq!(e.report.regions, 1);
    assert_eq!(a.inner.finish(), Err(SinkError::Refused));
    drop(a);
    none_restored(&p);
}

/// The sink keeps the header it was given, the volume's facts with it (a
/// restore never hides what the export left out).
#[test]
fn test_the_sink_keeps_the_export_s_header_and_its_facts() {
    let p = SimPlatform::new(SimConfig::volatile(5630));
    let s = small(&p);
    let want = kept(&s).header.unwrap();
    let mut ss = sink(&p);
    assert!(ss.header().is_none());
    assert!(export_into(&s, &mut ss).unwrap().complete);
    assert_eq!(ss.header(), Some(&want));
    drop(ss);
    // Facts a caller's framing carried (a wrapper adds them): kept as given.
    struct Facts(StoreSink<SimPlatform>);
    impl ExportSink for Facts {
        fn begin(&mut self, header: &ExportHeader) -> Result<(), SinkError> {
            let mut h = header.clone();
            h.facts.lost_pages = vec![3];
            h.facts.stale_pages = vec![5];
            h.facts.superseded = vec!["pages".to_owned()];
            self.0.begin(&h)
        }
        fn region(&mut self, info: &RegionInfo) -> Result<(), SinkError> {
            self.0.region(info)
        }
        fn data(&mut self, chunk: Chunk) -> Result<(), SinkError> {
            self.0.data(chunk)
        }
        fn end_region(&mut self) -> Result<(), SinkError> {
            self.0.end_region()
        }
        fn finish(&mut self) -> Result<(), SinkError> {
            self.0.finish()
        }
    }
    let q = SimPlatform::new(SimConfig::volatile(5631));
    let t = small(&q);
    let mut f = Facts(StoreSink::create(q.clone(), Path::new(TO), sink_opts()).unwrap());
    assert!(export_into(&t, &mut f).unwrap().complete);
    let h = f.0.header().unwrap();
    assert_eq!(h.facts.lost_pages, vec![3]);
    assert_eq!(h.facts.stale_pages, vec![5]);
    assert_eq!(h.facts.superseded, vec!["pages".to_owned()]);
}

/// A sink driven by hand, outside any export (its own call): an append
/// region with nothing durable and a slot never committed, no bytes to
/// push, restore whole.
#[test]
fn test_a_sink_driven_by_hand_restores() {
    let p = SimPlatform::new(SimConfig::volatile(5640));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let _log = s.provision_append_region("log", 8 * B).unwrap();
    let _e = s.provision_slot("empty").unwrap();
    let want = kept(&s);
    assert!(want.regions.iter().all(|(i, _)| i.stream_len() == 0));
    let mut ss = sink(&p);
    ss.begin(want.header.as_ref().unwrap()).unwrap();
    for (info, _) in &want.regions {
        ss.region(info).unwrap();
        ss.end_region().unwrap();
    }
    ss.finish().unwrap();
    assert!(ss.is_complete() && ss.restored().len() == 2);
    drop(ss);
    restored_matches(&p, &want);
}

/// Across devices with commits quiet (unpaced, each part on its own
/// device's record): completes.
#[test]
fn test_a_restore_across_devices_completes_with_commits_quiet() {
    let p = SimPlatform::new(SimConfig::volatile(5641).with_device(DeviceKind::VolatileCache));
    p.with_world(|w| w.place(TO, 1));
    let s = every_kind(&p);
    let want = kept(&s);
    p.with_world(|w| w.advance_clock_ns(10 * SECOND));
    assert!(!s.qos().active);
    let mut ss = sink(&p);
    assert!(export_into(&s, &mut ss).unwrap().complete);
    drop(ss);
    restored_matches(&p, &want);
}

/// A commit hold taken at the second chunk while the sink fills a piece
/// sized unpaced (up to the new store's chunk): that piece goes out as
/// pieces of the paced size, consecutive writes with no read between them
/// (a split: one call's), each charged to the call when it was submitted
/// (each write paired with its own submission's sample of the group's
/// pieces in flight), and from the hold on no write past `bound`.
fn split_under_hold(seed: u64, src: StoreOptions, so: StoreSinkOptions, bound: u64) {
    use store_io_sim::world::TraceEvent;

    let p = SimPlatform::new(SimConfig::volatile(seed));
    let s = Store::create(p.clone(), Path::new(DIR), src).unwrap();
    let r = s.provision_page_region("pages", 4 << 20).unwrap();
    let _d = r
        .write_durable(r.pos(0).unwrap(), &stamped(9, 4 << 20))
        .unwrap();
    let want = kept(&s);
    p.with_world(|w| w.advance_clock_ns(10 * SECOND));
    assert!(!s.qos().active);
    let mut a = Arm::with(&p, so, None, false, false);
    a.hold_at = Some((2, s.clone()));
    // At every submission from the hold on: the trace's length then, and
    // the group's pieces in flight.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (out, s2, q) = (Arc::clone(&seen), s.clone(), p.clone());
    p.with_world(|w| {
        w.set_submit_probe(Some(Arc::new(move || {
            if s2.qos().active {
                let at = q.with_world(|w| w.trace().len());
                out.lock().unwrap().push((at, s2.qos().in_flight));
            }
        })))
    });
    let r = export_into(&s, &mut a);
    p.with_world(|w| w.set_submit_probe(None));
    assert!(r.unwrap().complete);
    assert!(a.hold.is_some(), "the hold was never taken");
    let hold_trace = a.hold_trace;
    drop(a);
    let seen = seen.lock().unwrap().clone();
    // The new store's writes from the hold on, each at its completion's
    // place in the trace; and whether a read came between two of them.
    let (writes, reads_between): (Vec<(usize, u64, u64)>, Vec<bool>) = p.with_world(|w| {
        let id = w.file_id(&PathBuf::from(TO), CONTAINER).unwrap();
        let mut writes = Vec::new();
        let mut between = Vec::new();
        let mut read = false;
        for (t, e) in w.trace().iter().enumerate().skip(hold_trace) {
            match e {
                TraceEvent::Write {
                    file, offset, len, ..
                } if *file == id => {
                    writes.push((t, *offset, *len));
                    between.push(read);
                    read = false;
                }
                TraceEvent::Read { .. } => read = true,
                _ => {}
            }
        }
        (writes, between)
    });
    assert!(!writes.is_empty());
    // Each write's own submission: the last one before its completion (its
    // call submits nothing else while it waits).
    for &(t, offset, len) in &writes {
        let sample = seen.iter().rev().find(|(at, _)| *at <= t);
        assert!(
            sample.is_some_and(|(_, n)| *n >= 1),
            "the write at {offset} ({len} bytes) not charged: {sample:?}"
        );
        assert!(
            len <= bound,
            "a write past the paced piece: {len} at {offset}"
        );
    }
    // A split: the piece sized before the hold (no write of it before
    // then) is the first written after it, as writes of one size,
    // contiguous, with no read between (one call's).
    assert!(writes.len() >= 2);
    let ((_, a, n), (_, b, m)) = (writes[0], writes[1]);
    assert!(
        !reads_between[1] && n == m && a + n == b,
        "the first piece was not split: {writes:?}"
    );
    restored_matches(&p, &want);
}

/// The group's piece a power of two (the default, 128 KiB here: the
/// source's `max_io`), the new store's chunk 1 MiB.
#[test]
fn test_a_hold_taken_mid_piece_splits_it_and_every_write_is_charged() {
    let mut so = sink_opts();
    so.store.max_io = 1 << 20;
    split_under_hold(5650, opts(), so, CHUNK as u64);
}

/// The group's piece 192 KiB (no power of two), the new store asking for
/// a single buffer per class (a pool keeps at least four): the split at
/// that piece completes, its writes in whole blocks of at most the piece.
#[test]
fn test_a_split_at_an_odd_piece_with_the_fewest_buffers_completes() {
    let src = StoreOptions {
        background_io: 192 << 10,
        max_io: 1 << 20,
        ..opts()
    };
    let mut so = sink_opts();
    so.store.max_io = 1 << 20;
    so.store.buffers_per_class = 1;
    split_under_hold(5651, src, so, 192 << 10);
}

/// A header the sink refused is never kept: `header()` stays `None`.
#[test]
fn test_a_refused_header_is_not_kept() {
    let p = SimPlatform::new(SimConfig::volatile(5652));
    let s = small(&p);
    let mut w = Wrap::new(&p, Misuse::BadBlock);
    let _e = export_into(&s, &mut w).unwrap_err();
    assert!(w.inner.header().is_none());
}

/// On Windows a restored capped region's slack is written with its
/// pattern at provisioning (as a provisioning writes it): the restored
/// slack holds the source's keyed pattern, never unwritten space.
#[test]
fn test_a_restored_capped_region_keeps_its_slack_written_on_windows() {
    let probe = SimPlatform::new(SimConfig::volatile(5660));
    let mut evidence = Store::create(probe, Path::new(DIR), opts())
        .unwrap()
        .report()
        .evidence
        .clone();
    evidence.platform = store_io_core::evidence::PlatformKind::Windows;
    let mut o = opts();
    o.trust.override_refusal = true;
    let p = SimPlatform::new(SimConfig {
        evidence: Some(evidence),
        valid_data_model: true,
        ..SimConfig::volatile(5661)
    });
    let s = Store::create(p.clone(), Path::new(DIR), o.clone()).unwrap();
    let r = s
        .provision_page_region_with(
            "capped",
            RegionOptions::default().size(16 * B).max_size(256 * B),
        )
        .unwrap();
    let _d = r
        .write_durable(r.pos(0).unwrap(), &[9; B as usize])
        .unwrap();
    drop(r);
    let mut so = sink_opts();
    so.store = o;
    let mut ss = StoreSink::create(p.clone(), Path::new(TO), so).unwrap();
    assert!(export_into(&s, &mut ss).unwrap().complete);
    drop(ss);
    drop(s);
    let slack = |dir: &str| {
        let f = audit_in(&p, dir)
            .into_iter()
            .find(|f| f.name == "capped")
            .unwrap();
        assert_eq!(f.capacity, 256 * B, "{dir}");
        let (from, to) = (f.data_offset + 16 * B, f.data_offset + 256 * B);
        p.with_world(|w| {
            w.media_of(&PathBuf::from(dir), CONTAINER).unwrap()[from as usize..to as usize].to_vec()
        })
    };
    let (want, got) = (slack(DIR), slack(TO));
    assert!(want.iter().any(|&b| b != 0), "the source's slack is keyed");
    assert!(got == want, "the restored slack is the pattern, written");
}
