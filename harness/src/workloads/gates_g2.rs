//! G2's measurement: the barrier alone (`AppendRegion::sync_through` after a
//! 4 KiB append, timed from the append's return) against the raw data flush
//! alone (`flush_data` after a 4 KiB write), alternated in short rounds as
//! G1 is (`gates_g1`): two long windows measured one after the other judge
//! the virtual machine's drift between them, not store-io. Each round runs
//! a burst of each side, the order swapped every round; G2 is judged at p50
//! and p99 on the worse of the median per-round difference and the pooled
//! difference, with the rounds' spread of differences as its uncertainty.

use std::time::{Duration, Instant};

use super::gates_g1::{BOUND, ROUNDS, median};
use super::lone::verify_raw_slots;
use super::{Ctx, KIB4, Rec, ioerr, remove, sioerr, verify_appends, with_store};
use crate::aligned::AlignedBuf;
use crate::raw::RawFile;
use crate::report::{Point, System, Verify};

/// Payload stream (distinct from G1's and W1's).
const STREAM: u64 = 21;
/// The append region (recycled when full) and the raw file.
const SPAN: u64 = 256 << 20;
/// Read-back checks per side.
const CHECKS: usize = 1024;

/// The `q` quantile of `d` (0 to 1), microseconds; NaN when empty.
fn quantile_us(d: &mut [Duration], q: f64) -> f64 {
    if d.is_empty() {
        return f64::NAN;
    }
    d.sort_unstable();
    let i = ((d.len() - 1) as f64 * q).round() as usize;
    d[i].as_secs_f64() * 1e6
}

/// The per-round differences and the pooled difference at one quantile.
struct Side {
    raw: Vec<f64>,
    diff: Vec<f64>,
}

impl Side {
    /// Records `keys`: raw, median difference, its interval's low and high
    /// ends, pooled difference.
    fn record(
        &mut self,
        p: &mut Point,
        keys: [&'static str; 5],
        all_s: &mut [Duration],
        all_r: &mut [Duration],
        q: f64,
    ) {
        p.metric(keys[0], Some(median(&mut self.raw)));
        p.metric(keys[1], Some(median(&mut self.diff)));
        // `median` sorted `diff`.
        p.metric(keys[2], self.diff.get(BOUND - 1).copied());
        p.metric(
            keys[3],
            self.diff
                .len()
                .checked_sub(BOUND)
                .and_then(|i| self.diff.get(i))
                .copied(),
        );
        p.metric(keys[4], Some(quantile_us(all_s, q) - quantile_us(all_r, q)));
    }
}

/// Runs the alternated measurement. The point carries, for `p50` and `p99`:
/// `raw_{q}_us` (the median over rounds of the raw flush's quantile),
/// `diff_{q}_us` (the median per-round difference, store-io minus raw),
/// `diff_{q}_lo_us` / `diff_{q}_hi_us` (the [`BOUND`]th smallest and largest
/// differences: about a 96% interval for the median) and `pooled_{q}_us`.
///
/// Integrity: every write carries its index's payload; afterwards a sample
/// of the last generation's appends and of the raw file's slots reads back
/// as written, byte for byte.
pub fn alternated(ctx: &Ctx) -> Point {
    let p = Point::new(System::StoreIo, "G2 alternated: sync_through vs raw flush");
    let raw_path = ctx.path("g2-raw.dat");
    let mut out = with_store(ctx, "g2", p, |store, p| {
        let mut wal = store
            .provision_append_region("g2", SPAN)
            .map_err(|e| sioerr("provision_append_region", &e))?;
        let (f, _) = RawFile::create_ready(&raw_path, SPAN).map_err(|e| ioerr("raw create", &e))?;
        let slots = SPAN / KIB4 as u64;
        let mut last_r = vec![u64::MAX; slots as usize];
        let mut recs: Vec<Rec> = Vec::new();
        let mut src = vec![0u8; KIB4];
        let mut abuf = AlignedBuf::zeroed(KIB4);
        let (mut i_s, mut i_r) = (0u64, 0u64);
        let burst = Duration::from_secs_f64((ctx.secs * 2.0 / ROUNDS as f64).max(0.2));
        let mut store_burst = |until: Instant, keep: bool| -> Result<Vec<Duration>, String> {
            let mut d = Vec::new();
            while Instant::now() < until {
                if wal.tail() + KIB4 as u64 > wal.len() {
                    wal = wal.recycle().map_err(|e| sioerr("recycle", &e))?;
                    recs.clear();
                }
                ctx.pool.fill(STREAM, i_s, &mut src);
                let tk = wal.append(&src).map_err(|e| sioerr("append", &e))?;
                let t = Instant::now();
                let _rc = wal
                    .sync_through(&tk)
                    .map_err(|e| sioerr("sync_through", &e))?;
                if keep {
                    d.push(t.elapsed());
                }
                recs.push(Rec {
                    off: tk.pos().offset(),
                    stream: STREAM,
                    index: i_s,
                    len: KIB4,
                    aligned: true,
                });
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
                abuf.as_mut_slice().copy_from_slice(&raw_src);
                f.write_at(abuf.as_slice(), slot * KIB4 as u64)
                    .map_err(|e| ioerr("raw write", &e))?;
                let t = Instant::now();
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
        let mut p50 = Side {
            raw: Vec::new(),
            diff: Vec::new(),
        };
        let mut p99 = Side {
            raw: Vec::new(),
            diff: Vec::new(),
        };
        let (mut all_s, mut all_r) = (Vec::new(), Vec::new());
        let mut per_round = Vec::new();
        for round in 0..ROUNDS {
            let (mut a, mut b) = if round % 2 == 0 {
                let a = store_burst(Instant::now() + burst, true)?;
                (a, raw_burst(Instant::now() + burst, true)?)
            } else {
                let b = raw_burst(Instant::now() + burst, true)?;
                (store_burst(Instant::now() + burst, true)?, b)
            };
            let (s50, r50) = (quantile_us(&mut a, 0.5), quantile_us(&mut b, 0.5));
            let (s99, r99) = (quantile_us(&mut a, 0.99), quantile_us(&mut b, 0.99));
            p50.raw.push(r50);
            p50.diff.push(s50 - r50);
            p99.raw.push(r99);
            p99.diff.push(s99 - r99);
            per_round.push(format!("{s50:.1}/{r50:.1}"));
            all_s.extend_from_slice(&a);
            all_r.extend_from_slice(&b);
        }
        p.notes.push(format!(
            "per round p50 (us), store-io / raw: {}",
            per_round.join(", ")
        ));
        let k50 = [
            "raw_p50_us",
            "diff_p50_us",
            "diff_p50_lo_us",
            "diff_p50_hi_us",
            "pooled_p50_us",
        ];
        let k99 = [
            "raw_p99_us",
            "diff_p99_us",
            "diff_p99_lo_us",
            "diff_p99_hi_us",
            "pooled_p99_us",
        ];
        p50.record(p, k50, &mut all_s, &mut all_r, 0.5);
        p99.record(p, k99, &mut all_s, &mut all_r, 0.99);
        let raw_verify = verify_raw_slots(ctx, &f, &last_r, STREAM + 1, KIB4, CHECKS);
        p.verify = match (verify_appends(&wal, &ctx.pool, &recs, CHECKS), raw_verify) {
            (Verify::Passed(a), Verify::Passed(b)) => {
                Verify::Passed(format!("store-io: {a}; raw: {b}"))
            }
            (Verify::Passed(_), other) | (other, _) => other,
        };
        Ok(())
    });
    if let Some(e) = remove(&raw_path) {
        out.notes.push(e);
    }
    out
}
