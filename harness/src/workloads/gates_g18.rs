//! G18 (PLANNING R5.14): `StoreSet::barrier` over one store per device, one
//! durable 4 KiB append each, against the per-round maximum of the same
//! appends' lone barriers (each device's `sync_through` timed on its own:
//! a set barrier can be no faster than its slowest flush), in alternated
//! rounds. The allowance is one thread wake, measured in the same run: a
//! set barrier's caller flushes one store itself and is woken once by the
//! others' flush threads (a futex wake on Linux), so the wake is timed as
//! one thread blocked on a channel woken by another.
//!
//! Judged as G1 is: on the median of the per-round differences (the set's
//! p50 minus the lone maxima's p50 minus the wake's p50), the rounds'
//! spread its uncertainty. Besides, and failing the gate whatever the
//! latencies: every set barrier costs exactly one flush on every store (the
//! domains' flush counts before and after), no store's flush fell back to
//! the calling thread, the set is closer to the slowest lone barrier than
//! to all of them one after another (the flushes ran in parallel), every
//! receipt covers its ticket, and a sample of every store's records reads
//! back stamped with its index. NOT RUN with fewer than two devices, or
//! when a store needs no flush (a power-safe device: nothing to overlap).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use store_io_engine::StoreSet;

use super::gates::{Row, Verdict, f};
use super::gates_g1::{BOUND, ROUNDS, median};
use super::{Ctx, remove, sioerr};
use crate::json::Json;
use crate::report::{Point, Section, System, Table, Verify};
use crate::sio;

/// Barriers per side per round.
const PER_ROUND: usize = 200;
/// Set barriers before the first round (flush threads started, caches
/// warm).
const WARMUP: usize = 20;
/// Hand-offs timed per round for the wake.
const WAKES: usize = 200;
/// Records of each store read back at the end.
const CHECKS: usize = 64;

const GATE: &str = "StoreSet::barrier over N devices within one thread wake of the per-round maximum of N lone barriers (p50); one flush per device per call, in parallel";

/// The p50 of `d`, microseconds.
fn p50_us(d: &mut [Duration]) -> f64 {
    if d.is_empty() {
        return f64::NAN;
    }
    d.sort_unstable();
    d[d.len() / 2].as_secs_f64() * 1e6
}

/// One thread waking another `n` times, each timed one way: the waker
/// notes the time and sends; the woken thread notes when it runs.
fn wakes(n: usize) -> Vec<Duration> {
    use std::sync::mpsc;
    let (to_sleeper, wake) = mpsc::sync_channel::<Instant>(0);
    let (back, done) = mpsc::sync_channel::<Duration>(1);
    let sleeper = std::thread::spawn(move || {
        while let Ok(sent) = wake.recv() {
            if back.send(sent.elapsed()).is_err() {
                return;
            }
        }
    });
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        // The sleeper blocks first, as a set barrier's caller does.
        std::thread::sleep(Duration::from_micros(200));
        if to_sleeper.send(Instant::now()).is_err() {
            break;
        }
        match done.recv() {
            Ok(d) => out.push(d),
            Err(_) => break,
        }
    }
    drop(to_sleeper);
    let _joined = sleeper.join();
    out
}

/// The record of append `n` to store `s`: its index and store stamped in
/// front of a block of `n`'s low byte.
fn record(s: usize, n: u64) -> Vec<u8> {
    let mut b = vec![n as u8; 4096];
    b[..8].copy_from_slice(&n.to_le_bytes());
    b[8..16].copy_from_slice(&(s as u64).to_le_bytes());
    b
}

/// G18's row and point.
pub struct G18 {
    pub(super) row: Row,
    pub(super) point: Point,
}

fn not_run(why: &str) -> G18 {
    let mut point = Point::new(System::StoreIo, "G18 set barrier vs the lone maximum");
    point.verify = Verify::NotChecked(why.to_owned());
    G18 {
        row: Row {
            id: "G18",
            gate: GATE,
            measured: why.to_owned(),
            verdict: Verdict::NotRun,
        },
        point,
    }
}

/// G18 on `devices` (a directory on each device); NOT RUN with fewer than
/// two.
pub fn measure(ctx: &Ctx, devices: &[PathBuf]) -> G18 {
    if devices.len() < 2 {
        return not_run("needs --devices (two or more devices: the multi-device job)");
    }
    let dirs: Vec<PathBuf> = devices
        .iter()
        .enumerate()
        .map(|(i, d)| d.join(format!("store-io-g18-{}-{i}", std::process::id())))
        .collect();
    let outcome = run_on(ctx, &dirs);
    // The stores are gone by now (dropped in `run_on`, on error too).
    for d in &dirs {
        let _gone = remove(d);
    }
    match outcome {
        Ok(g) => g,
        Err(e) if e.starts_with("NOT RUN: ") => not_run(&e["NOT RUN: ".len()..]),
        Err(e) => {
            let mut g = not_run(&e);
            g.row.verdict = Verdict::Fail;
            g.point.error = Some(e);
            g
        }
    }
}

/// `multi-gates`: the gates that need several devices (`--devices`).
pub fn run(ctx: &Ctx) -> Section {
    let mut sec = Section::new(
        "multi-gates",
        "Multi-device gates (one store per device; relative, under an override per D-066)",
    );
    sec.method = vec![
        "G18: one durable 4 KiB append on each device's store, then `StoreSet::barrier` over the tickets, against the same appends each made durable by its own `sync_through` (the slowest of them per iteration, and their sum); 20 alternated rounds of 200 each after a warm-up, and 200 timed thread wakes per round. PASS when the median per-round difference (set p50 - lone maximum p50 - wake p50) is at most 0; NOT RESOLVABLE when it is not but the 6th smallest of the 20 is; FAIL otherwise, or when a set barrier did not cost exactly one flush per device, a store's flush fell back to the calling thread, the set's median round was nearer the sum of the lone barriers than their maximum (not in parallel), a receipt did not cover its ticket, or a record did not read back.".to_owned(),
    ];
    let g18 = measure(ctx, &ctx.devices);
    let mut t = Table::new("Gates:", &["gate", "requirement", "measured", "verdict"]);
    t.row(vec![
        g18.row.id.to_owned(),
        g18.row.gate.to_owned(),
        g18.row.measured.clone(),
        g18.row.verdict.name().to_owned(),
    ]);
    sec.tables.push(t);
    if g18.row.verdict == Verdict::Fail {
        sec.findings
            .push(format!("G18 FAIL: {} ({})", g18.row.gate, g18.row.measured));
    }
    sec.points = vec![g18.point];
    sec.collect_point_anomalies();
    sec
}

/// Refuses two directories on one file system (Linux: the same device
/// number): G18 would measure one device several times.
fn distinct_devices(dirs: &[PathBuf]) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mut seen = Vec::new();
        for d in dirs {
            let dev = std::fs::metadata(d)
                .map_err(|e| format!("{}: {e}", d.display()))?
                .dev();
            if seen.contains(&dev) {
                return Err(format!(
                    "NOT RUN: two of --devices are on one file system ({})",
                    d.display()
                ));
            }
            seen.push(dev);
        }
    }
    let _ = dirs;
    Ok(())
}

fn run_on(ctx: &Ctx, dirs: &[PathBuf]) -> Result<G18, String> {
    let mut stores = Vec::new();
    for d in dirs {
        stores.push(sio::create(d, &ctx.opts)?);
    }
    distinct_devices(dirs)?;
    if let Some(s) = stores
        .iter()
        .find(|s| !s.report().decision.class.needs_flush())
    {
        return Err(format!(
            "NOT RUN: a store needs no flush (class {:?}): nothing to run in parallel",
            s.report().decision.class
        ));
    }
    // Room for every append, warm-up included.
    let span =
        ((WARMUP + 2 * ROUNDS * PER_ROUND + 1) as u64 * 4096).next_multiple_of(1 << 20);
    let regions = stores
        .iter()
        .map(|s| {
            s.provision_append_region("g18", span)
                .map_err(|e| sioerr("provision g18", &e))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let set = StoreSet::new(stores.iter().cloned()).map_err(|e| sioerr("StoreSet::new", &e))?;
    // Every record's offset per store, in order (its index is its place).
    let mut written: Vec<Vec<u64>> = vec![Vec::new(); regions.len()];
    let append_all = |written: &mut Vec<Vec<u64>>| -> Result<Vec<_>, String> {
        let mut tickets = Vec::with_capacity(regions.len());
        for (s, r) in regions.iter().enumerate() {
            let n = written[s].len() as u64;
            let t = r.append(&record(s, n)).map_err(|e| sioerr("append", &e))?;
            written[s].push(t.pos().offset());
            tickets.push(t);
        }
        Ok(tickets)
    };
    for _ in 0..WARMUP {
        let tickets = append_all(&mut written)?;
        let refs: Vec<_> = tickets.iter().collect();
        let _r = set
            .barrier(&refs)
            .map_err(|e| sioerr("StoreSet::barrier", &e))?;
    }
    let mut flush_ok = true;
    let mut diffs = Vec::with_capacity(ROUNDS);
    let mut serial = Vec::with_capacity(ROUNDS);
    let (mut sets, mut lones, mut sums, mut wake_all) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for round in 0..ROUNDS {
        let mut set_side = |written: &mut Vec<Vec<u64>>| -> Result<Vec<Duration>, String> {
            let before: Vec<u64> = stores.iter().map(|s| s.domain_stats().flushes).collect();
            let mut d = Vec::with_capacity(PER_ROUND);
            for _ in 0..PER_ROUND {
                let tickets = append_all(written)?;
                let refs: Vec<_> = tickets.iter().collect();
                let t = Instant::now();
                let r = set
                    .barrier(&refs)
                    .map_err(|e| sioerr("StoreSet::barrier", &e))?;
                d.push(t.elapsed());
                if !r.iter().zip(&tickets).all(|(r, t)| r.covers(t)) {
                    return Err("a set barrier's receipt does not cover its ticket".to_owned());
                }
            }
            let after: Vec<u64> = stores.iter().map(|s| s.domain_stats().flushes).collect();
            flush_ok &= before
                .iter()
                .zip(&after)
                .all(|(b, a)| a - b == PER_ROUND as u64);
            Ok(d)
        };
        let lone_side =
            |written: &mut Vec<Vec<u64>>| -> Result<(Vec<Duration>, Vec<Duration>), String> {
                let (mut worst_all, mut sum_all) =
                    (Vec::with_capacity(PER_ROUND), Vec::with_capacity(PER_ROUND));
                for _ in 0..PER_ROUND {
                    let tickets = append_all(written)?;
                    let (mut worst, mut sum) = (Duration::ZERO, Duration::ZERO);
                    for (r, t) in regions.iter().zip(&tickets) {
                        let s = Instant::now();
                        let receipt = r.sync_through(t).map_err(|e| sioerr("sync_through", &e))?;
                        let took = s.elapsed();
                        if !receipt.covers(t) {
                            return Err(
                                "a lone barrier's receipt does not cover its ticket".to_owned()
                            );
                        }
                        worst = worst.max(took);
                        sum += took;
                    }
                    worst_all.push(worst);
                    sum_all.push(sum);
                }
                Ok((worst_all, sum_all))
            };
        let (mut s, (mut l, mut sum)) = if round % 2 == 0 {
            let s = set_side(&mut written)?;
            (s, lone_side(&mut written)?)
        } else {
            let l = lone_side(&mut written)?;
            (set_side(&mut written)?, l)
        };
        let mut w = wakes(WAKES);
        let (sp, lp, up, wp) = (
            p50_us(&mut s),
            p50_us(&mut l),
            p50_us(&mut sum),
            p50_us(&mut w),
        );
        diffs.push(sp - lp - wp);
        // Positive: nearer the serial sum than the parallel maximum.
        serial.push(sp - (lp + up) / 2.0);
        sets.push(sp);
        lones.push(lp);
        sums.push(up);
        wake_all.push(wp);
    }
    let stats = set.stats();
    // Integrity: a sample of every store's records reads back as written.
    let mut verify = Verify::Passed(format!(
        "every receipt covered its ticket; {CHECKS} records of each of {} stores read back stamped with their index",
        stores.len()
    ));
    'stores: for (s, (r, offs)) in regions.iter().zip(&written).enumerate() {
        let step = (offs.len() / CHECKS).max(1);
        for (n, &at) in offs.iter().enumerate().step_by(step) {
            let mut out = vec![0u8; 4096];
            if r.read(at, &mut out).is_err() || out != record(s, n as u64) {
                verify = Verify::Failed(format!("store {s}'s record {n} at {at} did not read back"));
                break 'stores;
            }
        }
    }
    drop((set, regions));
    drop(stores);
    let mut sorted = diffs;
    let m = median(&mut sorted);
    let (lo, hi) = (
        sorted.get(BOUND - 1).copied().unwrap_or(f64::NAN),
        sorted
            .len()
            .checked_sub(BOUND)
            .and_then(|i| sorted.get(i).copied())
            .unwrap_or(f64::NAN),
    );
    let parallel = median(&mut serial) < 0.0;
    let integrity = flush_ok && stats.fallbacks == 0 && matches!(verify, Verify::Passed(_));
    let verdict = if !integrity || !parallel {
        Verdict::Fail
    } else if m <= 0.0 {
        Verdict::Pass
    } else if lo <= 0.0 {
        Verdict::NotResolvable
    } else {
        // Also where a median is NaN (no samples): never a pass.
        Verdict::Fail
    };
    let (sp, lp, up, wp) = (
        median(&mut sets),
        median(&mut lones),
        median(&mut sums),
        median(&mut wake_all),
    );
    let calls = stats.barriers.max(1) as f64;
    let sleeps = stats.sleeps as f64 / calls;
    let mut point = Point::new(System::StoreIo, "G18 set barrier vs the lone maximum")
        .param("devices", Json::Num(dirs.len() as f64));
    point.metric("set_p50_us", Some(sp));
    point.metric("lone_max_p50_us", Some(lp));
    point.metric("lone_sum_p50_us", Some(up));
    point.metric("wake_p50_us", Some(wp));
    point.metric("diff_p50_us", Some(m));
    point.metric("diff_lo_us", Some(lo));
    point.metric("diff_hi_us", Some(hi));
    point.metric("sleeps_per_call", Some(sleeps));
    point.metric("fallbacks", Some(stats.fallbacks as f64));
    point.verify = verify;
    let measured = format!(
        "{} devices: set p50 {} us, lone maximum p50 {} us (sum {} us), wake p50 {} us; set - lone - wake {} us [{}, {}]; sleeps per call {}; one flush per device per call: {}; in parallel: {}; fallbacks {}",
        dirs.len(),
        f(Some(sp), 1),
        f(Some(lp), 1),
        f(Some(up), 1),
        f(Some(wp), 1),
        f(Some(m), 1),
        f(Some(lo), 1),
        f(Some(hi), 1),
        f(Some(sleeps), 2),
        if flush_ok { "yes" } else { "NO" },
        if parallel { "yes" } else { "NO" },
        stats.fallbacks,
    );
    Ok(G18 {
        row: Row {
            id: "G18",
            gate: GATE,
            measured,
            verdict,
        },
        point,
    })
}
