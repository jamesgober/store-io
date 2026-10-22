//! G15's measurement: durable appends from one and from eight writers on one
//! shared region, alternated three times, then fio's eight-writer controls
//! three times (after all the rounds, so neither store-io point follows
//! fio's load).
//!
//! Why the controls: G15 asks that flush sharing lift eight writers to 1.5x
//! one. That needs a device whose writes alone go faster than 1.5x one
//! writer's durable rate. On a memory-backed device with a cheap flush the
//! write path itself can be the ceiling, and no sharing can reach the gate.
//! fio's eight synchronous 4 KiB writers with no flush show that ceiling;
//! the same writers flushing after every write show what eight writers get
//! without sharing.

use store_io_engine::StoreOptions;

use super::gates::{Row, Verdict, f, med};
use super::{Ctx, concurrent};
use crate::fio;
use crate::report::Point;

/// Rounds of one writer then eight, and then of the two fio controls.
const ROUNDS: usize = 3;

/// Flushes in flight measured beside the run's own setting, at eight
/// writers (D-124: the default is chosen from these).
pub const SWEEP: [u8; 3] = [1, 2, 4];

/// The rounds' results, one entry per round that produced a value.
pub struct G15 {
    /// Durable appends per second from one writer.
    pub one: Vec<f64>,
    /// Durable appends per second from eight writers.
    pub eight: Vec<f64>,
    /// Flushes per append at eight writers.
    pub flushes_at_eight: Vec<f64>,
    /// fio, eight synchronous writers, no flush: writes per second.
    pub fio_writes: Vec<f64>,
    /// fio, eight synchronous writers, a data flush after every write.
    pub fio_durable: Vec<f64>,
    /// fio's failures, as text.
    pub fio_errors: Vec<String>,
    /// Eight writers at each of [`SWEEP`]'s flushes in flight: the setting
    /// asked for, the most the store actually ran in any round, and durable
    /// appends per second, one entry per round.
    pub sweep: Vec<(u8, u8, Vec<f64>)>,
    /// The most flushes in flight the eight-writer runs actually used.
    pub ran_at_eight: u8,
    /// store-io's points, in the order measured.
    pub points: Vec<Point>,
}

/// Runs the rounds. `job` runs one fio job with the given options on the
/// fio controls' file and returns its write side.
pub fn measure(
    ctx: &Ctx,
    job: &mut dyn FnMut(&[&str]) -> Result<fio::Stats, String>,
    val: impl Fn(&Point, &str) -> Option<f64>,
) -> G15 {
    let sync = format!("--ioengine={}", fio::sync_engine());
    let eight = [
        sync.as_str(),
        "--rw=randwrite",
        "--bs=4k",
        "--norandommap",
        "--numjobs=8",
        "--thread",
        "--group_reporting",
    ];
    let mut flushing = eight.to_vec();
    flushing.push("--fdatasync=1");
    let mut g = G15 {
        one: Vec::new(),
        eight: Vec::new(),
        flushes_at_eight: Vec::new(),
        fio_writes: Vec::new(),
        fio_durable: Vec::new(),
        fio_errors: Vec::new(),
        sweep: SWEEP.iter().map(|&f| (f, 0, Vec::new())).collect(),
        ran_at_eight: 0,
        points: Vec::new(),
    };
    for _ in 0..ROUNDS {
        let p1 = concurrent::store_point(ctx, 1);
        let p8 = concurrent::store_point(ctx, 8);
        g.one.extend(val(&p1, "ops_per_s"));
        g.eight.extend(val(&p8, "ops_per_s"));
        g.flushes_at_eight.extend(val(&p8, "flushes_per_op"));
        if let Some(r) = val(&p8, "flushes_run") {
            g.ran_at_eight = g.ran_at_eight.max(r as u8);
        }
        g.points.extend([p1, p8]);
    }
    // The sweep, alternated round by round like the points above.
    for _ in 0..ROUNDS {
        for (f, ran, into) in &mut g.sweep {
            let opts = StoreOptions {
                flushes_in_flight: *f,
                ..ctx.opts.clone()
            };
            let p = concurrent::store_point_with(ctx, 8, &opts);
            into.extend(val(&p, "ops_per_s"));
            if let Some(r) = val(&p, "flushes_run") {
                *ran = (*ran).max(r as u8);
            }
            g.points.push(p);
        }
    }
    for _ in 0..ROUNDS {
        for (args, into) in [
            (&eight[..], &mut g.fio_writes),
            (&flushing[..], &mut g.fio_durable),
        ] {
            match job(args) {
                Ok(s) => into.push(s.iops),
                Err(e) => g.fio_errors.push(format!("G15 fio control: {e}")),
            }
        }
    }
    g
}

impl G15 {
    /// Whether every round produced every store-io value.
    #[must_use]
    pub fn whole(&self) -> bool {
        self.one.len() == ROUNDS
            && self.eight.len() == ROUNDS
            && self.flushes_at_eight.len() == ROUNDS
    }
}

impl G15 {
    /// The gate's row: the medians, the flushes in flight each run used, and
    /// the verdict.
    pub(super) fn row(&self) -> Row {
        let (one, eight) = (&self.one, &self.eight);
        let (o1, o8, f8) = (med(one), med(eight), med(&self.flushes_at_eight));
        let sweep = self
            .sweep
            .iter()
            .map(|(n, ran, v)| match (med(v), *ran) {
                (None, _) => format!("{n}: no run"),
                (m, r) if r == *n => format!("{n}: {} /s", f(m, 0)),
                (m, r) => format!("{n} (ran {r}): {} /s", f(m, 0)),
            })
            .collect::<Vec<_>>()
            .join(", ");
        Row {
            id: "G15",
            gate: "Concurrent writers: durable ops/s rises with writers through flush sharing (8 writers at least 1.5x one, under 0.9 flushes per op; medians of 3 alternated runs)",
            measured: format!(
                "1 writer {one:.0?} /s, 8 writers {eight:.0?} /s, {} flushes per op at 8 ({} in flight); 8 writers by flushes in flight: {sweep}; fio 8 writers ({}): writes alone {:.0?} /s, each flushed {:.0?} /s",
                f(f8, 2),
                self.ran_at_eight,
                fio::sync_engine(),
                self.fio_writes,
                self.fio_durable
            ),
            verdict: match (self.whole(), o1, o8, f8) {
                (true, Some(a), Some(b), Some(x)) if b >= 1.5 * a && x < 0.9 => Verdict::Pass,
                // Sharing flushes, and 1.5x one writer lies within the range
                // of the eight-writer runs: this disk cannot tell.
                (true, Some(a), Some(_), Some(x))
                    if x < 0.9 && eight.iter().copied().fold(f64::MIN, f64::max) >= 1.5 * a =>
                {
                    Verdict::NotResolvable
                }
                _ => Verdict::Fail,
            },
        }
    }
}
