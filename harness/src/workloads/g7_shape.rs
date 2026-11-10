//! G7's shape study (an experiment, not a gate): the scan beside fio at
//! several request sizes and depths, all alternated on one device, so the
//! scan's gap to fio's 1 MiB QD8 reference can be split into what the
//! shape costs (fio at the scan's own 256 KiB x 8) and what store-io adds.

use store_io_engine::StoreOptions;

use super::Ctx;
use super::gates_scan;
use crate::fio;
use crate::report::{Section, Table};

/// The fio controls' file size (as `gates`).
const SPAN: u64 = 256 << 20;

/// One fio shape: its label and its own options.
struct Shape {
    label: &'static str,
    args: &'static [&'static str],
    /// io_uring (else the synchronous engine).
    aio: bool,
}

const SHAPES: &[Shape] = &[
    Shape {
        label: "fio 1 MiB QD8",
        args: &["--bs=1m", "--iodepth=8"],
        aio: true,
    },
    Shape {
        label: "fio 256 KiB QD8",
        args: &["--bs=256k", "--iodepth=8"],
        aio: true,
    },
    Shape {
        label: "fio 256 KiB QD8, submit batch 8",
        args: &[
            "--bs=256k",
            "--iodepth=8",
            "--iodepth_batch_submit=8",
            "--iodepth_batch_complete_max=8",
        ],
        aio: true,
    },
    Shape {
        label: "fio 256 KiB QD32",
        args: &["--bs=256k", "--iodepth=32"],
        aio: true,
    },
    Shape {
        label: "fio 128 KiB QD8",
        args: &["--bs=128k", "--iodepth=8"],
        aio: true,
    },
    Shape {
        label: "fio 1 MiB QD2",
        args: &["--bs=1m", "--iodepth=2"],
        aio: true,
    },
    Shape {
        label: "fio 1 MiB QD1 (sync)",
        args: &["--bs=1m"],
        aio: false,
    },
];

/// Runs the study.
pub fn run(ctx: &Ctx) -> Section {
    let mut sec = Section::new(
        "g7",
        "G7 shape study: the scan beside fio at several shapes (experiment)",
    );
    sec.method = vec![
        "Each store-io scan variant alternates with fio (G7's `gates_scan` loop, 3 reps): before each scan rep, every fio shape listed for it runs once, in order. MB/s medians; fio CPU is usr+sys seconds per GB read.".to_owned(),
    ];
    let file = ctx.path("g7-fio");
    if let Err(e) = fio::lay_out(&file, SPAN) {
        sec.anomalies.push(format!("fio layout: {e}"));
        return sec;
    }
    let aio = format!("--ioengine={}", fio::engine());
    let sync = format!("--ioengine={}", fio::sync_engine());
    let mut variants: Vec<(String, StoreOptions, Vec<usize>)> = Vec::new();
    variants.push((
        "scan 256 KiB x 8 (default)".to_owned(),
        ctx.opts.clone(),
        (0..SHAPES.len()).collect(),
    ));
    let mut deep = ctx.opts.clone();
    deep.background_unpaced = 32;
    variants.push((
        "scan 256 KiB x 32 (background_unpaced 32)".to_owned(),
        deep,
        vec![0, 1, 3],
    ));
    let mut small = ctx.opts.clone();
    small.background_io = 128 << 10;
    variants.push((
        "scan 128 KiB x 8 (background_io 128 KiB)".to_owned(),
        small,
        vec![0, 1, 4],
    ));
    let mut t = Table::new(
        "MB/s, medians of 3 alternated runs (each variant with its own fio runs)",
        &[
            "variant",
            "side",
            "MB/s (median)",
            "runs",
            "fio CPU s/GB",
            "% of fio 1 MiB QD8",
        ],
    );
    for (name, opts, shapes) in &variants {
        let mut got: Vec<(usize, Vec<f64>, Vec<f64>)> = shapes
            .iter()
            .map(|&i| (i, Vec::new(), Vec::new()))
            .collect();
        let mut errs = Vec::new();
        let point = {
            let mut ladder = || {
                let mut first = None;
                for (i, mbs, cpu) in &mut got {
                    let sh = &SHAPES[*i];
                    let eng = if sh.aio { aio.as_str() } else { sync.as_str() };
                    let mut a = vec![eng, "--rw=read"];
                    a.extend_from_slice(sh.args);
                    match fio::job(&file, SPAN, &a, "read", ctx.secs, ctx.warmup) {
                        Ok(s) => {
                            let m = s.bytes_per_s / 1e6;
                            mbs.push(m);
                            cpu.push(s.cpu_s / (s.bytes_per_s * ctx.secs / 1e9).max(1e-9));
                            if *i == 0 {
                                first = Some(m);
                            }
                        }
                        Err(e) => errs.push(format!("{}: {e}", sh.label)),
                    }
                }
                first.ok_or_else(|| "fio 1 MiB QD8 did not run".to_owned())
            };
            gates_scan::scan_using(ctx, opts, name, &mut ladder)
        };
        sec.anomalies.extend(errs);
        let base = got
            .iter()
            .find(|g| g.0 == 0)
            .map(|g| med(&g.1))
            .unwrap_or(f64::NAN);
        let ours = point
            .metrics
            .iter()
            .find(|(k, _)| *k == "mb_per_s")
            .and_then(|(_, v)| *v);
        if let Some(e) = &point.error {
            sec.anomalies.push(format!("{name}: {e}"));
        }
        let notes = point.notes.join("; ");
        t.row(vec![
            name.clone(),
            "store-io".to_owned(),
            ours.map_or("-".to_owned(), |v| format!("{v:.0}")),
            notes,
            "-".to_owned(),
            ours.map_or("-".to_owned(), |v| format!("{:.1}", v / base * 100.0)),
        ]);
        for (i, mbs, cpu) in &got {
            let m = med(mbs);
            t.row(vec![
                name.clone(),
                SHAPES[*i].label.to_owned(),
                format!("{m:.0}"),
                format!("{mbs:.0?}"),
                format!("{:.3}", med(cpu)),
                format!("{:.1}", m / base * 100.0),
            ]);
        }
        sec.points.push(point);
    }
    sec.tables.push(t);
    sec
}

fn med(v: &[f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut v = v.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}
