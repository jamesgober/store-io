//! The workloads, and the machinery they share: the run context, the
//! time-boxed single-thread loop, the time-boxed multi-thread runner, and
//! per-point snapshots of store-io's flush counters and the OS I/O counters.

pub mod caller_batch;
pub mod concurrent;
pub mod lone;
pub mod page_batch;
pub mod sequential;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use store_io_engine::domain::DomainStats;
use store_io_engine::{AppendRegion, StoreOptions};

use crate::data::Pool;
use crate::raw::{IoCounters, io_counters};
use crate::report::{Point, Verify};
use crate::sio::{self, Plat, SioStore};
use crate::stats::Samples;

/// Payload size of the 4 KiB workloads.
pub const KIB4: usize = 4096;

/// Everything a workload needs.
pub struct Ctx {
    /// Directory every store and file of the run lives under.
    pub base: PathBuf,
    /// Measured seconds per point.
    pub secs: f64,
    /// Warm-up seconds per point (not measured).
    pub warmup: f64,
    /// Options for every store.
    pub opts: StoreOptions,
    /// Payload source.
    pub pool: Pool,
    /// The fsys handle (built once).
    pub fsys: fsys::Handle,
    seq: AtomicU64,
}

impl Ctx {
    /// A context.
    pub fn new(base: PathBuf, secs: f64, opts: StoreOptions, fsys: fsys::Handle) -> Self {
        Self {
            base,
            secs,
            warmup: (secs * 0.1).clamp(0.2, 1.0),
            opts,
            pool: Pool::new(),
            fsys,
            seq: AtomicU64::new(0),
        }
    }

    /// A fresh, unique path under the base directory.
    pub fn path(&self, name: &str) -> PathBuf {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        self.base.join(format!("{name}-{n}"))
    }

    /// Creates a fresh store in its own directory.
    ///
    /// # Errors
    ///
    /// The store-io error, formatted.
    pub fn store(&self, name: &str) -> Result<(SioStore, PathBuf), String> {
        let dir = self.path(name);
        let s = sio::create(&dir, &self.opts)?;
        Ok((s, dir))
    }

    /// Total seconds one point occupies (warm-up plus measurement).
    #[must_use]
    pub fn point_secs(&self) -> f64 {
        self.secs + self.warmup
    }
}

/// Removes a file or directory tree, reporting failure as text.
pub fn remove(path: &Path) -> Option<String> {
    let r = if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match r {
        Ok(()) => None,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => Some(format!("could not remove {}: {e}", path.display())),
    }
}

/// Formats an I/O error with context.
pub fn ioerr(what: &str, e: &std::io::Error) -> String {
    format!("{what}: {e}")
}

/// Formats a store-io error with context (Display and Debug).
pub fn sioerr(what: &str, e: &store_io_core::Error) -> String {
    format!("{what}: {e} [{e:?}]")
}

/// Formats an fsys error with context.
pub fn fserr(what: &str, e: &fsys::Error) -> String {
    format!("{what}: {e}")
}

/// One step of a time-boxed loop.
pub enum Step {
    /// One operation completed, taking this long (the timed part only).
    Op(Duration),
    /// The target is full: end the point early (reported as a capacity
    /// stop, never as an error).
    Full,
}

/// Counter snapshot at one instant.
#[derive(Debug, Clone, Copy, Default)]
pub struct Snap {
    /// store-io flush-domain counters, for store-io points.
    pub domain: Option<DomainStats>,
    /// OS per-process I/O counters.
    pub io: Option<IoCounters>,
}

impl Snap {
    /// OS counters only.
    #[must_use]
    pub fn os() -> Self {
        Self {
            domain: None,
            io: io_counters(),
        }
    }

    /// OS counters and the store's flush-domain counters.
    #[must_use]
    pub fn store(s: &SioStore) -> Self {
        Self {
            domain: Some(s.domain_stats()),
            io: io_counters(),
        }
    }
}

/// Counter deltas over a measured window.
#[derive(Debug, Clone, Copy, Default)]
pub struct Delta {
    /// Flushes store-io issued.
    pub flushes: Option<u64>,
    /// Barriers that led a flush.
    pub led: Option<u64>,
    /// Barriers satisfied by another barrier's flush.
    pub joined: Option<u64>,
    /// OS counter deltas.
    pub io: Option<IoCounters>,
}

impl Delta {
    /// `after - before`.
    #[must_use]
    pub fn between(before: &Snap, after: &Snap) -> Self {
        let d = match (before.domain, after.domain) {
            (Some(b), Some(a)) => Some((
                a.flushes.saturating_sub(b.flushes),
                a.led.saturating_sub(b.led),
                a.joined.saturating_sub(b.joined),
            )),
            _ => None,
        };
        Self {
            flushes: d.map(|x| x.0),
            led: d.map(|x| x.1),
            joined: d.map(|x| x.2),
            io: match (before.io, after.io) {
                (Some(b), Some(a)) => Some(a.since(&b)),
                _ => None,
            },
        }
    }

    /// `x / ops`, when both exist and `ops > 0`.
    #[must_use]
    pub fn per(x: Option<u64>, ops: u64) -> Option<f64> {
        let x = x?;
        (ops > 0).then(|| x as f64 / ops as f64)
    }
}

/// The result of a single-thread time-boxed loop.
pub struct Run {
    /// Latency of every measured operation.
    pub lat: Samples,
    /// Measured operations.
    pub ops: u64,
    /// Operations including warm-up.
    pub total_ops: u64,
    /// Measured window: from the first measured operation's start to the
    /// last one's end.
    pub window: Duration,
    /// Whether the point ended early at capacity.
    pub capacity_stop: bool,
    /// Whether the point ended at its operation cap.
    pub op_cap_stop: bool,
    /// Counter deltas over the measured window.
    pub delta: Delta,
}

impl Run {
    /// Operations per second of time spent inside the measured operations
    /// (QD1, back to back: payload generation and checks between operations
    /// are excluded; the wall-clock window is recorded separately).
    #[must_use]
    pub fn rate(&self) -> Option<f64> {
        let s = self.lat.total().as_secs_f64();
        (self.ops > 0 && s > 0.0).then(|| self.ops as f64 / s)
    }
}

/// Runs `op(index)` repeatedly on this thread: first for the warm-up period
/// (not measured), then for the measured period. Indices are consecutive
/// from 0 across both phases, so every payload is unique. `op` times its own
/// I/O and returns that duration (payload preparation stays outside).
///
/// # Errors
///
/// The first error `op` returns, or "no measured operations" when capacity
/// ran out during warm-up.
pub fn time_boxed(
    ctx: &Ctx,
    snap: &dyn Fn() -> Snap,
    op: impl FnMut(u64) -> Result<Step, String>,
) -> Result<Run, String> {
    time_boxed_capped(ctx, u64::MAX, snap, op)
}

/// As [`time_boxed`], but the measured phase also ends after `max_ops`
/// operations (recorded on the run as `op_cap_stop`).
///
/// # Errors
///
/// As [`time_boxed`].
pub fn time_boxed_capped(
    ctx: &Ctx,
    max_ops: u64,
    snap: &dyn Fn() -> Snap,
    mut op: impl FnMut(u64) -> Result<Step, String>,
) -> Result<Run, String> {
    let mut i = 0u64;
    let mut capacity_stop = false;
    let warm_end = Instant::now() + Duration::from_secs_f64(ctx.warmup);
    while Instant::now() < warm_end {
        match op(i)? {
            Step::Op(_) => i += 1,
            Step::Full => {
                capacity_stop = true;
                break;
            }
        }
    }
    let before = snap();
    let start = Instant::now();
    let end = start + Duration::from_secs_f64(ctx.secs);
    let mut lat = Samples::with_capacity(1 << 14);
    let mut last = start;
    let warm_ops = i;
    let mut op_cap_stop = false;
    while !capacity_stop {
        if i - warm_ops >= max_ops {
            op_cap_stop = true;
            break;
        }
        match op(i)? {
            Step::Op(d) => {
                lat.push(d);
                i += 1;
                last = Instant::now();
                if last >= end {
                    break;
                }
            }
            Step::Full => capacity_stop = true,
        }
    }
    let after = snap();
    let ops = i - warm_ops;
    if ops == 0 {
        return Err("no measured operations (capacity exhausted during warm-up)".to_owned());
    }
    Ok(Run {
        lat,
        ops,
        total_ops: i,
        window: last - start,
        capacity_stop,
        op_cap_stop,
        delta: Delta::between(&before, &after),
    })
}

/// The result of a multi-thread time-boxed run.
pub struct Threaded<S> {
    /// Each thread's final state, in thread order.
    pub states: Vec<S>,
    /// Latency of every measured operation, all threads.
    pub lat: Samples,
    /// Operations that started and finished inside the window.
    pub ops: u64,
    /// Operations completed by all threads, including warm-up and the ones
    /// straddling the window edges.
    pub total_ops: u64,
    /// The measured window.
    pub window: Duration,
    /// Whether some thread ran out of capacity (the window is then cut at
    /// the first such stop).
    pub capacity_stop: bool,
    /// Errors, one per failing thread.
    pub errors: Vec<String>,
    /// Counter deltas over the window.
    pub delta: Delta,
}

impl<S> Threaded<S> {
    /// Operations per second over the window.
    #[must_use]
    pub fn rate(&self) -> Option<f64> {
        let s = self.window.as_secs_f64();
        (self.ops > 0 && s > 0.0).then(|| self.ops as f64 / s)
    }
}

const WARM: u8 = 0;
const MEASURE: u8 = 1;
const STOP: u8 = 2;

/// Runs `op(state, thread, index)` on `n` threads for the warm-up period and
/// then the measured period. An operation counts when it started at or after
/// the window opened and finished at or before it closed; the window closes
/// at the deadline or at the first capacity stop or error, whichever is
/// first. `op` times its own I/O and returns that duration.
pub fn threaded<S: Send>(
    ctx: &Ctx,
    n: usize,
    snap: &(dyn Fn() -> Snap + Sync),
    init: impl Fn(usize) -> S + Sync,
    op: impl Fn(&mut S, usize, u64) -> Result<Step, String> + Sync,
) -> Threaded<S> {
    let phase = AtomicU8::new(WARM);
    let base = Instant::now();
    let ns = |d: Duration| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
    struct Out<S> {
        state: S,
        recs: Vec<(u64, u64, u64)>,
        count: u64,
        stopped_at: Option<u64>,
        full: bool,
        error: Option<String>,
    }
    let (outs, t0, t1, before, after) = std::thread::scope(|sc| {
        let (phase, init, op) = (&phase, &init, &op);
        let handles: Vec<_> = (0..n)
            .map(|t| {
                sc.spawn(move || {
                    let mut state = init(t);
                    let mut recs = Vec::with_capacity(1 << 12);
                    let mut i = 0u64;
                    let mut out_full = false;
                    let mut error = None;
                    let mut stopped_at = None;
                    while phase.load(Ordering::Acquire) != STOP {
                        let s = ns(base.elapsed());
                        match op(&mut state, t, i) {
                            Ok(Step::Op(d)) => {
                                let e = ns(base.elapsed());
                                recs.push((s, e, ns(d)));
                                i += 1;
                            }
                            Ok(Step::Full) => {
                                out_full = true;
                                stopped_at = Some(ns(base.elapsed()));
                                break;
                            }
                            Err(x) => {
                                error = Some(x);
                                stopped_at = Some(ns(base.elapsed()));
                                break;
                            }
                        }
                    }
                    Out {
                        state,
                        recs,
                        count: i,
                        stopped_at,
                        full: out_full,
                        error,
                    }
                })
            })
            .collect();
        std::thread::sleep(Duration::from_secs_f64(ctx.warmup));
        let before = snap();
        let t0 = ns(base.elapsed());
        phase.store(MEASURE, Ordering::Release);
        std::thread::sleep(Duration::from_secs_f64(ctx.secs));
        let t1 = ns(base.elapsed());
        let after = snap();
        phase.store(STOP, Ordering::Release);
        let outs: Vec<Out<S>> = handles
            .into_iter()
            .map(|h| match h.join() {
                Ok(o) => o,
                Err(_) => panic!("a workload thread panicked"),
            })
            .collect();
        (outs, t0, t1, before, after)
    });
    let cut = outs
        .iter()
        .filter_map(|o| o.stopped_at)
        .min()
        .map_or(t1, |s| s.min(t1));
    let mut lat = Samples::with_capacity(outs.iter().map(|o| o.recs.len()).sum());
    let mut ops = 0u64;
    let mut total_ops = 0u64;
    let mut errors = Vec::new();
    let mut capacity_stop = false;
    let mut states = Vec::with_capacity(n);
    for o in outs {
        for &(s, e, d) in &o.recs {
            if s >= t0 && e <= cut {
                lat.push(Duration::from_nanos(d));
                ops += 1;
            }
        }
        total_ops += o.count;
        capacity_stop |= o.full;
        if let Some(e) = o.error {
            errors.push(e);
        }
        states.push(o.state);
    }
    Threaded {
        states,
        lat,
        ops,
        total_ops,
        window: Duration::from_nanos(cut.saturating_sub(t0)),
        capacity_stop,
        errors,
        delta: Delta::between(&before, &after),
    }
}

/// The capacity, in bytes, an append region needs for `bytes_per_op` at up
/// to `max_rate` operations per second for a whole point, clamped to
/// `[64 MiB, cap]` and rounded to 1 MiB. A point that outruns it ends early
/// and says so.
#[must_use]
pub fn region_bytes(ctx: &Ctx, bytes_per_op: u64, max_rate: f64, cap: u64) -> u64 {
    let want = (ctx.point_secs() * max_rate) as u64 * bytes_per_op;
    want.clamp(64 << 20, cap).div_ceil(1 << 20) * (1 << 20)
}

/// Records a single-thread run's rate (under `rate_name`), latency summary,
/// and per-operation counter deltas on `p`.
pub fn record_run(p: &mut Point, run: &Run, rate_name: &'static str) {
    p.metric(rate_name, run.rate());
    p.latency(run.lat.summary().as_ref());
    record_delta(p, &run.delta, run.ops);
    p.metric("window_s", Some(run.window.as_secs_f64()));
    p.metric("ops_total_incl_warmup", Some(run.total_ops as f64));
    if run.capacity_stop {
        p.notes.push(format!(
            "ended early at region capacity: {} measured ops in {:.2} s",
            run.ops,
            run.window.as_secs_f64()
        ));
    }
    if run.op_cap_stop {
        p.notes.push(format!(
            "ended at the {}-operation cap after {:.2} s",
            run.ops,
            run.window.as_secs_f64()
        ));
    }
}

/// Records per-operation counter deltas on `p`.
pub fn record_delta(p: &mut Point, d: &Delta, ops: u64) {
    p.metric("flushes_per_op", Delta::per(d.flushes, ops));
    p.metric(
        "write_calls_per_op",
        Delta::per(d.io.map(|x| x.write_calls), ops),
    );
    p.metric(
        "write_bytes_per_op",
        Delta::per(d.io.map(|x| x.write_bytes), ops),
    );
    p.metric(
        "other_calls_per_op",
        Delta::per(d.io.and_then(|x| x.other_calls), ops),
    );
}

/// Creates a store named `name`, runs `f` on it, records any error on the
/// point, then drops the store and deletes its directory.
pub fn with_store(
    ctx: &Ctx,
    name: &str,
    mut p: Point,
    f: impl FnOnce(&SioStore, &mut Point) -> Result<(), String>,
) -> Point {
    match ctx.store(name) {
        Ok((store, dir)) => {
            if let Err(e) = f(&store, &mut p) {
                p.error = Some(e);
            }
            drop(store);
            if let Some(e) = remove(&dir) {
                p.notes.push(e);
            }
        }
        Err(e) => p.error = Some(e),
    }
    p
}

/// Provisions an append region and notes how long provisioning took.
///
/// # Errors
///
/// The store-io error, formatted.
pub fn provision_append(
    store: &SioStore,
    name: &str,
    bytes: u64,
    p: &mut Point,
) -> Result<AppendRegion<Plat>, String> {
    let t = Instant::now();
    let r = store
        .provision_append_region(name, bytes)
        .map_err(|e| sioerr("provision_append_region", &e))?;
    let s = t.elapsed().as_secs_f64();
    p.notes.push(format!(
        "provisioned a {} MiB append region in {s:.2} s ({:.0} MB/s: fill, flush, verify)",
        bytes >> 20,
        bytes as f64 / s / 1e6
    ));
    p.metric("provision_mb_per_s", Some(bytes as f64 / s / 1e6));
    Ok(r)
}

/// One appended record: where store-io put it and which payload it is.
#[derive(Debug, Clone, Copy)]
pub struct Rec {
    /// Region offset returned by store-io.
    pub off: u64,
    /// Payload stream.
    pub stream: u64,
    /// Payload index.
    pub index: u64,
    /// Payload length.
    pub len: usize,
    /// Whether the record must start on a block boundary (a lone append),
    /// or is packed inside a batch.
    pub aligned: bool,
}

/// Indices of at most `limit` evenly spaced items of `n`, always including
/// the last.
#[must_use]
pub fn sample(n: usize, limit: usize) -> Vec<usize> {
    if n <= limit {
        return (0..n).collect();
    }
    let mut v: Vec<usize> = (0..limit).map(|k| k * n / limit).collect();
    if v.last() != Some(&(n - 1)) {
        v.push(n - 1);
    }
    v
}

/// Checks appended records: no two overlap (all of them, in memory), every
/// record that must start on a block does, and a sample of at most `limit`
/// reads back byte for byte through `AppendRegion::read` (packed records are
/// read from the block boundary below them).
#[must_use]
pub fn verify_appends(wal: &AppendRegion<Plat>, pool: &Pool, recs: &[Rec], limit: usize) -> Verify {
    let block = wal.block_size();
    let mut spans: Vec<(u64, u64)> = recs.iter().map(|r| (r.off, r.off + r.len as u64)).collect();
    spans.sort_unstable();
    for w in spans.windows(2) {
        if w[1].0 < w[0].1 {
            return Verify::Failed(format!(
                "records overlap: [{}, {}) and [{}, {})",
                w[0].0, w[0].1, w[1].0, w[1].1
            ));
        }
    }
    if let Some(r) = recs.iter().find(|r| r.aligned && r.off % block != 0) {
        return Verify::Failed(format!("append at {} is not block aligned", r.off));
    }
    let idx = sample(recs.len(), limit);
    let mut out = Vec::new();
    for &k in &idx {
        let r = recs[k];
        let base = r.off - r.off % block;
        let skip = (r.off - base) as usize;
        out.resize(skip + r.len, 0);
        match wal.read(base, &mut out) {
            Ok(n) if n == out.len() => {}
            Ok(n) => {
                return Verify::Failed(format!("short read at {base}: {n} of {} bytes", out.len()));
            }
            Err(e) => return Verify::Failed(sioerr("AppendRegion::read", &e)),
        }
        if !pool.matches(r.stream, r.index, &out[skip..]) {
            return Verify::Failed(format!(
                "record (stream {}, index {}) at offset {} reads back different bytes",
                r.stream, r.index, r.off
            ));
        }
    }
    Verify::Passed(format!(
        "{} of {} records read back and compared byte for byte; all {} checked for overlap and alignment",
        idx.len(),
        recs.len(),
        recs.len()
    ))
}

/// The usual latency cells: p50, p90, p99, p99.9, max, mean.
#[must_use]
pub fn lat_cells(p: &Point) -> Vec<String> {
    ["p50_us", "p90_us", "p99_us", "p999_us", "max_us", "mean_us"]
        .iter()
        .map(|k| p.cell(k, 1))
        .collect()
}
