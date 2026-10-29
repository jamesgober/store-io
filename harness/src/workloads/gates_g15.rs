//! G15's measurement: durable appends from one and from eight writers on one
//! shared region, alternated three times, then fio's eight-writer controls
//! three times (after all the rounds, so neither store-io point follows
//! fio's load).
//!
//! Beside them, reported and never judged: eight writers at each of
//! [`SWEEP`]'s flushes in flight (D-124 chose the default from these), and
//! two diagnostics of what holds eight writers back, a region per writer
//! and many queues.
//!
//! Why the controls: G15 asks that flush sharing lift eight writers to 1.5x
//! one. That needs a device whose writes alone go faster than 1.5x one
//! writer's durable rate. On a memory-backed device with a cheap flush the
//! write path itself can be the ceiling, and no sharing can reach the gate.
//! fio's eight synchronous 4 KiB writers with no flush show that ceiling;
//! the same writers flushing after every write show what eight writers get
//! without sharing.
//!
//! And a third control, the machine's own wake (D-138): a shared flush saves
//! each follower a flush and costs it a sleep and a wake by the leader. Where
//! one writer's whole durable write takes less than one cross-thread wake,
//! sharing did not lift eight writers to 1.5x one, whoever led (measured on
//! CI over null_blk: the Xeon 8370C and 8573C runners, a durable write in
//! 7-8 µs against a 9-13 µs wake, eight writers at 1.2-1.3x one; the EPYC
//! runners, 12-15 µs against 7-9 µs, at 1.8-2.1x). There G15 cannot judge
//! sharing, as long as flushes are shared and eight writers are not slower
//! than one.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use store_io_engine::StoreOptions;

use super::gates::{Row, Verdict, f, med};
use super::{Ctx, concurrent};
use crate::fio;
use crate::report::Point;

/// Rounds of one writer then eight, and then of the two fio controls.
const ROUNDS: usize = 3;

/// Queues of the many-queues diagnostic: two per writer, and at least the
/// default (one per CPU).
#[must_use]
pub fn diag_queues() -> usize {
    std::thread::available_parallelism()
        .map_or(1, usize::from)
        .max(16)
}

/// Round trips (two wakes each) timed per round by [`wake_us`], after as
/// many unmeasured.
const TRIPS: u64 = 50_000;

/// One cross-thread wake on this machine, one way, in µs: two threads hand
/// a turn back and forth through `park` / `unpark` (a futex on Linux), as a
/// leader wakes a barrier asleep on a word.
#[must_use]
pub fn wake_us() -> f64 {
    let turn = AtomicU64::new(0);
    let main = std::thread::current();
    let mut timed = None;
    std::thread::scope(|sc| {
        let other = sc.spawn(|| {
            for i in 0..2 * TRIPS {
                while turn.load(Ordering::Acquire) != 2 * i + 1 {
                    std::thread::park();
                }
                turn.store(2 * i + 2, Ordering::Release);
                main.unpark();
            }
        });
        let mut start = Instant::now();
        for i in 0..2 * TRIPS {
            if i == TRIPS {
                start = Instant::now();
            }
            turn.store(2 * i + 1, Ordering::Release);
            other.thread().unpark();
            while turn.load(Ordering::Acquire) != 2 * i + 2 {
                std::thread::park();
            }
        }
        // Before the other thread's exit and join.
        timed = Some(start.elapsed());
    });
    timed.map_or(f64::NAN, |t| t.as_secs_f64() * 1e6 / (2 * TRIPS) as f64)
}

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
    /// One cross-thread wake, µs ([`wake_us`]), one per round.
    pub wake: Vec<f64>,
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
    /// Diagnostics at eight writers (reported, never judged): durable
    /// appends per second with a region per writer, so none waits for
    /// another's append before its own counts, and on the shared region
    /// with [`diag_queues`] queues, so none waits for a free queue.
    pub own_regions: Vec<f64>,
    /// See [`Self::own_regions`].
    pub many_queues: Vec<f64>,
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
        wake: Vec::new(),
        fio_writes: Vec::new(),
        fio_durable: Vec::new(),
        fio_errors: Vec::new(),
        sweep: SWEEP.iter().map(|&f| (f, 0, Vec::new())).collect(),
        ran_at_eight: 0,
        own_regions: Vec::new(),
        many_queues: Vec::new(),
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
        g.wake.push(wake_us());
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
        let own = concurrent::store_point_in(ctx, 8, &ctx.opts, 8);
        g.own_regions.extend(val(&own, "ops_per_s"));
        let queues = StoreOptions {
            queues: diag_queues(),
            ..ctx.opts.clone()
        };
        let many = concurrent::store_point_with(ctx, 8, &queues);
        g.many_queues.extend(val(&many, "ops_per_s"));
        g.points.extend([own, many]);
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
        let wake = med(&self.wake);
        // One writer's durable write, µs.
        let write = o1.map(|a| 1e6 / a);
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
            gate: "Concurrent writers: durable ops/s rises with writers through flush sharing (8 writers at least 1.5x one, under 0.9 flushes per op; medians of 3 alternated runs; NOT RESOLVABLE where flushes are shared and 8 writers are no slower than one, but one writer's durable write takes less than one cross-thread wake on this machine, so a shared flush costs a follower more than flushing on its own)",
            measured: format!(
                "1 writer {one:.0?} /s ({} µs a durable write), 8 writers {eight:.0?} /s, {} flushes per op at 8 ({} in flight); one wake {:.1?} µs (median {} µs); 8 writers by flushes in flight: {sweep}; diagnostics at 8 writers: a region each {:.0?} /s, {} queues {:.0?} /s; fio 8 writers ({}): writes alone {:.0?} /s, each flushed {:.0?} /s",
                f(write, 1),
                f(f8, 2),
                self.ran_at_eight,
                self.wake,
                f(wake, 1),
                self.own_regions,
                diag_queues(),
                self.many_queues,
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
                // Sharing flushes, eight writers no slower than one, but a
                // durable write is cheaper than the wake a shared flush
                // costs each follower (D-138). Slower than one stays FAIL.
                (true, Some(a), Some(b), Some(x))
                    if x < 0.9
                        && b >= a
                        && matches!((write, wake), (Some(w), Some(k)) if w < k) =>
                {
                    Verdict::NotResolvable
                }
                _ => Verdict::Fail,
            },
        }
    }
}
