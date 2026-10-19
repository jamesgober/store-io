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
    Ctx, caller_batch, concurrent, gates_extra, gates_scan, lone, page_batch, remove, sequential,
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

/// `store` against `base + margin` (lower is better), with the baseline's
/// spread deciding what this machine can resolve.
/// A latency gate's margin over `base`: on Linux `us` microseconds, on
/// Windows and macOS 3% (dev/ROADMAP.md, flush-required devices).
fn margin(us: f64, base: Option<f64>) -> f64 {
    if cfg!(target_os = "linux") {
        us
    } else {
        base.map_or(0.0, |b| 0.03 * b)
    }
}

fn at_most(store: Option<f64>, base: Option<f64>, margin: f64, spread: f64) -> Verdict {
    // A baseline that is not a positive, finite number judges nothing.
    let base = base.filter(|b| b.is_finite() && *b > 0.0);
    let spread = if spread.is_finite() { spread } else { 0.0 };
    match (store, base) {
        (Some(s), Some(b)) if s <= b + margin => Verdict::Pass,
        (Some(s), Some(b)) if s <= b + margin + spread => Verdict::NotResolvable,
        (Some(_), Some(_)) => Verdict::Fail,
        _ => Verdict::Fail,
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
        "Workloads: W1 (lone) twice, W2 (concurrent), W3 (caller batch), W4 (page batch), W5 (sequential); fio controls on one 256 MiB file written in full first; the gate-only measurements in `gates_extra` (submitter loops, allocations, QoS).".to_owned(),
        "Verdicts: PASS within the gate; NOT RESOLVABLE when outside it by less than the baseline's own run-to-run spread on this machine (W1 run twice for the raw primitive; each fio control a gate uses run twice); FAIL beyond that, when a baseline did not run, or when the measurement failed or its data check did not pass; REPORTED where the table asks for a report; ELSEWHERE where another check measures it.".to_owned(),
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
    let durable = ["--ioengine=psync", "--rw=write", "--bs=4k", "--fdatasync=1"];
    let fio_d1 = job(&durable, "write");
    let fio_d2 = job(&durable, "write");
    let seq_write = ["--ioengine=psync", "--rw=write", "--bs=1m", "--end_fsync=1"];
    let fio_sw = job(&seq_write, "write");
    let fio_sw2 = job(&seq_write, "write");
    let fio_sr1 = job(&["--ioengine=psync", "--rw=read", "--bs=1m"], "read");
    let cpu_args = [
        aio,
        "--rw=randread",
        "--bs=4k",
        "--iodepth=128",
        "--norandommap",
    ];
    let fio_cpu = job(&cpu_args, "read");
    let fio_cpu2 = job(&cpu_args, "read");
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
    if let Some(e) = remove(&file) {
        sec.anomalies.push(e);
    }
    let fio_runs = [
        ("fio durable (1)", &fio_d1),
        ("fio durable (2)", &fio_d2),
        ("fio sequential write (1)", &fio_sw),
        ("fio sequential write (2)", &fio_sw2),
        ("fio sequential read QD1", &fio_sr1),
        ("fio CPU efficiency (1)", &fio_cpu),
        ("fio CPU efficiency (2)", &fio_cpu2),
        ("fio durable QD1", &fio_dq[0]),
        ("fio durable QD4", &fio_dq[1]),
        ("fio durable QD16", &fio_dq[2]),
    ];
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
    let g5 = gates_extra::read_qd128(ctx);
    let g8w = gates_extra::write_qd32(ctx);
    let g8b = gates_extra::blocking_allocs(ctx);
    let g10 = gates_extra::qos(ctx);

    let mut rows = Vec::new();
    // G1: QD1 4 KiB durable write p50, against the raw primitive + 2 us.
    fn page(s: &Section) -> Option<&Point> {
        find(s, System::StoreIo, "PageRegion::write_durable", None)
    }
    fn raw(s: &Section) -> Option<&Point> {
        find(s, System::Raw, "write + data flush", None)
    }
    let (s1, s2) = (get(page(&l1), "p50_us"), get(page(&l2), "p50_us"));
    let (r1, r2) = (get(raw(&l1), "p50_us"), get(raw(&l2), "p50_us"));
    // The baseline's own spread only: a noisy store-io never widens its
    // own tolerance.
    let spr = spread(r1, r2);
    rows.push(Row {
        id: "G1",
        gate: if cfg!(target_os = "linux") {
            "QD1 4 KiB durable write p50 within the raw primitive (pwritev2 + fdatasync) + 2 µs"
        } else {
            "QD1 4 KiB durable write p50 within the raw primitive (write + data flush) + 3%"
        },
        measured: format!(
            "store-io {} µs, raw {} µs (spread between runs {spr:.0} µs)",
            f(mean(s1, s2), 1),
            f(mean(r1, r2), 1)
        ),
        verdict: at_most(mean(s1, s2), mean(r1, r2), margin(2.0, mean(r1, r2)), spr),
    });
    // G2: barrier alone, against the raw flush alone.
    fn split(s: &Section) -> Option<&Point> {
        find(s, System::StoreIo, "halves timed separately", None)
    }
    for (q, k, us) in [("p50", "sync_p50_us", 2.0), ("p99", "sync_p99_us", 5.0)] {
        let (a, b) = (get(split(&l1), k), get(split(&l2), k));
        let (c, d) = (get(raw(&l1), k), get(raw(&l2), k));
        let spr = spread(c, d);
        rows.push(Row {
            id: "G2",
            gate: match (q, cfg!(target_os = "linux")) {
                ("p50", true) => "Barrier, one writer: p50 within the raw flush + 2 µs",
                (_, true) => "Barrier, one writer: p99 within the raw flush + 5 µs",
                ("p50", false) => "Barrier, one writer: p50 within the raw flush + 3%",
                (_, false) => "Barrier, one writer: p99 within the raw flush + 3%",
            },
            measured: format!(
                "store-io {} µs, raw {} µs (spread {spr:.0} µs)",
                f(mean(a, b), 1),
                f(mean(c, d), 1)
            ),
            verdict: at_most(mean(a, b), mean(c, d), margin(us, mean(c, d)), spr),
        });
    }
    // G3: QD1 durable writes/s, at least 90% of fio write+fdatasync.
    let (fa, fb) = (ok(&fio_d1).map(|s| s.iops), ok(&fio_d2).map(|s| s.iops));
    let fspread = mean(fa, fb).map_or(0.0, |m| spread(fa, fb) / m);
    let store_rate = mean(get(page(&l1), "ops_per_s"), get(page(&l2), "ops_per_s"));
    rows.push(Row {
        id: "G3",
        gate: "QD1 durable writes/s at least 90% of fio (psync write + fdatasync)",
        measured: format!(
            "store-io {} /s, fio {} /s (fio's spread {:.0}%)",
            f(store_rate, 0),
            f(mean(fa, fb), 0),
            fspread * 100.0
        ),
        verdict: at_least(store_rate, mean(fa, fb), 0.9, fspread),
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
    // G5: operations per CPU-second, at least 90% of fio io_uring.
    let eff = |r: &Result<fio::Stats, String>| ok(r).map(|s| s.iops * s.runtime_s / s.cpu_s);
    let (ea, eb) = (eff(&fio_cpu), eff(&fio_cpu2));
    let fio_eff = mean(ea, eb);
    let espread = fio_eff.map_or(0.0, |m| spread(ea, eb) / m);
    let store_eff = val(&g5, "ops_per_cpu_s");
    rows.push(Row {
        id: "G5",
        gate: "Per-core submit efficiency at least 90% of fio's asynchronous engine (4 KiB random read, QD128; ops per CPU-second; a memory-backed device in CI)",
        measured: format!("store-io {} ops/CPU-s ({} ops/s), fio {} ops/CPU-s (spread {:.0}%)", f(store_eff, 0), f(val(&g5, "ops_per_s"), 0), f(fio_eff, 0), espread * 100.0),
        verdict: if !cfg!(any(target_os = "linux", windows)) {
            // CPU time is measured on Linux and Windows only.
            Verdict::Elsewhere
        } else if ea.is_some() && eb.is_some() { at_least(store_eff, fio_eff, 0.9, espread) } else { Verdict::Fail },
    });
    // G6: sequential write, at least 90% of fio.
    // store-io's bytes over the wall clock of the whole run (payload
    // generation included, which can only count against it), as fio's
    // bandwidth is bytes over its runtime.
    let w5p = find(&w5, System::StoreIo, "append(1 MiB)", None);
    let sw = get(w5p, "bytes")
        .zip(get(w5p, "wall_s"))
        .and_then(|(b, w)| (w > 0.0).then(|| b / w / 1e6));
    let (sa, sb) = (
        ok(&fio_sw).map(|s| s.bytes_per_s / 1e6),
        ok(&fio_sw2).map(|s| s.bytes_per_s / 1e6),
    );
    let fsw = mean(sa, sb);
    let wspread = fsw.map_or(0.0, |m| spread(sa, sb) / m);
    rows.push(Row {
        id: "G6",
        gate: "Sequential write (1 MiB) at least 90% of fio",
        measured: format!(
            "store-io {} MB/s (wall clock), fio {} MB/s (spread {:.0}%)",
            f(sw, 0),
            f(fsw, 0),
            wspread * 100.0
        ),
        verdict: if sa.is_some() && sb.is_some() {
            at_least(sw, fsw, 0.9, wspread)
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
    let allocs = [&g8b, &g8w, &g5]
        .iter()
        .map(|p| val(p, "allocs").unwrap_or(f64::NAN))
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
    // alternated three times (medians), W2's own points for reference.
    let w15: Vec<Point> = (0..3)
        .flat_map(|_| {
            [
                concurrent::store_point(ctx, 1),
                concurrent::store_point(ctx, 8),
            ]
        })
        .collect();
    let side = |k: usize, m: &str| -> Vec<f64> {
        w15.iter()
            .skip(k)
            .step_by(2)
            .filter_map(|p| val(p, m))
            .collect()
    };
    let (one, eight, f8s) = (
        side(0, "ops_per_s"),
        side(1, "ops_per_s"),
        side(1, "flushes_per_op"),
    );
    let whole = one.len() == 3 && eight.len() == 3 && f8s.len() == 3;
    let (o1, o8, f8) = (med(&one), med(&eight), med(&f8s));
    rows.push(Row {
        id: "G15",
        gate: "Concurrent writers: durable ops/s rises with writers through flush sharing (8 writers at least 1.5x one, under 0.9 flushes per op; medians of 3 alternated runs)",
        measured: format!(
            "1 writer {one:.0?} /s, 8 writers {eight:.0?} /s, {} flushes per op at 8",
            f(f8, 2)
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
    let mut pts = vec![g5, g7, g8w, g8b, g10];
    pts.extend(g4);
    pts.extend(w15);
    sec.points = pts;
    for s in [&l1, &l2, &w2, &w3, &w4, &w5] {
        sec.anomalies.extend(s.anomalies.iter().cloned());
    }
    sec.anomalies.extend(anomalies);
    sec.collect_point_anomalies();
    sec
}
