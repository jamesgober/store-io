//! `observe-cost`: what the observer adds to one durable write.
//!
//! A durable write runs three spans (write, barrier, flush) and four counter
//! adds. On a real device the write itself takes microseconds, so the
//! observer's share cannot be seen end to end; this times exactly that hook
//! sequence in a tight loop, minus the cost of the empty loop, on 1, 4, 16
//! and 17 threads started together. The observer has one block per CPU
//! (rounded up to a power of two, at least 16), so on a machine with 17 or
//! more CPUs these threads do not share blocks and the rise with thread count
//! is the CPU's lower all-core clock; with fewer CPUs, 17 threads share. A
//! difference below the empty loop's cost (noise) is reported as 0.
//!
//! Each figure is the median of three runs. Build with `--features observe`
//! to measure the observer, and without it to see the compiled-out cost
//! (ROADMAP v0.4 gate: at most 20 ns per operation on, 0 off).

use std::hint::black_box;
use std::sync::Barrier;
use std::time::Instant;

use store_io_core::observe::{Counter, Observer, Stage, enabled};

/// Iterations per thread and run.
const N: u64 = 10_000_000;

/// Runs per figure; the median is reported.
const RUNS: usize = 3;

/// The observer work of one durable write.
#[inline(never)]
fn one_durable_write(o: &Observer) {
    let _write = o.span(Stage::Write);
    o.add(Counter::Writes, 1);
    o.add(Counter::WriteBytes, 4096);
    let _barrier = o.span(Stage::Barrier);
    o.add(Counter::Barriers, 1);
    let _flush = o.span(Stage::Flush);
    o.add(Counter::Flushes, 1);
}

/// The empty loop body, kept out of line like the measured one so the call
/// itself is not counted as observer cost.
#[inline(never)]
fn empty(o: &Observer) {
    black_box(o);
}

/// Nanoseconds per iteration of `f`, after a warm-up of a tenth.
fn per_op(f: impl Fn()) -> f64 {
    for _ in 0..N / 10 {
        f();
    }
    let t = Instant::now();
    for _ in 0..N {
        f();
    }
    t.elapsed().as_nanos() as f64 / N as f64
}

/// The worst per-thread cost of `threads` threads started together, or
/// `None` if a worker panicked.
fn worst_of(o: &Observer, threads: usize, f: fn(&Observer)) -> Option<f64> {
    let start = Barrier::new(threads);
    std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                let start = &start;
                s.spawn(move || {
                    start.wait();
                    per_op(|| f(black_box(o)))
                })
            })
            .collect();
        let mut worst = 0.0f64;
        for w in workers {
            worst = worst.max(w.join().ok()?);
        }
        Some(worst)
    })
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// Runs the measurement and returns the report lines.
#[must_use]
pub fn run() -> String {
    let o = Observer::new();
    let mut lines = vec![format!(
        "observer compiled in: {}\nper durable write (3 spans, 4 adds), empty loop subtracted, median of {RUNS} runs [measured, {N} iterations per thread]:",
        enabled()
    )];
    for threads in [1usize, 4, 16, 17] {
        let mut costs = Vec::new();
        let mut failed = false;
        for _ in 0..RUNS {
            let (hooks, base) = (
                worst_of(&o, threads, one_durable_write),
                worst_of(&o, threads, empty),
            );
            match (hooks, base) {
                (Some(h), Some(b)) => costs.push((h - b).max(0.0)),
                _ => failed = true,
            }
        }
        lines.push(if failed {
            format!("- {threads:>2} thread(s): a worker panicked; no figure")
        } else {
            format!(
                "- {threads:>2} thread(s): worst {:.2} ns per thread",
                median(costs)
            )
        });
    }
    let expected: u64 = if enabled() {
        (N + N / 10) * (1 + 4 + 16 + 17) * RUNS as u64
    } else {
        0
    };
    lines.push(format!(
        "writes counted: {} (expected {expected})\ngate: at most 20 ns per operation with the observer on; 0 off",
        o.snapshot().counter(Counter::Writes)
    ));
    lines.join("\n") + "\n"
}
