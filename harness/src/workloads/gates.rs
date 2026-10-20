//! `gates`: the ROADMAP's performance gate table for a flush-required device
//! (D-066: Linux gates are relative, against the raw primitive and fio on
//! the same box, kernel, file system and flags, and labelled Unverified).
//!
//! It runs the workloads the gates need (W1 twice, so the raw baseline's own
//! run-to-run spread is known, W2 to W5), fio controls, and the measurements
//! no workload makes (`gates_extra`), then judges each row:
//!
//! - PASS: within the gate.
//! - NOT RESOLVABLE: outside the gate by less than the baseline's own spread
//!   on this machine; neither a pass nor a failure can be claimed.
//! - FAIL: outside the gate by more than that.
//! - REPORTED: a row the table asks to report, not to gate.
//! - ELSEWHERE: measured by another check (cited).

use super::{
    Ctx, caller_batch, concurrent, gates_extra, gates_g1, gates_g2, gates_g15, gates_scan,
    gates_seq, lone, page_batch, remove, sequential,
};
use crate::fio;
use crate::json::Json;
use crate::report::{Better, Comparison, Point, Section, System, Table};

/// The fio controls' file size.
const SPAN: u64 = 256 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Pass,
    NotResolvable,
    Fail,
    Reported,
    Elsewhere,
}

impl Verdict {
    fn name(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::NotResolvable => "NOT RESOLVABLE",
            Self::Fail => "FAIL",
            Self::Reported => "REPORTED",
            Self::Elsewhere => "ELSEWHERE",
        }
    }
}

struct Row {
    id: &'static str,
    gate: &'static str,
    measured: String,
    verdict: Verdict,
}

/// A latency gate's margin over `base`: on Linux `us` microseconds, on
/// Windows and macOS 3% (dev/ROADMAP.md, flush-required devices).
fn margin(us: f64, base: Option<f64>) -> f64 {
    if cfg!(target_os = "linux") {
        us
    } else {
        base.map_or(0.0, |b| 0.03 * b)
    }
}

/// `store` against `min x base` (higher is better); `spread` is the
/// baseline's relative run-to-run spread.
fn at_least(store: Option<f64>, base: Option<f64>, min: f64, spread: f64) -> Verdict {
    // A baseline that swings by more than half is no baseline: the band
    // stops there, so a result below half the gate always fails.
    let spread = if spread.is_finite() {
        spread.min(0.5)
    } else {
        0.0
    };
    // A baseline that is not a positive, finite number judges nothing.
    let base = base.filter(|b| b.is_finite() && *b > 0.0);
    match (store, base) {
        (Some(s), Some(b)) if s >= min * b => Verdict::Pass,
        (Some(s), Some(b)) if s >= min * b * (1.0 - spread) => Verdict::NotResolvable,
        _ => Verdict::Fail,
    }
}

fn find<'a>(
    sec: &'a Section,
    sys: System,
    variant: &str,
    param: Option<(&str, f64)>,
) -> Option<&'a Point> {
    sec.points
        .iter()
        .find(|p| {
            p.system == sys
                && p.variant.contains(variant)
                && param.is_none_or(|(k, v)| {
                    p.params.iter().any(|(pk, pv)| {
                        *pk == k && matches!(pv, Json::Num(x) if (*x - v).abs() < 1e-9)
                    })
                })
        })
        // The first match only: never a later point that happens to match.
        .filter(|p| p.usable())
}

fn get(p: Option<&Point>, k: &str) -> Option<f64> {
    p.and_then(|p| val(p, k))
}

/// A gate-only point's metric, only if the point completed and its data
/// checked out (a failed integrity check never feeds a verdict).
fn val(p: &Point, k: &str) -> Option<f64> {
    if p.usable() { p.get(k) } else { None }
}

/// The worse of two PASS / NOT RESOLVABLE / FAIL verdicts.
fn worse(a: Verdict, b: Verdict) -> Verdict {
    let rank = |v: Verdict| match v {
        Verdict::Pass => 0,
        Verdict::NotResolvable => 1,
        _ => 2,
    };
    if rank(b) > rank(a) { b } else { a }
}

/// The mean of two runs; `None` unless both ran (a verdict never rests on
/// one run when the method asks for two).
fn mean(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    a.zip(b).map(|(a, b)| (a + b) / 2.0)
}

fn spread(a: Option<f64>, b: Option<f64>) -> f64 {
    a.zip(b).map_or(0.0, |(a, b)| (a - b).abs())
}

/// Counts comparisons at least 20% better than fsys, at the device floor
/// (within 10% of the raw primitive), and neither (described).
fn against_fsys<'a>(cs: impl Iterator<Item = &'a Comparison>) -> (usize, usize, Vec<String>) {
    let (mut better, mut floor, mut worse) = (0, 0, Vec::new());
    for c in cs {
        let Some(st) = c.store_io else {
            worse.push(format!("{} {}: no store-io value", c.case, c.metric));
            continue;
        };
        let Some(fs) = c.fsys else {
            worse.push(format!("{} {}: no fsys value", c.case, c.metric));
            continue;
        };
        let higher = matches!(c.better, Better::Higher);
        let gain = if higher { st / fs } else { fs / st };
        let at_floor = c
            .raw
            .is_some_and(|r| if higher { st >= 0.9 * r } else { st <= r * 1.1 });
        if gain >= 1.2 {
            better += 1;
        } else if at_floor {
            floor += 1;
        } else {
            worse.push(format!("{} {} ({gain:.2}x)", c.case, c.metric));
        }
    }
    (better, floor, worse)
}

/// The median of a set, `None` when empty.
fn med(v: &[f64]) -> Option<f64> {
    let mut v = v.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    match n {
        0 => None,
        _ if n % 2 == 1 => Some(v[n / 2]),
        _ => Some((v[n / 2 - 1] + v[n / 2]) / 2.0),
    }
}

/// The comparisons of several runs of one workload, each value the median
/// over the runs (`None` unless every run has it).
fn median_comparisons(runs: &[&Section]) -> Vec<Comparison> {
    let Some(first) = runs.first() else {
        return Vec::new();
    };
    first
        .comparisons
        .iter()
        .map(|c| {
            let of = |pick: fn(&Comparison) -> Option<f64>| {
                let v: Vec<f64> = runs
                    .iter()
                    .filter_map(|s| {
                        s.comparisons
                            .iter()
                            .find(|d| d.case == c.case && d.metric == c.metric)
                            .and_then(pick)
                    })
                    .collect();
                if v.len() == runs.len() { med(&v) } else { None }
            };
            Comparison {
                store_io: of(|d| d.store_io),
                raw: of(|d| d.raw),
                fsys: of(|d| d.fsys),
                ..c.clone()
            }
        })
        .collect()
}

/// Runs again the workload a G13 row judges.
fn rerun(ctx: &Ctx, id: &str) -> Section {
    match id {
        "G13a" => page_batch::run(ctx),
        "G13b" => caller_batch::run(ctx),
        _ => concurrent::run(ctx),
    }
}

fn f(v: Option<f64>, d: usize) -> String {
    v.map_or_else(|| "n/a".to_owned(), |v| format!("{v:.d$}"))
}

/// Runs the gates.
pub fn run(ctx: &Ctx) -> Section {
    let mut sec = Section::new(
        "gates",
        "Performance gates (flush-required device; relative, Unverified per D-066)",
    );
    sec.method = vec![
        "Workloads: W1 (lone) twice, W2 (concurrent), W3 (caller batch), W4 (page batch), W5 (sequential); fio controls on one 256 MiB file written in full first; the gate-only measurements in `gates_extra` (submitter loops, allocations, QoS), and `gates_g1` and `gates_g2` (G1 and G2, alternated rounds).".to_owned(),
        "Verdicts: PASS within the gate; NOT RESOLVABLE when outside it by less than the baseline's own run-to-run spread on this machine (W1 run twice for the raw primitive; each fio control a gate uses run twice); FAIL beyond that, when a baseline did not run, or when the measurement failed or its data check did not pass; REPORTED where the table asks for a report; ELSEWHERE where another check measures it.".to_owned(),
        "G1 and G2 are measured in 20 alternated rounds (`gates_g1`, `gates_g2`, G2 at p50 and p99): PASS when the worse of the median per-round difference and the pooled difference is within the gate; NOT RESOLVABLE when it is not but the median's interval (6th smallest to 6th largest difference, ~96%) reaches the gate; FAIL when the interval is over the gate.".to_owned(),
        "The latency gates (+2 µs, +5 µs) are far below the spread of a virtual disk's flush (milliseconds): on such a disk they are expected to be NOT RESOLVABLE; certified results need bare metal (dev/TODO.md).".to_owned(),
    ];
    let l1 = lone::run(ctx);
    let l2 = lone::run(ctx);
    let w2 = concurrent::run(ctx);
    let w3 = caller_batch::run(ctx);
    let w4 = page_batch::run(ctx);
    let w5 = sequential::run(ctx);
    // fio controls.
    let file = ctx.path("gates-fio");
    let laid = fio::lay_out(&file, SPAN);
    let job = |args: &[&str], side: &str| match &laid {
        Ok(()) => fio::job(&file, SPAN, args, side, ctx.secs, ctx.warmup),
        Err(e) => Err(e.clone()),
    };
    // fio's asynchronous engine here (`fio::engine`).
    let aio = format!("--ioengine={}", fio::engine());
    let aio = aio.as_str();
    // fio's synchronous engine here (`fio::sync_engine`).
    let sync = format!("--ioengine={}", fio::sync_engine());
    let sync = sync.as_str();
    let durable = [sync, "--rw=write", "--bs=4k", "--fdatasync=1"];
    let fio_d1 = job(&durable, "write");
    let fio_d2 = job(&durable, "write");
    let seq_write = [sync, "--rw=write", "--bs=1m", "--end_fsync=1"];
    let fio_sw = job(&seq_write, "write");
    let fio_sw2 = job(&seq_write, "write");
    let fio_sr1 = job(&[sync, "--rw=read", "--bs=1m"], "read");
    let cpu_args = [
        aio,
        "--rw=randread",
        "--bs=4k",
        "--iodepth=128",
        "--norandommap",
    ];
    let fio_dq: Vec<_> = [1u32, 4, 16]
        .iter()
        .map(|qd| {
            let d = format!("--iodepth={qd}");
            job(
                &[
                    aio,
                    "--rw=randwrite",
                    "--bs=4k",
                    "--fdatasync=1",
                    "--norandommap",
                    &d,
                ],
                "write",
            )
        })
        .collect();
    // G7: the scan alternates with fio's QD8 sequential read on this file.
    let mut q8 =
        || job(&[aio, "--rw=read", "--bs=1m", "--iodepth=8"], "read").map(|s| s.bytes_per_s / 1e6);
    let g7 = gates_scan::scan(ctx, &mut q8);
    // G5: the submitter's QD128 reads alternate with fio's, three times each
    // (CPU accounting on a virtual machine swings between runs). Both sides
    // count their whole process's CPU time (`fio::cpu_job`).
    let (mut g5, mut f5) = (Vec::new(), Vec::new());
    for _ in 0..3 {
        f5.push(match &laid {
            Ok(()) => fio::cpu_job(&file, SPAN, &cpu_args, "read", ctx.secs),
            Err(e) => Err(e.clone()),
        });
        g5.push(gates_extra::read_qd128(ctx));
    }
    // G6: the Submitter's sequential writer alternates with fio's QD8
    // sequential write on this file, three times each.
    let (mut g6, mut f6) = (Vec::new(), Vec::new());
    for _ in 0..3 {
        f6.push(job(
            &[aio, "--rw=write", "--bs=1m", "--iodepth=8", "--end_fsync=1"],
            "write",
        ));
        g6.push(gates_seq::seq_write(ctx));
    }
    let fio_runs = [
        ("fio durable (1)", &fio_d1),
        ("fio durable (2)", &fio_d2),
        ("fio sequential write (1)", &fio_sw),
        ("fio sequential write (2)", &fio_sw2),
        ("fio sequential read QD1", &fio_sr1),
        ("fio durable QD1", &fio_dq[0]),
        ("fio durable QD4", &fio_dq[1]),
        ("fio durable QD16", &fio_dq[2]),
    ];
    for (i, r) in f5.iter().enumerate() {
        if let Err(e) = r {
            sec.anomalies
                .push(format!("fio CPU efficiency ({}): {e}", i + 1));
        }
    }
    for (i, r) in f6.iter().enumerate() {
        if let Err(e) = r {
            sec.anomalies
                .push(format!("fio sequential write QD8 ({}): {e}", i + 1));
        }
    }
    for (what, r) in fio_runs {
        if let Err(e) = r {
            sec.anomalies.push(format!("{what}: {e}"));
        }
    }
    let ok = |r: &Result<fio::Stats, String>| r.as_ref().ok().copied();
    // Gate-only measurements.
    let g4: Vec<Point> = [1u32, 4, 16]
        .iter()
        .map(|qd| gates_extra::durable_qd(ctx, *qd))
        .collect();
    let g8w = gates_extra::write_qd32(ctx);
    let g8b = gates_extra::blocking_allocs(ctx);
    let g10 = gates_extra::qos(ctx);
    let (g1, g1_rounds) = gates_g1::alternated(ctx);
    let g2 = gates_g2::alternated(ctx);
    // Registration is io_uring's (Linux).
    let g3d = if cfg!(target_os = "linux") {
        gates_extra::registration(ctx)
    } else {
        Vec::new()
    };

    let mut rows = Vec::new();
    // G1: QD1 4 KiB durable write p50, against the raw primitive + 2 us:
    // alternated rounds (`gates_g1`); the worse of the median per-round
    // difference and the pooled one against the gate, the median's interval
    // deciding between NOT RESOLVABLE and FAIL.
    fn page(s: &Section) -> Option<&Point> {
        find(s, System::StoreIo, "PageRegion::write_durable", None)
    }
    fn raw(s: &Section) -> Option<&Point> {
        find(s, System::Raw, "write + data flush", None)
    }
    let g1r = val(&g1, "raw_p50_us");
    // Finite or nothing: a NaN never stands in for a measurement.
    let fin = |k| val(&g1, k).filter(|v: &f64| v.is_finite());
    let (g1m, g1lo, g1hi, g1p) = (
        fin("diff_p50_us"),
        fin("diff_lo_us"),
        fin("diff_hi_us"),
        fin("pooled_diff_us"),
    );
    let g1margin = margin(2.0, g1r);
    // The worse of the median per-round difference and the pooled one.
    let g1d = g1m.zip(g1p).map(|(m, p)| m.max(p));
    let over = g1_rounds.iter().filter(|d| **d > g1margin).count();
    rows.push(Row {
        id: "G1",
        gate: if cfg!(target_os = "linux") {
            "QD1 4 KiB durable write p50 within the raw primitive (pwritev2 + fdatasync) + 2 µs (20 alternated rounds; PASS: the worse of the median per-round difference and the pooled difference within the gate; NOT RESOLVABLE: not, but the median's ~96% interval reaches the gate; FAIL: the interval is over it)"
        } else {
            "QD1 4 KiB durable write p50 within the raw primitive (write + data flush) + 3% (20 alternated rounds; PASS: the worse of the median per-round difference and the pooled difference within the gate; NOT RESOLVABLE: not, but the median's ~96% interval reaches the gate; FAIL: the interval is over it)"
        },
        measured: format!(
            "store-io - raw: median {} µs (interval {} to {}), pooled {} µs; gate {} µs; raw p50 {} µs; {over} of {} rounds over the gate",
            f(g1m, 2),
            f(g1lo, 2),
            f(g1hi, 2),
            f(g1p, 2),
            f(Some(g1margin), 2),
            f(g1r, 1),
            g1_rounds.len()
        ),
        verdict: match (g1d, g1lo, g1r) {
            (Some(d), Some(_), Some(r)) if r > 0.0 && d <= g1margin => Verdict::Pass,
            (Some(_), Some(lo), Some(r)) if r > 0.0 && lo <= g1margin => Verdict::NotResolvable,
            _ => Verdict::Fail,
        },
    });
    // G2: barrier alone, against the raw flush alone, at p50 and p99:
    // alternated rounds (`gates_g2`), judged as G1 is.
    for (q, us) in [("p50", 2.0), ("p99", 5.0)] {
        let fin = |k: String| val(&g2, &k).filter(|v: &f64| v.is_finite());
        let raw_q = fin(format!("raw_{q}_us"));
        let (m, lo, hi, pooled) = (
            fin(format!("diff_{q}_us")),
            fin(format!("diff_{q}_lo_us")),
            fin(format!("diff_{q}_hi_us")),
            fin(format!("pooled_{q}_us")),
        );
        let gate_margin = margin(us, raw_q);
        let d = m.zip(pooled).map(|(m, p)| m.max(p));
        rows.push(Row {
            id: "G2",
            gate: match (q, cfg!(target_os = "linux")) {
                ("p50", true) => "Barrier, one writer: p50 within the raw flush + 2 µs (20 alternated rounds, judged as G1)",
                (_, true) => "Barrier, one writer: p99 within the raw flush + 5 µs (20 alternated rounds, judged as G1)",
                ("p50", false) => "Barrier, one writer: p50 within the raw flush + 3% (20 alternated rounds, judged as G1)",
                (_, false) => "Barrier, one writer: p99 within the raw flush + 3% (20 alternated rounds, judged as G1)",
            },
            measured: format!(
                "store-io - raw: median {} µs (interval {} to {}), pooled {} µs; gate {} µs; raw {q} {} µs",
                f(m, 2),
                f(lo, 2),
                f(hi, 2),
                f(pooled, 2),
                f(Some(gate_margin), 2),
                f(raw_q, 1)
            ),
            verdict: match (d, lo, raw_q) {
                (Some(d), Some(_), Some(r)) if r > 0.0 && d <= gate_margin => Verdict::Pass,
                (Some(_), Some(lo), Some(r)) if r > 0.0 && lo <= gate_margin => {
                    Verdict::NotResolvable
                }
                _ => Verdict::Fail,
            },
        });
    }
    // G3: QD1 durable writes/s, at least 90% of fio's synchronous write +
    // fdatasync and at least 90% of the raw primitive, each within its own
    // spread; the worse verdict stands (on Windows fio's fdatasync is a full
    // FlushFileBuffers and its engine hands every I/O to a completion
    // thread: a weaker baseline than store-io's own call).
    let (fa, fb) = (ok(&fio_d1).map(|s| s.iops), ok(&fio_d2).map(|s| s.iops));
    let (ra, rb) = (get(raw(&l1), "ops_per_s"), get(raw(&l2), "ops_per_s"));
    let rel = |a: Option<f64>, b: Option<f64>| mean(a, b).map_or(0.0, |m| spread(a, b) / m);
    let store_rate = mean(get(page(&l1), "ops_per_s"), get(page(&l2), "ops_per_s"));
    let against_fio = at_least(store_rate, mean(fa, fb), 0.9, rel(fa, fb));
    let against_raw = at_least(store_rate, mean(ra, rb), 0.9, rel(ra, rb));
    rows.push(Row {
        id: "G3",
        gate: "QD1 durable writes/s at least 90% of fio (synchronous write + fdatasync) and at least 90% of the raw primitive",
        measured: format!(
            "store-io {} /s, fio ({}) {} /s (spread {:.0}%), raw {} /s (spread {:.0}%)",
            f(store_rate, 0),
            fio::sync_engine(),
            f(mean(fa, fb), 0),
            rel(fa, fb) * 100.0,
            f(mean(ra, rb), 0),
            rel(ra, rb) * 100.0
        ),
        verdict: if [fa, fb, ra, rb].iter().any(Option::is_none) {
            Verdict::Fail
        } else {
            worse(against_fio, against_raw)
        },
    });
    // G4: durable random writes at QD 1, 4, 16: reported.
    let parts: Vec<String> = [1u32, 4, 16]
        .iter()
        .zip(&g4)
        .zip(&fio_dq)
        .map(|((qd, p), fq)| {
            format!(
                "QD{qd} {} vs fio {} /s",
                f(val(p, "ops_per_s"), 0),
                f(ok(fq).map(|s| s.iops), 0)
            )
        })
        .collect();
    rows.push(Row {
        id: "G4",
        gate: "4 KiB durable random writes per second at QD 1, 4, 16 against fio (reported; fio --fdatasync=1 syncs after every write, store-io shares flushes between the writes in flight)",
        measured: parts.join("; "),
        verdict: Verdict::Reported,
    });
    // G5: operations per CPU-second, at least 90% of fio's: alternated,
    // medians, the spread fio's own across its three runs.
    let theirs: Vec<f64> = f5
        .iter()
        .filter_map(|r| ok(r).map(|s| s.ios / s.cpu_s))
        .filter(|e| e.is_finite() && *e > 0.0)
        .collect();
    let ours: Vec<f64> = g5.iter().filter_map(|p| val(p, "ops_per_cpu_s")).collect();
    let (fio_eff, store_eff) = (med(&theirs), med(&ours));
    let espread = {
        let (lo, hi) = theirs
            .iter()
            .fold((f64::MAX, f64::MIN), |(l, h), x| (l.min(*x), h.max(*x)));
        fio_eff.filter(|m| *m > 0.0).map_or(0.0, |m| (hi - lo) / m)
    };
    rows.push(Row {
        id: "G5",
        gate: "Per-core submit efficiency at least 90% of fio's asynchronous engine (4 KiB random read, QD128; ops per CPU-second of the whole process, every thread counted, fio lean (gtod_reduce) with its start-up and exit taken out; medians of 3 alternated runs; a memory-backed device in CI)",
        measured: format!(
            "store-io {ours:.0?} ops/CPU-s, fio {theirs:.0?} ops/CPU-s (fio's spread {:.0}%)",
            espread * 100.0
        ),
        verdict: if !cfg!(any(target_os = "linux", windows)) {
            // CPU time is measured on Linux and Windows only.
            Verdict::Elsewhere
        } else if ours.len() == 3 && theirs.len() == 3 {
            at_least(store_eff, fio_eff, 0.9, espread)
        } else {
            Verdict::Fail
        },
    });
    // G6: sequential write, at least 90% of fio's best: the Submitter's
    // zero-copy writer and fio's QD8 write alternated (medians), fio's
    // synchronous write (two runs) as its other configuration. The simple API's
    // `append(&[u8])` copies the caller's bytes first (W5, reported).
    let ours: Vec<f64> = g6.iter().filter_map(|p| val(p, "mb_per_s")).collect();
    let q8w: Vec<f64> = f6
        .iter()
        .filter_map(|r| ok(r).map(|s| s.bytes_per_s / 1e6))
        .collect();
    let (sa, sb) = (
        ok(&fio_sw).map(|s| s.bytes_per_s / 1e6),
        ok(&fio_sw2).map(|s| s.bytes_per_s / 1e6),
    );
    let whole = ours.len() == 3 && q8w.len() == 3 && sa.is_some() && sb.is_some();
    let (sw, q8m, psm) = (med(&ours), med(&q8w), mean(sa, sb));
    let best = q8m.zip(psm).map(|(a, b)| a.max(b));
    let range = |v: &[f64], m: Option<f64>| {
        let (lo, hi) = v
            .iter()
            .fold((f64::MAX, f64::MIN), |(l, h), x| (l.min(*x), h.max(*x)));
        m.filter(|m| *m > 0.0).map_or(0.0, |m| (hi - lo) / m)
    };
    let wspread = if q8m >= psm {
        range(&q8w, q8m)
    } else {
        psm.map_or(0.0, |m| spread(sa, sb) / m)
    };
    let simple = get(
        find(&w5, System::StoreIo, "append(1 MiB)", None),
        "mb_per_s",
    );
    rows.push(Row {
        id: "G6",
        gate: "Sequential write (1 MiB) at least 90% of fio's best (store-io: Submitter appends 8 deep from pool buffers, no copy; medians of 3 alternated runs)",
        measured: format!(
            "store-io {ours:.0?} MB/s, fio QD8 {q8w:.0?} MB/s, fio {} {} MB/s (baseline spread {:.0}%); simple append(1 MiB), which copies, {} MB/s",
            fio::sync_engine(),
            f(psm, 0),
            wspread * 100.0,
            f(simple, 0)
        ),
        verdict: if whole {
            at_least(sw, best, 0.9, wspread)
        } else {
            Verdict::Fail
        },
    });
    // G7: scan, at least 85% of fio's best sequential read: the scan and
    // fio's QD8 read alternated (medians), fio's QD1 read for reference.
    let scan_mb = val(&g7, "mb_per_s");
    let q8 = val(&g7, "fio_mb_per_s");
    let q1 = ok(&fio_sr1).map(|s| s.bytes_per_s / 1e6);
    let r1m = get(find(&w5, System::StoreIo, "read, 1 MiB", None), "mb_per_s");
    let r8m = get(find(&w5, System::StoreIo, "read, 8 MiB", None), "mb_per_s");
    let rspread = val(&g7, "spread").unwrap_or(0.0);
    let best_fio = q8.zip(q1).map(|(a, b)| a.max(b));
    rows.push(Row {
        id: "G7",
        gate: "Scan (sequential read) at least 85% of fio's best sequential read",
        measured: format!(
            "store-io scan {} MB/s, fio QD8 {} MB/s (medians of {} alternated runs, spread {:.0}%); fio QD1 {} MB/s; read calls 1 MiB {} / 8 MiB {} MB/s",
            f(scan_mb, 0),
            f(q8, 0),
            gates_scan::REPS,
            rspread * 100.0,
            f(q1, 0),
            f(r1m, 0),
            f(r8m, 0)
        ),
        verdict: if best_fio.is_some() {
            at_least(scan_mb, best_fio, 0.85, rspread)
        } else {
            Verdict::Fail
        },
    });
    // G8: allocations on the hot paths after init.
    // The read loop's count: the largest of its three runs.
    let read_allocs = g5
        .iter()
        .map(|p| val(p, "allocs").unwrap_or(f64::NAN))
        .fold(0.0_f64, |m, a| {
            if a.is_nan() || m.is_nan() {
                f64::NAN
            } else {
                m.max(a)
            }
        });
    let allocs = [&g8b, &g8w]
        .iter()
        .map(|p| val(p, "allocs").unwrap_or(f64::NAN))
        .chain(std::iter::once(read_allocs))
        .collect::<Vec<_>>();
    rows.push(Row {
        id: "G8a",
        gate: "Hot path: 0 allocations after init (blocking durable write; submitter write QD32; submitter read QD128)",
        measured: format!("{:.0} / {:.0} / {:.0} allocations in the measured windows", allocs[0], allocs[1], allocs[2]),
        verdict: if allocs.iter().all(|a| *a == 0.0) { Verdict::Pass } else { Verdict::Fail },
    });
    rows.push(Row {
        id: "G8b",
        gate: "Hot path: no futex and no timed wait in a submitter's steady-state submit / wait loop (plain writes, io_uring)",
        measured: "CI: strace of tests/hotpath.rs (dev/tools/hotpath-check.py)".to_owned(),
        verdict: Verdict::Elsewhere,
    });
    rows.push(Row {
        id: "G9",
        gate: "Idle 60 s: 0 syscalls, 0 I/Os, 0 timer wakeups",
        measured: "tests/idle.rs in CI, both tiers".to_owned(),
        verdict: Verdict::Elsewhere,
    });
    // G10: QoS is a v0.6 exit criterion (I/O classes and pacing are v0.6
    // scope, dev/ROADMAP.md): measured and reported here, not gated.
    let ratio = val(&g10, "p99_ratio");
    rows.push(Row {
        id: "G10",
        gate: "QoS: commit p99 at most 2x idle p99 under a background writer (a v0.6 exit criterion; reported in v0.4)",
        measured: format!(
            "idle {} µs, loaded {} µs, ratio {}",
            f(val(&g10, "p99_idle_us"), 0),
            f(val(&g10, "p99_loaded_us"), 0),
            f(ratio, 2)
        ),
        verdict: if ratio.is_some() {
            Verdict::Reported
        } else {
            Verdict::Fail
        },
    });
    // 3d (D-078, D-107): the direct descriptor on against off, alternated;
    // reported (it decides `StoreOptions::register_file`'s default).
    {
        let side = |write: bool, on: bool| -> Vec<f64> {
            g3d.iter()
                .filter(|p| {
                    p.variant.contains(if write { "write" } else { "read" })
                        && p.variant.contains("(registered)") == on
                })
                .filter_map(|p| val(p, "ops_per_s"))
                .collect()
        };
        let whole = [true, false]
            .iter()
            .all(|w| side(*w, false).len() == 3 && side(*w, true).len() == 3);
        let gain = |w: bool| {
            med(&side(w, true))
                .zip(med(&side(w, false)))
                .map(|(a, b)| a / b)
        };
        // What the registered points' rings got, from their live path:
        // "... a direct descriptor on F".
        let got = g3d
            .iter()
            .flat_map(|p| p.notes.iter())
            .find(|n| n.contains("registration (D-078)"))
            .cloned()
            .unwrap_or_else(|| "no registration reported".to_owned());
        let count = |key: &str| {
            got.split(key)
                .nth(1)
                .and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next())
                .and_then(|d| d.parse::<u64>().ok())
                .unwrap_or(0)
        };
        let registered = count("a direct descriptor on ") > 0;
        rows.push(Row {
            id: "G3d",
            gate: "Direct descriptor (D-078; buffers never registered, D-107): submitter QD32 4 KiB ops/s, registered against not, medians of 3 alternated runs (reported; decides the default)",
            measured: format!(
                "write {:?} vs {:?} /s ({}x), read {:?} vs {:?} /s ({}x); {got}",
                side(true, true).iter().map(|x| x.round()).collect::<Vec<_>>(),
                side(true, false).iter().map(|x| x.round()).collect::<Vec<_>>(),
                f(gain(true), 2),
                side(false, true).iter().map(|x| x.round()).collect::<Vec<_>>(),
                side(false, false).iter().map(|x| x.round()).collect::<Vec<_>>(),
                f(gain(false), 2),
            ),
            // Fails closed unless the registered runs really registered.
            verdict: if !cfg!(target_os = "linux") {
                Verdict::Elsewhere
            } else if whole && registered {
                Verdict::Reported
            } else {
                Verdict::Fail
            },
        });
    }
    rows.push(Row {
        id: "G11",
        gate: "Observer: compiled out when off; at most 20 ns/op when on",
        measured: "D-100 (observe-cost)".to_owned(),
        verdict: Verdict::Elsewhere,
    });
    // G13: the v0.3 gates moved to v0.4 (dev/ROADMAP.md), each against fsys
    // on its own comparisons. A gate for the other operating system is
    // ELSEWHERE; one with no comparison fails closed.
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(windows) {
        "windows"
    } else {
        "other"
    };
    let mut anomalies = Vec::new();
    for (id, gate, sec, on_os, pick) in [
        (
            "G13a",
            "Linux page batches at least 20% better than fsys, or at the device floor (within 10% of the raw primitive)",
            &w4,
            "linux",
            (|_: &Comparison| true) as fn(&Comparison) -> bool,
        ),
        (
            "G13b",
            "Windows: N x append + one sync_through at least 20% better than fsys, or at the device floor",
            &w3,
            "windows",
            |c: &Comparison| c.case.contains(" x append("),
        ),
        (
            "G13c",
            "Windows: concurrent writers at 1, 2 and 4 at least 20% better than fsys, or at the device floor",
            &w2,
            "windows",
            |c: &Comparison| {
                ["1 ", "2 ", "4 "].iter().any(|n| c.case.starts_with(n))
                    && c.case.contains("concurrent")
            },
        ),
    ] {
        if on_os != os {
            rows.push(Row {
                id,
                gate,
                measured: "measured on the other operating system".to_owned(),
                verdict: Verdict::Elsewhere,
            });
            continue;
        }
        // Two more runs of the workload: each comparison is the median of
        // three (a single run on a virtual disk is noise-bound).
        let again: Vec<Section> = (0..2).map(|_| rerun(ctx, id)).collect();
        let all: Vec<&Section> = std::iter::once(sec).chain(again.iter()).collect();
        for s in &again {
            anomalies.extend(s.anomalies.iter().cloned());
        }
        let medians = median_comparisons(&all);
        let (better, floor, worse) = against_fsys(medians.iter().filter(|c| pick(c)));
        rows.push(Row {
            id,
            gate,
            measured: format!(
                "medians of 3 runs: {better} at least 20% better, {floor} at the floor, {} neither{}",
                worse.len(),
                if worse.is_empty() {
                    String::new()
                } else {
                    format!(": {}", worse.join("; "))
                }
            ),
            verdict: if worse.is_empty() && better + floor > 0 {
                Verdict::Pass
            } else {
                Verdict::Fail
            },
        });
    }
    // G14: lone writer, one primitive per durable write.
    let fl = get(page(&l1), "flushes_per_op");
    rows.push(Row {
        id: "G14",
        gate: "Lone writer: one durable write costs one flush and never waits for company",
        measured: format!("{} flushes per durable write", f(fl, 2)),
        verdict: if fl.is_some_and(|x| (x - 1.0).abs() < 0.05) {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
    });
    // G15: concurrent writers share flushes: one and eight writers
    // alternated three times (medians), then fio's eight-writer controls on
    // the laid-out file (`gates_g15`), W2's own points for reference. Last,
    // as before, so its fio load precedes no other gate.
    let g15 = gates_g15::measure(ctx, &mut |a: &[&str]| job(a, "write"), val);
    sec.anomalies.extend(g15.fio_errors.iter().cloned());
    if let Some(e) = remove(&file) {
        sec.anomalies.push(e);
    }
    let (one, eight) = (&g15.one, &g15.eight);
    let whole = g15.whole();
    let (o1, o8, f8) = (med(one), med(eight), med(&g15.flushes_at_eight));
    rows.push(Row {
        id: "G15",
        gate: "Concurrent writers: durable ops/s rises with writers through flush sharing (8 writers at least 1.5x one, under 0.9 flushes per op; medians of 3 alternated runs)",
        measured: format!(
            "1 writer {one:.0?} /s, 8 writers {eight:.0?} /s, {} flushes per op at 8; fio 8 writers ({}): writes alone {:.0?} /s, each flushed {:.0?} /s",
            f(f8, 2),
            fio::sync_engine(),
            g15.fio_writes,
            g15.fio_durable
        ),
        verdict: match (whole, o1, o8, f8) {
            (true, Some(a), Some(b), Some(x)) if b >= 1.5 * a && x < 0.9 => Verdict::Pass,
            // Sharing flushes, and 1.5x one writer lies within the range of
            // the eight-writer runs: this disk cannot tell.
            (true, Some(a), Some(_), Some(x))
                if x < 0.9 && eight.iter().copied().fold(f64::MIN, f64::max) >= 1.5 * a =>
            {
                Verdict::NotResolvable
            }
            _ => Verdict::Fail,
        },
    });
    // G16: caller batch: N writes + one flush per commit.
    let batch_fl: Vec<f64> = w3
        .points
        .iter()
        .filter(|p| p.system == System::StoreIo && p.variant.contains("AppendBatch"))
        .map(|p| val(p, "flushes_per_op").unwrap_or(f64::NAN))
        .collect();
    rows.push(Row {
        id: "G16",
        gate: "Caller batch: N writes + one barrier cost one flush",
        measured: format!(
            "flushes per commit across {} batch points: {:?}",
            batch_fl.len(),
            batch_fl
                .iter()
                .map(|x| (x * 100.0).round() / 100.0)
                .collect::<Vec<_>>()
        ),
        verdict: if !batch_fl.is_empty() && batch_fl.iter().all(|x| (x - 1.0).abs() < 0.05) {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
    });
    rows.push(Row {
        id: "G12",
        gate: "Multi-device",
        measured: "one device available".to_owned(),
        verdict: Verdict::Elsewhere,
    });

    let mut t = Table::new("Gates:", &["gate", "requirement", "measured", "verdict"]);
    for r in &rows {
        t.row(vec![
            r.id.to_owned(),
            r.gate.to_owned(),
            r.measured.clone(),
            r.verdict.name().to_owned(),
        ]);
    }
    sec.tables.push(t);
    for r in rows.iter().filter(|r| r.verdict == Verdict::Fail) {
        sec.findings
            .push(format!("{} FAIL: {} ({})", r.id, r.gate, r.measured));
    }
    let mut pts = vec![g1, g2, g7, g8w, g8b, g10];
    pts.extend(g3d);
    pts.extend(g5);
    pts.extend(g6);
    pts.extend(g4);
    pts.extend(g15.points);
    sec.points = pts;
    for s in [&l1, &l2, &w2, &w3, &w4, &w5] {
        sec.anomalies.extend(s.anomalies.iter().cloned());
    }
    sec.anomalies.extend(anomalies);
    sec.collect_point_anomalies();
    sec
}
