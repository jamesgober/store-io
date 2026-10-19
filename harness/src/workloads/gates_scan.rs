//! G7's scan measurement: `PageRegion::scan` over a whole region, front to
//! back, repeated for the measured time (the API the gate names; a
//! `read` call fills and drains its own pipeline each time, a scan keeps
//! its reads in flight from start to end).
//!
//! A virtual disk's read bandwidth swings several-fold between consecutive
//! runs (2.0 to 5.7 GB/s for two back-to-back fio runs, 2026-10-19), so the
//! scan alternates with the fio baseline, [`REPS`] times each, and the
//! medians are compared.

use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use store_io_engine::ScanItem;

use super::stamp::{self, BLOCK};
use super::{Ctx, sioerr, with_store};
use crate::report::{Point, System, Verify};

/// The region scanned.
const SPAN: u64 = 256 << 20;
/// Alternations of fio and the scan.
pub const REPS: usize = 3;
const MIB: u64 = 1 << 20;

/// Scans a written region repeatedly; MB/s over the time inside the scans,
/// alternating with `fio` (one baseline run, in MB/s): `mb_per_s` and
/// `fio_mb_per_s` are the medians of [`REPS`] each, `spread` the larger
/// relative range of the two.
/// Every block holds its own pattern ([`stamp`], block number = region
/// offset / 4 KiB). The measured scans' visitor checks offsets (in order,
/// no gap, whole blocks) and each block's first word; one more scan
/// afterwards, outside the window, checks every word.
pub fn scan(ctx: &Ctx, fio: &mut dyn FnMut() -> Result<f64, String>) -> Point {
    let p = Point::new(System::StoreIo, "PageRegion::scan, whole region");
    with_store(ctx, "g7", p, |store, p| {
        let pages = store
            .provision_page_region("g7", SPAN)
            .map_err(|e| sioerr("provision", &e))?;
        let mut chunk = vec![0u8; MIB as usize];
        for i in 0..SPAN / MIB {
            stamp::stamp(&mut chunk, i * MIB / BLOCK as u64, 0);
            let pos = pages.pos(i * MIB).map_err(|e| sioerr("pos", &e))?;
            let _t = pages.write(pos, &chunk).map_err(|e| sioerr("write", &e))?;
        }
        // One pass: `full` checks every word, else each block's first.
        let pass = |full: bool| -> Result<(u64, Option<String>), String> {
            let (mut expect, mut bad) = (0u64, None::<String>);
            let sum = pages
                .scan(0, |item| match item {
                    ScanItem::Data { offset, bytes } => {
                        if offset != expect || offset % BLOCK as u64 != 0 {
                            bad = Some(format!("data at {offset}, expected {expect}"));
                            return ControlFlow::Break(());
                        }
                        let first = offset / BLOCK as u64;
                        let right = if full {
                            stamp::holds(bytes, first, 0)
                        } else {
                            stamp::heads(bytes, first, 0)
                        };
                        if !right {
                            bad = Some(format!(
                                "[{offset}, {}) does not hold its blocks",
                                offset + bytes.len() as u64
                            ));
                            return ControlFlow::Break(());
                        }
                        expect = offset + bytes.len() as u64;
                        ControlFlow::Continue(())
                    }
                    ScanItem::Unreadable { start, end } => {
                        bad = Some(format!("[{start}, {end}) unreadable"));
                        ControlFlow::Break(())
                    }
                })
                .map_err(|e| sioerr("scan", &e))?;
            if bad.is_none() && (expect != SPAN || sum.end != SPAN) {
                bad = Some(format!("the scan ended at {} of {SPAN}", sum.end));
            }
            Ok((expect, bad))
        };
        let warm_end = Instant::now() + Duration::from_secs_f64(ctx.warmup);
        while Instant::now() < warm_end {
            if let (_, Some(why)) = pass(false)? {
                p.verify = Verify::Failed(why);
                return Ok(());
            }
        }
        let (mut ours, mut theirs, mut scans) = (Vec::new(), Vec::new(), 0u64);
        for _ in 0..REPS {
            theirs.push(fio()?);
            let (mut bytes, mut inside) = (0u64, Duration::ZERO);
            let t0 = Instant::now();
            while t0.elapsed().as_secs_f64() < ctx.secs {
                let t = Instant::now();
                let (n, bad) = pass(false)?;
                inside += t.elapsed();
                if let Some(why) = bad {
                    p.verify = Verify::Failed(why);
                    return Ok(());
                }
                bytes += n;
                scans += 1;
            }
            ours.push(bytes as f64 / inside.as_secs_f64().max(1e-9) / 1e6);
        }
        let (m, f) = (median(&ours), median(&theirs));
        p.metric("mb_per_s", Some(m));
        p.metric("fio_mb_per_s", Some(f));
        p.metric("spread", Some(range(&ours, m).max(range(&theirs, f))));
        p.metric("scans", Some(scans as f64));
        p.notes.push(format!(
            "alternated with fio: scan {ours:.0?} MB/s, fio {theirs:.0?} MB/s"
        ));
        p.verify = match pass(true)? {
            (_, Some(why)) => Verify::Failed(why),
            (_, None) => Verify::Passed(format!(
                "every measured scan delivered all {} MiB in order, each block's first word checked; a final scan checked every word",
                SPAN / MIB
            )),
        };
        Ok(())
    })
}

/// The median of a non-empty set.
fn median(v: &[f64]) -> f64 {
    let mut v = v.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// A set's range relative to `m`.
fn range(v: &[f64], m: f64) -> f64 {
    let (lo, hi) = v
        .iter()
        .fold((f64::MAX, f64::MIN), |(l, h), x| (l.min(*x), h.max(*x)));
    if m > 0.0 { (hi - lo) / m } else { 0.0 }
}
