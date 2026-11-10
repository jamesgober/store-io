//! G19 (PLANNING R5.14, D-146, D-157): a commit's latency beside background
//! work. Two stores on one device (so pacing per store instead of per
//! device would fail): store C makes commit-class 4 KiB durable writes at
//! QD1 (`PageRegion::write_durable`), store B runs **one** background
//! `Submitter` (`IoClass::Background`, depth 8: what `background_unpaced`
//! lets it keep in flight unpaced, so the negative control reaches the
//! shape the device was calibrated against) writing 256 KiB pieces.
//!
//! **G19** (`--g19-part main`): 12 alternated rounds of (idle, loaded,
//! idle): the background writer stopped and drained, then writing, then
//! stopped again; the commits' p99 in each. A round counts only if its two
//! idle p99s agree within 1.5x; its ratio is the loaded p99 over their
//! mean. PASS only with all of: at least 6 counted rounds, their median
//! ratio at most 2x, the background writer's rate while loaded at least
//! half the paced shape's rate alone (256 KiB, one in flight: fio's,
//! measured by the device script on CI, `--g19-ref-mbps`; else the same
//! writer measured here at depth 1 with no commits), and the **negative
//! control** failing: the same rounds with `background_pacing(false)` on
//! both stores must have a median ratio above 2x (a gate a wrong pacer also
//! passes measures nothing).
//!
//! Its variants (`--g19-part variants`), each against its own bracketing
//! idle runs (which must agree within 1.5x), all paced:
//! - **G19s**, sporadic commits: 60 commits one every 7 s (beyond
//!   `commit_quiet`, so only the store's `CommitHold` keeps the background
//!   paced, checked before each) beside the writer running throughout; at
//!   least 57 of them within 2x idle p99 (a p99 of 60 samples would mean
//!   little).
//! - **G19b**, a background barrier: the writer's writes made durable by
//!   a `barrier_all` after every 8 MiB (a compaction's checkpoint); the 12 rounds as G19's,
//!   their median ratio at most 2x, and a barrier after every 8 MiB but
//!   the last.
//! - **G19l**, a long loaded round: 3 minutes of continuous background
//!   with commits, the round's p99 and every 15-second window's within 2x
//!   idle (nothing drifts), the writer at least half the paced shape's
//!   rate alone, as G19's.
//!
//! Besides, failing any of them whatever the latencies: both stores in one
//! device group of two members, named (not the process-wide fallback),
//! paced as asked, splitting background I/O at 256 KiB (the shape fio's
//! reference measures); every write completing; a sample of the commit
//! pages and of the background pieces reading back as written. NOT RUN
//! without `--g19-dir` (a directory on G19's device: CI's calibrated
//! device, `harness/scripts/g19_device.sh`, or the dev box's NVMe).

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use store_io_core::IoClass;
use store_io_core::error::{Error, NotWrittenCause};
use store_io_engine::completion::{Completion, Done};
use store_io_engine::{PageRegion, StoreOptions, SubmitterConfig, WriteMode};

use super::gates::{Row, Verdict, f};
use super::gates_g1::median;
use super::stamp::{self, BLOCK};
use super::{Ctx, KIB4, remove, sample, sioerr};
use crate::cli::G19Part;
use crate::json::Json;
use crate::report::{Point, Section, System, Table, Verify};
use crate::sio::{self, Plat, SioStore};

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
/// G19s: commits, the spacing between them, and how many must hold.
const SPORADIC: usize = 60;
const SPACING: Duration = Duration::from_secs(7);
const SPORADIC_WITHIN: usize = 57;
/// G19b: bytes the writer writes between its barriers.
const CHECKPOINT: u64 = 8 << 20;
/// G19l: the loaded round, and its windows.
const LONG: Duration = Duration::from_secs(180);
const WINDOW: Duration = Duration::from_secs(15);
/// The tags of the writer's barriers: a checkpoint's, and the last.
const CHECKPOINT_TAG: u64 = u64::MAX - 1;
const LAST_TAG: u64 = u64::MAX;

const GATE: &str = "Commit-class 4 KiB durable p99 beside one paced background Submitter (two stores, one device) at most 2x idle p99: median of the counted rounds of 12 (idle, loaded, idle), at least 6 counted; background rate at least half the paced shape's alone; pacing off on both stores (the negative control) above 2x";
const GATE_T: &str = "G19 with auto-tuning on (published, not gated): the same rounds, both stores asking for background_autotune; its tuned budget per round and commit p99 beside the default's";
const GATE_S: &str = "G19 sporadic: 60 commits one every 7 s (beyond commit_quiet) under a CommitHold beside the paced background writer, the hold keeping commits active before each; at least 57 within 2x idle p99";
const GATE_B: &str = "G19 background barrier: the background writer's writes made durable by a barrier_all after every 8 MiB; commit p99 at most 2x idle (median of the counted rounds of 12, at least 6 counted)";
const GATE_L: &str = "G19 long round: 3 minutes of continuous paced background with commits; the round's commit p99 and every 15 s window's at most 2x idle; background rate at least half the paced shape's alone";

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
    /// Bytes between the writer's barriers (0: none, plain writes).
    checkpoint: u64,
    /// Bytes completed, and nanoseconds spent from a start to the drain
    /// after it.
    bytes: AtomicU64,
    busy_ns: AtomicU64,
    /// Writes refused `Paced` (reaped and retried).
    paced: AtomicU64,
    /// Writes in flight at once, at most, while running.
    deepest: AtomicU64,
    /// Checkpoint barriers completed.
    barriers: AtomicU64,
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

    /// Starts the writer at the depth G19 asks of it.
    fn run(&self) {
        self.want.store(DEPTH, Ordering::SeqCst);
    }

    /// Sleeps `d`, ending early with the writer's error.
    fn sleep(&self, d: Duration) -> Result<(), String> {
        let until = Instant::now() + d;
        loop {
            if let Some(e) = self.failed() {
                return Err(e);
            }
            let now = Instant::now();
            if now >= until {
                return Ok(());
            }
            std::thread::sleep((until - now).min(Duration::from_millis(100)));
        }
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

/// Whether two idle p99s agree within [`AGREE`] (false when either is NaN).
fn agree(a: f64, b: f64) -> bool {
    a.max(b) <= AGREE * a.min(b)
}

/// Takes one of the writer's completions: a write's bytes counted, a
/// checkpoint barrier counted; `Ok(true)` for the last barrier.
fn take(bg: &Bg, c: Completion) -> Result<bool, String> {
    match c.outcome.map_err(|e| sioerr("background completion", &e))? {
        Done::Written(_) => {
            let _b = bg.bytes.fetch_add(PIECE as u64, Ordering::SeqCst);
            Ok(false)
        }
        Done::Flushed if c.tag == CHECKPOINT_TAG => {
            let _b = bg.barriers.fetch_add(1, Ordering::SeqCst);
            Ok(false)
        }
        Done::Flushed if c.tag == LAST_TAG => Ok(true),
        other => Err(format!("unexpected {other:?} (tag {})", c.tag)),
    }
}

/// The background writer, on its own thread: 256 KiB plain writes over
/// `BG_SPAN` (with a `barrier_all` after every `bg.checkpoint` bytes when
/// that is set), each stamped at its blocks'
/// ends with its write number, as many in flight as asked (at most
/// `DEPTH`); a `Paced` refusal waits for a completion (or, with none in
/// flight, briefly) and retries. At `STOP`: drains, makes everything
/// durable, and reads a sample back.
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
    let (mut k, mut live, mut since) = (0u64, 0usize, 0u64);
    let mut started: Option<Instant> = None;
    loop {
        let want = bg.want.load(Ordering::SeqCst);
        if want == IDLE || want == STOP {
            if live > 0 {
                for c in sub.wait(1).map_err(|e| sioerr("wait", &e))? {
                    live -= 1;
                    let _last = take(bg, c)?;
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
        if bg.checkpoint > 0 && since >= bg.checkpoint && live == 0 {
            // A checkpoint, issued with nothing in flight: a `barrier_all`
            // covers the writes completed before it, so every write since
            // the last one. The writer keeps writing while it runs.
            sub.barrier_all(CHECKPOINT_TAG)
                .map_err(|r| sioerr("barrier_all", &r.error))?;
            live += 1;
            since = 0;
        }
        let cap = want.min(DEPTH) as usize;
        let mut refused = false;
        while live < cap && !(bg.checkpoint > 0 && since >= bg.checkpoint) {
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
                    since += PIECE as u64;
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
                let _last = take(bg, c)?;
            }
        } else if refused {
            std::thread::sleep(Duration::from_micros(50));
        }
    }
    sub.barrier_all(LAST_TAG)
        .map_err(|r| sioerr("barrier_all", &r.error))?;
    let mut flushed = false;
    while !flushed {
        for c in sub.wait(1).map_err(|e| sioerr("barrier wait", &e))? {
            flushed |= take(bg, c)?;
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

/// The commit side: QD1 4 KiB `write_durable` over its page region, each
/// page's latest write remembered for the read-back.
struct Commits {
    pages: PageRegion<Plat>,
    last: Vec<u64>,
    src: Vec<u8>,
    i: u64,
}

impl Commits {
    fn new(store: &SioStore) -> Result<Self, String> {
        Ok(Self {
            pages: store
                .provision_page_region("g19", COMMIT_SPAN)
                .map_err(|e| sioerr("provision_page_region", &e))?,
            last: vec![u64::MAX; (COMMIT_SPAN / KIB4 as u64) as usize],
            src: vec![0u8; KIB4],
            i: 0,
        })
    }

    /// One commit, timed.
    fn one(&mut self, ctx: &Ctx) -> Result<Duration, String> {
        let slots = self.last.len() as u64;
        let slot = self.i % slots;
        ctx.pool.fill(STREAM, self.i, &mut self.src);
        let pos = self
            .pages
            .pos(slot * KIB4 as u64)
            .map_err(|e| sioerr("PageRegion::pos", &e))?;
        let t = Instant::now();
        let _rc = self
            .pages
            .write_durable(pos, &self.src)
            .map_err(|e| sioerr("write_durable", &e))?;
        let d = t.elapsed();
        self.last[slot as usize] = self.i;
        self.i += 1;
        Ok(d)
    }

    /// Commits one after another for `dur`, each timed; ends early with the
    /// writer's error.
    fn burst(&mut self, ctx: &Ctx, bg: &Bg, dur: Duration) -> Result<Vec<Duration>, String> {
        let mut d = Vec::new();
        let until = Instant::now() + dur;
        while Instant::now() < until {
            if let Some(e) = bg.failed() {
                return Err(e);
            }
            d.push(self.one(ctx)?);
        }
        Ok(d)
    }

    /// A sample of the pages read back as their latest writes: `Ok(Err)`
    /// names the first that did not.
    fn verify(&self, ctx: &Ctx) -> Result<Result<(usize, usize), String>, String> {
        let mut out = vec![0u8; KIB4];
        let written: Vec<(u64, u64)> = self
            .last
            .iter()
            .enumerate()
            .filter(|(_, n)| **n != u64::MAX)
            .map(|(s, n)| (s as u64, *n))
            .collect();
        let idx = sample(written.len(), CHECKS);
        for &n in &idx {
            let (slot, w) = written[n];
            let pos = self
                .pages
                .pos(slot * KIB4 as u64)
                .map_err(|e| sioerr("PageRegion::pos", &e))?;
            let got = self
                .pages
                .read(pos, &mut out)
                .map_err(|e| sioerr("PageRegion::read", &e))?;
            if got != KIB4 || !ctx.pool.matches(STREAM, w, &out) {
                return Ok(Err(format!(
                    "commit page {slot} reads back different bytes"
                )));
            }
        }
        Ok(Ok((idx.len(), written.len())))
    }
}

/// What the writer did, for the points.
struct Seen {
    paced: u64,
    deepest: u64,
    barriers: u64,
    bytes: u64,
}

/// Runs `f` on two fresh stores under `base` (the commit side, the
/// background writer's controls and the commit store), pacing on or off,
/// auto-tuning on or off, the writer's barriers every `checkpoint` bytes
/// (0: none). The writer
/// starts idle and is stopped, drained and read back after `f`; with the
/// commit side's read-back, the outcome's `Verify`.
fn with_pair<T>(
    ctx: &Ctx,
    base: &Path,
    paced: bool,
    autotune: bool,
    checkpoint: u64,
    f: impl FnOnce(&mut Commits, &Bg, &SioStore) -> Result<T, String>,
) -> Result<(T, Verify, Seen), String> {
    let opts = StoreOptions {
        background_pacing: paced,
        background_autotune: autotune,
        ..ctx.opts.clone()
    };
    std::fs::create_dir_all(base).map_err(|e| super::ioerr("creating G19's directory", &e))?;
    let commits = sio::create(&base.join("commits"), &opts)?;
    let back = sio::create(&base.join("background"), &opts)?;
    let (q, qc) = (back.qos(), commits.qos());
    if q.unknown {
        return Err(
            "the device could not be named: the stores are paced by the process, not the device"
                .to_owned(),
        );
    } else if q.members != 2 || qc.members != 2 {
        return Err(format!(
            "the two stores are not one device group of two (members {} and {})",
            qc.members, q.members
        ));
    } else if q.paced != paced || qc.paced != paced {
        return Err(format!(
            "the group's pacing is {} (background store) and {} (commit store), not {paced}",
            q.paced, qc.paced
        ));
    } else if q.autotune != autotune || qc.autotune != autotune {
        return Err(format!(
            "the group's auto-tuning is {} (background store) and {} (commit store), not {autotune}",
            q.autotune, qc.autotune
        ));
    } else if q.background_io as usize != PIECE {
        return Err(format!(
            "the background store splits at {} bytes, not {PIECE} (the paced shape G19 measures)",
            q.background_io
        ));
    }
    let bg = Bg {
        checkpoint,
        ..Bg::default()
    };
    let outcome = std::thread::scope(|s| {
        let worker = s.spawn(|| {
            let r = background(&back, &bg);
            if let Err(e) = &r {
                *bg.error
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e.clone());
            }
            r
        });
        let result = (|| {
            let mut c = Commits::new(&commits)?;
            bg.idle()?;
            let t = f(&mut c, &bg, &commits)?;
            Ok::<_, String>((t, c.verify(ctx)?))
        })();
        bg.want.store(STOP, Ordering::SeqCst);
        let joined = worker
            .join()
            .map_err(|_| "the background writer panicked".to_owned())?;
        let (t, commit_check) = result?;
        let verify = match (commit_check, joined?) {
            (Err(e), _) => Verify::Failed(e),
            (Ok((n, of)), Verify::Passed(m)) => {
                Verify::Passed(format!("{n} of {of} commit pages read back; {m}"))
            }
            (Ok(_), other) => other,
        };
        Ok::<_, String>((t, verify))
    });
    let seen = Seen {
        paced: bg.paced.load(Ordering::SeqCst),
        deepest: bg.deepest.load(Ordering::SeqCst),
        barriers: bg.barriers.load(Ordering::SeqCst),
        bytes: bg.bytes.load(Ordering::SeqCst),
    };
    drop((commits, back));
    let (t, verify) = outcome?;
    Ok((t, verify, seen))
}

/// The alternated rounds' outcome.
struct Rounds {
    /// The median ratio of the counted rounds, and how many counted.
    ratio: f64,
    counted: usize,
    lo: Option<f64>,
    hi: Option<f64>,
    idle: f64,
    loaded: f64,
    notes: String,
    /// The tuned budget after each loaded phase (auto-tuning's runs).
    trace: Vec<u64>,
}

/// [`ROUNDS`] rounds of (idle, loaded, idle), `phase` each; with `trace`,
/// its store's tuned budget after each loaded phase.
fn rounds(
    ctx: &Ctx,
    c: &mut Commits,
    bg: &Bg,
    phase: Duration,
    trace: Option<&SioStore>,
) -> Result<Rounds, String> {
    let (mut ratios, mut notes) = (Vec::new(), Vec::new());
    let mut budgets = Vec::new();
    let (mut idles, mut loads) = (Vec::new(), Vec::new());
    for _ in 0..ROUNDS {
        let i0 = p99_us(&mut c.burst(ctx, bg, phase)?);
        bg.run();
        let l = p99_us(&mut c.burst(ctx, bg, phase)?);
        if let Some(s) = trace {
            budgets.push(s.qos().tuned_budget);
        }
        bg.idle()?;
        let i1 = p99_us(&mut c.burst(ctx, bg, phase)?);
        let anchor = (i0 + i1) / 2.0;
        let counts = agree(i0, i1);
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
    let counted = ratios.len();
    let ratio = median(&mut ratios);
    Ok(Rounds {
        ratio,
        counted,
        lo: ratios.first().copied(),
        hi: ratios.last().copied(),
        idle: median(&mut idles),
        loaded: median(&mut loads),
        notes: notes.join(", "),
        trace: budgets,
    })
}

/// The phase length: `--secs`, at least 1 s.
fn phase(ctx: &Ctx) -> Duration {
    Duration::from_secs_f64(ctx.secs.max(1.0))
}

/// The paced shape alone (one in flight, no commits), MB/s: the
/// throughput criterion's reference when fio's is not given. Run before any
/// commit, the writer idle after it.
fn alone(bg: &Bg, ph: Duration) -> Result<f64, String> {
    let (b0, t0) = bg.totals();
    bg.want.store(1, Ordering::SeqCst);
    bg.sleep(ph)?;
    bg.idle()?;
    let (b1, t1) = bg.totals();
    Ok((b1 - b0) as f64 / (t1 - t0) / 1e6)
}

/// The throughput criterion's reference: fio's when given, else `alone`.
fn reference(ctx: &Ctx, alone: Option<f64>) -> (f64, &'static str) {
    match ctx.g19_ref_mbps {
        Some(r) => (r, "fio's"),
        None => (alone.unwrap_or(f64::NAN), "the writer's own"),
    }
}

/// Untimed commits before any measurement.
fn warm(ctx: &Ctx, c: &mut Commits, bg: &Bg) -> Result<(), String> {
    let _w = c.burst(ctx, bg, Duration::from_secs_f64(ctx.warmup.max(0.5)))?;
    Ok(())
}

/// A point for a run of rounds.
fn rounds_point(name: &str, r: &Rounds, seen: &Seen, verify: Verify, ctx: &Ctx) -> Point {
    let mut p = Point::new(System::StoreIo, name)
        .param("rounds", Json::Num(ROUNDS as f64))
        .param("phase_s", Json::Num(phase(ctx).as_secs_f64()));
    p.notes.push(format!(
        "per round commit p99 (us), idle/loaded/idle: {}",
        r.notes
    ));
    p.metric("ratio", Some(r.ratio));
    p.metric("counted", Some(r.counted as f64));
    p.metric("ratio_lo", r.lo);
    p.metric("ratio_hi", r.hi);
    p.metric("idle_p99_us", Some(r.idle));
    p.metric("loaded_p99_us", Some(r.loaded));
    p.metric("paced_refusals", Some(seen.paced as f64));
    p.metric("deepest", Some(seen.deepest as f64));
    p.verify = verify;
    p
}

/// One configuration of G19's rounds.
struct Config {
    point: Point,
    rounds: Rounds,
    /// The background writer's MB/s while loaded.
    mbps: f64,
    /// The paced shape's MB/s alone, measured here (paced run only).
    alone_mbps: Option<f64>,
    integrity: bool,
}

/// G19's rounds on two fresh stores under `base`, pacing on or off,
/// auto-tuning on or off.
fn configuration(ctx: &Ctx, base: &Path, paced: bool, autotune: bool) -> Result<Config, String> {
    let ph = phase(ctx);
    let ((rounds, mbps, alone_mbps), verify, seen) =
        with_pair(ctx, base, paced, autotune, 0, |c, bg, store| {
            // The paced shape alone (one in flight, no commits): the
            // throughput criterion's reference when fio's is not given.
            let alone_mbps = if paced { Some(alone(bg, ph)?) } else { None };
            warm(ctx, c, bg)?;
            let (b0, t0) = bg.totals();
            let r = rounds(ctx, c, bg, ph, autotune.then_some(store))?;
            let (b1, t1) = bg.totals();
            Ok((r, (b1 - b0) as f64 / (t1 - t0) / 1e6, alone_mbps))
        })?;
    let integrity = matches!(verify, Verify::Passed(_));
    let label = match (paced, autotune) {
        (true, true) => "paced, auto-tuned",
        (true, false) => "paced",
        (false, _) => "pacing off",
    };
    let mut point = rounds_point(
        &format!(
            "{}, {label}",
            point_name(if autotune { "G19t" } else { "G19" })
        ),
        &rounds,
        &seen,
        verify,
        ctx,
    )
    .param("pacing", Json::Bool(paced))
    .param("autotune", Json::Bool(autotune));
    if autotune {
        point.notes.push(format!(
            "tuned budget after each loaded phase: {:?}",
            rounds.trace
        ));
        point.metric("tuned_budget_last", rounds.trace.last().map(|b| *b as f64));
    }
    point.metric("background_mb_per_s", Some(mbps));
    point.metric("alone_mb_per_s", alone_mbps);
    Ok(Config {
        point,
        rounds,
        mbps,
        alone_mbps,
        integrity,
    })
}

/// G19's (or a variant's) row and points.
pub struct G19 {
    pub(super) rows: Vec<Row>,
    pub(super) points: Vec<Point>,
}

/// The point's name for each gate (the same whatever the outcome).
fn point_name(id: &str) -> &'static str {
    match id {
        "G19" => "G19 commits beside a background Submitter",
        "G19t" => "G19t commits beside an auto-tuned background Submitter",
        "G19s" => "G19s sporadic commits under a CommitHold",
        "G19b" => "G19b commits beside a background writer with barriers",
        _ => "G19l a long loaded round",
    }
}

fn not_run(id: &'static str, gate: &'static str, why: &str) -> (Row, Point) {
    let mut point = Point::new(System::StoreIo, point_name(id));
    point.verify = Verify::NotChecked(why.to_owned());
    (
        Row {
            id,
            gate,
            measured: why.to_owned(),
            verdict: Verdict::NotRun,
        },
        point,
    )
}

/// A gate that could not complete: FAIL, with the error.
fn failed(id: &'static str, gate: &'static str, e: String) -> (Row, Point) {
    let (mut row, mut point) = not_run(id, gate, &e);
    row.verdict = Verdict::Fail;
    point.error = Some(e);
    (row, point)
}

const NO_DIR: &str =
    "needs --g19-dir (a directory on G19's device: the g19 CI job, or the dev box)";

/// G19 on `ctx.g19_dir`; NOT RUN without one.
pub fn measure(ctx: &Ctx) -> G19 {
    let (rows, points) = measure_main(ctx);
    G19 { rows, points }
}

fn measure_main(ctx: &Ctx) -> (Vec<Row>, Vec<Point>) {
    let Some(dir) = ctx.g19_dir.as_ref() else {
        let (r, p) = not_run("G19", GATE, NO_DIR);
        let (rt, pt) = not_run("G19t", GATE_T, NO_DIR);
        return (vec![r, rt], vec![p, pt]);
    };
    let base = dir.join(format!("store-io-g19-{}", std::process::id()));
    // The control runs only after the paced run succeeded: an error in
    // either fails the gate at once.
    let both = configuration(ctx, &base.join("paced"), true, false).and_then(|p| {
        let _gone = remove(&base.join("paced"));
        configuration(ctx, &base.join("control"), false, false).map(|c| (p, c))
    });
    let _gone = remove(&base.join("control"));
    let (paced, control) = match both {
        Ok(pc) => pc,
        Err(e) => {
            let _gone = remove(&base);
            let (r, p) = failed("G19", GATE, e);
            return (vec![r], vec![p]);
        }
    };
    // Auto-tuning on (published, never gated): the same rounds.
    let tuned = configuration(ctx, &base.join("tuned"), true, true);
    let _gone = remove(&base);
    let (reference, source) = reference(ctx, paced.alone_mbps);
    let integrity = paced.integrity && control.integrity;
    let holds = paced.rounds.counted >= COUNTED && paced.rounds.ratio <= LIMIT;
    let fast = paced.mbps >= RATE * reference;
    let control_fails = control.rounds.counted >= COUNTED && control.rounds.ratio > LIMIT;
    let verdict = if integrity && holds && fast && control_fails {
        Verdict::Pass
    } else {
        // Also where a ratio or rate is NaN: never a pass.
        Verdict::Fail
    };
    let measured = format!(
        "paced: loaded/idle p99 {}x over {} counted rounds of {ROUNDS} (idle p99 {} us); background {} MB/s against {source} paced shape alone {} MB/s (at least {:.0}%: {}); pacing off: {}x over {} counted (must exceed {LIMIT}x: {}); integrity {}",
        f(Some(paced.rounds.ratio), 2),
        paced.rounds.counted,
        f(Some(paced.rounds.idle), 0),
        f(Some(paced.mbps), 1),
        f(Some(reference), 1),
        RATE * 100.0,
        if fast { "yes" } else { "NO" },
        f(Some(control.rounds.ratio), 2),
        control.rounds.counted,
        if control_fails { "yes" } else { "NO" },
        if integrity { "ok" } else { "FAILED" },
    );
    let mut rows = vec![Row {
        id: "G19",
        gate: GATE,
        measured,
        verdict,
    }];
    let mut points = vec![paced.point, control.point];
    match tuned {
        Ok(t) => {
            rows.push(Row {
                id: "G19t",
                gate: GATE_T,
                measured: format!(
                    "auto-tuned: loaded/idle p99 {}x over {} counted rounds of {ROUNDS} (the default's {}x), loaded commit p99 {} us (the default's {} us); tuned budget after each loaded phase {:?}; background {} MB/s (the default's {}); integrity {}",
                    f(Some(t.rounds.ratio), 2),
                    t.rounds.counted,
                    f(Some(paced.rounds.ratio), 2),
                    f(Some(t.rounds.loaded), 0),
                    f(Some(paced.rounds.loaded), 0),
                    t.rounds.trace,
                    f(Some(t.mbps), 1),
                    f(Some(paced.mbps), 1),
                    if t.integrity { "ok" } else { "FAILED" },
                ),
                // Published, never gated; an integrity failure fails it.
                verdict: if t.integrity {
                    Verdict::Reported
                } else {
                    Verdict::Fail
                },
            });
            points.push(t.point);
        }
        Err(e) => {
            let (r, p) = failed("G19t", GATE_T, e);
            rows.push(r);
            points.push(p);
        }
    }
    (rows, points)
}

/// G19s: sporadic commits under a `CommitHold`.
fn sporadic(ctx: &Ctx, base: &Path) -> Result<(Row, Point), String> {
    if ctx.opts.commit_quiet >= SPACING {
        return Err(format!(
            "commit_quiet ({:?}) is not under the commits' spacing ({SPACING:?})",
            ctx.opts.commit_quiet
        ));
    }
    let ph = phase(ctx);
    let ((i0, i1, lat, held, mbps), verify, seen) =
        with_pair(ctx, base, true, false, 0, |c, bg, store| {
            warm(ctx, c, bg)?;
            let i0 = p99_us(&mut c.burst(ctx, bg, ph)?);
            let hold = store.commit_hold();
            let (b0, t0) = bg.totals();
            bg.run();
            let (mut lat, mut held) = (Vec::with_capacity(SPORADIC), 0usize);
            for _ in 0..SPORADIC {
                bg.sleep(SPACING)?;
                // Past `commit_quiet` since the last commit: only the hold
                // keeps the group's commits active.
                if store.qos().active {
                    held += 1;
                }
                lat.push(c.one(ctx)?);
            }
            bg.idle()?;
            let (b1, t1) = bg.totals();
            drop(hold);
            let i1 = p99_us(&mut c.burst(ctx, bg, ph)?);
            Ok((i0, i1, lat, held, (b1 - b0) as f64 / (t1 - t0) / 1e6))
        })?;
    let anchor = (i0 + i1) / 2.0;
    let us: Vec<f64> = lat.iter().map(|d| d.as_secs_f64() * 1e6).collect();
    let within = us.iter().filter(|&&u| u <= LIMIT * anchor).count();
    let agrees = agree(i0, i1);
    let integrity = matches!(verify, Verify::Passed(_));
    let verdict = if integrity && agrees && within >= SPORADIC_WITHIN && held == SPORADIC {
        Verdict::Pass
    } else {
        Verdict::Fail
    };
    let mut sorted = us.clone();
    let worst = sorted.iter().copied().fold(f64::NAN, f64::max);
    let mid = median(&mut sorted);
    let mut p = Point::new(System::StoreIo, point_name("G19s"));
    p.notes.push(format!(
        "commit latencies (us), one every {} s: {}",
        SPACING.as_secs(),
        us.iter()
            .map(|u| format!("{u:.0}"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    p.metric("idle_p99_us", Some(anchor));
    p.metric("within", Some(within as f64));
    p.metric("median_us", Some(mid));
    p.metric("worst_us", Some(worst));
    p.metric("paced_refusals", Some(seen.paced as f64));
    p.metric("held", Some(held as f64));
    p.metric("background_mb_per_s", Some(mbps));
    p.verify = verify;
    let measured = format!(
        "{within} of {SPORADIC} within {LIMIT}x idle p99 {} us (at least {SPORADIC_WITHIN}); median {} us, worst {} us; idle runs {} / {} us (agree: {}); commits active by the hold alone before {held} of {SPORADIC} (all: {}); background {} MB/s ({} Paced refusals); integrity {}",
        f(Some(anchor), 0),
        f(Some(mid), 0),
        f(Some(worst), 0),
        f(Some(i0), 0),
        f(Some(i1), 0),
        if agrees { "yes" } else { "NO" },
        if held == SPORADIC { "yes" } else { "NO" },
        f(Some(mbps), 1),
        seen.paced,
        if integrity { "ok" } else { "FAILED" },
    );
    Ok((
        Row {
            id: "G19s",
            gate: GATE_S,
            measured,
            verdict,
        },
        p,
    ))
}

/// G19b: the background writer durable, barriers every [`CHECKPOINT`].
fn barrier(ctx: &Ctx, base: &Path) -> Result<(Row, Point), String> {
    let ph = phase(ctx);
    let ((r, mbps), verify, seen) =
        with_pair(ctx, base, true, false, CHECKPOINT, |c, bg, _store| {
            warm(ctx, c, bg)?;
            let (b0, t0) = bg.totals();
            let r = rounds(ctx, c, bg, ph, None)?;
            let (b1, t1) = bg.totals();
            Ok((r, (b1 - b0) as f64 / (t1 - t0) / 1e6))
        })?;
    let integrity = matches!(verify, Verify::Passed(_));
    // A checkpoint after every 8 MiB but the last (the final barrier
    // covers the tail).
    let expected = (seen.bytes / CHECKPOINT).saturating_sub(1);
    let checkpoints = seen.barriers > 0 && seen.barriers >= expected;
    let verdict = if integrity && r.counted >= COUNTED && r.ratio <= LIMIT && checkpoints {
        Verdict::Pass
    } else {
        Verdict::Fail
    };
    let measured = format!(
        "loaded/idle p99 {}x over {} counted rounds of {ROUNDS} (idle p99 {} us); {} barriers after every {} MiB of {} MiB written (at least {expected}: {}); background {} MB/s; integrity {}",
        f(Some(r.ratio), 2),
        r.counted,
        f(Some(r.idle), 0),
        seen.barriers,
        CHECKPOINT >> 20,
        seen.bytes >> 20,
        if checkpoints { "yes" } else { "NO" },
        f(Some(mbps), 1),
        if integrity { "ok" } else { "FAILED" },
    );
    let mut p = rounds_point(point_name("G19b"), &r, &seen, verify, ctx);
    p.metric("barriers", Some(seen.barriers as f64));
    p.metric("background_mb_per_s", Some(mbps));
    Ok((
        Row {
            id: "G19b",
            gate: GATE_B,
            measured,
            verdict,
        },
        p,
    ))
}

/// G19l: one long loaded round.
fn long(ctx: &Ctx, base: &Path) -> Result<(Row, Point), String> {
    let ph = phase(ctx);
    let ((i0, i1, all, windows, mbps, own), verify, seen) =
        with_pair(ctx, base, true, false, 0, |c, bg, _store| {
            let own = alone(bg, ph)?;
            warm(ctx, c, bg)?;
            let i0 = p99_us(&mut c.burst(ctx, bg, ph)?);
            let (b0, t0) = bg.totals();
            bg.run();
            let (mut all, mut windows) = (Vec::new(), Vec::new());
            let until = Instant::now() + LONG;
            while Instant::now() < until {
                let mut w = c.burst(ctx, bg, WINDOW)?;
                all.extend_from_slice(&w);
                windows.push(p99_us(&mut w));
            }
            bg.idle()?;
            let (b1, t1) = bg.totals();
            let i1 = p99_us(&mut c.burst(ctx, bg, ph)?);
            Ok((
                i0,
                i1,
                all,
                windows,
                (b1 - b0) as f64 / (t1 - t0) / 1e6,
                own,
            ))
        })?;
    let mut all = all;
    let anchor = (i0 + i1) / 2.0;
    let overall = p99_us(&mut all) / anchor;
    let last = windows.last().copied().unwrap_or(f64::NAN) / anchor;
    // Every window within the limit (none when there are none: never a
    // pass).
    let worst = windows.iter().map(|w| w / anchor).fold(f64::NAN, f64::max);
    let agrees = agree(i0, i1);
    let integrity = matches!(verify, Verify::Passed(_));
    let (reference, source) = reference(ctx, Some(own));
    let fast = mbps >= RATE * reference;
    let verdict = if integrity && agrees && overall <= LIMIT && worst <= LIMIT && fast {
        Verdict::Pass
    } else {
        Verdict::Fail
    };
    let mut p = Point::new(System::StoreIo, point_name("G19l"))
        .param("loaded_s", Json::Num(LONG.as_secs_f64()));
    p.notes.push(format!(
        "commit p99 (us) per {} s window: {}",
        WINDOW.as_secs(),
        windows
            .iter()
            .map(|u| format!("{u:.0}"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    p.metric("idle_p99_us", Some(anchor));
    p.metric("ratio", Some(overall));
    p.metric("last_window_ratio", Some(last));
    p.metric("worst_window_ratio", Some(worst));
    p.metric("background_mb_per_s", Some(mbps));
    p.metric("alone_mb_per_s", Some(own));
    p.metric("commits", Some(all.len() as f64));
    p.metric("paced_refusals", Some(seen.paced as f64));
    p.verify = verify;
    let measured = format!(
        "{} s loaded: commit p99 {}x idle p99 {} us, its {} s windows at most {}x (the last {}x); background {} MB/s against {source} paced shape alone {} MB/s (at least {:.0}%: {}); idle runs {} / {} us (agree: {}); integrity {}",
        LONG.as_secs(),
        f(Some(overall), 2),
        f(Some(anchor), 0),
        WINDOW.as_secs(),
        f(Some(worst), 2),
        f(Some(last), 2),
        f(Some(mbps), 1),
        f(Some(reference), 1),
        RATE * 100.0,
        if fast { "yes" } else { "NO" },
        f(Some(i0), 0),
        f(Some(i1), 0),
        if agrees { "yes" } else { "NO" },
        if integrity { "ok" } else { "FAILED" },
    );
    Ok((
        Row {
            id: "G19l",
            gate: GATE_L,
            measured,
            verdict,
        },
        p,
    ))
}

/// G19's variants on `ctx.g19_dir`, each on fresh stores; NOT RUN without
/// one.
pub fn measure_variants(ctx: &Ctx) -> G19 {
    type Variant = fn(&Ctx, &Path) -> Result<(Row, Point), String>;
    let all: [(&'static str, &'static str, &str, Variant); 3] = [
        ("G19s", GATE_S, "sporadic", sporadic),
        ("G19b", GATE_B, "barrier", barrier),
        ("G19l", GATE_L, "long", long),
    ];
    let mut g = G19 {
        rows: Vec::new(),
        points: Vec::new(),
    };
    for (id, gate, name, run) in all {
        let (row, point) = match ctx.g19_dir.as_ref() {
            None => not_run(id, gate, NO_DIR),
            Some(dir) => {
                let base = dir.join(format!("store-io-g19{name}-{}", std::process::id()));
                let r = run(ctx, &base);
                let _gone = remove(&base);
                r.unwrap_or_else(|e| failed(id, gate, e))
            }
        };
        g.rows.push(row);
        g.points.push(point);
    }
    g
}

/// `g19`: G19 and its variants (`--g19-part`), on `--g19-dir`.
pub fn run(ctx: &Ctx) -> Section {
    let mut sec = Section::new(
        "g19",
        "G19: commits beside background work (two stores on one device; relative)",
    );
    sec.method = vec![
        format!(
            "Two stores on the --g19-dir device: one makes QD1 4 KiB `write_durable` commits, the other runs one background Submitter (IoClass::Background, depth {DEPTH}) writing {} KiB pieces over a {} MiB page region. G19: per configuration, {ROUNDS} rounds of (idle, loaded, idle), {:.1} s each, the writer drained before each idle phase; a round counts when its idle p99s are within {AGREE}x, its ratio the loaded p99 over their mean. Paced (the default options) must have at least {COUNTED} counted rounds, a median ratio of at most {LIMIT}x and a background rate while loaded of at least {:.0}% of the paced shape's alone (256 KiB, one in flight; fio's from the device script when given, else the writer at depth 1 with no commits); the same rounds with background_pacing(false) on both stores must exceed {LIMIT}x. G19t, published and never gated: the same rounds with background_autotune on both stores, the tuned budget after each loaded phase beside its commit p99 and the default's.",
            PIECE >> 10,
            BG_SPAN >> 20,
            phase(ctx).as_secs_f64(),
            RATE * 100.0,
        ),
        format!(
            "Variants, paced, each against its own bracketing idle runs (within {AGREE}x): G19s, {SPORADIC} commits one every {} s beside the writer under the commit store's CommitHold, at least {SPORADIC_WITHIN} within {LIMIT}x idle p99, the group's commits active (by the hold alone) before every one; G19b, the writer's writes made durable by a barrier_all after every {} MiB, G19's rounds, median ratio at most {LIMIT}x, a barrier after every {} MiB but the last; G19l, {} s of continuous background with commits, the round's p99 and every {} s window's at most {LIMIT}x idle, the writer at least {:.0}% of the paced shape's rate alone.",
            SPACING.as_secs(),
            CHECKPOINT >> 20,
            CHECKPOINT >> 20,
            LONG.as_secs(),
            WINDOW.as_secs(),
            RATE * 100.0,
        ),
        "All: both stores one named device group of two, paced as asked, splitting background I/O at 256 KiB; a sample of each side's writes reads back.".to_owned(),
    ];
    let mut g = G19 {
        rows: Vec::new(),
        points: Vec::new(),
    };
    if matches!(ctx.g19_part, G19Part::Main | G19Part::All) {
        let m = measure(ctx);
        g.rows.extend(m.rows);
        g.points.extend(m.points);
    }
    if matches!(ctx.g19_part, G19Part::Variants | G19Part::All) {
        let v = measure_variants(ctx);
        g.rows.extend(v.rows);
        g.points.extend(v.points);
    }
    let mut t = Table::new("Gates:", &["gate", "requirement", "measured", "verdict"]);
    for r in &g.rows {
        t.row(vec![
            r.id.to_owned(),
            r.gate.to_owned(),
            r.measured.clone(),
            r.verdict.name().to_owned(),
        ]);
        if r.verdict == Verdict::Fail {
            sec.findings
                .push(format!("{} FAIL: {} ({})", r.id, r.gate, r.measured));
        }
    }
    sec.tables.push(t);
    sec.points = g.points;
    sec.collect_point_anomalies();
    sec
}
