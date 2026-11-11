//! G17 (PLANNING R5.14, exit criterion 1): summed sequential write across
//! devices. For N = 1, 2 and 4 (as many as `--devices` gives), one store on
//! each of the first N devices, each its own `Submitter` appending 1 MiB
//! from pool buffers 8 deep on a thread of its own (G6's writer), started
//! together; their MB/s summed. Against fio on the same N devices together:
//! one QD8 1 MiB sequential-write job per device in one fio process,
//! reported as a group. Three alternated runs per N (each side first in
//! turn); PASS when, for every N, store-io's median is at least 90% of
//! fio's (NOT RESOLVABLE when only within fio's own spread of it, as G6 is
//! judged); labelled with the CPU count (four vCPUs bound it on CI, E3).
//! NOT RUN with fewer than two devices; NOT RESOLVABLE at best with two or
//! three (N = 4 not measured). Directories sharing a device are refused on
//! Unix (their device numbers); Windows does not check, so there N stores
//! on one disk would be summed against N fio jobs on it.

use std::path::PathBuf;
use std::sync::Barrier;

use store_io_engine::StoreOptions;

use super::gates::{Row, Verdict, at_least, f, med, range, worse};
use super::gates_seq::{WINDOW, write_seq_on};
use super::{Ctx, remove};
use crate::fio;
use crate::json::Json;
use crate::report::{Point, System, Verify};

/// Each device's fio file, and each store's append region: two of them fit
/// a CI device of 2 GiB with its file system.
const SPAN: u64 = 512 << 20;
/// Alternated runs per N.
const RUNS: usize = 3;

const GATE: &str = "Summed sequential write (1 MiB, the Submitter 8 deep, one store per device) over N = 1, 2, 4 devices at least 90% of fio's QD8 on the same N devices together (medians of 3 alternated runs; labelled with the CPU count)";

/// G17's row and points.
pub struct G17 {
    pub(super) row: Row,
    pub(super) points: Vec<Point>,
}

fn not_run(why: &str) -> G17 {
    let mut point = Point::new(
        System::StoreIo,
        "G17 summed sequential write across devices",
    );
    point.verify = Verify::NotChecked(why.to_owned());
    G17 {
        row: Row {
            id: "G17",
            gate: GATE,
            measured: why.to_owned(),
            verdict: Verdict::NotRun,
        },
        points: vec![point],
    }
}

/// store-io on the first `n` of `dirs` at once: the summed MB/s, and each
/// writer's point (its read-back).
fn ours(ctx: &Ctx, dirs: &[PathBuf], n: usize) -> (Option<f64>, Vec<Point>) {
    let opts = StoreOptions {
        append_window: WINDOW.max(ctx.opts.append_window),
        ..ctx.opts.clone()
    };
    let start = Barrier::new(n);
    let points: Vec<Point> = std::thread::scope(|s| {
        let handles: Vec<_> = dirs[..n]
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let (opts, start) = (&opts, &start);
                s.spawn(move || {
                    // Removed however the writer leaves, a panic included:
                    // a leaked store would fill its device for later runs.
                    struct Gone(PathBuf);
                    impl Drop for Gone {
                        fn drop(&mut self) {
                            let _left = remove(&self.0);
                        }
                    }
                    let dir = d.join(format!("store-io-g17-{}-{n}-{i}", std::process::id()));
                    let _gone = Gone(dir.clone());
                    let p = Point::new(System::StoreIo, format!("G17 writer {i} of {n}"))
                        .param("devices", Json::Num(n as f64));
                    write_seq_on(ctx, &dir, opts, 8, SPAN, Some(start), p)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join().unwrap_or_else(|_| {
                    let mut p = Point::new(System::StoreIo, "G17 writer");
                    p.error = Some("the writer panicked".to_owned());
                    p
                })
            })
            .collect()
    });
    // As fio's group sums them: every writer's bytes over the longest
    // window (each times its own, started together at the barrier).
    let bytes: Vec<f64> = points.iter().filter_map(|p| p.get("bytes")).collect();
    let secs = points
        .iter()
        .filter_map(|p| p.get("secs"))
        .fold(0.0f64, f64::max);
    let whole = bytes.len() == n
        && secs > 0.0
        && points.iter().all(|p| matches!(p.verify, Verify::Passed(_)));
    (
        whole.then(|| bytes.iter().sum::<f64>() / secs / 1e6),
        points,
    )
}

/// fio on the first `n` of `files` together: the group's MB/s.
fn theirs(ctx: &Ctx, files: &[PathBuf], n: usize) -> Result<f64, String> {
    let engine = format!("--ioengine={}", fio::engine());
    fio::jobs_together(
        &files[..n],
        SPAN,
        &[
            &engine,
            "--rw=write",
            "--bs=1m",
            "--iodepth=8",
            "--end_fsync=1",
        ],
        "write",
        ctx.secs,
        ctx.warmup,
    )
    .map(|s| s.bytes_per_s / 1e6)
}

/// G17 on `devices` (a directory on each device); NOT RUN with fewer than
/// two.
pub fn measure(ctx: &Ctx, devices: &[PathBuf]) -> G17 {
    if devices.len() < 2 {
        return not_run("needs --devices (two or more devices: the multi-device job)");
    }
    // One directory per device: two on one file system would sum one
    // device twice.
    match super::gates_g18::distinct_devices(devices) {
        Ok(()) => {}
        Err(e) if e.starts_with("NOT RUN: ") => return not_run(&e["NOT RUN: ".len()..]),
        Err(e) => {
            let mut g = not_run(&e);
            g.row.verdict = Verdict::Fail;
            g.points[0].error = Some(e);
            return g;
        }
    }
    let files: Vec<PathBuf> = devices
        .iter()
        .enumerate()
        .map(|(i, d)| d.join(format!("store-io-g17-fio-{}-{i}.dat", std::process::id())))
        .collect();
    let laid: Result<(), String> = files.iter().try_for_each(|f| fio::lay_out(f, SPAN));
    let cpus = std::thread::available_parallelism().map_or(0, std::num::NonZero::get);
    let mut points = Vec::new();
    let mut verdict = Verdict::Pass;
    let mut measured = Vec::new();
    for n in [1usize, 2, 4].into_iter().filter(|&n| n <= devices.len()) {
        let (mut mine, mut fios) = (Vec::new(), Vec::new());
        let mut errors = Vec::new();
        for run in 0..RUNS {
            let fio_side = || match &laid {
                Ok(()) => theirs(ctx, &files, n),
                Err(e) => Err(e.clone()),
            };
            let (o, t) = if run % 2 == 0 {
                let t = fio_side();
                (ours(ctx, devices, n), t)
            } else {
                let o = ours(ctx, devices, n);
                (o, fio_side())
            };
            let (sum, ps) = o;
            match sum {
                Some(x) => mine.push(x),
                None => errors.push(format!("store-io run {run} incomplete or unverified")),
            }
            match t {
                Ok(x) => fios.push(x),
                Err(e) => errors.push(format!("fio run {run}: {e}")),
            }
            points.extend(ps);
        }
        let (m, b) = (med(&mine), med(&fios));
        let v = if mine.len() == RUNS && fios.len() == RUNS {
            at_least(m, b, 0.9, range(&fios, b))
        } else {
            Verdict::Fail
        };
        verdict = worse(verdict, v);
        let mut p = Point::new(System::StoreIo, format!("G17 summed over {n} devices"))
            .param("devices", Json::Num(n as f64))
            .param("cpus", Json::Num(cpus as f64));
        p.metric("mb_per_s", m);
        p.metric("fio_mb_per_s", b);
        p.metric("ratio", m.zip(b).map(|(a, b)| a / b));
        p.notes.extend(errors.iter().cloned());
        p.verify = Verify::NotChecked("each writer's read-back is its own point".to_owned());
        points.push(p);
        measured.push(format!(
            "N {n}: store-io {mine:.0?} MB/s, fio {fios:.0?} MB/s, {}% ({}){}",
            f(m.zip(b).map(|(a, b)| a / b * 100.0), 1),
            v.name(),
            if errors.is_empty() {
                String::new()
            } else {
                format!(", {}", errors.join("; "))
            }
        ));
    }
    if devices.len() < 4 {
        measured.push(format!(
            "N 4 not measured ({} devices given)",
            devices.len()
        ));
        verdict = worse(verdict, Verdict::NotResolvable);
    }
    for f in &files {
        let _gone = remove(f);
    }
    if let Err(e) = &laid {
        measured.push(format!("fio's files could not be laid out: {e}"));
        verdict = Verdict::Fail;
    }
    G17 {
        row: Row {
            id: "G17",
            gate: GATE,
            measured: format!("{cpus} CPUs; {}", measured.join("; ")),
            verdict,
        },
        points,
    }
}
