//! `FileSink` (PLANNING R5.6, D-168): an export written to a directory, one
//! data file per region and a `regions.info` record; what it is pushed
//! checked, every refusal sticky; its failures the export's, whatever a
//! caller's wrapper returned.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use store_io_core::error::{Error, NotWrittenCause, Op, ReserveTag};
use store_io_engine::{
    Chunk, ExportError, ExportHeader, ExportOptions, ExportReport, ExportSink, FileSink,
    FileSinkOptions, REGIONS_INFO, RegionInfo, RegionOptions, SinkError, Store, StoreOptions,
};
use store_io_format::crc32c::crc32c;
use store_io_format::exportinfo::{self, SPAN};
use store_io_platform::Platform;
use store_io_sim::{SimConfig, SimPlatform};

const DIR: &str = "/db";
const OUT: &str = "/backup";
const CHUNK: usize = 1 << 17;
const B: u64 = 4096;

fn opts() -> StoreOptions {
    StoreOptions {
        queues: 2,
        buffers_per_class: 64,
        max_io: CHUNK,
        ..StoreOptions::default()
    }
}

fn store(seed: u64) -> (SimPlatform, Store<SimPlatform>) {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    (p, s)
}

/// Each 8-byte word holds its own offset plus `salt`.
fn stamped(salt: u64, len: usize) -> Vec<u8> {
    (0..len.div_ceil(8) as u64)
        .flat_map(|w| (w * 8 + salt).to_le_bytes())
        .take(len)
        .collect()
}

fn page(s: &Store<SimPlatform>, name: &str, len: u64, salt: u64) -> Vec<u8> {
    let r = s.provision_page_region(name, len).unwrap();
    let data = stamped(salt, len as usize);
    let _d = r.write_durable(r.pos(0).unwrap(), &data).unwrap();
    data
}

fn cause(e: &Error) -> Option<NotWrittenCause> {
    match e {
        Error::NotWritten { cause, .. } => Some(*cause),
        _ => None,
    }
}

/// An empty directory for the sink.
fn out_dir(p: &SimPlatform) {
    drop(p.open_dir(Path::new(OUT), true).unwrap());
}

fn sink(p: &SimPlatform) -> FileSink<SimPlatform> {
    FileSink::create(p.clone(), Path::new(OUT), FileSinkOptions::default()).unwrap()
}

/// A file's durable bytes in the sink's directory (`None`: not there).
fn media(p: &SimPlatform, name: &str) -> Option<Vec<u8>> {
    p.with_world(|w| w.media_of(&PathBuf::from(OUT), name).map(<[u8]>::to_vec))
}

/// What each region's bytes are, by a sink that keeps them.
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

/// What a wrapper does to the calls it forwards to its `FileSink`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Misuse {
    None,
    /// `begin` forwarded twice.
    BeginTwice,
    /// Region 0's `end_region` not forwarded.
    SkipFirstEnd,
    /// The first chunk not forwarded (the next is not at the region's next
    /// byte).
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
    /// The header forwarded with a block size that is no power of two.
    BadBlock,
    /// The header forwarded with a region count past the format's table.
    HugeCount,
    /// `finish` not forwarded: the sink never finishes.
    NoFinish,
}

/// A caller's wrapper around a `FileSink` that misuses it as told and
/// swallows every error the sink returns (so only the call can tell the
/// export the sink failed), keeping which call each came from.
struct Wrap {
    inner: FileSink<SimPlatform>,
    misuse: Misuse,
    regions: usize,
    chunks: usize,
    first: String,
    errors: Vec<(&'static str, SinkError)>,
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
        }
    }

    fn eat(&mut self, call: &'static str, r: Result<(), SinkError>) -> Result<(), SinkError> {
        if let Err(e) = r {
            self.errors.push((call, e));
        }
        Ok(())
    }

    /// Whether the current region's calls are forwarded.
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
        if self.chunks == 1 {
            match self.misuse {
                Misuse::DropFirstChunk => return Ok(()),
                Misuse::EndEarly => {
                    let r = self.inner.end_region();
                    self.eat("end_region", r)?;
                }
                _ => {}
            }
        }
        let r = self.inner.data(chunk);
        self.eat("data", r)
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

/// A store of every kind: a page region of three spans and a bit, a
/// capped page region, a positioned append region, a slot and an empty
/// slot.
fn every_kind(seed: u64) -> (SimPlatform, Store<SimPlatform>) {
    let (p, s) = store(seed);
    let _p = page(&s, "pages", 2 * SPAN + 3 * B, 1);
    let capped = s
        .provision_page_region_with(
            "capped",
            RegionOptions::default().size(4 * B).max_size(16 * B),
        )
        .unwrap();
    let _d = capped
        .write_durable(capped.pos(0).unwrap(), &stamped(2, 4 * B as usize))
        .unwrap();
    let log = s.provision_append_region("log", 4 * CHUNK as u64).unwrap();
    let t = log.append(&stamped(3, CHUNK + 1000)).unwrap();
    let _d = log.sync_through(&t).unwrap();
    let _d = s
        .provision_slot("slot")
        .unwrap()
        .commit(b"the object")
        .unwrap();
    let _e = s.provision_slot("empty").unwrap();
    out_dir(&p);
    (p, s)
}

fn export(
    s: &Store<SimPlatform>,
    sink: &mut dyn ExportSink,
) -> Result<ExportReport, Box<ExportError>> {
    s.export(sink)
}

/// Every region's bytes in its own file, exactly its stream length, and a
/// `regions.info` that decodes to the header, the facts, every region's
/// attributes and the CRC-32C of each 1 MiB span of each file.
#[test]
fn test_every_kind_is_written_and_described() {
    let (p, s) = every_kind(3000);
    let mut keep = Keep::default();
    let _r = export(&s, &mut keep).unwrap();
    let mut fs = sink(&p);
    let r = export(&s, &mut fs).unwrap();
    assert!(r.complete);
    assert_eq!(r.regions, 5);
    let h = keep.header.unwrap();
    for (i, (info, bytes)) in keep.regions.iter().enumerate() {
        let got = media(&p, &format!("region-{i:05}.data")).unwrap();
        assert!(got == *bytes, "region {i}'s file");
        assert_eq!(got.len() as u64, info.stream_len());
    }
    let raw = media(&p, REGIONS_INFO).unwrap();
    let x = exportinfo::decode(&raw).unwrap();
    assert_eq!(x.header.region_count, 5);
    assert_eq!(1u64 << x.header.block_log2, h.block_size);
    assert_eq!(x.header.total, h.total);
    assert_eq!(x.header.lend as usize, h.lend);
    assert_eq!(x.header.writable, h.writable);
    let recs: Vec<_> = x.regions().collect();
    assert_eq!(recs.len(), 5);
    for ((rec, spans), (info, bytes)) in recs.iter().zip(&keep.regions) {
        assert_eq!(rec.name.as_str(), info.name);
        assert_eq!(rec.kind, info.kind);
        assert_eq!(rec.generation, info.generation);
        assert_eq!(rec.tag, info.tag);
        assert_eq!(rec.cap, info.cap);
        assert_eq!(rec.fill, info.fill);
        assert_eq!(rec.fill_key, info.fill_key);
        assert_eq!(rec.data_size, info.data_size);
        assert_eq!(rec.durable_len, info.durable_len);
        assert_eq!(rec.object_len.map(u64::from), info.object_len);
        assert_eq!(rec.stream_len, info.stream_len());
        let want: Vec<u32> = bytes.chunks(SPAN as usize).map(crc32c).collect();
        assert_eq!(spans.len(), want.len(), "{}'s spans", info.name);
        for (k, c) in want.iter().enumerate() {
            assert_eq!(spans.get(k), Some(*c), "{} span {k}", info.name);
        }
    }
}

/// A store with no region: `begin` then `finish`, and a `regions.info`
/// with none.
#[test]
fn test_no_region_writes_only_the_record() {
    let (p, s) = store(3001);
    out_dir(&p);
    let mut fs = sink(&p);
    let r = export(&s, &mut fs).unwrap();
    assert!(r.complete && r.regions == 0);
    let raw = media(&p, REGIONS_INFO).unwrap();
    assert_eq!(exportinfo::decode(&raw).unwrap().regions().count(), 0);
    assert!(media(&p, "region-00000.data").is_none());
}

/// The directory must exist and be empty: the sink never creates one.
#[test]
fn test_the_directory_must_exist_and_be_empty() {
    let p = SimPlatform::new(SimConfig::volatile(3002));
    let e = FileSink::create(p.clone(), Path::new(OUT), FileSinkOptions::default()).unwrap_err();
    assert!(matches!(e, Error::NotFound { .. }), "{e:?}");
    out_dir(&p);
    let d = p.open_dir(Path::new(OUT), false).unwrap();
    let _f = p
        .create_file(&d, &store_io_platform::FileName::new("x").unwrap())
        .unwrap();
    let e = FileSink::create(p.clone(), Path::new(OUT), FileSinkOptions::default()).unwrap_err();
    assert_eq!(cause(&e), Some(NotWrittenCause::InvalidOptions));
}

/// Each misuse is refused at the call that misuses it, the refusal
/// sticky (so `finish` never writes `regions.info`), and the export is not
/// complete although the wrapper swallowed every error: the call knows its
/// sink failed.
#[test]
fn test_each_misuse_is_refused_and_the_export_is_not_complete() {
    // Each misuse, the call first refused, and where the report says the
    // export stopped: the regions ended before it and the region it was in
    // (none at `begin` or at `finish`).
    for (n, (m, at, ended, inside)) in [
        (Misuse::BeginTwice, "begin", 0, None),
        (Misuse::SkipFirstEnd, "region", 1, Some("capped")),
        (Misuse::DropFirstChunk, "data", 0, Some("pages")),
        (Misuse::EndEarly, "end_region", 0, Some("pages")),
        (Misuse::SkipSecondRegion, "finish", 5, None),
        (Misuse::FinishTwice, "finish", 5, None),
        (Misuse::BadInfo, "region", 0, Some("pages")),
        (Misuse::SmallTotal, "region", 0, Some("pages")),
        (Misuse::SameName, "region", 1, Some("capped")),
        (Misuse::FewerCount, "region", 1, Some("capped")),
        (Misuse::BadBlock, "begin", 0, None),
        (Misuse::HugeCount, "begin", 0, None),
    ]
    .into_iter()
    .enumerate()
    {
        let (p, s) = every_kind(3010 + n as u64);
        let mut w = Wrap::new(&p, m);
        let e = export(&s, &mut w).unwrap_err();
        assert!(!e.report.complete, "{m:?}");
        assert_eq!(e.report.regions, ended, "{m:?}");
        assert_eq!(e.report.region.as_deref(), inside, "{m:?}");
        assert_eq!(e.report.offset, 0, "{m:?}");
        if ended < 5 {
            // The bytes of the regions before it, none of its own.
            let before = if ended == 1 { 2 * SPAN + 3 * B } else { 0 };
            assert_eq!(e.report.bytes, before, "{m:?}");
        }
        assert_eq!(
            cause(&e.error),
            Some(NotWrittenCause::InvalidOptions),
            "{m:?}"
        );
        assert_eq!(
            w.errors.first().map(|x| x.0),
            Some(at),
            "{m:?}: {:?}",
            w.errors
        );
        assert!(
            w.errors.iter().all(|x| x.1 == SinkError::Refused),
            "{m:?}: {:?}",
            w.errors
        );
        // Never restorable, except a second `finish` after a first that
        // wrote it (the export is still refused).
        let info = media(&p, REGIONS_INFO).unwrap_or_default();
        if m == Misuse::FinishTwice {
            assert!(exportinfo::decode(&info).is_ok());
        } else {
            assert!(info.is_empty(), "{m:?}: regions.info written");
        }
    }
}

/// A sink the wrapper never finishes (it dropped it, or fell back to
/// another) leaves the export incomplete, with no error of the sink's.
#[test]
fn test_a_sink_never_finished_leaves_the_export_incomplete() {
    let (p, s) = every_kind(3025);
    let mut w = Wrap::new(&p, Misuse::NoFinish);
    let e = export(&s, &mut w).unwrap_err();
    assert!(!e.report.complete && w.errors.is_empty());
    assert_eq!(cause(&e.error), Some(NotWrittenCause::Interrupted));
    assert!(media(&p, REGIONS_INFO).unwrap_or_default().is_empty());
}

/// A short write fails the export: never a file shorter than it says.
#[test]
fn test_a_short_write_fails_the_export() {
    let (p, s) = every_kind(3026);
    p.with_world(|w| {
        let n = w.writes();
        w.faults_mut().short_write = Some(n + 1);
    });
    let mut fs = sink(&p);
    let e = export(&s, &mut fs).unwrap_err();
    assert!(!e.report.complete);
    assert_eq!(e.report.region.as_deref(), Some("pages"));
    assert!(media(&p, REGIONS_INFO).unwrap_or_default().is_empty());
}

/// The export fails at a sink write's failure, the report clamped to the
/// region and offset where that write began, also through a wrapper that
/// swallowed it; `regions.info` is never written.
#[test]
fn test_a_failed_write_fails_the_export_where_it_began() {
    for swallow in [false, true] {
        let (p, s) = every_kind(3020 + u64::from(swallow));
        // The sink's second write: region 0's second piece.
        p.with_world(|w| {
            let n = w.writes();
            w.faults_mut().fail_write = Some(n + 2);
        });
        let e = if swallow {
            let mut w = Wrap::new(&p, Misuse::None);
            let e = export(&s, &mut w).unwrap_err();
            assert!(matches!(w.errors.first(), Some((_, SinkError::Io(_)))));
            e
        } else {
            let mut fs = sink(&p);
            let e = export(&s, &mut fs).unwrap_err();
            assert!(matches!(e.sink, Some(SinkError::Io(_))));
            e
        };
        assert!(
            matches!(e.error, Error::Io { op: Op::Write, .. }),
            "{:?}",
            e.error
        );
        let r = &e.report;
        assert!(!r.complete);
        assert_eq!(r.region.as_deref(), Some("pages"));
        assert_eq!(r.regions, 0);
        // Nothing past the failed write's start counted.
        assert_eq!(r.bytes, r.offset);
        assert!(
            r.offset > 0 && r.offset < 2 * SPAN && r.offset % B == 0,
            "{}",
            r.offset
        );
        assert!(media(&p, REGIONS_INFO).unwrap_or_default().is_empty());
    }
}

/// Too little free space for the header's total: refused at `begin`
/// (`NoSpace`), nothing written.
#[test]
fn test_too_little_space_is_refused_at_begin() {
    let (p, s) = every_kind(3030);
    let used = p.with_world(|w| w.allocated_bytes());
    p.with_world(|w| w.faults_mut().capacity = Some(used + 64 * B));
    let mut fs = sink(&p);
    let e = export(&s, &mut fs).unwrap_err();
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
    assert!(media(&p, "region-00000.data").is_none());
}

/// With commits held active (the group paced), an export through a sink
/// on the store's own device completes: the sink's writes and the stream's
/// reads are one call, and each wait collects the other's pieces first.
#[test]
fn test_a_paced_export_through_the_sink_completes() {
    let (p, s) = every_kind(3040);
    let hold = s.commit_hold();
    let mut o = ExportOptions::default();
    o.depth = Some(4);
    let mut fs = sink(&p);
    let r = s.export_with(&mut fs, o).unwrap();
    assert!(r.complete && r.regions == 5);
    drop(hold);
    let raw = media(&p, REGIONS_INFO).unwrap();
    assert_eq!(exportinfo::decode(&raw).unwrap().regions().count(), 5);
}

/// The power cut at every step of an export through the sink (each write,
/// size change, flush and directory sync), then a crash that keeps, drops
/// or tears what was not flushed: every region the export counted as ended
/// is there, whole and exact, and whenever `regions.info` decodes, every
/// region is. A backup that says it holds a region holds it.
#[test]
fn test_a_power_cut_at_any_step_leaves_no_false_backup() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use store_io_sim::CrashMode;

    let want: Vec<Vec<u8>> = {
        let (_p, s) = every_kind(3050);
        let mut keep = Keep::default();
        let _r = export(&s, &mut keep).unwrap();
        keep.regions.into_iter().map(|(_, b)| b).collect()
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
    // The steps of a whole export.
    let all = {
        let (p, s) = every_kind(3050);
        let steps = probe(&p, u64::MAX);
        let mut fs = sink(&p);
        let _r = export(&s, &mut fs).unwrap();
        steps.load(Ordering::Relaxed)
    };
    assert!(all > 10, "{all} steps");
    for cut in 0..=all {
        let (p, s) = every_kind(3050);
        let _steps = probe(&p, cut);
        let mut fs = sink(&p);
        let ended = match export(&s, &mut fs) {
            Ok(r) => r.regions,
            Err(e) => e.report.regions,
        };
        drop(fs);
        p.with_world(|w| {
            w.set_step_probe(None);
            w.crash(&CrashMode::Random);
        });
        let whole = |i: usize| media(&p, &format!("region-{i:05}.data")).as_ref() == Some(&want[i]);
        for i in 0..ended {
            assert!(whole(i), "cut {cut}: region {i} counted ended, not durable");
        }
        if let Some(info) = media(&p, REGIONS_INFO) {
            if exportinfo::decode(&info).is_ok() {
                for i in 0..want.len() {
                    assert!(
                        whole(i),
                        "cut {cut}: regions.info decodes, region {i} not whole"
                    );
                }
            }
        }
    }
}
