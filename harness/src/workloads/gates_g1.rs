//! G1's measurement: store-io's `PageRegion::write_durable` against the raw
//! primitive (copy into an aligned buffer, one unbuffered write, one data
//! flush), QD1 4 KiB, alternated in short rounds.
//!
//! Why alternated: a virtual machine's flush latency drifts by several
//! microseconds between windows of a few seconds (measured in CI: two raw
//! loops on the same device 3.7 us apart), more than G1's +2 us gate, so two
//! long windows measured one after the other judge the drift, not
//! store-io. Each round runs a burst of each side, the order swapped every
//! round (each side first in half of them); G1 is judged on the worse of the
//! median per-round difference of the two p50s and the difference of the
//! pooled p50s, with the rounds' spread of differences as its uncertainty.
//! G3's comparison with raw is judged on the same rounds, as the ratio of
//! the two sides' mean latencies (store-io's rate over raw's).

use std::time::{Duration, Instant};

use super::{Ctx, KIB4, ioerr, remove, sample, sioerr, with_store};
use crate::aligned::AlignedBuf;
use crate::raw::RawFile;
use crate::report::{Point, System, Verify};

/// Payload stream (distinct from W1's).
const STREAM: u64 = 11;
/// Rounds, each one burst per side (even: each side first in half).
pub const ROUNDS: usize = 20;
/// The order statistic (1-based, from either end) bounding the median's
/// interval: the 6th of 20 holds the true median about 96% of the time
/// (1 - 2 P(Binomial(20, 1/2) <= 5)).
pub(super) const BOUND: usize = 6;
/// The page region and the raw file.
const SPAN: u64 = 256 << 20;
/// Read-back checks per side.
const CHECKS: usize = 1024;

/// The median of `v` (the mean of the middle two for an even count; NaN
/// when empty).
pub(super) fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// The p50 of `d`, microseconds.
fn p50_us(d: &mut [Duration]) -> f64 {
    if d.is_empty() {
        return f64::NAN;
    }
    d.sort_unstable();
    d[d.len() / 2].as_secs_f64() * 1e6
}

/// Runs the alternated measurement. The point carries `raw_p50_us` (the
/// median over rounds of the raw p50), `diff_p50_us` (the median of the
/// per-round differences, store-io minus raw), `diff_lo_us` / `diff_hi_us`
/// (the [`BOUND`]th smallest and largest differences: an interval holding
/// the true median about 96% of the time) and `pooled_diff_us`
/// (the p50 of every store-io sample minus that of every raw sample). The
/// per-round differences are returned too. For G3: `rate_ratio` (the
/// median over rounds of raw's mean latency over store-io's, so store-io's
/// rate over raw's), `rate_ratio_lo` / `rate_ratio_hi` (its interval, as
/// above) and `pooled_rate_ratio` (the same over every sample).
///
/// Integrity: every write carries its index's payload; afterwards a sample
/// of each side's pages reads back as its latest write, byte for byte.
pub fn alternated(ctx: &Ctx) -> (Point, Vec<f64>) {
    let mut rounds = Vec::new();
    let p = alternated_into(ctx, &mut rounds);
    (p, rounds)
}

fn alternated_into(ctx: &Ctx, rounds: &mut Vec<f64>) -> Point {
    let p = Point::new(System::StoreIo, "G1 alternated: write_durable vs raw");
    let raw_path = ctx.path("g1-raw.dat");
    let mut out = with_store(ctx, "g1", p, |store, p| {
        let pages = store
            .provision_page_region("g1", SPAN)
            .map_err(|e| sioerr("provision_page_region", &e))?;
        let (f, _) = RawFile::create_ready(&raw_path, SPAN).map_err(|e| ioerr("raw create", &e))?;
        let slots = SPAN / KIB4 as u64;
        let (mut last_s, mut last_r) = (
            vec![u64::MAX; slots as usize],
            vec![u64::MAX; slots as usize],
        );
        let mut src = vec![0u8; KIB4];
        let mut abuf = AlignedBuf::zeroed(KIB4);
        let (mut i_s, mut i_r) = (0u64, 0u64);
        let burst = Duration::from_secs_f64((ctx.secs * 2.0 / ROUNDS as f64).max(0.2));
        let mut store_burst = |until: Instant, keep: bool| -> Result<Vec<Duration>, String> {
            let mut d = Vec::new();
            while Instant::now() < until {
                let slot = i_s % slots;
                ctx.pool.fill(STREAM, i_s, &mut src);
                let pos = pages
                    .pos(slot * KIB4 as u64)
                    .map_err(|e| sioerr("PageRegion::pos", &e))?;
                let t = Instant::now();
                let _rc = pages
                    .write_durable(pos, &src)
                    .map_err(|e| sioerr("write_durable", &e))?;
                if keep {
                    d.push(t.elapsed());
                }
                last_s[slot as usize] = i_s;
                i_s += 1;
            }
            Ok(d)
        };
        let mut raw_src = vec![0u8; KIB4];
        let mut raw_burst = |until: Instant, keep: bool| -> Result<Vec<Duration>, String> {
            let mut d = Vec::new();
            while Instant::now() < until {
                let slot = i_r % slots;
                ctx.pool.fill(STREAM + 1, i_r, &mut raw_src);
                let t = Instant::now();
                abuf.as_mut_slice().copy_from_slice(&raw_src);
                f.write_at(abuf.as_slice(), slot * KIB4 as u64)
                    .map_err(|e| ioerr("raw write", &e))?;
                f.flush_data().map_err(|e| ioerr("raw flush", &e))?;
                if keep {
                    d.push(t.elapsed());
                }
                last_r[slot as usize] = i_r;
                i_r += 1;
            }
            Ok(d)
        };
        let warm = Duration::from_secs_f64(ctx.warmup.max(0.1));
        let _ = store_burst(Instant::now() + warm, false)?;
        let _ = raw_burst(Instant::now() + warm, false)?;
        let (mut s, mut r, mut diff) = (Vec::new(), Vec::new(), Vec::new());
        let mut ratio = Vec::new();
        let mean = |d: &[Duration]| d.iter().sum::<Duration>().as_secs_f64() / d.len() as f64;
        let (mut all_s, mut all_r) = (Vec::new(), Vec::new());
        for round in 0..ROUNDS {
            let (mut a, mut b) = if round % 2 == 0 {
                let a = store_burst(Instant::now() + burst, true)?;
                (a, raw_burst(Instant::now() + burst, true)?)
            } else {
                let b = raw_burst(Instant::now() + burst, true)?;
                (store_burst(Instant::now() + burst, true)?, b)
            };
            let (ps, pr) = (p50_us(&mut a), p50_us(&mut b));
            s.push(ps);
            r.push(pr);
            diff.push(ps - pr);
            ratio.push(mean(&b) / mean(&a));
            all_s.extend_from_slice(&a);
            all_r.extend_from_slice(&b);
        }
        rounds.clone_from(&diff);
        p.notes.push(format!(
            "per round p50 (us), store-io / raw: {}",
            s.iter()
                .zip(&r)
                .map(|(a, b)| format!("{a:.1}/{b:.1}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        p.metric("raw_p50_us", Some(median(&mut r)));
        p.metric("diff_p50_us", Some(median(&mut diff)));
        // `median` sorted `diff`.
        p.metric("diff_lo_us", diff.get(BOUND - 1).copied());
        p.metric(
            "diff_hi_us",
            diff.len()
                .checked_sub(BOUND)
                .and_then(|i| diff.get(i))
                .copied(),
        );
        p.metric("pooled_rate_ratio", Some(mean(&all_r) / mean(&all_s)));
        p.metric(
            "pooled_diff_us",
            Some(p50_us(&mut all_s) - p50_us(&mut all_r)),
        );
        p.metric("rate_ratio", Some(median(&mut ratio)));
        // `median` sorted `ratio`.
        p.metric("rate_ratio_lo", ratio.get(BOUND - 1).copied());
        p.metric(
            "rate_ratio_hi",
            ratio
                .len()
                .checked_sub(BOUND)
                .and_then(|i| ratio.get(i))
                .copied(),
        );
        // Read-back: store-io's pages, then the raw file's slots.
        let mut back = vec![0u8; KIB4];
        let written: Vec<(u64, u64)> = last_s
            .iter()
            .enumerate()
            .filter(|(_, i)| **i != u64::MAX)
            .map(|(k, i)| (k as u64, *i))
            .collect();
        let idx = sample(written.len(), CHECKS);
        for &k in &idx {
            let (slot, i) = written[k];
            let pos = pages
                .pos(slot * KIB4 as u64)
                .map_err(|e| sioerr("PageRegion::pos", &e))?;
            let n = pages
                .read(pos, &mut back)
                .map_err(|e| sioerr("PageRegion::read", &e))?;
            if n != KIB4 || !ctx.pool.matches(STREAM, i, &back) {
                p.verify =
                    Verify::Failed(format!("store-io page {slot} reads back different bytes"));
                return Ok(());
            }
        }
        let raw_verify = super::lone::verify_raw_slots(ctx, &f, &last_r, STREAM + 1, KIB4, CHECKS);
        p.verify = match raw_verify {
            Verify::Passed(m) => Verify::Passed(format!(
                "{} of {} store-io pages read back byte for byte; raw: {m}",
                idx.len(),
                written.len()
            )),
            other => other,
        };
        Ok(())
    });
    if let Some(e) = remove(&raw_path) {
        out.notes.push(e);
    }
    out
}

/// Reads timed per batch for the commit's clock read.
const CLOCK_BATCH: u32 = 10_000;
/// Batches timed.
const CLOCK_BATCHES: usize = 200;

/// Linux clock sources whose `CLOCK_MONOTONIC` read the vDSO can serve
/// (kvm-clock and xen only when the host marks the clock stable, arm64's
/// counter not on parts whose timer errata turn it off, none under
/// `vdso=0`): a name is not proof, so the label also judges the measured
/// cost.
#[cfg(target_os = "linux")]
const VDSO_SOURCES: [&str; 5] = [
    "tsc",
    "kvm-clock",
    "hyperv_clocksource_tsc_page",
    "arch_sys_counter",
    "xen",
];

/// A read slower than this is taken for a kernel transition (a system
/// call, or a slow timer such as HPET), whatever the clock's name: a
/// user-space read of a TSC or arch counter is tens of nanoseconds.
const SLOW_NS: f64 = 100.0;

/// The verdict on the measured cost.
fn cost(ns: f64) -> String {
    if ns > SLOW_NS {
        format!("**{ns:.0} ns a read: slow, likely a system call per commit**")
    } else if ns.is_finite() {
        format!("{ns:.0} ns a read: a user-space read")
    } else {
        "not measured".to_owned()
    }
}

fn clock_label(ns: f64) -> String {
    #[cfg(target_os = "linux")]
    let clock = {
        let path = "/sys/devices/system/clocksource/clocksource0/current_clocksource";
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let src = s.trim();
                let kind = if VDSO_SOURCES.contains(&src) {
                    "vDSO-capable"
                } else {
                    "no vDSO read"
                };
                format!("CLOCK_MONOTONIC, clock source {src} ({kind})")
            }
            Err(e) => format!("CLOCK_MONOTONIC, clock source unknown ({path}: {e})"),
        }
    };
    #[cfg(windows)]
    let clock = "QueryPerformanceCounter".to_owned();
    #[cfg(not(any(target_os = "linux", windows)))]
    let clock = "the platform's monotonic clock".to_owned();
    format!(
        "{clock}; {} (a hot-loop figure: a lower bound on a commit's)",
        cost(ns)
    )
}

/// Times `batches` batches of `batch` reads of `clock`: the nanoseconds
/// per read of each batch, and how many reads went backwards.
fn time_clock(clock: &dyn Fn() -> u64, batches: usize, batch: u32) -> (Vec<f64>, u32) {
    let mut per = Vec::with_capacity(batches);
    let mut last = 0u64;
    let mut backwards = 0u32;
    for _ in 0..batches {
        let t = Instant::now();
        for _ in 0..batch {
            let now = std::hint::black_box(clock());
            if now < last {
                backwards += 1;
            }
            last = now;
        }
        per.push(t.elapsed().as_secs_f64() * 1e9 / f64::from(batch));
    }
    (per, backwards)
}

/// Times the platform's own clock (`Platform::clock`, the closure the
/// domain reads at each commit): [`CLOCK_BATCHES`] batches of
/// [`CLOCK_BATCH`] reads, the median per read.
/// The clock read every commit barrier adds (V3: `Domain::note_commit`
/// marks the device's commits active with the platform's `monotonic_ns`):
/// its label (the clock, its cost and the verdict on it) and its point.
pub fn clock_read(ctx: &Ctx) -> (String, Point) {
    use store_io_platform::Platform;
    let mut p = Point::new(System::StoreIo, "the commit barrier's clock read");
    let clock = match crate::sio::platform(&ctx.opts) {
        Ok(plat) => plat.clock(),
        Err(e) => {
            p.error = Some(e.clone());
            return (e, p);
        }
    };
    let (mut per, backwards) = time_clock(&*clock, CLOCK_BATCHES, CLOCK_BATCH);
    let ns = median(&mut per);
    let label = clock_label(ns);
    p.metric("ns_per_read", Some(ns));
    // `median` sorted `per`.
    p.metric("p90_ns", per.get(CLOCK_BATCHES * 9 / 10).copied());
    p.notes.push(label.clone());
    p.verify = if backwards == 0 {
        Verify::Passed(format!(
            "{} reads, never backwards",
            CLOCK_BATCHES as u64 * u64::from(CLOCK_BATCH)
        ))
    } else {
        Verify::Failed(format!("the clock went backwards {backwards} times"))
    };
    (label, p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_time_clock_counts_reads_that_go_backwards() {
        let n = std::sync::atomic::AtomicU64::new(0);
        // 10, 9, 8, ...: every read after the first goes backwards.
        let down = || 10 - n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (per, backwards) = time_clock(&down, 2, 5);
        assert_eq!(per.len(), 2);
        assert_eq!(backwards, 9);
        let up = || n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(time_clock(&up, 3, 4).1, 0);
    }

    #[test]
    fn test_the_platform_clock_reads_and_never_goes_backwards() {
        use store_io_platform::Platform;
        let plat = crate::sio::platform(&store_io_engine::StoreOptions::default())
            .unwrap_or_else(|e| panic!("{e}"));
        let clock = plat.clock();
        let (mut per, backwards) = time_clock(&*clock, 20, 1000);
        assert_eq!(backwards, 0);
        let ns = median(&mut per);
        // A slow clock source (HPET, an emulated guest) is still a clock:
        // only an absurd cost fails.
        assert!(ns > 0.0 && ns < 50_000.0, "{ns} ns per read");
        assert!(!clock_label(ns).is_empty());
    }

    #[test]
    fn test_a_slow_read_is_flagged_whatever_the_clock_is_called() {
        assert!(cost(30.0).contains("a user-space read"));
        assert!(cost(SLOW_NS + 1.0).contains("likely a system call"));
        assert_eq!(cost(f64::NAN), "not measured");
    }
}
