//! G19 (PLANNING R5.14, D-146): a commit's latency beside background work.
//! Two stores on one device (so pacing per store instead of per device
//! would fail): store C makes commit-class 4 KiB durable writes at QD1
//! (`PageRegion::write_durable`), store B runs **one** background
//! `Submitter` (`IoClass::Background`, depth 8: what `background_unpaced`
//! lets it keep in flight unpaced, so the negative control reaches the
//! shape the device was calibrated against) writing 256 KiB pieces. 12
//! alternated rounds of (idle, loaded, idle): the background writer
//! stopped and drained, then writing, then stopped again; the commits' p99
//! in each. A round counts only if its two idle
//! p99s agree within 1.5x; its ratio is the loaded p99 over their mean.
//!
//! PASS only with all of: at least 6 counted rounds, their median ratio at
//! most 2x, the background writer's rate while loaded at least half the
//! paced shape's rate alone (256 KiB, one in flight: fio's, measured by the
//! device script on CI, `--g19-ref-mbps`; else the same writer measured
//! here at depth 1 with no commits), and the **negative control** failing:
//! the same rounds with `background_pacing(false)` on both stores must have
//! a median ratio above 2x (a gate a wrong pacer also passes measures
//! nothing). Besides, failing the gate whatever the latencies: both stores
//! in one device group of two members, named (not the process-wide
//! fallback), paced as asked, splitting background I/O at 256 KiB (the
//! shape fio's reference measures); every write completing; a sample of the
//! commit pages and of the background pieces reading back as written.
//! NOT RUN without `--g19-dir` (a directory on G19's device: CI's
//! calibrated device, `harness/scripts/g19_device.sh`, or the dev box's
//! NVMe).

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use store_io_core::IoClass;
use store_io_core::error::{Error, NotWrittenCause};
use store_io_engine::completion::Done;
use store_io_engine::{StoreOptions, SubmitterConfig, WriteMode};

use super::gates::{Row, Verdict, f};
use super::gates_g1::median;
use super::stamp::{self, BLOCK};
use super::{Ctx, KIB4, remove, sample, sioerr};
use crate::json::Json;
use crate::report::{Point, Section, System, Table, Verify};
use crate::sio::{self, SioStore};

/// Alternated rounds per configuration.
pub const ROUNDS: usize = 12;
/// Counted rounds a configuration needs.
const COUNTED: usize = 6;
/// A round counts when its idle p99s are within this factor.
const AGREE: f64 = 1.5;
/// The gate: loaded p99 over idle p99, at most.
const LIMIT: f64 = 2.0;
/// The background writer's rate while loaded, as a fraction of the paced
/// shape's alone, at least.
const RATE: f64 = 0.5;
/// The background writer's piece, its operations' size.
const PIECE: usize = 256 << 10;
/// The background submitter's depth (`background_unpaced`'s default).
const DEPTH: u32 = 8;
/// The commit store's page region.
const COMMIT_SPAN: u64 = 64 << 20;
/// The background store's page region, overwritten in a cycle.
const BG_SPAN: u64 = 512 << 20;
/// Payload stream of the commits.
const STREAM: u64 = 19;
/// Records of each side read back at the end.
const CHECKS: usize = 256;
/// How long a stop may take to drain the background writer.
const DRAIN: Duration = Duration::from_secs(30);

const GATE: &str = "Commit-class 4 KiB durable p99 beside one paced background Submitter (two stores, one device) at most 2x idle p99: median of the counted rounds of 12 (idle, loaded, idle), at least 6 counted; background rate at least half the paced shape's alone; pacing off on both stores (the negative control) above 2x";

/// What the background writer is asked to do: `IDLE`, `STOP`, or write
/// with at most that many in flight.
const IDLE: u32 = 0;
const STOP: u32 = u32::MAX;

/// The background writer's controls and counters, shared with its thread.
#[derive(Default)]
struct Bg {
    want: AtomicU32,
    /// Set by the thread when idle with nothing in flight; cleared by the
    /// caller before it asks for idle.
    drained: AtomicBool,
    /// Bytes completed, and nanoseconds spent from a start to the drain
    /// after it.
    bytes: AtomicU64,
    busy_ns: AtomicU64,
    /// Writes refused `Paced` (reaped and retried).
    paced: AtomicU64,
    /// Writes in flight at once, at most, while running.
    deepest: AtomicU64,
    error: Mutex<Option<String>>,
}

impl Bg {
    fn failed(&self) -> Option<String> {
        self.error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Stops the writer and waits until nothing is in flight.
    fn idle(&self) -> Result<(), String> {
        self.drained.store(false, Ordering::SeqCst);
        self.want.store(IDLE, Ordering::SeqCst);
        let until = Instant::now() + DRAIN;
        while !self.drained.load(Ordering::SeqCst) {
            if let Some(e) = self.failed() {
                return Err(e);
            }
            if Instant::now() > until {
                return Err("the background writer did not drain within 30 s".to_owned());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    /// (bytes, seconds) so far, read while idle.
    fn totals(&self) -> (u64, f64) {
        (
            self.bytes.load(Ordering::SeqCst),
            self.busy_ns.load(Ordering::SeqCst) as f64 / 1e9,
        )
    }
}

/// The p99 of `d`, microseconds (NaN when empty).
fn p99_us(d: &mut [Duration]) -> f64 {
    if d.is_empty() {
        return f64::NAN;
    }
    d.sort_unstable();
    let i = (d.len() * 99).div_ceil(100).max(1) - 1;
    d[i].as_secs_f64() * 1e6
}

/// The background writer, on its own thread: 256 KiB plain writes over
/// `BG_SPAN`, each stamped at its blocks' ends with its write number, as
/// many in flight as asked (at most `DEPTH`); a `Paced` refusal waits for a
/// completion (or, with none in flight, briefly) and retries. At `STOP`:
/// drains, makes everything durable, and reads a sample back.
fn background(store: &SioStore, bg: &Bg) -> Result<Verify, String> {
    let pages = store
        .provision_page_region("g19-bg", BG_SPAN)
        .map_err(|e| sioerr("provision_page_region", &e))?;
    let mut sub = store
        .submitter(
            SubmitterConfig::default()
                .with_depth(DEPTH)
                .with_class(IoClass::Background),
        )
        .map_err(|e| sioerr("submitter", &e))?;
    let per = (PIECE / BLOCK) as u64;
    let slots = BG_SPAN / PIECE as u64;
    let mut last = vec![u64::MAX; slots as usize];
    let (mut k, mut live) = (0u64, 0usize);
    let mut started: Option<Instant> = None;
    loop {
        let want = bg.want.load(Ordering::SeqCst);
        if want == IDLE || want == STOP {
            if live > 0 {
                for c in sub.wait(1).map_err(|e| sioerr("wait", &e))? {
                    live -= 1;
                    match c.outcome.map_err(|e| sioerr("background write", &e))? {
                        Done::Written(_) => {
                            let _b = bg.bytes.fetch_add(PIECE as u64, Ordering::SeqCst);
                        }
                        other => return Err(format!("unexpected {other:?}")),
                    }
                }
                continue;
            }
            if let Some(t) = started.take() {
                let ns = u64::try_from(t.elapsed().as_nanos()).unwrap_or(u64::MAX);
                let _b = bg.busy_ns.fetch_add(ns, Ordering::SeqCst);
            }
            if want == STOP {
                break;
            }
            bg.drained.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_micros(200));
            continue;
        }
        let _s = started.get_or_insert_with(Instant::now);
        let cap = want.min(DEPTH) as usize;
        let mut refused = false;
        while live < cap {
            let slot = k % slots;
            let mut b = sub.take_buf(PIECE).map_err(|e| sioerr("take_buf", &e))?;
            stamp::stamp_ends(b.as_mut_slice(), slot * per, k);
            let pos = pages
                .pos(slot * PIECE as u64)
                .map_err(|e| sioerr("PageRegion::pos", &e))?;
            match sub.write(&pages, pos, b, WriteMode::Plain, slot) {
                Ok(()) => {
                    last[slot as usize] = k;
                    k += 1;
                    live += 1;
                }
                Err(r)
                    if matches!(
                        r.error,
                        Error::NotWritten {
                            cause: NotWrittenCause::Paced,
                            ..
                        }
                    ) =>
                {
                    let _p = bg.paced.fetch_add(1, Ordering::SeqCst);
                    refused = true;
                    break;
                }
                Err(r) => return Err(sioerr("background write", &r.error)),
            }
        }
        let _d = bg.deepest.fetch_max(live as u64, Ordering::SeqCst);
        if live > 0 {
            for c in sub.wait(1).map_err(|e| sioerr("wait", &e))? {
                live -= 1;
                match c.outcome.map_err(|e| sioerr("background write", &e))? {
                    Done::Written(_) => {
                        let _b = bg.bytes.fetch_add(PIECE as u64, Ordering::SeqCst);
                    }
                    other => return Err(format!("unexpected {other:?}")),
                }
            }
        } else if refused {
            std::thread::sleep(Duration::from_micros(50));
        }
    }
    sub.barrier_all(u64::MAX)
        .map_err(|r| sioerr("barrier_all", &r.error))?;
    let mut flushed = false;
    while !flushed {
        for c in sub.wait(1).map_err(|e| sioerr("barrier wait", &e))? {
            match c.outcome.map_err(|e| sioerr("barrier", &e))? {
                Done::Flushed if c.tag == u64::MAX => flushed = true,
                other => return Err(format!("unexpected {other:?} (tag {})", c.tag)),
            }
        }
    }
    drop(sub);
    let written: Vec<(u64, u64)> = last
        .iter()
        .enumerate()
        .filter(|(_, i)| **i != u64::MAX)
        .map(|(s, i)| (s as u64, *i))
        .collect();
    let idx = sample(written.len(), CHECKS);
    let mut back = vec![0u8; PIECE];
    for &n in &idx {
        let (slot, i) = written[n];
        let pos = pages
            .pos(slot * PIECE as u64)
            .map_err(|e| sioerr("PageRegion::pos", &e))?;
        let got = pages
            .read(pos, &mut back)
            .map_err(|e| sioerr("PageRegion::read", &e))?;
        if got != PIECE || !stamp::ends_hold(&back, slot * per, i) {
            return Ok(Verify::Failed(format!(
                "background piece {slot} reads back different bytes"
            )));
        }
    }
    Ok(Verify::Passed(format!(
        "{} of {} background pieces read back",
        idx.len(),
        written.len()
    )))
}

/// One configuration's outcome.
struct Config {
    point: Point,
    /// The median ratio of the counted rounds, and how many counted.
    ratio: f64,
    counted: usize,
    /// The background writer's MB/s while loaded.
    mbps: f64,
    /// The paced shape's MB/s alone, measured here (paced run only).
    alone_mbps: Option<f64>,
    /// Commit pages read back, of those written.
    checked: (usize, usize),
    integrity: Result<(), String>,
}

/// Runs the rounds on two fresh stores under `base`, pacing on or off.
fn configuration(ctx: &Ctx, base: &Path, paced: bool) -> Result<Config, String> {
    let opts = StoreOptions {
        background_pacing: paced,
        ..ctx.opts.clone()
    };
    let label = if paced { "paced" } else { "pacing off" };
    let (cdir, bdir) = (base.join("commits"), base.join("background"));
    std::fs::create_dir_all(base).map_err(|e| super::ioerr("creating G19's directory", &e))?;
    let commits = sio::create(&cdir, &opts)?;
    let back = sio::create(&bdir, &opts)?;
    let q = back.qos();
    let group = if q.unknown {
        Err(
            "the device could not be named: the stores are paced by the process, not the device"
                .to_owned(),
        )
    } else if q.members != 2 || commits.qos().members != 2 {
        Err(format!(
            "the two stores are not one device group of two (members {} and {})",
            commits.qos().members,
            q.members
        ))
    } else if q.paced != paced || commits.qos().paced != paced {
        Err(format!("the group's pacing is {}, not {paced}", q.paced))
    } else if q.background_io as usize != PIECE {
        Err(format!(
            "the background store splits at {} bytes, not {PIECE} (the paced shape G19 measures)",
            q.background_io
        ))
    } else {
        Ok(())
    };
    let bg = Bg::default();
    let phase = Duration::from_secs_f64(ctx.secs.max(1.0));
    let mut point = Point::new(
        System::StoreIo,
        format!("G19 commits beside a background Submitter, {label}"),
    )
    .param("pacing", Json::Bool(paced))
    .param("rounds", Json::Num(ROUNDS as f64))
    .param("phase_s", Json::Num(phase.as_secs_f64()));
    let outcome = std::thread::scope(|s| -> Result<Config, String> {
        let worker = s.spawn(|| {
            let r = background(&back, &bg);
            if let Err(e) = &r {
                *bg.error
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e.clone());
            }
            r
        });
        let mut run = || -> Result<Config, String> {
            group.clone()?;
            let pages = commits
                .provision_page_region("g19", COMMIT_SPAN)
                .map_err(|e| sioerr("provision_page_region", &e))?;
            let slots = COMMIT_SPAN / KIB4 as u64;
            let mut last = vec![u64::MAX; slots as usize];
            let mut src = vec![0u8; KIB4];
            let mut i = 0u64;
            let mut burst = |dur: Duration| -> Result<Vec<Duration>, String> {
                let mut d = Vec::new();
                let until = Instant::now() + dur;
                while Instant::now() < until {
                    if let Some(e) = bg.failed() {
                        return Err(e);
                    }
                    let slot = i % slots;
                    ctx.pool.fill(STREAM, i, &mut src);
                    let pos = pages
                        .pos(slot * KIB4 as u64)
                        .map_err(|e| sioerr("PageRegion::pos", &e))?;
                    let t = Instant::now();
                    let _rc = pages
                        .write_durable(pos, &src)
                        .map_err(|e| sioerr("write_durable", &e))?;
                    d.push(t.elapsed());
                    last[slot as usize] = i;
                    i += 1;
                }
                Ok(d)
            };
            bg.idle()?;
            // The paced shape alone (one in flight, no commits): the
            // throughput criterion's reference when fio's is not given.
            let alone_mbps = if paced {
                let (b0, t0) = bg.totals();
                bg.want.store(1, Ordering::SeqCst);
                std::thread::sleep(phase);
                bg.idle()?;
                let (b1, t1) = bg.totals();
                Some((b1 - b0) as f64 / (t1 - t0) / 1e6)
            } else {
                None
            };
            let _warm = burst(Duration::from_secs_f64(ctx.warmup.max(0.5)))?;
            let (b0, t0) = bg.totals();
            let (mut ratios, mut notes) = (Vec::new(), Vec::new());
            let (mut idles, mut loads) = (Vec::new(), Vec::new());
            for _ in 0..ROUNDS {
                let i0 = p99_us(&mut burst(phase)?);
                bg.want.store(DEPTH, Ordering::SeqCst);
                let l = p99_us(&mut burst(phase)?);
                bg.idle()?;
                let i1 = p99_us(&mut burst(phase)?);
                let anchor = (i0 + i1) / 2.0;
                let counts = i0.max(i1) <= AGREE * i0.min(i1);
                if counts {
                    ratios.push(l / anchor);
                }
                idles.push(anchor);
                loads.push(l);
                notes.push(format!(
                    "{i0:.0}/{l:.0}/{i1:.0}{}",
                    if counts { "" } else { " (not counted)" }
                ));
            }
            let (b1, t1) = bg.totals();
            let mbps = (b1 - b0) as f64 / (t1 - t0) / 1e6;
            let counted = ratios.len();
            let mut sorted = ratios.clone();
            let ratio = median(&mut sorted);
            point.notes.push(format!(
                "per round commit p99 (us), idle/loaded/idle: {}",
                notes.join(", ")
            ));
            point.metric("ratio", Some(ratio));
            point.metric("counted", Some(counted as f64));
            point.metric("ratio_lo", sorted.first().copied());
            point.metric("ratio_hi", sorted.last().copied());
            point.metric("idle_p99_us", Some(median(&mut idles)));
            point.metric("loaded_p99_us", Some(median(&mut loads)));
            point.metric("background_mb_per_s", Some(mbps));
            point.metric("alone_mb_per_s", alone_mbps);
            point.metric(
                "paced_refusals",
                Some(bg.paced.load(Ordering::SeqCst) as f64),
            );
            point.metric("deepest", Some(bg.deepest.load(Ordering::SeqCst) as f64));
            // The commit pages read back as their latest writes.
            let mut out = vec![0u8; KIB4];
            let written: Vec<(u64, u64)> = last
                .iter()
                .enumerate()
                .filter(|(_, n)| **n != u64::MAX)
                .map(|(s, n)| (s as u64, *n))
                .collect();
            let idx = sample(written.len(), CHECKS);
            let mut integrity = Ok(());
            for &n in &idx {
                let (slot, w) = written[n];
                let pos = pages
                    .pos(slot * KIB4 as u64)
                    .map_err(|e| sioerr("PageRegion::pos", &e))?;
                let got = pages
                    .read(pos, &mut out)
                    .map_err(|e| sioerr("PageRegion::read", &e))?;
                if got != KIB4 || !ctx.pool.matches(STREAM, w, &out) {
                    integrity = Err(format!("commit page {slot} reads back different bytes"));
                    break;
                }
            }
            Ok(Config {
                point: Point::new(System::StoreIo, ""),
                ratio,
                counted,
                mbps,
                alone_mbps,
                checked: (idx.len(), written.len()),
                integrity,
            })
        };
        let result = run();
        bg.want.store(STOP, Ordering::SeqCst);
        let joined = worker
            .join()
            .map_err(|_| "the background writer panicked".to_owned())?;
        let mut c = result?;
        match joined? {
            Verify::Passed(m) => {
                if c.integrity.is_ok() {
                    point.verify = Verify::Passed(format!(
                        "{} of {} commit pages read back; {m}",
                        c.checked.0, c.checked.1
                    ));
                }
            }
            other => {
                if c.integrity.is_ok() {
                    c.integrity = Err(format!("{other:?}"));
                }
                point.verify = other;
            }
        }
        if let Err(e) = &c.integrity {
            point.verify = Verify::Failed(e.clone());
        }
        Ok(c)
    });
    drop((commits, back));
    let mut c = outcome?;
    c.point = point;
    Ok(c)
}

/// G19's row and points.
pub struct G19 {
    pub(super) row: Row,
    pub(super) points: Vec<Point>,
}

fn not_run(why: &str) -> G19 {
    let mut point = Point::new(System::StoreIo, "G19 commits beside a background Submitter");
    point.verify = Verify::NotChecked(why.to_owned());
    G19 {
        row: Row {
            id: "G19",
            gate: GATE,
            measured: why.to_owned(),
            verdict: Verdict::NotRun,
        },
        points: vec![point],
    }
}

/// G19 on `ctx.g19_dir`; NOT RUN without one.
pub fn measure(ctx: &Ctx) -> G19 {
    let Some(dir) = ctx.g19_dir.as_ref() else {
        return not_run(
            "needs --g19-dir (a directory on G19's device: the g19 CI job, or the dev box)",
        );
    };
    let base = dir.join(format!("store-io-g19-{}", std::process::id()));
    // The control runs only after the paced run succeeded: an error in
    // either fails the gate at once.
    let both = configuration(ctx, &base.join("paced"), true).and_then(|p| {
        let _gone = remove(&base.join("paced"));
        configuration(ctx, &base.join("control"), false).map(|c| (p, c))
    });
    let _gone = remove(&base);
    let (paced, control) = match both {
        Ok(pc) => pc,
        Err(e) => {
            let mut g = not_run(&e);
            g.row.verdict = Verdict::Fail;
            g.points[0].error = Some(e);
            return g;
        }
    };
    let (reference, source) = match ctx.g19_ref_mbps {
        Some(r) => (r, "fio's"),
        None => (paced.alone_mbps.unwrap_or(f64::NAN), "the writer's own"),
    };
    let integrity = paced.integrity.is_ok() && control.integrity.is_ok();
    let holds = paced.counted >= COUNTED && paced.ratio <= LIMIT;
    let fast = paced.mbps >= RATE * reference;
    let control_fails = control.counted >= COUNTED && control.ratio > LIMIT;
    let verdict = if integrity && holds && fast && control_fails {
        Verdict::Pass
    } else {
        // Also where a ratio or rate is NaN: never a pass.
        Verdict::Fail
    };
    let measured = format!(
        "paced: loaded/idle p99 {}x over {} counted rounds of {ROUNDS} (idle p99 {} us); background {} MB/s against {source} paced shape alone {} MB/s (at least {:.0}%: {}); pacing off: {}x over {} counted (must exceed {LIMIT}x: {}); integrity {}",
        f(Some(paced.ratio), 2),
        paced.counted,
        f(paced.point.get("idle_p99_us"), 0),
        f(Some(paced.mbps), 1),
        f(Some(reference), 1),
        RATE * 100.0,
        if fast { "yes" } else { "NO" },
        f(Some(control.ratio), 2),
        control.counted,
        if control_fails { "yes" } else { "NO" },
        if integrity { "ok" } else { "FAILED" },
    );
    G19 {
        row: Row {
            id: "G19",
            gate: GATE,
            measured,
            verdict,
        },
        points: vec![paced.point, control.point],
    }
}

/// `g19`: G19 alone, on `--g19-dir`.
pub fn run(ctx: &Ctx) -> Section {
    let mut sec = Section::new(
        "g19",
        "G19: commits beside background work (two stores on one device; relative)",
    );
    sec.method = vec![format!(
        "Two stores on the --g19-dir device: one makes QD1 4 KiB `write_durable` commits, the other runs one background Submitter (IoClass::Background, depth {DEPTH}) writing {} KiB pieces over a {} MiB page region. Per configuration, {ROUNDS} rounds of (idle, loaded, idle), {:.1} s each, the writer drained before each idle phase; a round counts when its idle p99s are within {AGREE}x, its ratio the loaded p99 over their mean. Paced (the default options) must have at least {COUNTED} counted rounds, a median ratio of at most {LIMIT}x and a background rate while loaded of at least {:.0}% of the paced shape's alone (256 KiB, one in flight; fio's from the device script when given, else the writer at depth 1 with no commits); the same rounds with background_pacing(false) on both stores must exceed {LIMIT}x. Both stores must form one named device group of two; a sample of each side's writes reads back.",
        PIECE >> 10,
        BG_SPAN >> 20,
        ctx.secs.max(1.0),
        RATE * 100.0,
    )];
    let g19 = measure(ctx);
    let mut t = Table::new("Gates:", &["gate", "requirement", "measured", "verdict"]);
    t.row(vec![
        g19.row.id.to_owned(),
        g19.row.gate.to_owned(),
        g19.row.measured.clone(),
        g19.row.verdict.name().to_owned(),
    ]);
    sec.tables.push(t);
    if g19.row.verdict == Verdict::Fail {
        sec.findings
            .push(format!("G19 FAIL: {} ({})", g19.row.gate, g19.row.measured));
    }
    sec.points = g19.points;
    sec.collect_point_anomalies();
    sec
}
