//! Export through a caller's sink (R5.6, D-148) on the deterministic
//! simulator: every kind's info and bytes, the header, the plan's refusals
//! (options, subsets, writable opens, frozen stores), sizes fixed at the
//! plan, a region recycled or released between the plan and its turn or
//! while it streams, a slot object too large at its turn, media errors,
//! `settle`, the sink's own errors, and `RegionInfo::validate`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

mod format2_util;

use std::path::{Path, PathBuf};

use format2_util::craft::{region, tear_newer_copy_of, write_regions};
use store_io_core::error::{CorruptionKind, Error, NotWrittenCause, Op, OsError, OsErrorSource};
use store_io_core::id::RegionId;
use store_io_engine::{
    CONTAINER, Chunk, ExportError, ExportHeader, ExportOptions, ExportReport, ExportSink,
    FillPattern, RegionInfo, RegionKind, RegionOptions, SinkError, Store, StoreOptions,
};
use store_io_format::meta::RegionState;
use store_io_format::slot::FORMAT_1;
use store_io_sim::{FaultPlan, SimConfig, SimPlatform};

const DIR: &str = "/db";
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

fn store(seed: u64, o: StoreOptions) -> (SimPlatform, Store<SimPlatform>) {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    let s = Store::create(p.clone(), Path::new(DIR), o).unwrap();
    (p, s)
}

/// Stamped bytes: each 8-byte word holds its own offset plus `salt`, so a
/// byte read from the wrong place or region shows.
fn stamped(salt: u64, len: usize) -> Vec<u8> {
    (0..len.div_ceil(8) as u64)
        .flat_map(|w| (w * 8 + salt).to_le_bytes())
        .take(len)
        .collect()
}

fn page(s: &Store<SimPlatform>, name: &str, len: u64, salt: u64) -> Vec<u8> {
    let r = s.provision_page_region(name, len).unwrap();
    let data = stamped(salt, len as usize);
    r.write_durable(r.pos(0).unwrap(), &data).unwrap();
    data
}

fn cause(e: &Error) -> Option<NotWrittenCause> {
    match e {
        Error::NotWritten { cause, .. } => Some(*cause),
        _ => None,
    }
}

/// What a test sink saw, and what it was told to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ev {
    Region(usize),
    Data(usize),
    End(usize),
}

#[derive(Default)]
struct VecSink {
    header: Option<ExportHeader>,
    regions: Vec<(RegionInfo, Vec<u8>)>,
    ends: usize,
    finished: bool,
    /// Keep every chunk; `settle` drops them when `drops`.
    keep: bool,
    drops: bool,
    held: Vec<Chunk>,
    settles: usize,
    datas: usize,
    fail_data: Option<(usize, SinkError)>,
    fail_finish: Option<SinkError>,
    hook: Option<Box<dyn FnMut(Ev)>>,
}

impl VecSink {
    fn on(&mut self, ev: Ev) {
        if let Some(h) = self.hook.as_mut() {
            h(ev);
        }
    }
}

impl ExportSink for VecSink {
    fn begin(&mut self, header: &ExportHeader) -> Result<(), SinkError> {
        assert!(self.header.is_none(), "begin twice");
        self.header = Some(header.clone());
        Ok(())
    }

    fn region(&mut self, info: &RegionInfo) -> Result<(), SinkError> {
        assert_eq!(
            self.ends,
            self.regions.len(),
            "a region before the last ended"
        );
        self.regions.push((info.clone(), Vec::new()));
        self.on(Ev::Region(self.regions.len() - 1));
        Ok(())
    }

    fn data(&mut self, chunk: Chunk) -> Result<(), SinkError> {
        if let Some((n, e)) = self.fail_data {
            if self.datas == n {
                return Err(e);
            }
        }
        self.datas += 1;
        let i = self.regions.len() - 1;
        let bytes = &mut self.regions[i].1;
        assert_eq!(chunk.offset(), bytes.len() as u64, "contiguous");
        bytes.extend_from_slice(chunk.bytes());
        if self.keep {
            self.held.push(chunk);
        }
        self.on(Ev::Data(i));
        Ok(())
    }

    fn settle(&mut self) -> Result<(), SinkError> {
        self.settles += 1;
        if self.drops {
            self.held.clear();
        }
        Ok(())
    }

    fn end_region(&mut self) -> Result<(), SinkError> {
        let i = self.ends;
        assert_eq!(
            self.regions[i].0.stream_len(),
            self.regions[i].1.len() as u64
        );
        self.ends += 1;
        self.on(Ev::End(i));
        Ok(())
    }

    fn finish(&mut self) -> Result<(), SinkError> {
        if let Some(e) = self.fail_finish {
            return Err(e);
        }
        self.finished = true;
        Ok(())
    }
}

fn export(s: &Store<SimPlatform>, sink: &mut VecSink) -> Result<ExportReport, Box<ExportError>> {
    s.export(sink)
}

fn export_with(
    s: &Store<SimPlatform>,
    sink: &mut VecSink,
    o: ExportOptions,
) -> Result<ExportReport, Box<ExportError>> {
    s.export_with(sink, o)
}

fn region_named<'a>(sink: &'a VecSink, name: &str) -> &'a (RegionInfo, Vec<u8>) {
    sink.regions.iter().find(|(i, _)| i.name == name).unwrap()
}

/// Every kind, in the store's order, its info and its bytes: a page
/// region's data area, a capped one's cap, a positioned append region's
/// durable prefix (not what is appended but unsynced), a slot's object,
/// an empty slot; the header's count, block, `lend`, total and the
/// writable flag; the report complete.
#[test]
fn test_every_kind_exports_its_info_and_bytes_in_order() {
    let (_p, s) = store(
        2000,
        StoreOptions {
            keyed_fill: true,
            ..opts()
        },
    );
    let pages = page(&s, "pages", 3 * CHUNK as u64, 1);
    let capped = s
        .provision_page_region_with(
            "capped",
            RegionOptions::default().size(4 * B).max_size(16 * B),
        )
        .unwrap();
    let cdata = stamped(2, 4 * B as usize);
    capped
        .write_durable(capped.pos(0).unwrap(), &cdata)
        .unwrap();
    let log = s.provision_append_region("log", 4 * CHUNK as u64).unwrap();
    let synced = stamped(3, CHUNK + 1000);
    let t = log.append(&synced).unwrap();
    log.sync_through(&t).unwrap();
    log.append(&[9; 500]).unwrap();
    // The durable prefix as of the export (whole blocks of the tail may
    // count: the stream's rule, R5.5).
    let durable = log.durable_through();
    assert!(durable >= synced.len() as u64);
    let slot = s.provision_slot("slot").unwrap();
    slot.commit(b"the object").unwrap();
    s.provision_slot("empty").unwrap();
    let mut sink = VecSink::default();
    let r = export(&s, &mut sink).unwrap();
    assert!(r.complete && r.writable && r.region.is_none());
    assert_eq!(r.regions, 5);
    let bytes = (3 * CHUNK + 4 * B as usize) as u64 + durable + 10;
    assert_eq!(r.bytes, bytes);
    let h = sink.header.clone().unwrap();
    assert_eq!(h.region_count, 5);
    assert_eq!(h.block_size, B);
    assert!(h.writable && h.lend >= 1);
    assert!(h.total >= bytes + 5 * 2 * B + 12 * B, "{}", h.total);
    let names: Vec<_> = sink.regions.iter().map(|(i, _)| i.name.as_str()).collect();
    assert_eq!(names, ["pages", "capped", "log", "slot", "empty"]);
    let (i, b) = region_named(&sink, "pages");
    assert!(b == &pages);
    assert_eq!(
        (i.kind, i.data_size, i.cap, i.durable_len, i.object_len),
        (RegionKind::Page, 3 * CHUNK as u64, None, None, None)
    );
    assert_eq!(i.fill, FillPattern::KeyedV1);
    assert!(i.generation >= 1 && i.block_size == B && !i.full_copy && !i.superseded);
    let (i, b) = region_named(&sink, "capped");
    assert!(b == &cdata);
    assert_eq!((i.data_size, i.cap), (4 * B, Some(16 * B)));
    let (i, b) = region_named(&sink, "log");
    assert_eq!(b.len() as u64, durable, "the durable prefix only");
    assert!(b[..synced.len()] == synced[..]);
    assert_eq!((i.kind, i.durable_len), (RegionKind::Append, Some(durable)));
    let (i, b) = region_named(&sink, "slot");
    assert_eq!((i.kind, i.object_len), (RegionKind::Slot, Some(10)));
    assert_eq!(b, b"the object");
    let (i, b) = region_named(&sink, "empty");
    assert!(i.object_len.is_none() && b.is_empty());
    assert!(sink.finished && sink.ends == 5);
    for (i, _) in &sink.regions {
        i.validate(B).unwrap();
    }
}

/// A store opened read-only: the header says so, and an append region
/// (unpositioned, as after any reopen) exports its whole data area as
/// read, with no durable length.
#[test]
fn test_an_unpositioned_append_region_exports_its_whole_data_area() {
    let (p, s) = store(2001, opts());
    let log = s.provision_append_region("log", 2 * CHUNK as u64).unwrap();
    let data = stamped(4, CHUNK);
    let t = log.append(&data).unwrap();
    log.sync_through(&t).unwrap();
    drop((log, s));
    let s = Store::open_readonly(p, Path::new(DIR), opts()).unwrap();
    let mut sink = VecSink::default();
    let r = export(&s, &mut sink).unwrap();
    assert!(r.complete && !r.writable && !sink.header.as_ref().unwrap().writable);
    let (i, b) = region_named(&sink, "log");
    assert_eq!((i.durable_len, i.data_size), (None, 2 * CHUNK as u64));
    assert_eq!(b.len(), 2 * CHUNK);
    assert!(b[..CHUNK] == data[..]);
}

/// No region: `begin`, then `finish`; complete.
#[test]
fn test_a_store_with_no_region_exports_its_header_and_finishes() {
    let (_p, s) = store(2002, opts());
    let mut sink = VecSink::default();
    let r = export(&s, &mut sink).unwrap();
    assert!(r.complete && r.regions == 0 && r.bytes == 0);
    assert_eq!(sink.header.unwrap().region_count, 0);
    assert!(sink.finished && sink.regions.is_empty());
}

/// A subset in the store's order; the options refused before the sink is
/// called: a name twice, a name no region has, an invalid name, a depth
/// out of range.
#[test]
fn test_a_subset_and_the_options_refused() {
    let (_p, s) = store(2003, opts());
    page(&s, "a", B, 1);
    page(&s, "b", B, 2);
    page(&s, "c", B, 3);
    let mut sink = VecSink::default();
    let mut o = ExportOptions::default();
    o.regions = Some(vec!["c".into(), "a".into()]);
    let r = export_with(&s, &mut sink, o).unwrap();
    assert!(r.complete && r.regions == 2);
    let names: Vec<_> = sink.regions.iter().map(|(i, _)| i.name.as_str()).collect();
    assert_eq!(names, ["a", "c"]);
    assert_eq!(sink.header.unwrap().region_count, 2);
    for (regions, depth) in [
        (Some(vec!["a".to_owned(), "a".to_owned()]), None),
        (Some(vec!["zz".to_owned()]), None),
        (Some(vec![String::new()]), None),
        (None, Some(0)),
        (None, Some(100_000)),
    ] {
        let mut sink = VecSink::default();
        let mut o = ExportOptions::default();
        o.regions = regions;
        o.depth = depth;
        let e = export_with(&s, &mut sink, o).unwrap_err();
        assert_eq!(cause(&e.error), Some(NotWrittenCause::InvalidOptions));
        assert!(sink.header.is_none() && !e.report.complete);
    }
}

/// Sizes are fixed at the plan: a page region grown, and an append region
/// grown and appended past its planned data size, before their turns, are
/// exported at their planned sizes (the durable prefix clamped).
#[test]
fn test_sizes_are_fixed_at_the_plan() {
    let (_p, s) = store(2004, opts());
    page(&s, "a", B, 1);
    let grown = page(&s, "grown", 2 * CHUNK as u64, 2);
    let log = s.provision_append_region("log", 2 * CHUNK as u64).unwrap();
    let data = stamped(3, 2 * CHUNK);
    let t = log.append(&data).unwrap();
    log.sync_through(&t).unwrap();
    let (pr, lg) = (s.page_region("grown").unwrap(), log);
    let mut sink = VecSink {
        hook: Some(Box::new(move |ev| {
            if ev == Ev::End(0) {
                pr.grow(4 * CHUNK as u64).unwrap();
                pr.write_durable(pr.pos(3 * CHUNK as u64).unwrap(), &[7; B as usize])
                    .unwrap();
                lg.grow(4 * CHUNK as u64).unwrap();
                let t = lg.append(&[8; CHUNK]).unwrap();
                lg.sync_through(&t).unwrap();
            }
        })),
        ..VecSink::default()
    };
    let r = export(&s, &mut sink).unwrap();
    assert!(r.complete);
    let (i, b) = region_named(&sink, "grown");
    assert_eq!(i.data_size, 2 * CHUNK as u64);
    assert!(b == &grown);
    let (i, b) = region_named(&sink, "log");
    assert_eq!(
        (i.data_size, i.durable_len),
        (2 * CHUNK as u64, Some(2 * CHUNK as u64))
    );
    assert!(b == &data);
}

/// The header's total charges each region its footprint exactly: two
/// header blocks and its data size, a capped region its cap, a slot the
/// largest object a format-2 slot holds; the rest is the metadata.
#[test]
fn test_the_total_charges_each_region_its_footprint() {
    let (_p, s) = store(2016, opts());
    page(&s, "pages", 3 * CHUNK as u64, 1);
    s.provision_page_region_with(
        "capped",
        RegionOptions::default().size(4 * B).max_size(16 * B),
    )
    .unwrap();
    s.provision_slot("slot").unwrap().commit(b"x").unwrap();
    let total = |names: &[&str]| {
        let mut sink = VecSink::default();
        let mut o = ExportOptions::default();
        o.regions = Some(names.iter().map(|n| (*n).to_owned()).collect());
        export_with(&s, &mut sink, o).unwrap();
        sink.header.unwrap().total
    };
    // The metadata's term, from a store with no region (the default
    // geometry, whatever the regions).
    let none = {
        let (_q, empty) = store(2017, opts());
        let mut sink = VecSink::default();
        export(&empty, &mut sink).unwrap();
        sink.header.unwrap().total
    };
    let max2 = store_io_format::meta2::max_object(12) as u64;
    assert!(none > 0, "the metadata");
    assert_eq!(total(&["pages"]) - none, 2 * B + 3 * CHUNK as u64);
    assert_eq!(total(&["capped"]) - none, 2 * B + 16 * B);
    assert_eq!(total(&["slot"]) - none, 2 * B + max2);
    assert_eq!(
        total(&["pages", "capped", "slot"]) - none,
        6 * B + 3 * CHUNK as u64 + 16 * B + max2
    );
}

/// A region recycled or released between the plan and its turn, or
/// recycled while it streams, fails the export with the region named and
/// how far it got.
#[test]
fn test_a_region_retired_after_the_plan_fails_the_export() {
    for (seed, release) in [(2005u64, false), (2006, true)] {
        let (_p, s) = store(seed, opts());
        page(&s, "a", B, 1);
        page(&s, "b", B, 2);
        let mut b = Some(s.page_region("b").unwrap());
        let mut sink = VecSink {
            hook: Some(Box::new(move |ev| {
                if ev == Ev::End(0) {
                    let r = b.take().unwrap();
                    if release {
                        r.release().unwrap();
                    } else {
                        r.recycle().unwrap();
                    }
                }
            })),
            ..VecSink::default()
        };
        let e = export(&s, &mut sink).unwrap_err();
        let want = if release {
            NotWrittenCause::NotReady
        } else {
            NotWrittenCause::StaleGeneration
        };
        assert_eq!(cause(&e.error), Some(want), "{e:?}");
        assert_eq!(e.report.regions, 1);
        assert_eq!(e.report.region.as_deref(), Some("b"));
        assert!(!e.report.complete && !sink.finished && sink.regions.len() == 1);
    }
    // While it streams.
    let (_p, s) = store(2007, opts());
    page(&s, "a", 6 * CHUNK as u64, 1);
    let a = s.page_region("a").unwrap();
    let mut sink = VecSink {
        hook: Some(Box::new(move |ev| {
            if ev == Ev::Data(0) {
                let _ = a.recycle();
            }
        })),
        ..VecSink::default()
    };
    let e = export(&s, &mut sink).unwrap_err();
    assert_eq!(cause(&e.error), Some(NotWrittenCause::StaleGeneration));
    assert_eq!(e.report.offset, CHUNK as u64);
    assert_eq!(e.report.regions, 0);
}

/// A format-1 slot holds more than a format-2 slot: an object committed
/// after the plan that a format-2 slot cannot hold is refused at the
/// slot's turn, before the sink hears of the slot.
#[test]
fn test_a_slot_object_too_large_at_its_turn_is_refused() {
    let (_p, s) = store(
        2008,
        StoreOptions {
            container_format: FORMAT_1,
            ..opts()
        },
    );
    page(&s, "a", B, 1);
    let slot = s.provision_slot("s").unwrap();
    slot.commit(b"small").unwrap();
    let max2 = store_io_format::meta2::max_object(12);
    let mut sink = VecSink {
        hook: Some(Box::new(move |ev| {
            if ev == Ev::End(0) {
                slot.commit(&vec![1u8; max2 + 6]).unwrap();
            }
        })),
        ..VecSink::default()
    };
    let e = export(&s, &mut sink).unwrap_err();
    assert_eq!(cause(&e.error), Some(NotWrittenCause::TooLarge), "{e:?}");
    assert_eq!(e.report.region.as_deref(), Some("s"));
    assert_eq!(sink.regions.len(), 1);
}

/// A media error fails the export at once, the range named, the report's
/// offset the bytes the sink took before it.
#[test]
fn test_a_media_error_fails_the_export() {
    let (p, s) = store(2009, opts());
    let len = 4 * CHUNK as u64;
    page(&s, "pages", len, 0);
    let base = {
        let first = stamped(0, 64);
        p.with_world(|w| {
            let media = w.media_of(&PathBuf::from(DIR), CONTAINER).unwrap();
            media.windows(64).position(|win| win == &first[..]).unwrap() as u64
        })
    };
    let bad = CHUNK as u64 + 8192..CHUNK as u64 + 12288;
    p.with_world(|w| {
        let file = w.file_id(&PathBuf::from(DIR), CONTAINER).unwrap();
        *w.faults_mut() = FaultPlan {
            bad_ranges: vec![(file, base + bad.start, base + bad.end)],
            ..FaultPlan::default()
        };
    });
    let mut sink = VecSink::default();
    let e = export(&s, &mut sink).unwrap_err();
    match &e.error {
        Error::Corruption {
            kind: CorruptionKind::MediaError,
            ctx,
        } => assert_eq!(ctx.range.unwrap().start, bad.start),
        other => panic!("{other:?}"),
    }
    assert_eq!(e.report.offset, bad.start);
    assert_eq!(e.report.region.as_deref(), Some("pages"));
    assert!(!e.report.complete && !sink.finished);
}

/// A sink holding every lent chunk is asked to `settle`: one that drops
/// them goes on (a region's reads and a slot's object), one that keeps
/// them fails the export `TooManyInFlight`.
#[test]
fn test_settle_is_asked_when_every_chunk_is_lent() {
    // Also with the stream's reads not completed inline (reaps held back,
    // as a real device's: it keeps its depth, D-177).
    for (drops, held) in [(true, false), (false, false), (true, true), (false, true)] {
        let (p, s) = store(2010, opts());
        if held {
            p.with_world(|w| w.hold_reaps(1 << 40));
        }
        page(&s, "pages", 8 * CHUNK as u64, 1);
        s.provision_slot("s").unwrap().commit(b"obj").unwrap();
        page(&s, "more", 20 * CHUNK as u64, 2);
        let mut sink = VecSink {
            keep: true,
            drops,
            ..VecSink::default()
        };
        let r = export(&s, &mut sink);
        p.with_world(|w| w.hold_reaps(0));
        let lend = sink.header.as_ref().unwrap().lend;
        assert_eq!(lend, 8, "the default lend this pool gives");
        if drops {
            let r = r.unwrap();
            assert!(r.complete);
            assert!(sink.settles >= 3, "{}", sink.settles);
            assert_eq!(region_named(&sink, "s").1, b"obj");
        } else {
            let e = r.unwrap_err();
            assert_eq!(cause(&e.error), Some(NotWrittenCause::TooManyInFlight));
            assert!(e.sink.is_none());
            assert_eq!(e.report.region.as_deref(), Some("s"), "the slot's chunk");
            assert_eq!(sink.settles, 1);
        }
    }
}

/// The sink's own errors stop the export: kept in `sink`, `Io` for an I/O
/// error and `NotWritten(Interrupted)` otherwise, the offset where the
/// refused chunk began; a failed `finish` leaves it incomplete.
#[test]
fn test_the_sink_s_errors_stop_the_export() {
    let raw = OsError {
        code: 5,
        source: OsErrorSource::Errno,
    };
    for (at, err) in [
        (1usize, SinkError::Other(7)),
        (0, SinkError::Io(raw)),
        (2, SinkError::Refused),
    ] {
        let (_p, s) = store(2011, opts());
        page(&s, "pages", 4 * CHUNK as u64, 1);
        let mut sink = VecSink {
            fail_data: Some((at, err)),
            ..VecSink::default()
        };
        let e = export(&s, &mut sink).unwrap_err();
        assert_eq!(e.sink, Some(err));
        match err {
            SinkError::Io(_) => {
                assert!(matches!(e.error, Error::Io { op: Op::Write, raw: r, .. } if r == raw))
            }
            _ => assert_eq!(cause(&e.error), Some(NotWrittenCause::Interrupted)),
        }
        assert_eq!(e.report.offset, (at * CHUNK) as u64);
        assert_eq!(e.report.bytes, (at * CHUNK) as u64);
        assert!(!e.report.complete && e.report.regions == 0);
    }
    let (_p, s) = store(2012, opts());
    page(&s, "pages", B, 1);
    let mut sink = VecSink {
        fail_finish: Some(SinkError::Refused),
        ..VecSink::default()
    };
    let e = export(&s, &mut sink).unwrap_err();
    assert_eq!(e.sink, Some(SinkError::Refused));
    assert!(!e.report.complete && e.report.regions == 1 && e.report.region.is_none());
}

/// `RegionInfo::validate`: an exported info passes; each field out of
/// bounds is refused `InvalidOptions`.
#[test]
fn test_region_info_validate_refuses_each_bad_field() {
    let (_p, s) = store(2013, opts());
    page(&s, "pages", 2 * B, 1);
    let log = s.provision_append_region("log", 2 * B).unwrap();
    let t = log.append(&[1; 100]).unwrap();
    log.sync_through(&t).unwrap();
    s.provision_slot("slot").unwrap().commit(b"x").unwrap();
    let mut sink = VecSink::default();
    export(&s, &mut sink).unwrap();
    let page_info = region_named(&sink, "pages").0.clone();
    let log_info = region_named(&sink, "log").0.clone();
    let slot_info = region_named(&sink, "slot").0.clone();
    for i in [&page_info, &log_info, &slot_info] {
        i.validate(B).unwrap();
    }
    let max2 = store_io_format::meta2::max_object(12) as u64;
    let mut bad: Vec<RegionInfo> = Vec::new();
    let mut push = |base: &RegionInfo, f: &dyn Fn(&mut RegionInfo)| {
        let mut i = base.clone();
        f(&mut i);
        bad.push(i);
    };
    push(&page_info, &|i| i.name = String::new());
    push(&page_info, &|i| i.name = "x".repeat(25));
    push(&page_info, &|i| i.generation = 0);
    push(&page_info, &|i| i.block_size = 2 * B);
    push(&page_info, &|i| i.data_size = B + 1);
    push(&page_info, &|i| i.cap = Some(B));
    push(&page_info, &|i| i.cap = Some(4 * B + 1));
    push(&page_info, &|i| i.durable_len = Some(B));
    push(&page_info, &|i| i.object_len = Some(1));
    push(&page_info, &|i| {
        i.data_size = u64::MAX / B * B;
        i.cap = None;
    });
    push(&log_info, &|i| i.durable_len = Some(3 * B));
    push(&log_info, &|i| i.full_copy = true);
    push(&slot_info, &|i| i.object_len = Some(max2 + 1));
    push(&slot_info, &|i| i.full_copy = true);
    for (n, i) in bad.iter().enumerate() {
        let e = i.validate(B).unwrap_err();
        assert_eq!(cause(&e), Some(NotWrittenCause::InvalidOptions), "case {n}");
    }
    // The block size must be the target's, a power of two in range.
    assert!(page_info.validate(2 * B).is_err());
    let mut i = page_info.clone();
    i.block_size = 256;
    i.data_size = 0;
    assert!(i.validate(256).is_err());
    let mut i = page_info.clone();
    i.block_size = 3 * B;
    i.data_size = 0;
    assert!(i.validate(3 * B).is_err());
    let mut i = slot_info.clone();
    i.object_len = Some(max2);
    i.validate(B).unwrap();
    // Above the format's largest block (64 KiB), even as the target's.
    let mut i = page_info.clone();
    i.block_size = 1 << 17;
    i.data_size = 0;
    i.cap = None;
    assert!(i.validate(1 << 17).is_err());
    let mut i = page_info.clone();
    i.block_size = 1 << 16;
    i.data_size = 0;
    i.cap = None;
    i.validate(1 << 16).unwrap();
}

/// An entry of a kind this version does not know refuses the export of a
/// writable store as long as it stands, and not once it is given up
/// (the plan reads the entries as they stand, not the open's list).
#[test]
fn test_an_invalid_entry_given_up_since_the_open_no_longer_refuses() {
    let p = SimPlatform::new(SimConfig::volatile(2014));
    drop(format2_util::create(&p));
    let base = 2 * B + 2 * u64::from(format2_util::PAGES) * format2_util::TABLE;
    let mut newer = region(0, RegionKind::Append, "newer", base, 16 * B);
    newer.kind_raw = Some(9);
    write_regions(&p, 0, &[newer]);
    let s = format2_util::open(&p);
    let &[(page, seq)] = &s.report().metadata.invalid_entries[..] else {
        panic!("{:?}", s.report().metadata.invalid_entries);
    };
    let id = RegionId::from_raw((u64::from(page) << 32) | u64::from(seq));
    let e = export(&s, &mut VecSink::default()).unwrap_err();
    assert_eq!(cause(&e.error), Some(NotWrittenCause::NotReady));
    match e.error {
        Error::NotWritten { ctx, .. } => assert_eq!(ctx.region, Some(id)),
        other => panic!("{other:?}"),
    }
    s.repair_unavailable(id).unwrap();
    let r = export(&s, &mut VecSink::default()).unwrap();
    assert!(r.complete && r.regions == 0);
}

/// A stale page's regions: refused while frozen; with
/// `accept_stale_pages`, refused on a writable open (naming the region)
/// and exported in full from a read-only one, by container offset: a page
/// region's capacity and its extra extent, an append region's capacity;
/// the facts reported.
#[test]
fn test_a_stale_page_s_regions_export_in_full_only_read_only() {
    let p = SimPlatform::new(SimConfig::volatile(2015));
    drop(format2_util::create(&p));
    let table = format2_util::TABLE;
    let base = 2 * B + 2 * u64::from(format2_util::PAGES) * table;
    let cap = 16 * B;
    let p_at = base;
    let mut pr = region(0, RegionKind::Page, "p", p_at, cap);
    // Apart from its capacity (a gap of free space between), so a read by
    // the wrong container offset shows.
    let extra = (p_at + 2 * B + cap + 4 * B, 4 * B);
    pr.extents.push(extra);
    pr.header.extent_count = 2;
    let q_at = extra.0 + extra.1;
    let qr = region(1, RegionKind::Append, "q", q_at, cap);
    let (p_size, q_size) = (pr.entry.data_size, qr.entry.data_size);
    write_regions(&p, 0, &[pr, qr]);
    let pdata = stamped(11, (cap + 4 * B) as usize);
    format2_util::put(&p, p_at + 2 * B, &pdata[..cap as usize]);
    format2_util::put(&p, extra.0, &pdata[cap as usize..]);
    // The gap holds bytes of its own.
    format2_util::put(&p, p_at + 2 * B + cap, &[0x5A; 4 * B as usize]);
    let qdata = stamped(12, cap as usize);
    format2_util::put(&p, q_at + 2 * B, &qdata);
    tear_newer_copy_of(&p, 0);
    let s = format2_util::open(&p);
    let e = export(&s, &mut VecSink::default()).unwrap_err();
    assert_eq!(cause(&e.error), Some(NotWrittenCause::Frozen));
    let mut o = ExportOptions::default();
    o.accept_stale_pages = true;
    let e = export_with(&s, &mut VecSink::default(), o.clone()).unwrap_err();
    assert_eq!(cause(&e.error), Some(NotWrittenCause::InvalidOptions));
    assert!(matches!(e.error, Error::NotWritten { ctx, .. } if ctx.region.is_some()));
    drop(s);
    let s = Store::open_readonly(p.clone(), Path::new(DIR), format2_util::opts()).unwrap();
    let mut sink = VecSink::default();
    let r = export_with(&s, &mut sink, o).unwrap();
    assert!(r.complete && !r.writable);
    assert_eq!(r.facts.stale_pages, [0]);
    assert_eq!(
        r.facts.verified_up_to,
        [("p".to_owned(), p_size), ("q".to_owned(), q_size)]
    );
    let (i, b) = region_named(&sink, "p");
    assert!(i.full_copy && i.durable_len.is_none());
    assert_eq!(i.data_size, cap + 4 * B);
    assert!(b == &pdata, "p's capacity and extra extent");
    let (i, b) = region_named(&sink, "q");
    assert!(i.full_copy && i.data_size == cap);
    assert!(b == &qdata, "q's capacity");
    assert_eq!(sink.header.unwrap().facts, r.facts);
}

/// A possibly superseded region (an unavailable entry's newer extents over
/// its bytes) is refused on a writable open, the region named, and exported
/// with its flag from a read-only one; an empty subset is refused, and so
/// is the unavailable entry named in a subset (`NotReady`, not a name no
/// region has).
#[test]
fn test_a_superseded_region_exports_only_read_only() {
    let p = SimPlatform::new(SimConfig::volatile(2018));
    drop(format2_util::create(&p));
    let base = 2 * B + 2 * u64::from(format2_util::PAGES) * format2_util::TABLE;
    let cap = 16 * B;
    let mut old = region(0, RegionKind::Append, "old", base, cap);
    old.entry.footprint_seq = 1;
    let old_size = old.entry.data_size;
    let next = base + 2 * B + cap;
    let mut gone = region(1, RegionKind::Append, "gone", next, cap);
    gone.entry.span_start = base + 2 * B;
    gone.extents = vec![(base + 2 * B, cap)];
    gone.header.state = RegionState::Released;
    write_regions(&p, 0, &[old, gone]);
    let odata = stamped(13, cap as usize);
    format2_util::put(&p, base + 2 * B, &odata);
    let mut o = ExportOptions::default();
    o.accept_stale_pages = true;
    let s = format2_util::open(&p);
    let e = export_with(&s, &mut VecSink::default(), o.clone()).unwrap_err();
    assert_eq!(cause(&e.error), Some(NotWrittenCause::InvalidOptions));
    assert!(
        matches!(e.error, Error::NotWritten { ctx, .. } if ctx.region == Some(RegionId::from_raw(0)))
    );
    drop(s);
    let s = Store::open_readonly(p.clone(), Path::new(DIR), format2_util::opts()).unwrap();
    let mut sub = o.clone();
    sub.regions = Some(Vec::new());
    let e = export_with(&s, &mut VecSink::default(), sub).unwrap_err();
    assert_eq!(cause(&e.error), Some(NotWrittenCause::InvalidOptions));
    let mut sub = o.clone();
    sub.regions = Some(vec!["gone".to_owned()]);
    let e = export_with(&s, &mut VecSink::default(), sub).unwrap_err();
    assert_eq!(cause(&e.error), Some(NotWrittenCause::NotReady));
    assert!(
        matches!(e.error, Error::NotWritten { ctx, .. } if ctx.region == Some(RegionId::from_raw(1)))
    );
    let mut sink = VecSink::default();
    let r = export_with(&s, &mut sink, o).unwrap();
    assert!(r.complete && !r.writable);
    assert_eq!(r.facts.superseded, ["old".to_owned()]);
    let (i, b) = region_named(&sink, "old");
    assert!(i.superseded && !i.full_copy);
    assert!(b[..] == odata[..old_size as usize], "old's data area");
}
