//! Import (PLANNING R5.6, D-169): a store rebuilt from an export, through
//! `FileSink` and `FileSource` or a caller's own source; the source
//! untrusted, every refusal leaving nothing under the target name.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use store_io_buf::{BufPool, PoolConfig};
use store_io_core::error::{CorruptionKind, Error, Named, NotWrittenCause, ReserveTag};
use store_io_engine::{
    Chunk, ExportHeader, ExportSink, FileSink, FileSinkOptions, FileSource, FileSourceOptions,
    FillPattern, ImportOptions, ImportReport, ImportSource, REGIONS_INFO, RegionInfo,
    RegionOptions, SinkError, Store, StoreOptions, import,
};
use store_io_format::crc32c::crc32c;
use store_io_format::exportinfo::{self, InfoHeader, RegionRecord};
use store_io_format::fill::fill_keyed_v1;
use store_io_format::meta::RegionName;
use store_io_platform::{CompletionBuf, FileMode, FileName, IoOp, Platform, Queue, QueueConfig};
use store_io_sim::{CrashMode, DeviceKind, SimConfig, SimPlatform};

const DIR: &str = "/db";
const OUT: &str = "/backup";
const TO: &str = "/restored";
const CHUNK: usize = 1 << 17;
const B: u64 = 4096;
const SPAN: u64 = 1 << 20;

fn opts() -> StoreOptions {
    StoreOptions {
        queues: 2,
        buffers_per_class: 64,
        max_io: CHUNK,
        keyed_fill: true,
        ..StoreOptions::default()
    }
}

fn import_opts() -> ImportOptions {
    let mut o = ImportOptions::default();
    o.store = opts();
    o
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

/// The restored store's slot committed empty reads back committed and
/// empty, and the slot never committed reads back none.
fn slots_restored(p: &SimPlatform) {
    let s = Store::open(p.clone(), Path::new(TO), opts()).unwrap();
    assert_eq!(s.slot("blank").unwrap().read(), Some(Vec::new()));
    assert_eq!(s.slot("empty").unwrap().read(), None);
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

/// `s` exported through a `FileSink` into `OUT`.
fn to_files(p: &SimPlatform, s: &Store<SimPlatform>) {
    drop(p.open_dir(Path::new(OUT), true).unwrap());
    let mut fs = FileSink::create(p.clone(), Path::new(OUT), FileSinkOptions::default()).unwrap();
    let r = s.export(&mut fs).unwrap();
    assert!(r.complete);
}

fn source(p: &SimPlatform) -> FileSource<SimPlatform> {
    FileSource::open(p.clone(), Path::new(OUT), FileSourceOptions::default()).unwrap()
}

fn import_files(p: &SimPlatform) -> Result<ImportReport, Box<store_io_engine::ImportError>> {
    let mut src = source(p);
    import(p.clone(), Path::new(TO), &mut src, import_opts())
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

/// A file's bytes `at` in `dir` changed (one bit flipped), its length kept.
fn flip(p: &SimPlatform, dir: &str, name: &str, at: u64) {
    let d = p.open_dir(Path::new(dir), false).unwrap();
    let f = p
        .open_file(&d, &FileName::new(name).unwrap(), FileMode::ReadWrite)
        .unwrap();
    let len = p.size(&f).unwrap();
    let start = at / B * B;
    let pool = BufPool::new(&PoolConfig::uniform(B as usize, B as usize, B as usize, 2)).unwrap();
    let mut q = p.queue(QueueConfig::default()).unwrap();
    let buf = pool.take(B as usize).unwrap();
    q.submit(
        IoOp::Read {
            file: &f,
            offset: start,
            buf,
        },
        1,
    )
    .unwrap();
    let mut out = CompletionBuf::with_capacity(2);
    let _n = q.wait(1, &mut out).unwrap();
    let c = out.drain().next().unwrap();
    let mut buf = c.buf.unwrap();
    buf.as_mut_capacity()[(at - start) as usize] ^= 1;
    assert!(buf.set_len(B as usize));
    q.submit(
        IoOp::Write {
            file: &f,
            offset: start,
            buf,
            dsync: false,
        },
        2,
    )
    .unwrap();
    let _n = q.wait(1, &mut out).unwrap();
    let _done = out.drain().count();
    p.set_len(&f, len).unwrap();
    p.flush_all(&f).unwrap();
}

/// Every kind through `FileSink`, `FileSource` and `import`: the restored
/// store holds every region with its generation, tag, cap, fill and key,
/// and its bytes; an append region's durable length is in the report.
#[test]
fn test_every_kind_round_trips_through_files() {
    let p = SimPlatform::new(SimConfig::volatile(4000));
    let s = every_kind(&p);
    let want = kept(&s);
    to_files(&p, &s);
    let r = import_files(&p).unwrap();
    assert!(r.complete && r.regions.len() == 7);
    assert_eq!(
        r.bytes,
        want.regions
            .iter()
            .map(|(_, b)| b.len() as u64)
            .sum::<u64>()
    );
    let log = r.regions.iter().find(|i| i.name == "log").unwrap();
    let was = want.regions.iter().find(|(i, _)| i.name == "log").unwrap();
    assert!(was.0.durable_len.is_some());
    assert_eq!(log.durable_len, was.0.durable_len);
    let again = r.regions.iter().find(|i| i.name == "again").unwrap();
    assert_eq!(again.generation, 2);
    let blank = r.regions.iter().find(|i| i.name == "blank").unwrap();
    assert_eq!(blank.object_len, Some(0));
    restored_matches(&p, &want);
    slots_restored(&p);
}

/// A store with no region round-trips to one with none.
#[test]
fn test_no_region_round_trips() {
    let p = SimPlatform::new(SimConfig::volatile(4001));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    to_files(&p, &s);
    let r = import_files(&p).unwrap();
    assert!(r.complete && r.regions.is_empty());
    let t = Store::open(p.clone(), Path::new(TO), opts()).unwrap();
    assert!(kept(&t).regions.is_empty());
}

/// A bit flipped in a data file or in `regions.info` is `Corruption`:
/// nothing renamed; once the files are whole again an import into the
/// same directory succeeds (the temporary removed).
#[test]
fn test_a_flipped_byte_is_never_restored() {
    let p = SimPlatform::new(SimConfig::volatile(4002));
    let s = every_kind(&p);
    let want = kept(&s);
    to_files(&p, &s);
    let corrupt = |e: &Error| {
        matches!(
            e,
            Error::Corruption {
                kind: CorruptionKind::Metadata,
                ..
            }
        )
    };
    // In the first region's second span.
    flip(&p, OUT, "region-00000.data", SPAN + 77);
    let e = import_files(&p).unwrap_err();
    assert!(corrupt(&e.error), "{:?}", e.error);
    assert!(!e.report.complete);
    assert_eq!(e.report.index, Some(0));
    assert_eq!(e.report.region.as_deref(), Some("pages"));
    none_restored(&p);
    flip(&p, OUT, "region-00000.data", SPAN + 77);
    // Region 0's tag (the header's 64 bytes, then its record: the tag at
    // 28): a value any record may hold, so only the record's CRC-32C sees
    // it.
    flip(&p, OUT, REGIONS_INFO, 64 + 28);
    let e = import_files(&p).unwrap_err();
    assert!(corrupt(&e.error), "{:?}", e.error);
    assert_eq!(e.report.index, None);
    none_restored(&p);
    flip(&p, OUT, REGIONS_INFO, 64 + 28);
    let r = import_files(&p).unwrap();
    assert!(r.complete);
    restored_matches(&p, &want);
}

/// What a caller's source does wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wrong {
    None,
    /// The first region one byte short.
    Short,
    /// The first region one byte long.
    Long,
    /// A read answers one byte more than it was asked for.
    OverRead,
    /// One region fewer than the header counts.
    Fewer,
    /// One region more.
    More,
    /// A region's generation 0.
    BadInfo,
    /// The second region under the first's name.
    SameName,
    /// A header block size no power of two.
    BadBlock,
    /// A header total too small for the regions.
    SmallTotal,
    /// A header block size past the format's largest (128 KiB).
    BigBlock,
    /// A header region count past the format's table.
    HugeCount,
    /// A header block size smaller than the target device's (512 B).
    SmallBlock,
    /// The source's own error.
    Fails,
}

/// A caller's source, its framing the export's bytes
/// (`ExportHeader::to_bytes`, `RegionInfo::to_bytes`), doing `wrong`.
struct Framed {
    head: ExportHeader,
    header: Vec<u8>,
    regions: Vec<([u8; 128], Vec<u8>)>,
    wrong: Wrong,
    next: usize,
    at: usize,
}

impl Framed {
    fn new(k: &Keep, wrong: Wrong) -> Self {
        let mut h = k.header.clone().unwrap();
        // A header the import must refuse before anything is created: its
        // total past any free space, so a refusal that came too late shows
        // as `NoSpace`.
        match wrong {
            Wrong::BadBlock => {
                h.block_size = 3000;
                h.total = u64::MAX;
            }
            Wrong::BigBlock => {
                h.block_size = 1 << 17;
                h.total = u64::MAX;
            }
            Wrong::HugeCount => {
                h.region_count = (1 << 22) + 1;
                h.total = u64::MAX;
            }
            Wrong::SmallBlock => h.block_size = 512,
            Wrong::SmallTotal => h.total = 1,
            _ => {}
        }
        // A header no encoding carries (a block size out of range) is
        // answered as built.
        let header = if matches!(wrong, Wrong::BadBlock | Wrong::BigBlock) {
            Vec::new()
        } else {
            h.to_bytes().unwrap()
        };
        let mut regions: Vec<([u8; 128], Vec<u8>)> = k
            .regions
            .iter()
            .map(|(i, b)| (i.to_bytes().unwrap(), b.clone()))
            .collect();
        match wrong {
            // The first region (bytes of its own).
            Wrong::Short => {
                let _b = regions[0].1.pop();
            }
            Wrong::Long => regions[0].1.push(0),
            Wrong::Fewer => {
                let _r = regions.pop();
            }
            Wrong::More => regions.push(regions[0].clone()),
            Wrong::SameName => {
                let mut i = k.regions[1].0.clone();
                i.name = k.regions[0].0.name.clone();
                regions[1].0 = i.to_bytes().unwrap();
            }
            _ => {}
        }
        Self {
            head: h,
            header,
            regions,
            wrong,
            next: 0,
            at: 0,
        }
    }
}

impl ImportSource for Framed {
    fn header(&mut self) -> Result<ExportHeader, SinkError> {
        if matches!(self.wrong, Wrong::BadBlock | Wrong::BigBlock) {
            return Ok(self.head.clone());
        }
        ExportHeader::from_bytes(&self.header).map_err(|_| SinkError::Refused)
    }

    fn next_region(&mut self) -> Result<Option<RegionInfo>, SinkError> {
        let Some((rec, _)) = self.regions.get(self.next) else {
            return Ok(None);
        };
        self.next += 1;
        self.at = 0;
        if self.wrong == Wrong::BadInfo && self.next == 2 {
            // An info no record carries (generation 0), as a source might
            // build it by hand.
            let mut i = RegionInfo::from_bytes(rec, B).unwrap();
            i.generation = 0;
            return Ok(Some(i));
        }
        RegionInfo::from_bytes(rec, self.head.block_size)
            .map(Some)
            .map_err(|_| SinkError::Refused)
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize, SinkError> {
        if self.wrong == Wrong::Fails {
            return Err(SinkError::Other(7));
        }
        let data = &self.regions[self.next - 1].1;
        let n = (data.len() - self.at).min(buf.len());
        buf[..n].copy_from_slice(&data[self.at..self.at + n]);
        self.at += n;
        if self.wrong == Wrong::OverRead && n > 0 {
            return Ok(n + 1);
        }
        Ok(n)
    }
}

/// A caller's source carrying the export in its own framing restores the
/// store; each way it goes wrong is refused, nothing renamed.
#[test]
fn test_a_caller_s_source_is_checked() {
    let p = SimPlatform::new(SimConfig::volatile(4003));
    let s = every_kind(&p);
    let want = kept(&s);
    for wrong in [
        Wrong::Short,
        Wrong::Long,
        Wrong::OverRead,
        Wrong::Fewer,
        Wrong::More,
        Wrong::BadInfo,
        Wrong::SameName,
        Wrong::BadBlock,
        Wrong::SmallTotal,
        Wrong::BigBlock,
        Wrong::HugeCount,
        Wrong::SmallBlock,
        Wrong::Fails,
    ] {
        let mut src = Framed::new(&want, wrong);
        let e = import(p.clone(), Path::new(TO), &mut src, import_opts()).unwrap_err();
        let want_cause = match wrong {
            Wrong::Fails => NotWrittenCause::Interrupted,
            // The target device's blocks are larger than the export's.
            Wrong::SmallBlock => NotWrittenCause::Misaligned,
            _ => NotWrittenCause::InvalidOptions,
        };
        assert_eq!(
            cause(&e.error),
            Some(want_cause),
            "{wrong:?}: {:?}",
            e.error
        );
        assert!(!e.report.complete, "{wrong:?}");
        if wrong == Wrong::Fails {
            assert_eq!(e.source, Some(SinkError::Other(7)));
        }
        if wrong == Wrong::Fewer {
            // Refused where the missing region should have been, every
            // region before it imported.
            let last = want.regions.len() - 1;
            assert_eq!(e.report.index, Some(last as u32), "{wrong:?}");
            assert_eq!(e.report.regions.len(), last, "{wrong:?}");
        }
        none_restored(&p);
    }
    let mut src = Framed::new(&want, Wrong::None);
    let r = import(p.clone(), Path::new(TO), &mut src, import_opts()).unwrap();
    assert!(r.complete);
    restored_matches(&p, &want);
    slots_restored(&p);
    // A second import never replaces it.
    let mut src = Framed::new(&want, Wrong::None);
    let e = import(p.clone(), Path::new(TO), &mut src, import_opts()).unwrap_err();
    assert!(
        matches!(e.error, Error::AlreadyExists { .. }),
        "{:?}",
        e.error
    );
}

/// The header's and each region's bytes round-trip; a flipped byte or a
/// record of the wrong block is refused.
#[test]
fn test_the_header_and_infos_round_trip_as_bytes() {
    let p = SimPlatform::new(SimConfig::volatile(4004));
    let s = every_kind(&p);
    let k = kept(&s);
    let h = k.header.clone().unwrap();
    let bytes = h.to_bytes().unwrap();
    let back = ExportHeader::from_bytes(&bytes).unwrap();
    assert_eq!(back, h);
    let mut bad = bytes.clone();
    bad[64] ^= 1;
    assert_eq!(
        cause(&ExportHeader::from_bytes(&bad).unwrap_err()),
        Some(NotWrittenCause::InvalidOptions)
    );
    for (i, _) in &k.regions {
        let b = i.to_bytes().unwrap();
        assert_eq!(&RegionInfo::from_bytes(&b, B).unwrap(), i);
        // Validated for the block given: a data size of odd 4 KiB blocks is
        // no stream of 8 KiB ones.
        if i.name == "pages" {
            assert!(RegionInfo::from_bytes(&b, 2 * B).is_err());
        }
        // An unknown kind.
        let mut bad = b;
        bad[24] = 0xEE;
        assert!(RegionInfo::from_bytes(&bad, B).is_err(), "{}", i.name);
    }
    // An info whose fields disagree with its kind is no record.
    let mut i = k.regions[0].0.clone();
    i.durable_len = Some(B);
    assert!(i.to_bytes().is_err());
    // A block size no power of two, even one the codec's range takes by
    // its trailing zeros.
    let mut h2 = h.clone();
    h2.block_size = 4096 + 512;
    assert!(h2.to_bytes().is_err());
}

/// The source's directory and the target's: a missing directory or one
/// with no `regions.info` is no source; a target holding a store is
/// refused.
#[test]
fn test_directories_are_checked() {
    let p = SimPlatform::new(SimConfig::volatile(4005));
    let e = FileSource::open(p.clone(), Path::new(OUT), FileSourceOptions::default()).unwrap_err();
    assert!(matches!(e, Error::NotFound { .. }), "{e:?}");
    drop(p.open_dir(Path::new(OUT), true).unwrap());
    let e = FileSource::open(p.clone(), Path::new(OUT), FileSourceOptions::default()).unwrap_err();
    assert!(matches!(e, Error::Corruption { .. }), "{e:?}");
    let s = every_kind(&p);
    to_files(&p, &s);
    drop(Store::create(p.clone(), Path::new(TO), opts()).unwrap());
    let e = import_files(&p).unwrap_err();
    assert!(
        matches!(e.error, Error::AlreadyExists { .. }),
        "{:?}",
        e.error
    );
}

/// Too little free space for the header's total: `NoSpace` before
/// anything is written.
#[test]
fn test_too_little_space_is_refused_first() {
    let p = SimPlatform::new(SimConfig::volatile(4007));
    let s = every_kind(&p);
    let total = kept(&s).header.unwrap().total;
    to_files(&p, &s);
    // Room for the new store's metadata and some of its regions, short of
    // the header's total: refused before the store is created (no region
    // begun), not part way through.
    let used = p.with_world(|w| w.allocated_bytes());
    p.with_world(|w| w.faults_mut().capacity = Some(used + total - 4 * B));
    let e = import_files(&p).unwrap_err();
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
    assert!(e.report.regions.is_empty() && e.report.index.is_none());
    none_restored(&p);
}

/// The backup on another device, the target on the store's device with
/// its commits held (paced): the import's reads and writes are one call,
/// each charged to its own device's record, and it completes.
#[test]
fn test_an_import_across_devices_completes_with_commits_held() {
    let p = SimPlatform::new(SimConfig::volatile(4008).with_device(DeviceKind::VolatileCache));
    p.with_world(|w| w.place(OUT, 1));
    let s = every_kind(&p);
    let want = kept(&s);
    to_files(&p, &s);
    let hold = s.commit_hold();
    let r = import_files(&p).unwrap();
    assert!(r.complete);
    drop(hold);
    restored_matches(&p, &want);
}

/// A small store for the power-cut enumeration.
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

/// The power cut at every step of an import, then a crash that keeps,
/// drops or tears what was not flushed: a store under the target name is
/// whole, and where there is none an import into the same directory
/// succeeds (the temporary removed).
#[test]
fn test_a_power_cut_at_any_step_leaves_no_store_or_the_whole_one() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    // Built once (the store and its export to files, every handle
    // dropped); each run on a fork of it, the world exactly as a rebuild
    // from the same seed leaves it.
    let base = {
        let p = SimPlatform::new(SimConfig::volatile(4010));
        let s = small(&p);
        let want = kept(&s);
        to_files(&p, &s);
        drop(s);
        (p, want)
    };
    let fixture = || (base.0.fork(), &base.1);
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
        let (p, _w) = fixture();
        let steps = probe(&p, u64::MAX);
        let _r = import_files(&p).unwrap();
        steps.load(Ordering::Relaxed)
    };
    assert!(all > 10, "{all} steps");
    each_cut(all, |cut| {
        let (p, want) = fixture();
        let _steps = probe(&p, cut);
        let done = import_files(&p).is_ok();
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
                assert!(!done, "cut {cut}: an import that returned Ok is not there");
                assert!(matches!(e, Error::NotFound { .. }), "cut {cut}: {e:?}");
                let r = import_files(&p).unwrap();
                assert!(r.complete, "cut {cut}");
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

/// `regions.info`'s bytes as `OUT` holds them.
fn info_of(p: &SimPlatform) -> Vec<u8> {
    p.with_world(|w| {
        w.media_of(&PathBuf::from(OUT), REGIONS_INFO)
            .map(<[u8]>::to_vec)
    })
    .unwrap()
}

/// `name` in `dir` replaced by `bytes`, durable.
fn put_file(p: &SimPlatform, dir: &str, name: &str, bytes: &[u8]) {
    let d = p.open_dir(Path::new(dir), false).unwrap();
    let n = FileName::new(name).unwrap();
    // Replaced, or made.
    let _gone = p.unlink(&d, &n);
    let f = p.create_file(&d, &n).unwrap();
    let pool = BufPool::new(&PoolConfig::uniform(B as usize, B as usize, B as usize, 2)).unwrap();
    let mut q = p.queue(QueueConfig::default()).unwrap();
    let mut out = CompletionBuf::with_capacity(2);
    for (i, c) in bytes.chunks(B as usize).enumerate() {
        let mut buf = pool.take(B as usize).unwrap();
        buf.as_mut_capacity().fill(0);
        buf.as_mut_capacity()[..c.len()].copy_from_slice(c);
        assert!(buf.set_len(B as usize));
        q.submit(
            IoOp::Write {
                file: &f,
                offset: i as u64 * B,
                buf,
                dsync: false,
            },
            1,
        )
        .unwrap();
        let _n = q.wait(1, &mut out).unwrap();
        let _done = out.drain().count();
    }
    p.set_len(&f, bytes.len() as u64).unwrap();
    p.flush_all(&f).unwrap();
    p.sync_dir(&d).unwrap();
}

/// `regions.info` re-encoded with `verified` as its verified list.
fn with_verified(raw: &[u8], verified: &[(RegionName, u64)]) -> Vec<u8> {
    let x = exportinfo::decode(raw).unwrap();
    let regions: Vec<(RegionRecord, Vec<u32>)> = x
        .regions()
        .map(|(r, s)| (r, (0..s.len()).map(|i| s.get(i).unwrap()).collect()))
        .collect();
    let items: Vec<exportinfo::RegionItem<'_>> =
        regions.iter().map(|(r, s)| (*r, s.as_slice())).collect();
    let h: InfoHeader = x.header;
    let len = exportinfo::encoded_len(0, 0, 0, verified.len(), &items).unwrap();
    let mut out = vec![0u8; len];
    let n = exportinfo::encode(&h, &[], &[], &[], verified, &items, &mut out).unwrap();
    out.truncate(n);
    out
}

/// What `FileSource` checks of the directory itself: bytes after the last
/// region (sealed by a CRC of their own), a verified region not exported
/// in full, a data file longer than its region's stream: each
/// `Corruption`, nothing renamed. And its calls in order: a region not read
/// to its end before the next is asked for is refused.
#[test]
fn test_a_file_export_s_layout_is_checked() {
    let fixture = || {
        let p = SimPlatform::new(SimConfig::volatile(4020));
        let s = every_kind(&p);
        to_files(&p, &s);
        p
    };
    let corrupt = |e: &Error| {
        matches!(
            e,
            Error::Corruption {
                kind: CorruptionKind::Metadata,
                ..
            }
        )
    };
    // Bytes after the last region, the record resealed.
    let p = fixture();
    let raw = info_of(&p);
    let mut long = raw[..raw.len() - 4].to_vec();
    long.extend_from_slice(&[0; 8]);
    let crc = crc32c(&long);
    long.extend_from_slice(&crc.to_le_bytes());
    put_file(&p, OUT, REGIONS_INFO, &long);
    let e = import_files(&p).unwrap_err();
    assert!(corrupt(&e.error), "{:?}", e.error);
    none_restored(&p);
    // A verified region that was not exported in full.
    let p = fixture();
    let raw = info_of(&p);
    put_file(
        &p,
        OUT,
        REGIONS_INFO,
        &with_verified(&raw, &[(RegionName::new("pages").unwrap(), B)]),
    );
    let e = import_files(&p).unwrap_err();
    assert!(corrupt(&e.error), "{:?}", e.error);
    none_restored(&p);
    // A data file longer than its region's stream.
    let p = fixture();
    let d = p.open_dir(Path::new(OUT), false).unwrap();
    let f = p
        .open_file(
            &d,
            &FileName::new("region-00000.data").unwrap(),
            FileMode::ReadWrite,
        )
        .unwrap();
    let len = p.size(&f).unwrap();
    p.set_len(&f, len + B).unwrap();
    p.flush_all(&f).unwrap();
    let e = import_files(&p).unwrap_err();
    assert!(corrupt(&e.error), "{:?}", e.error);
    assert_eq!(e.report.index, Some(0));
    none_restored(&p);
    // Driven by hand: the next region before this one's bytes are read.
    let p = fixture();
    let mut src = source(&p);
    let _h = src.header().unwrap();
    let _first = src.next_region().unwrap().unwrap();
    assert_eq!(src.next_region().unwrap_err(), SinkError::Refused);
    assert_eq!(src.read(&mut [0u8; 8]).unwrap_err(), SinkError::Refused);
}

/// `regions.info`'s last four bytes made the CRC-32C of the rest.
fn reseal(mut b: Vec<u8>) -> Vec<u8> {
    let n = b.len() - 4;
    let crc = crc32c(&b[..n]);
    b[n..].copy_from_slice(&crc.to_le_bytes());
    b
}

/// `regions.info` re-encoded with each region record changed by `f`.
fn rewrite(raw: &[u8], f: &dyn Fn(usize, &mut RegionRecord)) -> Vec<u8> {
    let x = exportinfo::decode(raw).unwrap();
    let regions: Vec<(RegionRecord, Vec<u32>)> = x
        .regions()
        .enumerate()
        .map(|(i, (mut r, s))| {
            f(i, &mut r);
            (r, (0..s.len()).map(|k| s.get(k).unwrap()).collect())
        })
        .collect();
    let items: Vec<exportinfo::RegionItem<'_>> =
        regions.iter().map(|(r, s)| (*r, s.as_slice())).collect();
    let len = exportinfo::encoded_len(0, 0, 0, 0, &items).unwrap();
    let mut out = vec![0u8; len];
    let n = exportinfo::encode(&x.header, &[], &[], &[], &[], &items, &mut out).unwrap();
    out.truncate(n);
    out
}

/// A `FileSource` whose `regions.info` changes after its CRC pass (between
/// `header` and the first region): what it decodes is no longer what it
/// checked.
struct Swapped<'a> {
    inner: FileSource<SimPlatform>,
    p: &'a SimPlatform,
}

impl ImportSource for Swapped<'_> {
    fn header(&mut self) -> Result<ExportHeader, SinkError> {
        let h = self.inner.header()?;
        // Region 0's tag: a value any record may hold.
        flip(self.p, OUT, REGIONS_INFO, 64 + 28);
        Ok(h)
    }
    fn next_region(&mut self) -> Result<Option<RegionInfo>, SinkError> {
        self.inner.next_region()
    }
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, SinkError> {
        self.inner.read(buf)
    }
}

/// `regions.info` changed between `FileSource`'s CRC pass and its decoding
/// is refused at the end (`Corruption`), nothing renamed.
#[test]
fn test_a_record_changed_after_its_crc_pass_is_refused() {
    let p = SimPlatform::new(SimConfig::volatile(4030));
    let s = every_kind(&p);
    to_files(&p, &s);
    let mut src = Swapped {
        inner: source(&p),
        p: &p,
    };
    let e = import(p.clone(), Path::new(TO), &mut src, import_opts()).unwrap_err();
    assert!(
        matches!(
            e.error,
            Error::Corruption {
                kind: CorruptionKind::Metadata,
                ..
            }
        ),
        "{:?}",
        e.error
    );
    none_restored(&p);
}

/// A store created under the target name after the first look and before
/// the copy lock is seen by the second look, under the lock: the import
/// is refused `AlreadyExists` having written nothing and made no
/// temporary.
#[test]
fn test_a_store_made_before_the_lock_is_seen_under_it() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    let p = SimPlatform::new(SimConfig::volatile(4031));
    let s = every_kind(&p);
    to_files(&p, &s);
    drop(s);
    let fired = Arc::new(AtomicBool::new(false));
    let after = Arc::new(AtomicU64::new(0));
    let (q, f, a) = (p.clone(), Arc::clone(&fired), Arc::clone(&after));
    p.with_world(|w| {
        w.set_lock_probe(Some(Arc::new(move || {
            if !f.swap(true, Ordering::SeqCst) {
                drop(Store::create(q.clone(), Path::new(TO), opts()).unwrap());
                a.store(q.with_world(|w| w.write_submits()), Ordering::SeqCst);
            }
        })));
    });
    let e = import_files(&p).unwrap_err();
    p.with_world(|w| w.set_lock_probe(None));
    assert!(fired.load(Ordering::SeqCst), "the probe never ran");
    assert!(
        matches!(e.error, Error::AlreadyExists { .. }),
        "{:?}",
        e.error
    );
    assert_eq!(
        p.with_world(|w| w.write_submits()),
        after.load(Ordering::SeqCst),
        "the import wrote"
    );
    let made = p.with_world(|w| {
        w.media_of(&PathBuf::from(TO), store_io_engine::COPYING)
            .is_some()
    });
    assert!(!made, "a temporary was made");
}

/// A store of one page region of one block (a small `regions.info`).
fn one_page(p: &SimPlatform) {
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let r = s.provision_page_region("p", B).unwrap();
    let _d = r
        .write_durable(r.pos(0).unwrap(), &stamped(9, B as usize))
        .unwrap();
    to_files(p, &s);
}

/// What `FileSource` refuses of `regions.info` before trusting it, driven
/// by hand: a file too short for a header, a region count past the
/// format's table, lists running past the body, a record whose spans run
/// past it, and calls out of order.
#[test]
fn test_file_source_bounds_and_order() {
    let corrupt = |e: SinkError| assert_eq!(e, SinkError::Refused);
    let fresh = |seed: u64| {
        let p = SimPlatform::new(SimConfig::volatile(seed));
        one_page(&p);
        p
    };
    // Too short for a header and its CRC.
    let p = fresh(4040);
    put_file(&p, OUT, REGIONS_INFO, &[1, 2]);
    corrupt(source(&p).header().unwrap_err());
    // A region count past the format's table (sealed).
    let p = fresh(4041);
    let mut raw = info_of(&p);
    raw[16..20].copy_from_slice(&((1u32 << 22) + 1).to_le_bytes());
    put_file(&p, OUT, REGIONS_INFO, &reseal(raw));
    corrupt(source(&p).header().unwrap_err());
    // Lists running past the body (a lost page counted, none written: its
    // item would be the CRC's bytes).
    let p = SimPlatform::new(SimConfig::volatile(4042));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    to_files(&p, &s);
    let mut raw = info_of(&p);
    raw[36..40].copy_from_slice(&1u32.to_le_bytes());
    put_file(&p, OUT, REGIONS_INFO, &reseal(raw));
    corrupt(source(&p).header().unwrap_err());
    // A record whose spans run past the body: its stream one span longer
    // (its data file too), its span CRCs as they were.
    let p = fresh(4043);
    let mut raw = info_of(&p);
    for at in [64 + 64, 64 + 88] {
        let v = u64::from_le_bytes(raw[at..at + 8].try_into().unwrap()) + SPAN;
        raw[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }
    put_file(&p, OUT, REGIONS_INFO, &reseal(raw));
    let d = p.open_dir(Path::new(OUT), false).unwrap();
    let f = p
        .open_file(
            &d,
            &FileName::new("region-00000.data").unwrap(),
            FileMode::ReadWrite,
        )
        .unwrap();
    p.set_len(&f, B + SPAN).unwrap();
    let mut src = source(&p);
    let _h = src.header().unwrap();
    corrupt(src.next_region().unwrap_err());
    // Out of order: a second header, a read before any region.
    let p = fresh(4044);
    let mut src = source(&p);
    let _h = src.header().unwrap();
    corrupt(src.header().unwrap_err());
    let mut src = source(&p);
    let _h = src.header().unwrap();
    corrupt(src.read(&mut [0u8; 8]).unwrap_err());
}

/// A region record the format accepts but `RegionInfo::validate` does not
/// (a cap of no whole blocks) is refused by `FileSource` itself, as its
/// directory not being restorable (`Corruption`), before its files are
/// opened.
#[test]
fn test_file_source_validates_each_region() {
    let p = SimPlatform::new(SimConfig::volatile(4045));
    let s = every_kind(&p);
    to_files(&p, &s);
    let raw = info_of(&p);
    let bad = rewrite(&raw, &|i, r| {
        if i == 1 {
            r.cap = r.cap.map(|c| c + 1);
        }
    });
    put_file(&p, OUT, REGIONS_INFO, &bad);
    let e = import_files(&p).unwrap_err();
    assert!(
        matches!(
            e.error,
            Error::Corruption {
                kind: CorruptionKind::Metadata,
                ..
            }
        ),
        "{:?}",
        e.error
    );
    assert_eq!(e.report.index, Some(1));
    none_restored(&p);
}

/// An export of 64 KiB blocks read back by a `FileSource` paced at 20 KiB
/// pieces, imported into a store of 64 KiB blocks on the same device: the
/// fill's pieces are whole blocks of the target, and every byte arrives.
#[test]
fn test_an_import_of_large_blocks_with_small_source_pieces() {
    blocks_against_pieces(4046, 1 << 16);
}

/// The same with 8 KiB blocks, smaller than the source's 20 KiB piece: the
/// source reads its data files in whole blocks (its piece rounded down).
#[test]
fn test_an_import_of_blocks_under_the_source_s_piece() {
    blocks_against_pieces(4047, 8 << 10);
}

/// An export of `block`-byte blocks read by a source paced at 20 KiB
/// pieces into a store of the same blocks: every write to the new store
/// and every read of a data file in whole blocks, every byte restored.
fn blocks_against_pieces(seed: u64, block: u64) {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    let big = StoreOptions {
        block_size: Some(block as u32),
        ..opts()
    };
    let want = {
        let s = Store::create(p.clone(), Path::new(DIR), big.clone()).unwrap();
        assert_eq!(s.report().block_size, block);
        let r = s.provision_page_region("p", 64 * block).unwrap();
        let _d = r
            .write_durable(r.pos(0).unwrap(), &stamped(5, (64 * block) as usize))
            .unwrap();
        let k = kept(&s);
        to_files(&p, &s);
        k
    };
    let mut so = FileSourceOptions::default();
    so.store.background_io = 20 << 10;
    let mut src = FileSource::open(p.clone(), Path::new(OUT), so).unwrap();
    let mut io = ImportOptions::default();
    io.store = big.clone();
    let r = import(p.clone(), Path::new(TO), &mut src, io).unwrap();
    assert!(r.complete);
    let t = Store::open(p.clone(), Path::new(TO), big).unwrap();
    let got = kept(&t);
    assert!(got.regions[0].1 == want.regions[0].1);
    // Every write to the new store's container in whole blocks (the
    // group's piece is the source's 20 KiB), and every read of a data
    // file at a whole block.
    let (odd, reads): (Vec<(u64, u64)>, Vec<u64>) = p.with_world(|w| {
        let id = w
            .file_id(&PathBuf::from(TO), store_io_engine::CONTAINER)
            .unwrap();
        let data = w.file_id(&PathBuf::from(OUT), "region-00000.data").unwrap();
        let writes = w
            .trace()
            .iter()
            .filter_map(|e| match e {
                store_io_sim::world::TraceEvent::Write {
                    file, offset, len, ..
                } if *file == id && (offset % block != 0 || len % block != 0) => {
                    Some((*offset, *len))
                }
                _ => None,
            })
            .collect();
        let reads = w
            .trace()
            .iter()
            .filter_map(|e| match e {
                store_io_sim::world::TraceEvent::Read { file, offset, .. }
                    if *file == data && offset % block != 0 =>
                {
                    Some(*offset)
                }
                _ => None,
            })
            .collect();
        (writes, reads)
    });
    assert!(odd.is_empty(), "writes not in whole blocks: {odd:?}");
    assert!(reads.is_empty(), "reads not at whole blocks: {reads:?}");
}

/// A header the import cannot take (a block size no power of two or past
/// 64 KiB, a region count past the format's table) is refused before the
/// target directory is touched: nothing is made there.
#[test]
fn test_a_bad_header_is_refused_before_the_target_is_touched() {
    for (seed, wrong) in [
        (4050, Wrong::BadBlock),
        (4051, Wrong::BigBlock),
        (4052, Wrong::HugeCount),
    ] {
        let p = SimPlatform::new(SimConfig::volatile(seed));
        let s = every_kind(&p);
        let want = kept(&s);
        let mut src = Framed::new(&want, wrong);
        let e = import(p.clone(), Path::new(TO), &mut src, import_opts()).unwrap_err();
        assert_eq!(
            cause(&e.error),
            Some(NotWrittenCause::InvalidOptions),
            "{wrong:?}"
        );
        assert!(
            p.open_dir(Path::new(TO), false).is_err(),
            "{wrong:?}: the target directory was made"
        );
    }
}

/// A `regions.info` whose head is past `FileSource`'s window (8,200
/// omitted ids, no region) is read from its start twice; its header
/// changed between the two reads (here: the writable flag) is refused,
/// though every byte the running CRC covers is unchanged.
#[test]
fn test_a_long_head_s_header_changed_between_its_reads_is_refused() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    let p = SimPlatform::new(SimConfig::volatile(4060));
    drop(p.open_dir(Path::new(OUT), true).unwrap());
    let omitted: Vec<u64> = (1..=8200).collect();
    let h = InfoHeader {
        writable: false,
        block_log2: 12,
        region_count: 0,
        lend: 5,
        total: 1 << 30,
        omitted_count: 0,
        lost_count: 0,
        stale_count: 0,
        verified_count: 0,
    };
    let len = exportinfo::encoded_len(omitted.len(), 0, 0, 0, &[]).unwrap();
    assert!(len > (64 << 10) + 64, "a head past the window");
    let mut out = vec![0u8; len];
    let n = exportinfo::encode(&h, &omitted, &[], &[], &[], &[], &mut out).unwrap();
    put_file(&p, OUT, REGIONS_INFO, &out[..n]);
    // Unchanged, it reads back.
    let got = source(&p).header().unwrap();
    assert_eq!(got.facts.omitted.len(), 8200);
    assert!(!got.writable);
    // The third read (after the CRC pass and the first window) is the
    // head's second: the writable flag flipped just before it.
    let reads = Arc::new(AtomicU64::new(0));
    let fired = Arc::new(AtomicBool::new(false));
    let (q, r, f) = (p.clone(), Arc::clone(&reads), Arc::clone(&fired));
    let mut src = source(&p);
    p.with_world(|w| {
        w.set_submit_probe(Some(Arc::new(move || {
            if f.load(Ordering::SeqCst) {
                return;
            }
            if r.fetch_add(1, Ordering::SeqCst) == 2 {
                f.store(true, Ordering::SeqCst);
                flip(&q, OUT, REGIONS_INFO, 10);
            }
        })));
    });
    let e = src.header();
    p.with_world(|w| w.set_submit_probe(None));
    assert!(fired.load(Ordering::SeqCst), "the probe never ran");
    assert_eq!(e.unwrap_err(), SinkError::Refused);
}

/// A data file cut short after its region began (its size checked) is
/// refused at the read that meets its end, never read forever.
#[test]
fn test_a_data_file_cut_short_mid_region_is_refused() {
    let p = SimPlatform::new(SimConfig::volatile(4061));
    let s = every_kind(&p);
    to_files(&p, &s);
    let mut src = source(&p);
    let _h = src.header().unwrap();
    let first = src.next_region().unwrap().unwrap();
    assert!(first.stream_len() > 2 * SPAN);
    let d = p.open_dir(Path::new(OUT), false).unwrap();
    let f = p
        .open_file(
            &d,
            &FileName::new("region-00000.data").unwrap(),
            FileMode::ReadWrite,
        )
        .unwrap();
    p.set_len(&f, SPAN + 100).unwrap();
    let mut buf = vec![0u8; CHUNK];
    let e = loop {
        match src.read(&mut buf) {
            Ok(0) => panic!("the region ended early with no refusal"),
            Ok(_) => {}
            Err(e) => break e,
        }
    };
    assert_eq!(e, SinkError::Refused);
}

/// A verified length past its region's stream is refused (`Corruption`),
/// however the rest checks out; at the stream's length it imports.
#[test]
fn test_a_verified_length_past_its_stream_is_refused() {
    for (seed, past) in [(4062, true), (4063, false)] {
        let p = SimPlatform::new(SimConfig::volatile(seed));
        let s = every_kind(&p);
        to_files(&p, &s);
        let raw = info_of(&p);
        let x = exportinfo::decode(&raw).unwrap();
        let regions: Vec<(RegionRecord, Vec<u32>)> = x
            .regions()
            .enumerate()
            .map(|(i, (mut r, sp))| {
                // The first region (a page region) as one exported in full.
                if i == 0 {
                    r.full_copy = true;
                }
                (r, (0..sp.len()).map(|k| sp.get(k).unwrap()).collect())
            })
            .collect();
        let stream = regions[0].0.stream_len;
        let verified = [(
            RegionName::new("pages").unwrap(),
            if past { 1 << 60 } else { stream },
        )];
        let items: Vec<exportinfo::RegionItem<'_>> =
            regions.iter().map(|(r, sp)| (*r, sp.as_slice())).collect();
        let len = exportinfo::encoded_len(0, 0, 0, 1, &items).unwrap();
        let mut out = vec![0u8; len];
        let n = exportinfo::encode(&x.header, &[], &[], &[], &verified, &items, &mut out).unwrap();
        put_file(&p, OUT, REGIONS_INFO, &out[..n]);
        let got = import_files(&p);
        if past {
            let e = got.unwrap_err();
            assert!(
                matches!(
                    e.error,
                    Error::Corruption {
                        kind: CorruptionKind::Metadata,
                        ..
                    }
                ),
                "{:?}",
                e.error
            );
            none_restored(&p);
        } else {
            assert!(got.unwrap().complete);
        }
    }
}
