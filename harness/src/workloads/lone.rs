//! W1: one writer, queue depth 1, durable 4 KiB writes.
//!
//! - store-io: `AppendRegion::append_durable` and `PageRegion::write_durable`
//!   (page offsets cycle sequentially through the region).
//! - raw: copy into an aligned buffer, one unbuffered write, one data flush
//!   (offsets cycle sequentially through a ready 256 MiB file).
//! - fsys: journal `append` + `sync_through` in three configurations, and
//!   `Handle::write_at` + `Handle::sync` for the positioned-write case.

use std::time::Instant;

use super::{
    Ctx, KIB4, Rec, Snap, Step, fserr, ioerr, lat_cells, provision_append, record_run,
    region_bytes, remove, sample, sioerr, time_boxed, verify_appends, with_store,
};
use crate::aligned::AlignedBuf;
use crate::fsys_cmp::{self, Journal};
use crate::json::Json;
use crate::raw::RawFile;
use crate::report::{Better, Comparison, Point, Section, System, Table, Verify};
use crate::stats::Samples;

/// Payload stream of this workload.
const STREAM: u64 = 1;
/// Size of the page region and of the raw file.
const SPAN: u64 = 256 << 20;
/// Read-back checks per point.
const CHECKS: usize = 2048;

/// Runs W1.
pub fn run(ctx: &Ctx) -> Section {
    let mut sec = Section::new("lone", "W1: lone writer, QD1 durable 4 KiB");
    sec.method = vec![
        "One thread, one operation at a time; each operation is a 4 KiB write made durable before the next starts.".to_owned(),
        "store-io `append_durable` appends to a freshly provisioned append region; `write_durable` overwrites a 256 MiB page region at sequentially cycling 4 KiB offsets.".to_owned(),
        format!("raw: copy the payload into an aligned buffer (the copy store-io also makes), then {}, at sequentially cycling offsets of a ready 256 MiB file.", crate::raw::PRIMITIVE),
        "fsys: `JournalHandle::append` + `sync_through(lsn)` (default, window off, direct+window off) on a fresh journal file; `Handle::write_at` + `Handle::sync` on a ready 256 MiB file (fsys opens the file on every call; that is its API).".to_owned(),
        "Latency covers the write and the durability call only; payload generation happens before the timer starts.".to_owned(),
    ];
    let mut pts = vec![
        store_append(ctx),
        store_split(ctx),
        store_page(ctx),
        raw(ctx),
    ];
    for j in [
        Journal::Default,
        Journal::WindowOff,
        Journal::DirectWindowOff,
    ] {
        pts.push(fsys_journal(ctx, j));
    }
    pts.push(fsys_write_at(ctx));

    let mut t = Table::new(
        "Results:",
        &[
            "system",
            "variant",
            "durable ops/s",
            "p50 µs",
            "p90 µs",
            "p99 µs",
            "p99.9 µs",
            "max µs",
            "mean µs",
            "flushes/op",
            "write calls/op",
            "other calls/op",
        ],
    );
    for p in &pts {
        let mut row = vec![
            p.system.name().to_owned(),
            p.variant.clone(),
            p.cell("ops_per_s", 0),
        ];
        row.extend(lat_cells(p));
        row.push(p.cell("flushes_per_op", 2));
        row.push(p.cell("write_calls_per_op", 2));
        row.push(p.cell("other_calls_per_op", 2));
        t.row(row);
    }
    sec.tables.push(t);
    let mut split = Table::new(
        "Where the time goes (write call vs durability call, timed separately on the same thread):",
        &[
            "system",
            "variant",
            "write p50 µs",
            "write p99 µs",
            "write mean µs",
            "durability p50 µs",
            "durability p99 µs",
            "durability mean µs",
        ],
    );
    for p in pts.iter().filter(|p| p.get("write_p50_us").is_some()) {
        split.row(vec![
            p.system.name().to_owned(),
            p.variant.clone(),
            p.cell("write_p50_us", 1),
            p.cell("write_p99_us", 1),
            p.cell("write_mean_us", 1),
            p.cell("sync_p50_us", 1),
            p.cell("sync_p99_us", 1),
            p.cell("sync_mean_us", 1),
        ]);
    }
    sec.tables.push(split);

    let get = |sys: System, v: &str, k: &str| {
        pts.iter()
            .find(|p| p.system == sys && p.variant.contains(v))
            .and_then(|p| p.get(k))
    };
    let append = "append_durable";
    let page = "write_durable";
    let fsys_best = Journal::WindowOff.label();
    for (k, metric, better) in [
        ("ops_per_s", "durable ops/s", Better::Higher),
        ("p50_us", "p50 µs", Better::Lower),
        ("p99_us", "p99 µs", Better::Lower),
    ] {
        sec.comparisons.push(Comparison {
            case: "QD1 durable 4 KiB append".to_owned(),
            metric: metric.to_owned(),
            better,
            store_io: get(System::StoreIo, append, k),
            raw: get(System::Raw, "", k),
            fsys: get(System::Fsys, fsys_best, k),
            note: "append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config)".to_owned(),
        });
    }
    sec.comparisons.push(Comparison {
        case: "QD1 durable 4 KiB append, fsys default config".to_owned(),
        metric: "durable ops/s".to_owned(),
        better: Better::Higher,
        store_io: get(System::StoreIo, append, "ops_per_s"),
        raw: get(System::Raw, "", "ops_per_s"),
        fsys: get(System::Fsys, Journal::Default.label(), "ops_per_s"),
        note: "fsys JournalOptions::new() (500 µs group-commit window)".to_owned(),
    });
    for (k, metric, better) in [
        ("ops_per_s", "durable ops/s", Better::Higher),
        ("p50_us", "p50 µs", Better::Lower),
        ("p99_us", "p99 µs", Better::Lower),
    ] {
        sec.comparisons.push(Comparison {
            case: "QD1 durable 4 KiB page overwrite".to_owned(),
            metric: metric.to_owned(),
            better,
            store_io: get(System::StoreIo, page, k),
            raw: get(System::Raw, "", k),
            fsys: get(System::Fsys, "write_at", k),
            note: "write_durable vs raw write+flush vs fsys write_at+sync".to_owned(),
        });
    }
    sec.points = pts;
    sec.collect_point_anomalies();
    sec
}

fn store_append(ctx: &Ctx) -> Point {
    let p = Point::new(System::StoreIo, "AppendRegion::append_durable")
        .param("bytes", Json::Num(KIB4 as f64));
    with_store(ctx, "w1-append", p, |store, p| {
        let bytes = region_bytes(ctx, KIB4 as u64, 25_000.0, 2 << 30);
        let wal = provision_append(store, "w1", bytes, p)?;
        let mut buf = vec![0u8; KIB4];
        let mut recs = Vec::with_capacity(1 << 15);
        let mut short_receipts = 0u64;
        let run = time_boxed(ctx, &|| Snap::store(store), |i| {
            if wal.tail() + KIB4 as u64 > wal.len() {
                return Ok(Step::Full);
            }
            ctx.pool.fill(STREAM, i, &mut buf);
            let t = Instant::now();
            let (pos, rc) = wal
                .append_durable(&buf)
                .map_err(|e| sioerr("append_durable", &e))?;
            let d = t.elapsed();
            if rc.durable_through() < pos.offset() + KIB4 as u64 {
                short_receipts += 1;
            }
            recs.push(Rec {
                off: pos.offset(),
                stream: STREAM,
                index: i,
                len: KIB4,
                aligned: true,
            });
            Ok(Step::Op(d))
        })?;
        record_run(p, &run, "ops_per_s");
        p.verify = verify_appends(&wal, &ctx.pool, &recs, CHECKS);
        if short_receipts > 0 {
            p.verify = Verify::Failed(format!(
                "{short_receipts} receipts did not cover their own append"
            ));
        }
        Ok(())
    })
}

fn store_page(ctx: &Ctx) -> Point {
    let p = Point::new(System::StoreIo, "PageRegion::write_durable")
        .param("bytes", Json::Num(KIB4 as f64))
        .param("region_bytes", Json::Num(SPAN as f64));
    with_store(ctx, "w1-page", p, |store, p| {
        let pages = store
            .provision_page_region("w1p", SPAN)
            .map_err(|e| sioerr("provision_page_region", &e))?;
        let slots = SPAN / KIB4 as u64;
        let mut last = vec![u64::MAX; slots as usize];
        let mut buf = vec![0u8; KIB4];
        let run = time_boxed(ctx, &|| Snap::store(store), |i| {
            let slot = i % slots;
            ctx.pool.fill(STREAM, i, &mut buf);
            let pos = pages
                .pos(slot * KIB4 as u64)
                .map_err(|e| sioerr("PageRegion::pos", &e))?;
            let t = Instant::now();
            let _rc = pages
                .write_durable(pos, &buf)
                .map_err(|e| sioerr("write_durable", &e))?;
            let d = t.elapsed();
            last[slot as usize] = i;
            Ok(Step::Op(d))
        })?;
        record_run(p, &run, "ops_per_s");
        let written: Vec<(u64, u64)> = last
            .iter()
            .enumerate()
            .filter(|(_, i)| **i != u64::MAX)
            .map(|(s, i)| (s as u64, *i))
            .collect();
        let idx = sample(written.len(), CHECKS);
        let mut out = vec![0u8; KIB4];
        for &k in &idx {
            let (slot, i) = written[k];
            let pos = pages
                .pos(slot * KIB4 as u64)
                .map_err(|e| sioerr("PageRegion::pos", &e))?;
            let n = pages
                .read(pos, &mut out)
                .map_err(|e| sioerr("PageRegion::read", &e))?;
            if n != KIB4 || !ctx.pool.matches(STREAM, i, &out) {
                p.verify = Verify::Failed(format!(
                    "page {slot} (last write index {i}) reads back different bytes"
                ));
                return Ok(());
            }
        }
        p.verify = Verify::Passed(format!(
            "{} of {} written pages read back (latest write each) and compared byte for byte",
            idx.len(),
            written.len()
        ));
        Ok(())
    })
}

/// Reads back the latest write to a sample of slots of a raw file.
pub fn verify_raw_slots(
    ctx: &Ctx,
    f: &RawFile,
    last: &[u64],
    stream: u64,
    len: usize,
    limit: usize,
) -> Verify {
    let written: Vec<(u64, u64)> = last
        .iter()
        .enumerate()
        .filter(|(_, i)| **i != u64::MAX)
        .map(|(s, i)| (s as u64, *i))
        .collect();
    let idx = sample(written.len(), limit);
    let mut out = AlignedBuf::zeroed(len);
    for &k in &idx {
        let (slot, i) = written[k];
        match f.read_at(out.as_mut_slice(), slot * len as u64) {
            Ok(n) if n == len => {}
            Ok(n) => return Verify::Failed(format!("short raw read: {n} of {len}")),
            Err(e) => return Verify::Failed(ioerr("raw read", &e)),
        }
        if !ctx.pool.matches(stream, i, out.as_slice()) {
            return Verify::Failed(format!("raw slot {slot} reads back different bytes"));
        }
    }
    Verify::Passed(format!(
        "{} of {} written slots read back unbuffered and compared byte for byte",
        idx.len(),
        written.len()
    ))
}

fn raw(ctx: &Ctx) -> Point {
    let mut p = Point::new(System::Raw, "write + data flush")
        .param("bytes", Json::Num(KIB4 as f64))
        .param("file_bytes", Json::Num(SPAN as f64));
    let path = ctx.path("w1-raw.dat");
    let r = (|| -> Result<(), String> {
        let (f, prep) = RawFile::create_ready(&path, SPAN).map_err(|e| ioerr("raw create", &e))?;
        p.notes.push(format!(
            "file made ready (zero-filled unbuffered + flushed) in {:.2} s ({:.0} MB/s)",
            prep.as_secs_f64(),
            SPAN as f64 / prep.as_secs_f64() / 1e6
        ));
        let slots = SPAN / KIB4 as u64;
        let mut last = vec![u64::MAX; slots as usize];
        let mut src = vec![0u8; KIB4];
        let mut abuf = AlignedBuf::zeroed(KIB4);
        let (mut wl, mut sl) = (Samples::default(), Samples::default());
        let run = time_boxed(ctx, &Snap::os, |i| {
            let slot = i % slots;
            ctx.pool.fill(STREAM, i, &mut src);
            let t0 = Instant::now();
            abuf.as_mut_slice().copy_from_slice(&src);
            f.write_at(abuf.as_slice(), slot * KIB4 as u64)
                .map_err(|e| ioerr("raw write", &e))?;
            let t1 = Instant::now();
            f.flush_data().map_err(|e| ioerr("raw flush", &e))?;
            let t2 = Instant::now();
            wl.push(t1 - t0);
            sl.push(t2 - t1);
            last[slot as usize] = i;
            Ok(Step::Op(t2 - t0))
        })?;
        record_run(&mut p, &run, "ops_per_s");
        record_split(&mut p, &wl, &sl);
        p.verify = verify_raw_slots(ctx, &f, &last, STREAM, KIB4, CHECKS);
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

fn fsys_journal(ctx: &Ctx, j: Journal) -> Point {
    let mut p = Point::new(
        System::Fsys,
        format!("{}: append + sync_through", j.label()),
    )
    .param("bytes", Json::Num(KIB4 as f64));
    let path = ctx.path("w1-fsys.wal");
    let r = (|| -> Result<(), String> {
        let journal = ctx
            .fsys
            .journal_with(&path, j.options())
            .map_err(|e| fserr("journal_with", &e))?;
        p.notes.push(format!(
            "journal backend {:?}, direct active {}",
            journal.backend_kind(),
            journal.is_direct_active()
        ));
        let mut buf = vec![0u8; KIB4];
        let run = time_boxed(ctx, &Snap::os, |i| {
            ctx.pool.fill(STREAM, i, &mut buf);
            let t = Instant::now();
            let lsn = journal.append(&buf).map_err(|e| fserr("append", &e))?;
            journal
                .sync_through(lsn)
                .map_err(|e| fserr("sync_through", &e))?;
            Ok(Step::Op(t.elapsed()))
        })?;
        record_run(&mut p, &run, "ops_per_s");
        journal.close().map_err(|e| fserr("close", &e))?;
        p.verify = fsys_cmp::verify_journal(&ctx.pool, &path, run.total_ops);
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

fn fsys_write_at(ctx: &Ctx) -> Point {
    let mut p = Point::new(System::Fsys, "Handle::write_at + Handle::sync")
        .param("bytes", Json::Num(KIB4 as f64))
        .param("file_bytes", Json::Num(SPAN as f64));
    let path = ctx.path("w1-fsys-pages.dat");
    let r = (|| -> Result<(), String> {
        let (f, _) = RawFile::create_ready(&path, SPAN).map_err(|e| ioerr("prepare", &e))?;
        drop(f);
        let slots = SPAN / KIB4 as u64;
        let mut last = vec![u64::MAX; slots as usize];
        let mut buf = vec![0u8; KIB4];
        let run = time_boxed(ctx, &Snap::os, |i| {
            let slot = i % slots;
            ctx.pool.fill(STREAM, i, &mut buf);
            let t = Instant::now();
            ctx.fsys
                .write_at(&path, slot * KIB4 as u64, &buf)
                .map_err(|e| fserr("write_at", &e))?;
            ctx.fsys.sync(&path).map_err(|e| fserr("sync", &e))?;
            let d = t.elapsed();
            last[slot as usize] = i;
            Ok(Step::Op(d))
        })?;
        record_run(&mut p, &run, "ops_per_s");
        let f = RawFile::open(&path).map_err(|e| ioerr("reopen", &e))?;
        p.verify = verify_raw_slots(ctx, &f, &last, STREAM, KIB4, CHECKS);
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

/// Records the write and durability halves of an operation separately.
fn record_split(p: &mut Point, write: &Samples, sync: &Samples) {
    let (w, s) = (write.summary(), sync.summary());
    p.metric("write_p50_us", w.map(|x| x.p50));
    p.metric("write_p99_us", w.and_then(|x| x.p99));
    p.metric("write_mean_us", w.map(|x| x.mean));
    p.metric("sync_p50_us", s.map(|x| x.p50));
    p.metric("sync_p99_us", s.and_then(|x| x.p99));
    p.metric("sync_mean_us", s.map(|x| x.mean));
}

fn store_split(ctx: &Ctx) -> Point {
    let p = Point::new(
        System::StoreIo,
        "AppendRegion::append + sync_through (halves timed separately)",
    )
    .param("bytes", Json::Num(KIB4 as f64));
    with_store(ctx, "w1-split", p, |store, p| {
        let bytes = region_bytes(ctx, KIB4 as u64, 25_000.0, 2 << 30);
        let wal = provision_append(store, "w1s", bytes, p)?;
        let mut buf = vec![0u8; KIB4];
        let mut recs = Vec::with_capacity(1 << 15);
        let (mut wl, mut sl) = (Samples::default(), Samples::default());
        let run = time_boxed(ctx, &|| Snap::store(store), |i| {
            if wal.tail() + KIB4 as u64 > wal.len() {
                return Ok(Step::Full);
            }
            ctx.pool.fill(STREAM + 1, i, &mut buf);
            let t0 = Instant::now();
            let tk = wal.append(&buf).map_err(|e| sioerr("append", &e))?;
            let t1 = Instant::now();
            let _rc = wal
                .sync_through(&tk)
                .map_err(|e| sioerr("sync_through", &e))?;
            let t2 = Instant::now();
            wl.push(t1 - t0);
            sl.push(t2 - t1);
            recs.push(Rec {
                off: tk.pos().offset(),
                stream: STREAM + 1,
                index: i,
                len: KIB4,
                aligned: true,
            });
            Ok(Step::Op(t2 - t0))
        })?;
        record_run(p, &run, "ops_per_s");
        record_split(p, &wl, &sl);
        p.verify = verify_appends(&wal, &ctx.pool, &recs, CHECKS);
        Ok(())
    })
}
