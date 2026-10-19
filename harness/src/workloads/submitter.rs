//! W6: N operations in flight from one thread, random 4 KiB.
//!
//! For queue depths N in {1, 8, 32, 128}, random 4 KiB writes (plain) and
//! reads over a 256 MiB region / file, kept N deep from one thread:
//!
//! - store-io: the engine-layer `Submitter` on a provisioned page region
//!   (on Linux, the tier is the run's `--tier`: io_uring rings or the
//!   synchronous pool).
//! - raw: fio at the same depth, block size and span, with direct I/O
//!   (`io_uring` on Linux, `windowsaio` on Windows), on one file written in
//!   full first (as store-io's region is): the best the OS offers one
//!   thread, through a mature tool.
//!
//! Each point runs `REPS` times, the two systems alternating which goes
//! first; the median repetition is reported, with the spread.

use std::path::Path;
use std::time::{Duration, Instant};

use store_io_engine::completion::Done;
use store_io_engine::{SubmitterConfig, WriteMode};

use super::{Ctx, KIB4, fallbacks, remove, sioerr, with_store};
use crate::data::Rng;
use crate::fio;
use crate::json::Json;
use crate::report::{Better, Comparison, Point, Section, System, Table, Verify};
use crate::stats::Samples;

/// Queue depths.
pub const DEPTHS: [u32; 4] = [1, 8, 32, 128];
/// Repetitions per point.
const REPS: usize = 3;
/// Region / file size.
const SPAN: u64 = 256 << 20;
/// Pages in the span.
const PAGES: u64 = SPAN / KIB4 as u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rw {
    Write,
    Read,
}

impl Rw {
    fn name(self) -> &'static str {
        match self {
            Self::Write => "randwrite",
            Self::Read => "randread",
        }
    }
}

/// One repetition's numbers.
#[derive(Debug, Clone, Copy)]
struct Rep {
    ops: f64,
    p50: Option<f64>,
    p99: Option<f64>,
    p999: Option<f64>,
}

/// Runs W6.
pub fn run(ctx: &Ctx) -> Section {
    let mut sec = Section::new(
        "submitter",
        "W6: N in flight from one thread, random 4 KiB (Submitter vs fio)",
    );
    sec.method = vec![
        format!("Queue depth N in {DEPTHS:?}; uniformly random 4 KiB-aligned offsets over 256 MiB, with repeats (fio: --norandommap); one thread keeps N operations in flight, resubmitting as each completes; writes are plain (no per-write durability), reads read back and are checked for length."),
        "store-io: `Store::submitter(depth N)`, `write(region, pos, buf, Plain)` / `read_page`, `wait(1)`; buffers from the submitter's pool (`take_buf`); the region is written in full by provisioning.".to_owned(),
        format!("raw: fio --ioengine={} --direct=1 --bs=4k --iodepth=N --numjobs=1 --norandommap on one 256 MiB file written in full first (1 MiB direct writes, then fsync) and reused by every point; time-based, the same warm-up as ramp time.", fio_engine()),
        "Latency: per operation, submit call to completion observed (store-io); fio's total latency (lat: submission and completion).".to_owned(),
        format!("Each point runs {REPS} times, alternating which system goes first; the table shows the repetition with the median ops/s, and the notes give every repetition."),
    ];
    let fio_file = ctx.path("w6-fio");
    let laid = lay_out(&fio_file);
    let mut pts = Vec::new();
    for rw in [Rw::Write, Rw::Read] {
        for &qd in &DEPTHS {
            let mut store = Vec::new();
            let mut fio = Vec::new();
            for rep in 0..REPS {
                let order = if rep % 2 == 0 {
                    [true, false]
                } else {
                    [false, true]
                };
                for store_first in order {
                    if store_first {
                        store.push(store_point(ctx, rw, qd));
                    } else {
                        fio.push(match &laid {
                            Ok(()) => fio_point(ctx, &fio_file, rw, qd),
                            Err(e) => Err(e.clone()),
                        });
                    }
                }
            }
            pts.push(summarize(
                System::StoreIo,
                format!("Submitter {} QD{qd}", rw.name()),
                rw,
                qd,
                store,
            ));
            pts.push(summarize(
                System::Raw,
                format!("fio {} {} QD{qd}", fio_engine(), rw.name()),
                rw,
                qd,
                fio.into_iter().map(|r| r.map(|rep| (rep, None))).collect(),
            ));
        }
    }
    if let Some(e) = remove(&fio_file) {
        sec.anomalies.push(e);
    }
    let mut t = Table::new(
        "Results (median of the repetitions):",
        &[
            "pattern",
            "N",
            "system",
            "variant",
            "ops/s",
            "ops/s min..max",
            "p50 µs",
            "p99 µs",
            "p99.9 µs",
        ],
    );
    for p in &pts {
        t.row(vec![
            p.params
                .iter()
                .find_map(|(k, v)| match v {
                    Json::Str(s) if *k == "rw" => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_default(),
            p.cell("qd", 0),
            p.system.name().to_owned(),
            p.variant.clone(),
            p.cell("ops_per_s", 0),
            format!(
                "{}..{}",
                p.cell("ops_per_s_min", 0),
                p.cell("ops_per_s_max", 0)
            ),
            p.cell("p50_us", 1),
            p.cell("p99_us", 1),
            p.cell("p999_us", 1),
        ]);
    }
    sec.tables.push(t);
    for rw in [Rw::Write, Rw::Read] {
        for &qd in &DEPTHS {
            let get = |sys: System, k: &str| {
                pts.iter()
                    .find(|p| {
                        p.system == sys
                            && p.variant.contains(rw.name())
                            && p.get("qd") == Some(f64::from(qd))
                    })
                    .and_then(|p| p.get(k))
            };
            for (k, metric, better) in [
                ("ops_per_s", "ops/s", Better::Higher),
                ("p99_us", "p99 µs", Better::Lower),
            ] {
                sec.comparisons.push(Comparison {
                    case: format!("{} 4 KiB, {qd} in flight", rw.name()),
                    metric: metric.to_owned(),
                    better,
                    store_io: get(System::StoreIo, k),
                    raw: get(System::Raw, k),
                    fsys: None,
                    note: format!("Submitter vs fio {} (medians of {REPS})", fio_engine()),
                });
            }
        }
    }
    sec.points = pts;
    sec.collect_point_anomalies();
    sec
}

/// One point from its repetitions: the median repetition by ops/s, with
/// the spread and every repetition in the notes; any failed repetition
/// fails the point. A repetition may carry the store's io line.
fn summarize(
    system: System,
    variant: String,
    rw: Rw,
    qd: u32,
    reps: Vec<Result<(Rep, Option<String>), String>>,
) -> Point {
    let mut p = Point::new(system, variant)
        .param("qd", Json::Num(f64::from(qd)))
        .param("rw", Json::Str(rw.name().to_owned()));
    p.verify = Verify::NotChecked(if system == System::StoreIo {
        "throughput point; read lengths and tags are checked".to_owned()
    } else {
        "fio baseline: fio's own checks".to_owned()
    });
    let mut ok = Vec::new();
    for r in reps {
        match r {
            Ok((rep, io)) => {
                if let Some(io) = io {
                    if !p.notes.contains(&io) {
                        p.notes.push(io);
                    }
                }
                ok.push(rep);
            }
            Err(e) => {
                p.error.get_or_insert(e);
            }
        }
    }
    if ok.is_empty() {
        return p;
    }
    ok.sort_by(|a, b| a.ops.total_cmp(&b.ops));
    let med = ok[ok.len() / 2];
    p.metric("ops_per_s", Some(med.ops));
    p.metric("ops_per_s_min", ok.first().map(|r| r.ops));
    p.metric("ops_per_s_max", ok.last().map(|r| r.ops));
    p.metric("p50_us", med.p50);
    p.metric("p99_us", med.p99);
    p.metric("p999_us", med.p999);
    p.notes.push(format!(
        "repetitions (ops/s, p99 µs): {}",
        ok.iter()
            .map(|r| format!(
                "{:.0}/{}",
                r.ops,
                r.p99.map_or("-".to_owned(), |v| format!("{v:.0}"))
            ))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    p
}

fn store_point(ctx: &Ctx, rw: Rw, qd: u32) -> Result<(Rep, Option<String>), String> {
    let mut out = Err("not run".to_owned());
    let p = with_store(ctx, "w6", Point::new(System::StoreIo, ""), |store, _| {
        let pages = store
            .provision_page_region("w6", SPAN)
            .map_err(|e| sioerr("provision_page_region", &e))?;
        let mut sub = store
            .submitter(SubmitterConfig::default().with_depth(qd))
            .map_err(|e| sioerr("submitter", &e))?;
        let mut rng = Rng::new(6_000_000 + u64::from(qd));
        let fallbacks_before = fallbacks();
        let mut submitted = 0u64;
        let n = qd as usize;
        let mut started = vec![Instant::now(); n];
        let mut free: Vec<usize> = (0..n).collect();
        // Which slots have an operation in flight (a second completion for
        // one fails the point).
        let mut busy = vec![false; n];
        let begin = Instant::now();
        let measure_from = begin + Duration::from_secs_f64(ctx.warmup);
        let stop_at = measure_from + Duration::from_secs_f64(ctx.secs);
        // Room for every sample of a fast point, so the vector never grows
        // inside the measured window.
        let mut lat = Samples::with_capacity((ctx.secs * 600_000.0) as usize + 1024);
        let mut last_done = measure_from;
        loop {
            if Instant::now() < stop_at {
                while let Some(slot) = free.pop() {
                    let off = rng.below(PAGES) * KIB4 as u64;
                    let pos = pages.pos(off).map_err(|e| sioerr("pos", &e))?;
                    let mut buf = sub.take_buf(KIB4).map_err(|e| sioerr("take_buf", &e))?;
                    started[slot] = Instant::now();
                    busy[slot] = true;
                    let r = match rw {
                        Rw::Write => {
                            buf.as_mut_slice().fill(slot as u8);
                            sub.write(&pages, pos, buf, WriteMode::Plain, slot as u64)
                        }
                        Rw::Read => sub.read_page(&pages, pos, buf, slot as u64),
                    };
                    r.map_err(|r| sioerr("submit", &r.error))?;
                    submitted += 1;
                }
            }
            if free.len() == n {
                break;
            }
            let mut got = 0;
            let done = sub.wait(1).map_err(|e| sioerr("wait", &e))?;
            let at = Instant::now();
            for c in done {
                got += 1;
                let slot = usize::try_from(c.tag).unwrap_or(usize::MAX);
                match c.outcome.map_err(|e| sioerr("completion", &e))? {
                    Done::Read { len } if len != KIB4 => {
                        return Err(format!("a 4 KiB read returned {len} bytes"));
                    }
                    Done::Read { .. } | Done::Written(_) => {}
                    other => return Err(format!("unexpected completion {other:?}")),
                }
                let t0 = *started
                    .get(slot)
                    .ok_or("a completion with an unknown tag")?;
                if !std::mem::replace(&mut busy[slot], false) {
                    return Err(format!("a second completion for tag {slot}"));
                }
                if t0 >= measure_from && t0 < stop_at {
                    lat.push(at - t0);
                    last_done = at;
                }
                free.push(slot);
            }
            if got == 0 && sub.in_flight() == 0 {
                return Err(format!(
                    "{} operations outstanding, none in flight: a completion was lost",
                    n - free.len()
                ));
            }
        }
        let window = last_done
            .saturating_duration_since(measure_from)
            .as_secs_f64();
        let s = lat.summary();
        let ops = s.map_or(0, |s| s.n);
        if window <= 0.0 || ops == 0 {
            return Err("no measured operations".to_owned());
        }
        let io = &store.report().io;
        let fell = match (fallbacks_before, fallbacks()) {
            (Some(b), Some(a)) if io.interface == store_io_platform::IoInterface::IoUring => {
                format!(
                    "; io_uring fallbacks {} of {submitted} operations",
                    a.saturating_sub(b)
                )
            }
            _ => String::new(),
        };
        out = Ok((
            Rep {
                ops: ops as f64 / window,
                p50: s.map(|s| s.p50),
                p99: s.and_then(|s| s.p99),
                p999: s.and_then(|s| s.p999),
            },
            Some(format!("io: {} ({}){fell}", io.interface, io.reason)),
        ));
        Ok(())
    });
    match p.error {
        Some(e) => Err(e),
        None => out,
    }
}

/// fio's asynchronous engine on this system.
fn fio_engine() -> &'static str {
    fio::engine()
}

/// Writes fio's file in full, so random writes overwrite allocated, written
/// blocks, as store-io's do.
fn lay_out(file: &Path) -> Result<(), String> {
    fio::lay_out(file, SPAN)
}

fn fio_point(ctx: &Ctx, file: &Path, rw: Rw, qd: u32) -> Result<Rep, String> {
    let rwarg = format!("--rw={}", rw.name());
    let depth = format!("--iodepth={qd}");
    let engine = format!("--ioengine={}", fio::engine());
    let side = match rw {
        Rw::Write => "write",
        Rw::Read => "read",
    };
    let st = fio::job(
        file,
        SPAN,
        &[&engine, &rwarg, "--bs=4k", &depth, "--norandommap"],
        side,
        ctx.secs,
        ctx.warmup,
    )?;
    Ok(Rep {
        ops: st.iops,
        p50: Some(st.p50_us),
        p99: Some(st.p99_us),
        p999: Some(st.p999_us),
    })
}
