//! W4: page batches: N random 4 KiB pages, one barrier.
//!
//! For N in {1, 16, 64, 256} distinct random pages of a 256 MiB region:
//!
//! - store-io: a `PageBatch` (reused across commits): N × `write`, then
//!   `commit()` (sorted, submitted together, one barrier).
//! - raw QD1: N unbuffered writes one after another, then one flush.
//! - raw QD-N (Windows only): the N writes issued together as overlapped
//!   I/O, then one flush; the queue-depth > 1 floor store-io-win aims at.
//!   (store-io-posix is synchronous, so on Linux QD1 is the matching floor.)
//! - fsys: N × `Handle::write_at`, then one `Handle::sync`.

use std::time::Instant;

use super::{
    Ctx, KIB4, Snap, Step, fserr, ioerr, record_run, remove, sample, sioerr, time_boxed, with_store,
};
use crate::aligned::AlignedBuf;
use crate::data::Rng;
use crate::json::Json;
use crate::raw::RawFile;
use crate::report::{Better, Comparison, Point, Section, System, Table, Verify};

/// Pages per commit.
pub const COUNTS: [usize; 4] = [1, 16, 64, 256];
/// Region / file size.
const SPAN: u64 = 256 << 20;
/// Pages in the span.
const PAGES: usize = (SPAN / KIB4 as u64) as usize;
/// Read-back checks per point.
const CHECKS: usize = 1024;

fn stream_of(n: usize, sys: u64) -> u64 {
    4_000_000 + n as u64 * 10 + sys
}

/// Chooses `n` distinct random pages for commit `c`.
struct Picker {
    rng: Rng,
    mark: Vec<u64>,
}

impl Picker {
    fn new(seed: u64) -> Self {
        Self {
            rng: Rng::new(seed),
            mark: vec![u64::MAX; PAGES],
        }
    }

    fn pick(&mut self, c: u64, n: usize, out: &mut Vec<usize>) {
        out.clear();
        while out.len() < n {
            let p = self.rng.below(PAGES as u64) as usize;
            if self.mark[p] != c {
                self.mark[p] = c;
                out.push(p);
            }
        }
    }
}

/// Runs W4.
pub fn run(ctx: &Ctx) -> Section {
    let mut sec = Section::new(
        "page-batch",
        "W4: page batch (N random 4 KiB pages, one barrier)",
    );
    sec.method = vec![
        format!("N in {COUNTS:?} distinct pages chosen uniformly at random from a 256 MiB region / file for every commit; one commit = N page writes + one barrier; commits back to back on one thread."),
        "store-io: one `PageBatch` reused for every commit: N x `write(pos, page)` then `commit()`.".to_owned(),
        format!("raw QD1: N x {} one after another, then one data flush.", if cfg!(windows) { "WriteFile" } else { "pwritev2" }),
        "raw QD-N (Windows only): the N writes issued together on an overlapped NO_BUFFERING handle (one event each), all awaited, then one data flush on that handle.".to_owned(),
        "fsys: N x `Handle::write_at(path, offset, page)` then one `Handle::sync(path)`.".to_owned(),
        "Every system rewrites its own ready file / region; page payloads are copied once inside the timed section by every system.".to_owned(),
    ];
    let mut pts = Vec::new();
    for &n in &COUNTS {
        pts.push(store_point(ctx, n));
        pts.push(raw_point(ctx, n, false));
        if cfg!(windows) {
            pts.push(raw_point(ctx, n, true));
        }
        pts.push(fsys_point(ctx, n));
    }
    let mut t = Table::new(
        "Results:",
        &[
            "N",
            "system",
            "variant",
            "commits/s",
            "pages/s",
            "commit p50 µs",
            "commit p99 µs",
            "commit p99.9 µs",
            "write calls/commit",
            "flushes/commit",
        ],
    );
    for p in &pts {
        t.row(vec![
            p.cell("n", 0),
            p.system.name().to_owned(),
            p.variant.clone(),
            p.cell("commits_per_s", 0),
            p.cell("pages_per_s", 0),
            p.cell("p50_us", 1),
            p.cell("p99_us", 1),
            p.cell("p999_us", 1),
            p.cell("write_calls_per_op", 2),
            p.cell("flushes_per_op", 2),
        ]);
    }
    sec.tables.push(t);
    let raw_floor = if cfg!(windows) { "QD-N" } else { "QD1" };
    for &n in &COUNTS {
        let get = |sys: System, v: &str, k: &str| {
            pts.iter()
                .find(|p| p.system == sys && p.variant.contains(v) && p.get("n") == Some(n as f64))
                .and_then(|p| p.get(k))
        };
        for (k, metric, better) in [
            ("pages_per_s", "pages/s", Better::Higher),
            ("p99_us", "commit p99 µs", Better::Lower),
        ] {
            sec.comparisons.push(Comparison {
                case: format!("{n} random 4 KiB pages + one barrier"),
                metric: metric.to_owned(),
                better,
                store_io: get(System::StoreIo, "", k),
                raw: get(System::Raw, raw_floor, k),
                fsys: get(System::Fsys, "", k),
                note: format!("PageBatch::commit vs raw {raw_floor} writes + flush vs fsys write_at x N + sync"),
            });
        }
    }
    sec.points = pts;
    sec.collect_point_anomalies();
    sec
}

fn record(p: &mut Point, run: &super::Run, n: usize) {
    record_run(p, run, "commits_per_s");
    p.metric("pages_per_s", run.rate().map(|c| c * n as f64));
}

fn point(sys: System, variant: &str, n: usize) -> Point {
    Point::new(sys, variant)
        .param("n", Json::Num(n as f64))
        .param("page_bytes", Json::Num(KIB4 as f64))
}

fn store_point(ctx: &Ctx, n: usize) -> Point {
    let p = point(System::StoreIo, "PageBatch: N x write + commit", n);
    with_store(ctx, &format!("w4-{n}"), p, |store, p| {
        let pages = store
            .provision_page_region("w4", SPAN)
            .map_err(|e| sioerr("provision_page_region", &e))?;
        let stream = stream_of(n, 0);
        let mut picker = Picker::new(0xC0FFEE ^ n as u64);
        let mut chosen = Vec::with_capacity(n);
        let mut bufs: Vec<Vec<u8>> = (0..n).map(|_| vec![0u8; KIB4]).collect();
        let mut last = vec![u64::MAX; PAGES];
        let mut batch = pages.batch();
        let mut short = 0u64;
        let run = time_boxed(ctx, &|| Snap::store(store), |c| {
            picker.pick(c, n, &mut chosen);
            let mut poss = Vec::with_capacity(n);
            for (k, (&pg, b)) in chosen.iter().zip(bufs.iter_mut()).enumerate() {
                ctx.pool.fill(stream, c * n as u64 + k as u64, b);
                poss.push(
                    pages
                        .pos((pg * KIB4) as u64)
                        .map_err(|e| sioerr("PageRegion::pos", &e))?,
                );
            }
            let t = Instant::now();
            for (pos, b) in poss.into_iter().zip(bufs.iter()) {
                batch
                    .write(pos, b)
                    .map_err(|e| sioerr("PageBatch::write", &e))?;
            }
            let rc = batch
                .commit()
                .map_err(|e| sioerr("PageBatch::commit", &e))?;
            let d = t.elapsed();
            let hi = chosen.iter().max().copied().unwrap_or(0);
            if rc.durable_through() < ((hi + 1) * KIB4) as u64 {
                short += 1;
            }
            for (k, &pg) in chosen.iter().enumerate() {
                last[pg] = c * n as u64 + k as u64;
            }
            Ok(Step::Op(d))
        })?;
        record(p, &run, n);
        let written: Vec<(usize, u64)> = last
            .iter()
            .enumerate()
            .filter(|(_, i)| **i != u64::MAX)
            .map(|(s, i)| (s, *i))
            .collect();
        let idx = sample(written.len(), CHECKS);
        let mut out = vec![0u8; KIB4];
        for &k in &idx {
            let (pg, i) = written[k];
            let pos = pages
                .pos((pg * KIB4) as u64)
                .map_err(|e| sioerr("PageRegion::pos", &e))?;
            let got = pages
                .read(pos, &mut out)
                .map_err(|e| sioerr("PageRegion::read", &e))?;
            if got != KIB4 || !ctx.pool.matches(stream, i, &out) {
                p.verify =
                    Verify::Failed(format!("page {pg} (write {i}) reads back different bytes"));
                return Ok(());
            }
        }
        p.verify = if short > 0 {
            Verify::Failed(format!("{short} receipts did not reach their highest page"))
        } else {
            Verify::Passed(format!(
                "{} of {} written pages read back (latest write each) and compared byte for byte",
                idx.len(),
                written.len()
            ))
        };
        Ok(())
    })
}

fn raw_point(ctx: &Ctx, n: usize, overlapped: bool) -> Point {
    let variant = if overlapped {
        "QD-N: N overlapped writes in flight + one flush"
    } else {
        "QD1: N writes one at a time + one flush"
    };
    let mut p = point(System::Raw, variant, n);
    let stream = stream_of(n, if overlapped { 2 } else { 1 });
    let path = ctx.path("w4-raw.dat");
    let r = (|| -> Result<(), String> {
        let (f, _) = RawFile::create_ready(&path, SPAN).map_err(|e| ioerr("raw create", &e))?;
        let mut ov = if overlapped {
            Some(open_overlapped(&path, n)?)
        } else {
            None
        };
        let mut picker = Picker::new(0xC0FFEE ^ n as u64);
        let mut chosen = Vec::with_capacity(n);
        let mut srcs: Vec<Vec<u8>> = (0..n).map(|_| vec![0u8; KIB4]).collect();
        let mut bufs: Vec<AlignedBuf> = (0..n).map(|_| AlignedBuf::zeroed(KIB4)).collect();
        let mut last = vec![u64::MAX; PAGES];
        let run = time_boxed(ctx, &Snap::os, |c| {
            picker.pick(c, n, &mut chosen);
            for (k, s) in srcs.iter_mut().enumerate() {
                ctx.pool.fill(stream, c * n as u64 + k as u64, s);
            }
            let t = Instant::now();
            for (b, s) in bufs.iter_mut().zip(srcs.iter()) {
                b.as_mut_slice().copy_from_slice(s);
            }
            write_commit(&f, ov.as_mut(), &chosen, &bufs)?;
            let d = t.elapsed();
            for (k, &pg) in chosen.iter().enumerate() {
                last[pg] = c * n as u64 + k as u64;
            }
            Ok(Step::Op(d))
        })?;
        record(&mut p, &run, n);
        if let Some(pending) = pending_flushes(ov.as_ref()) {
            p.metric("flush_status_pending", Some(pending as f64));
            p.notes.push(format!(
                "NtFlushBuffersFileEx on this overlapped handle returned STATUS_PENDING {pending} times in {} flushes",
                run.total_ops
            ));
        }
        p.verify = super::lone::verify_raw_slots(ctx, &f, &last, stream, KIB4, CHECKS);
        Ok(())
    })();
    if let Err(e) = r {
        p.error = Some(e);
    }
    if let Some(e) = remove(&path) {
        p.notes.push(e);
    }
    p
}

#[cfg(windows)]
type Ov = crate::raw::OverlappedFile;
#[cfg(not(windows))]
type Ov = ();

#[cfg(windows)]
fn open_overlapped(path: &std::path::Path, n: usize) -> Result<Ov, String> {
    crate::raw::OverlappedFile::open(path, n).map_err(|e| ioerr("overlapped open", &e))
}
#[cfg(not(windows))]
fn open_overlapped(_path: &std::path::Path, _n: usize) -> Result<Ov, String> {
    Err("overlapped raw writes are measured on Windows only".to_owned())
}

#[cfg(windows)]
fn pending_flushes(ov: Option<&Ov>) -> Option<u64> {
    ov.map(|o| o.flush_pending)
}
#[cfg(not(windows))]
fn pending_flushes(_ov: Option<&Ov>) -> Option<u64> {
    None
}

/// Writes one commit's pages and flushes.
fn write_commit(
    f: &RawFile,
    ov: Option<&mut Ov>,
    chosen: &[usize],
    bufs: &[AlignedBuf],
) -> Result<(), String> {
    match ov {
        #[cfg(windows)]
        Some(o) => {
            let writes: Vec<(u64, &[u8])> = chosen
                .iter()
                .zip(bufs)
                .map(|(&pg, b)| ((pg * KIB4) as u64, b.as_slice()))
                .collect();
            o.write_all_at(&writes)
                .map_err(|e| ioerr("overlapped write", &e))?;
            o.flush_data().map_err(|e| ioerr("overlapped flush", &e))
        }
        #[cfg(not(windows))]
        Some(()) => Err("overlapped raw writes are measured on Windows only".to_owned()),
        None => {
            for (&pg, b) in chosen.iter().zip(bufs) {
                f.write_at(b.as_slice(), (pg * KIB4) as u64)
                    .map_err(|e| ioerr("raw write", &e))?;
            }
            f.flush_data().map_err(|e| ioerr("raw flush", &e))
        }
    }
}

fn fsys_point(ctx: &Ctx, n: usize) -> Point {
    let mut p = point(System::Fsys, "Handle::write_at x N + Handle::sync", n);
    let stream = stream_of(n, 3);
    let path = ctx.path("w4-fsys.dat");
    let r = (|| -> Result<(), String> {
        let (f, _) = RawFile::create_ready(&path, SPAN).map_err(|e| ioerr("prepare", &e))?;
        drop(f);
        let mut picker = Picker::new(0xC0FFEE ^ n as u64);
        let mut chosen = Vec::with_capacity(n);
        let mut bufs: Vec<Vec<u8>> = (0..n).map(|_| vec![0u8; KIB4]).collect();
        let mut last = vec![u64::MAX; PAGES];
        let run = time_boxed(ctx, &Snap::os, |c| {
            picker.pick(c, n, &mut chosen);
            for (k, b) in bufs.iter_mut().enumerate() {
                ctx.pool.fill(stream, c * n as u64 + k as u64, b);
            }
            let t = Instant::now();
            for (&pg, b) in chosen.iter().zip(bufs.iter()) {
                ctx.fsys
                    .write_at(&path, (pg * KIB4) as u64, b)
                    .map_err(|e| fserr("write_at", &e))?;
            }
            ctx.fsys.sync(&path).map_err(|e| fserr("sync", &e))?;
            let d = t.elapsed();
            for (k, &pg) in chosen.iter().enumerate() {
                last[pg] = c * n as u64 + k as u64;
            }
            Ok(Step::Op(d))
        })?;
        record(&mut p, &run, n);
        let f = RawFile::open(&path).map_err(|e| ioerr("reopen", &e))?;
        p.verify = super::lone::verify_raw_slots(ctx, &f, &last, stream, KIB4, CHECKS);
        Ok(())
    })();
    if let Err(e) = r {
        p.error = Some(e);
    }
    if let Some(e) = remove(&path) {
        p.notes.push(e);
    }
    p
}
