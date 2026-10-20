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

use super::{Ctx, concurrent};
use crate::fio;
use crate::report::Point;

/// Rounds of one writer then eight, and then of the two fio controls.
const ROUNDS: usize = 3;

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
        points: Vec::new(),
    };
    for _ in 0..ROUNDS {
        let p1 = concurrent::store_point(ctx, 1);
        let p8 = concurrent::store_point(ctx, 8);
        g.one.extend(val(&p1, "ops_per_s"));
        g.eight.extend(val(&p8, "ops_per_s"));
        g.flushes_at_eight.extend(val(&p8, "flushes_per_op"));
        g.points.extend([p1, p8]);
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
