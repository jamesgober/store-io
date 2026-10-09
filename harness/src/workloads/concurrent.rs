//! W2: concurrent durable writers (1 to 64 threads, 4 KiB each).
//!
//! - store-io: every thread calls `append_durable` on one shared append
//!   region; the flush domain shares device flushes between concurrent
//!   barriers (no timer). `domain_stats()` deltas give flushes issued, and
//!   barriers that led or joined a flush.
//! - raw: every thread writes its own range of one ready file and issues its
//!   own data flush (no sharing: what an application gets from the OS).
//! - fsys: every thread calls `append` + `sync_through` on one shared
//!   journal, window off (fsys's best QD1 configuration) and default (its
//!   500 µs group-commit window, designed for exactly this case).

use std::sync::Arc;
use std::time::Instant;

use super::{
    Ctx, KIB4, Rec, Snap, Step, Threaded, fserr, ioerr, provision_append, record_delta,
    region_bytes, remove, sample, sioerr, threaded, verify_appends, with_store,
};
use crate::aligned::AlignedBuf;
use crate::fsys_cmp::{self, Journal};
use crate::json::Json;
use crate::raw::RawFile;
use crate::report::{Better, Comparison, Point, Section, System, Table, Verify};
use crate::stats::fmt;

/// Writer counts measured.
pub const THREADS: [usize; 7] = [1, 2, 4, 8, 16, 32, 64];
/// Payload streams are `STREAM_BASE + thread`.
const STREAM_BASE: u64 = 100;
/// Size of the raw file.
const SPAN: u64 = 256 << 20;
/// Read-back checks per point.
const CHECKS: usize = 1024;

/// Runs W2.
pub fn run(ctx: &Ctx) -> Section {
    let mut sec = Section::new("concurrent", "W2: concurrent durable writers, 4 KiB");
    sec.method = vec![
        format!("Writer counts {THREADS:?}; every thread loops a durable 4 KiB write at queue depth 1 for the whole point."),
        "store-io: all threads call `AppendRegion::append_durable` on one append region of one store (a fresh store per point). Flush sharing is read from `Store::domain_stats()` deltas over the window: `flushes` issued, barriers that `led` a flush, barriers that `joined` one.".to_owned(),
        format!("raw: each thread writes its own contiguous range of one ready 256 MiB file and makes it durable itself with {} (no sharing).", crate::raw::PRIMITIVE),
        "fsys: all threads call `append` + `sync_through` on one shared journal (fresh per point), window off and default (500 µs group-commit window).".to_owned(),
        "An operation counts when it started and finished inside the measured window; rate = counted operations / window.".to_owned(),
    ];
    let mut pts = Vec::new();
    let raw_path = ctx.path("w2-raw.dat");
    let raw_file = RawFile::create_ready(&raw_path, SPAN).map(|(f, _)| Arc::new(f));
    for &n in &THREADS {
        pts.push(store_point(ctx, n));
        pts.push(match &raw_file {
            Ok(f) => raw_point(ctx, f, n),
            Err(e) => {
                let mut p = Point::new(System::Raw, "own write + own data flush")
                    .param("threads", Json::Num(n as f64));
                p.error = Some(ioerr("raw create", e));
                p
            }
        });
        pts.push(fsys_point(ctx, n, Journal::WindowOff));
        pts.push(fsys_point(ctx, n, Journal::Default));
    }
    drop(raw_file);
    if let Some(e) = remove(&raw_path) {
        sec.anomalies.push(e);
    }

    let mut t = Table::new(
        "Results:",
        &[
            "threads",
            "system",
            "variant",
            "durable ops/s",
            "p50 µs",
            "p99 µs",
            "p99.9 µs",
            "flushes issued",
            "barriers led",
            "barriers joined",
            "writes per flush",
        ],
    );
    for p in &pts {
        t.row(vec![
            p.cell("threads", 0),
            p.system.name().to_owned(),
            p.variant.clone(),
            p.cell("ops_per_s", 0),
            p.cell("p50_us", 1),
            p.cell("p99_us", 1),
            p.cell("p999_us", 1),
            p.cell("flushes", 0),
            p.cell("led", 0),
            p.cell("joined", 0),
            p.cell("writes_per_flush", 2),
        ]);
    }
    sec.tables.push(t);

    for &n in &THREADS {
        let get = |sys: System, v: &str, k: &str| {
            pts.iter()
                .find(|p| {
                    p.system == sys && p.variant.contains(v) && p.get("threads") == Some(n as f64)
                })
                .and_then(|p| p.get(k))
        };
        for (k, metric, better) in [
            ("ops_per_s", "durable ops/s", Better::Higher),
            ("p99_us", "p99 µs", Better::Lower),
        ] {
            let s = get(System::StoreIo, "", k);
            let off = get(System::Fsys, Journal::WindowOff.label(), k);
            let def = get(System::Fsys, Journal::Default.label(), k);
            // Compare with fsys's better configuration at this writer count.
            let (fsys, which) = match (off, def) {
                (Some(a), Some(b)) => {
                    let first_better = if better == Better::Higher {
                        a >= b
                    } else {
                        a <= b
                    };
                    if first_better {
                        (Some(a), "window off")
                    } else {
                        (Some(b), "default window")
                    }
                }
                (Some(a), None) => (Some(a), "window off"),
                (None, Some(b)) => (Some(b), "default window"),
                (None, None) => (None, "n/a"),
            };
            sec.comparisons.push(Comparison {
                case: format!("{n} concurrent durable 4 KiB writers"),
                metric: metric.to_owned(),
                better,
                store_io: s,
                raw: get(System::Raw, "", k),
                fsys,
                note: format!("store-io shared flushes vs raw own flush per writer vs fsys shared journal ({which}, the better fsys config here)"),
            });
        }
    }
    let store_pts: Vec<&Point> = pts.iter().filter(|p| p.system == System::StoreIo).collect();
    if let (Some(one), Some(top)) = (store_pts.first(), store_pts.last()) {
        sec.findings.push(format!(
            "store-io scaling: {} durable ops/s at 1 writer, {} at {} writers ({} writes per flush) [derived from the table].",
            one.cell("ops_per_s", 0),
            top.cell("ops_per_s", 0),
            top.cell("threads", 0),
            top.cell("writes_per_flush", 2),
        ));
    }
    sec.points = pts;
    sec.collect_point_anomalies();
    sec
}

/// Records a threaded run on `p`.
fn record(p: &mut Point, r: &Threaded<impl Sized>) {
    p.metric("ops_per_s", r.rate());
    p.latency(r.lat.summary().as_ref());
    record_delta(p, &r.delta, r.ops);
    p.metric("flushes", r.delta.flushes.map(|x| x as f64));
    p.metric("led", r.delta.led.map(|x| x as f64));
    p.metric("joined", r.delta.joined.map(|x| x as f64));
    p.metric(
        "writes_per_flush",
        r.delta
            .flushes
            .and_then(|f| (f > 0).then(|| r.ops as f64 / f as f64)),
    );
    p.metric("window_s", Some(r.window.as_secs_f64()));
    if r.capacity_stop {
        p.notes.push(format!(
            "window cut at region capacity: {} ops in {:.2} s",
            r.ops,
            r.window.as_secs_f64()
        ));
    }
    if let Some(e) = r.errors.first() {
        p.error = Some(format!("{} thread(s) failed; first: {e}", r.errors.len()));
    }
    if let Some(d) = r.delta.io {
        let calls = d.write_calls as f64;
        if r.ops > 0 {
            p.notes.push(format!(
                "OS write calls in window / counted ops = {} (straddling ops make this slightly above 1)",
                fmt(Some(calls / r.ops as f64), 3)
            ));
        }
    }
}

struct StoreThread {
    buf: Vec<u8>,
    recs: Vec<Rec>,
    short_receipts: u64,
}

fn store_point(ctx: &Ctx, n: usize) -> Point {
    let p = Point::new(
        System::StoreIo,
        "AppendRegion::append_durable (shared region)",
    )
    .param("threads", Json::Num(n as f64))
    .param("bytes", Json::Num(KIB4 as f64));
    let mut p = with_store(ctx, &format!("w2-store-{n}"), p, |store, p| {
        let bytes = region_bytes(ctx, KIB4 as u64, 40_000.0, 2 << 30);
        let wal = provision_append(store, "w2", bytes, p)?;
        let margin = (n as u64 + 1) * KIB4 as u64;
        let r = threaded(
            ctx,
            n,
            &|| Snap::store(store),
            |_| StoreThread {
                buf: vec![0u8; KIB4],
                recs: Vec::with_capacity(1 << 12),
                short_receipts: 0,
            },
            |st, t, i| {
                if wal.tail() + margin > wal.len() {
                    return Ok(Step::Full);
                }
                let stream = STREAM_BASE + t as u64;
                ctx.pool.fill(stream, i, &mut st.buf);
                let start = Instant::now();
                let (pos, rc) = wal
                    .append_durable(&st.buf)
                    .map_err(|e| sioerr("append_durable", &e))?;
                let d = start.elapsed();
                if rc.durable_through() < pos.offset() + KIB4 as u64 {
                    st.short_receipts += 1;
                }
                st.recs.push(Rec {
                    off: pos.offset(),
                    stream,
                    index: i,
                    len: KIB4,
                    aligned: true,
                });
                Ok(Step::Op(d))
            },
        );
        record(p, &r);
        let short: u64 = r.states.iter().map(|s| s.short_receipts).sum();
        let recs: Vec<Rec> = r.states.into_iter().flat_map(|s| s.recs).collect();
        p.verify = verify_appends(&wal, &ctx.pool, &recs, CHECKS);
        if short > 0 {
            p.verify = Verify::Failed(format!("{short} receipts did not cover their own append"));
        }
        let ds = store.domain_stats();
        if ds.led + ds.joined < r.total_ops {
            p.notes.push(format!(
                "domain counters: {} barriers for {} durable appends",
                ds.led + ds.joined,
                r.total_ops
            ));
        }
        Ok(())
    });
    if p.error.is_none() && p.get("ops_per_s").is_none() {
        p.error = Some("no operations completed inside the window".to_owned());
    }
    p
}

struct RawThread {
    src: Vec<u8>,
    buf: AlignedBuf,
    last: Vec<u64>,
}

fn raw_point(ctx: &Ctx, f: &Arc<RawFile>, n: usize) -> Point {
    let mut p = Point::new(System::Raw, "own write + own data flush (per thread range)")
        .param("threads", Json::Num(n as f64))
        .param("bytes", Json::Num(KIB4 as f64));
    let per = SPAN / KIB4 as u64 / n as u64;
    let r = threaded(
        ctx,
        n,
        &Snap::os,
        |_| RawThread {
            src: vec![0u8; KIB4],
            buf: AlignedBuf::zeroed(KIB4),
            last: vec![u64::MAX; per as usize],
        },
        |st, t, i| {
            let slot = i % per;
            let stream = STREAM_BASE + t as u64;
            ctx.pool.fill(stream, i, &mut st.src);
            let start = Instant::now();
            st.buf.as_mut_slice().copy_from_slice(&st.src);
            f.write_at(st.buf.as_slice(), (t as u64 * per + slot) * KIB4 as u64)
                .map_err(|e| ioerr("raw write", &e))?;
            f.flush_data().map_err(|e| ioerr("raw flush", &e))?;
            let d = start.elapsed();
            st.last[slot as usize] = i;
            Ok(Step::Op(d))
        },
    );
    record(&mut p, &r);
    // Read back a sample of each thread's latest writes.
    let mut checked = 0usize;
    let mut out = AlignedBuf::zeroed(KIB4);
    for (t, st) in r.states.iter().enumerate() {
        let written: Vec<(usize, u64)> = st
            .last
            .iter()
            .enumerate()
            .filter(|(_, i)| **i != u64::MAX)
            .map(|(s, i)| (s, *i))
            .collect();
        for k in sample(written.len(), (CHECKS / n).max(4)) {
            let (slot, i) = written[k];
            let off = (t as u64 * per + slot as u64) * KIB4 as u64;
            let ok = matches!(f.read_at(out.as_mut_slice(), off), Ok(x) if x == KIB4)
                && ctx.pool.matches(STREAM_BASE + t as u64, i, out.as_slice());
            if !ok {
                p.verify =
                    Verify::Failed(format!("thread {t} slot {slot} reads back different bytes"));
                return p;
            }
            checked += 1;
        }
    }
    p.verify = Verify::Passed(format!(
        "{checked} latest writes (sampled across all threads) read back unbuffered and compared"
    ));
    p
}

fn fsys_point(ctx: &Ctx, n: usize, j: Journal) -> Point {
    let mut p = Point::new(
        System::Fsys,
        format!("{}: shared journal append + sync_through", j.label()),
    )
    .param("threads", Json::Num(n as f64))
    .param("bytes", Json::Num(KIB4 as f64));
    let path = ctx.path("w2-fsys.wal");
    let r = (|| -> Result<(), String> {
        let journal = ctx
            .fsys
            .journal_with(&path, j.options())
            .map_err(|e| fserr("journal_with", &e))?;
        let r = threaded(
            ctx,
            n,
            &Snap::os,
            |_| vec![0u8; KIB4],
            |buf, t, i| {
                ctx.pool.fill(STREAM_BASE + t as u64, i, buf);
                let start = Instant::now();
                let lsn = journal.append(buf).map_err(|e| fserr("append", &e))?;
                journal
                    .sync_through(lsn)
                    .map_err(|e| fserr("sync_through", &e))?;
                Ok(Step::Op(start.elapsed()))
            },
        );
        record(&mut p, &r);
        journal.close().map_err(|e| fserr("close", &e))?;
        p.verify = fsys_cmp::verify_journal(&ctx.pool, &path, r.total_ops);
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
