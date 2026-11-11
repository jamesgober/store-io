//! G18 (PLANNING R5.14, as D-153 amends it): `StoreSet::barrier` over one
//! store per device, one durable 4 KiB append each, against **the raw
//! parallel barrier**: the same appends made durable at once, each by its
//! store's own `sync_through`, N-1 of them on helper threads asleep on their
//! own condvars and woken one after another by the caller (as a set barrier
//! wakes each store's flush thread), which makes store 0's itself and then
//! spin-waits for the rest (what any implementation that keeps no thread
//! spinning between calls can do; the analogue of G1's raw primitive). The
//! gate is that p50 plus 2 us and 3%: what `StoreSet` adds of its own (its
//! tickets, answers and poison registrations).
//!
//! Published beside it, from the same alternated rounds: the slowest lone
//! barrier per iteration and their sum (each device's `sync_through` timed
//! on its own), the parallel barrier with its helpers spinning instead of
//! asleep (the device's and kernel's own cost of concurrent flushes, no
//! wake), and one thread wake (a channel hand-off). R5.14's first form,
//! "within one wake of the slowest lone barrier", charged the set for both
//! of what those show it cannot avoid: on CI's four `null_blk` devices the
//! spinning barrier was 5 us over the lone maximum and waking three sleeping
//! helpers 11.5 us more (one `notify_all`), while `StoreSet` was 1.4 us over
//! the woken one.
//!
//! Judged as G1 is: on the median of the per-round differences (the set's
//! p50 minus the raw parallel barrier's p50 minus the allowance), the
//! rounds' spread its uncertainty. Besides, and failing the gate whatever the
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
/// Untimed iterations at the start of each parallel side (its helpers are
/// started for the side).
const SIDE_WARMUP: usize = 5;
/// Hand-offs timed per round for the wake.
const WAKES: usize = 200;
/// Records of each store read back at the end.
const CHECKS: usize = 64;

const GATE: &str = "StoreSet::barrier over N devices within 2 us + 3% of the raw parallel barrier (N-1 sleeping helpers woken in turn and the caller, each making its store's own barrier; p50); one flush per device per call, in parallel";

/// The allowance over the raw parallel barrier: microseconds, and a
/// fraction of its p50.
const ALLOW_US: f64 = 2.0;
const ALLOW_FRACTION: f64 = 0.03;

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
    let mut point = Point::new(
        System::StoreIo,
        "G18 set barrier vs the raw parallel barrier",
    );
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
        "G18: one durable 4 KiB append on each device's store, then `StoreSet::barrier` over the tickets, against the raw parallel barrier: the same appends made durable at once, each by its store's own `sync_through`, N-1 on helper threads asleep on their own condvars and woken in turn, the caller making store 0's and spin-waiting for the rest, 5 untimed iterations first (published beside it: the same appends' lone barriers, the slowest per iteration and their sum; the parallel barrier with its helpers spinning; 200 timed thread wakes); 20 alternated rounds of 200 each after a warm-up. PASS when the median per-round difference (set p50 - raw parallel p50 - (2 us + 3% of it)) is at most 0; NOT RESOLVABLE when it is not but the 6th smallest of the 20 is; FAIL otherwise, or when a set barrier did not cost exactly one flush per device, a store's flush fell back to the calling thread, the set's median round was nearer the sum of the lone barriers than their maximum (not in parallel), a receipt did not cover its ticket, or a record did not read back.".to_owned(),
    ];
    sec.method.push("G17: for N = 1, 2 and 4 (at most the devices given), one store on each of the first N devices, each its own Submitter appending 1 MiB from pool buffers 8 deep on its own thread, all started together, their MB/s summed; against fio on the same N devices together (one QD8 1 MiB sequential-write job per device in one process, group reporting); three alternated runs per N; PASS when every N's median is at least 90% of fio's (NOT RESOLVABLE when only within fio's spread of it); labelled with the CPU count; each writer's appends read back.".to_owned());
    let g17 = super::gates_g17::measure(ctx, &ctx.devices);
    let g18 = measure(ctx, &ctx.devices);
    let mut t = Table::new("Gates:", &["gate", "requirement", "measured", "verdict"]);
    for row in [&g17.row, &g18.row] {
        t.row(vec![
            row.id.to_owned(),
            row.gate.to_owned(),
            row.measured.clone(),
            row.verdict.name().to_owned(),
        ]);
        if row.verdict == Verdict::Fail {
            sec.findings
                .push(format!("{} FAIL: {} ({})", row.id, row.gate, row.measured));
        }
    }
    sec.tables.push(t);
    let mut points = g17.points;
    points.push(g18.point);
    sec.points = points;
    sec.collect_point_anomalies();
    sec
}

/// Refuses two directories on one file system (Linux: the same device
/// number): G18 would measure one device several times.
pub(super) fn distinct_devices(dirs: &[PathBuf]) -> Result<(), String> {
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
    let span = ((WARMUP + 4 * ROUNDS * PER_ROUND + 2 * ROUNDS * SIDE_WARMUP + 1) as u64 * 4096)
        .next_multiple_of(1 << 20);
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
    let (mut raws, mut spins) = (Vec::new(), Vec::new());
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
        // The parallel barrier: one helper per store but the first, spinning
        // (`true`) or asleep on its own condvar (`false`) between
        // iterations, released one after another (as a set barrier hands
        // each store to its own flush thread); the caller makes store 0's
        // barrier itself, then spin-waits for the helpers (it never sleeps).
        // Each side starts with `SIDE_WARMUP` untimed iterations, so freshly
        // started helpers are not timed.
        let parallel_side = |spin: bool,
                             written: &mut Vec<Vec<u64>>|
         -> Result<Vec<Duration>, String> {
            use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
            use std::sync::{Condvar, Mutex};
            let n = regions.len();
            let slots: Vec<Mutex<Option<_>>> = (0..n).map(|_| Mutex::new(None)).collect();
            let generation = AtomicU64::new(0);
            let gates: Vec<(Mutex<u64>, Condvar)> =
                (0..n).map(|_| (Mutex::new(0), Condvar::new())).collect();
            let done = AtomicUsize::new(0);
            let stop = AtomicBool::new(false);
            // The first helper failure: its error, or a receipt short of its
            // ticket, or a panic (counted as done, so the caller never waits
            // on it).
            let failed: Mutex<Option<String>> = Mutex::new(None);
            let fail = |why: String| {
                let mut f = failed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                f.get_or_insert(why);
            };
            let mut d = Vec::with_capacity(PER_ROUND);
            std::thread::scope(|scope| -> Result<(), String> {
                for i in 1..n {
                    let (slots, generation, gate, done, stop, fail, r) = (
                        &slots,
                        &generation,
                        &gates[i],
                        &done,
                        &stop,
                        &fail,
                        &regions[i],
                    );
                    scope.spawn(move || {
                        let mut seen = 0u64;
                        loop {
                            if spin {
                                let mut k = 0u32;
                                while generation.load(Ordering::Acquire) == seen {
                                    if stop.load(Ordering::Acquire) {
                                        return;
                                    }
                                    k += 1;
                                    if k < 4096 {
                                        std::hint::spin_loop();
                                    } else {
                                        std::thread::yield_now();
                                    }
                                }
                            } else {
                                let mut g = gate
                                    .0
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                while *g == seen && !stop.load(Ordering::Acquire) {
                                    g = gate
                                        .1
                                        .wait(g)
                                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                                }
                                if stop.load(Ordering::Acquire) {
                                    return;
                                }
                            }
                            seen += 1;
                            let t = slots[i]
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .take();
                            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                                || match t {
                                    None => Err("no ticket".to_owned()),
                                    Some(t) => match r.sync_through(&t) {
                                        Ok(rc) if rc.covers(&t) => Ok(()),
                                        Ok(_) => {
                                            Err("a receipt does not cover its ticket".to_owned())
                                        }
                                        Err(e) => Err(sioerr("sync_through", &e)),
                                    },
                                },
                            ));
                            match ran {
                                Ok(Ok(())) => {}
                                Ok(Err(why)) => fail(format!("helper {i}: {why}")),
                                Err(_) => fail(format!("helper {i} panicked")),
                            }
                            done.fetch_add(1, Ordering::AcqRel);
                        }
                    });
                }
                let result = (|| {
                    for it in 0..SIDE_WARMUP + PER_ROUND {
                        let tickets = append_all(written)?;
                        let mut tickets = tickets.into_iter();
                        let first = tickets.next().ok_or("no store")?;
                        for (i, t) in tickets.enumerate() {
                            *slots[i + 1]
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(t);
                        }
                        done.store(0, Ordering::Release);
                        let t0 = Instant::now();
                        if spin {
                            generation.fetch_add(1, Ordering::AcqRel);
                        } else {
                            for g in &gates[1..] {
                                *g.0.lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
                                g.1.notify_one();
                            }
                        }
                        let rc = regions[0]
                            .sync_through(&first)
                            .map_err(|e| sioerr("sync_through", &e))?;
                        if !rc.covers(&first) {
                            return Err(
                                "a parallel barrier's receipt does not cover its ticket".to_owned()
                            );
                        }
                        let mut k = 0u32;
                        while done.load(Ordering::Acquire) < n - 1 {
                            k += 1;
                            if k < 4096 {
                                std::hint::spin_loop();
                            } else {
                                std::thread::yield_now();
                            }
                        }
                        if it >= SIDE_WARMUP {
                            d.push(t0.elapsed());
                        }
                        if let Some(why) = failed
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .take()
                        {
                            return Err(format!("a parallel barrier failed: {why}"));
                        }
                    }
                    Ok(())
                })();
                stop.store(true, Ordering::Release);
                for g in &gates[1..] {
                    *g.0.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
                    g.1.notify_one();
                }
                result
            })?;
            Ok(d)
        };
        // Alternated: the set and its reference each run first, after the
        // previous round's idle wakes, in half the rounds, and next to each
        // other in every round.
        let (mut s, (mut l, mut sum), mut raw, mut spun) = if round % 2 == 0 {
            let raw = parallel_side(false, &mut written)?;
            let s = set_side(&mut written)?;
            let l = lone_side(&mut written)?;
            (s, l, raw, parallel_side(true, &mut written)?)
        } else {
            let s = set_side(&mut written)?;
            let raw = parallel_side(false, &mut written)?;
            let spun = parallel_side(true, &mut written)?;
            (s, lone_side(&mut written)?, raw, spun)
        };
        let mut w = wakes(WAKES);
        let (sp, lp, up, wp, rp, pp) = (
            p50_us(&mut s),
            p50_us(&mut l),
            p50_us(&mut sum),
            p50_us(&mut w),
            p50_us(&mut raw),
            p50_us(&mut spun),
        );
        diffs.push(sp - rp - (ALLOW_US + ALLOW_FRACTION * rp));
        raws.push(rp);
        spins.push(pp);
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
                verify =
                    Verify::Failed(format!("store {s}'s record {n} at {at} did not read back"));
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
    let (sp, lp, up, wp, rp, pp) = (
        median(&mut sets),
        median(&mut lones),
        median(&mut sums),
        median(&mut wake_all),
        median(&mut raws),
        median(&mut spins),
    );
    let calls = stats.barriers.max(1) as f64;
    let sleeps = stats.sleeps as f64 / calls;
    let mut point = Point::new(
        System::StoreIo,
        "G18 set barrier vs the raw parallel barrier",
    )
    .param("devices", Json::Num(dirs.len() as f64));
    point.metric("set_p50_us", Some(sp));
    point.metric("lone_max_p50_us", Some(lp));
    point.metric("lone_sum_p50_us", Some(up));
    point.metric("wake_p50_us", Some(wp));
    point.metric("raw_parallel_p50_us", Some(rp));
    point.metric("spinning_parallel_p50_us", Some(pp));
    point.metric("diff_p50_us", Some(m));
    point.metric("diff_lo_us", Some(lo));
    point.metric("diff_hi_us", Some(hi));
    point.metric("sleeps_per_call", Some(sleeps));
    point.metric("fallbacks", Some(stats.fallbacks as f64));
    point.verify = verify;
    let measured = format!(
        "{} devices: set p50 {} us, raw parallel p50 {} us; set - raw - allowance {} us [{}, {}]; published: lone maximum p50 {} us (sum {} us), spinning parallel p50 {} us, wake p50 {} us; sleeps per call {}; one flush per device per call: {}; in parallel: {}; fallbacks {}",
        dirs.len(),
        f(Some(sp), 1),
        f(Some(rp), 1),
        f(Some(m), 1),
        f(Some(lo), 1),
        f(Some(hi), 1),
        f(Some(lp), 1),
        f(Some(up), 1),
        f(Some(pp), 1),
        f(Some(wp), 1),
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
