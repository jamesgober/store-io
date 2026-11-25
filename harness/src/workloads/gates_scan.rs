//! G7's scan measurement: a `Read`-class `PageRegion::scan_with` over a
//! whole region, front to back, repeated for the measured time (the API the
//! gate names; a `read` call fills and drains its own pipeline each time, a
//! scan keeps its reads in flight from start to end). A caller's default
//! scan is a background call (R5.4, `background_io` pieces): its figure is
//! published beside it, never gated, as G20 does for read streams.
//!
//! A virtual disk's read bandwidth swings several-fold between consecutive
//! runs (2.0 to 5.7 GB/s for two back-to-back fio runs, 2026-10-19), so the
//! scan alternates with the fio baseline in [`paired::ROUNDS`] rounds, the
//! side first alternating, judged on the rounds' ratios ([`paired`]).

use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use store_io_core::IoClass;
use store_io_engine::{PageRegion, ScanItem, ScanOptions};
use store_io_platform::Platform;

use super::paired::{self, median};
use super::stamp::{self, BLOCK};
use super::{Ctx, sioerr, with_store};
use crate::report::{Point, System, Verify};

/// The region scanned.
const SPAN: u64 = 256 << 20;
const MIB: u64 = 1 << 20;
/// Runs of the published `Background` scan.
const QUIET: usize = 3;

/// Scans a written region repeatedly with `Read`-class scans; MB/s over
/// the time inside the scans, alternating with `fio` (one round of the
/// baseline: its QD8 and QD1 reads, in MB/s, the better of the two taken)
/// in [`paired::ROUNDS`] rounds: the rounds' ratios as [`paired::record`]
/// names them, `mb_per_s` and `fio_mb_per_s` the medians of each side,
/// `spread` the larger relative range of the two; `background_mb_per_s` a
/// default (`Background`) scan's median of [`QUIET`] runs, measured after.
/// Every block holds its own pattern ([`stamp`], block number = region
/// offset / 4 KiB). The measured scans' visitor checks offsets (in order,
/// no gap, whole blocks) and each block's first word; one more scan
/// afterwards, outside the window, checks every word.
pub fn scan(ctx: &Ctx, fio: &mut dyn FnMut() -> Result<(f64, f64), String>) -> Point {
    let p = Point::new(System::StoreIo, "PageRegion::scan_with, Read class");
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
        let read = ScanOptions::default().class(IoClass::Read);
        let background = ScanOptions::default();
        let warm_end = Instant::now() + Duration::from_secs_f64(ctx.warmup);
        while Instant::now() < warm_end {
            if let (_, Some(why)) = pass(&pages, read, false)? {
                p.verify = Verify::Failed(why);
                return Ok(());
            }
        }
        let (mut ours, mut theirs, mut scans) = (Vec::new(), Vec::new(), 0u64);
        let mut rounds = Vec::new();
        for i in 0..paired::ROUNDS {
            let first = paired::ours_first(i);
            for ours_turn in [first, !first] {
                if ours_turn {
                    match timed(ctx, &pages, read, &mut scans)? {
                        Ok(mb) => ours.push(mb),
                        Err(why) => {
                            p.verify = Verify::Failed(why);
                            return Ok(());
                        }
                    }
                } else {
                    let (q8, q1) = fio()?;
                    rounds.push((q8, q1));
                    theirs.push(q8.max(q1));
                }
            }
        }
        // The gated figures first: the published `Background` runs below
        // never lose them.
        let (m, f) = (median(&ours), median(&theirs));
        p.metric("mb_per_s", Some(m));
        p.metric("fio_mb_per_s", Some(f));
        p.metric("spread", Some(range(&ours, m).max(range(&theirs, f))));
        paired::record(p, &ours, &theirs);
        p.metric("scans", Some(scans as f64));
        p.notes.push(format!(
            "alternated with fio (store-io first in the 2nd, 4th, ... rounds): Read scan {ours:.0?} MB/s, fio's best {theirs:.0?} MB/s (QD8, QD1 each round: {rounds:.0?})"
        ));
        // Published, not gated, after the rounds: a refusal for buffers
        // (`PoolExhausted`, in the error's text) is noted and leaves the
        // gate to the `Read` scan; any other error fails the point, as do
        // wrong bytes (a scan's integrity failure).
        let mut quiet = Vec::new();
        for _ in 0..QUIET {
            match timed(ctx, &pages, background, &mut scans) {
                Ok(Ok(mb)) => quiet.push(mb),
                Ok(Err(why)) => {
                    p.verify = Verify::Failed(format!("the Background scan: {why}"));
                    return Ok(());
                }
                Err(why) if why.contains("PoolExhausted") => {
                    p.notes
                        .push(format!("the Background scan not measured: {why}"));
                    break;
                }
                Err(why) => return Err(format!("the Background scan: {why}")),
            }
        }
        if !quiet.is_empty() {
            let b = median(&quiet);
            p.metric("background_mb_per_s", Some(b));
            // Against fio's median: comparable from run to run, though
            // measured after the rounds, not alternated.
            p.metric("background_ratio", (f > 0.0).then(|| b / f));
            p.notes.push(format!(
                "the Background scan, after the rounds: {quiet:.0?} MB/s"
            ));
        }
        p.verify = match pass(&pages, read, true)? {
            (_, Some(why)) => Verify::Failed(why),
            (_, None) => Verify::Passed(format!(
                "every measured scan delivered all {} MiB in order, each block's first word checked; a final scan checked every word",
                SPAN / MIB
            )),
        };
        Ok(())
    })
}

/// Scans for the measured time: MB/s over the time inside the scans, or
/// why a scan's bytes were wrong.
fn timed<P: Platform>(
    ctx: &Ctx,
    pages: &PageRegion<P>,
    opts: ScanOptions,
    scans: &mut u64,
) -> Result<Result<f64, String>, String>
where
    P::Queue: Send,
{
    let (mut bytes, mut inside) = (0u64, Duration::ZERO);
    let t0 = Instant::now();
    while t0.elapsed().as_secs_f64() < ctx.secs {
        let t = Instant::now();
        let (n, bad) = pass(pages, opts, false)?;
        inside += t.elapsed();
        if let Some(why) = bad {
            return Ok(Err(why));
        }
        bytes += n;
        *scans += 1;
    }
    Ok(Ok(bytes as f64 / inside.as_secs_f64().max(1e-9) / 1e6))
}

/// One scan of the whole region: the bytes delivered, and why they were
/// wrong (`full` checks every word, else each block's first).
fn pass<P: Platform>(
    pages: &PageRegion<P>,
    opts: ScanOptions,
    full: bool,
) -> Result<(u64, Option<String>), String>
where
    P::Queue: Send,
{
    let (mut expect, mut bad) = (0u64, None::<String>);
    let sum = pages
        .scan_with(0, opts, |item| match item {
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
}

/// A set's range relative to `m`.
fn range(v: &[f64], m: f64) -> f64 {
    let (lo, hi) = v
        .iter()
        .fold((f64::MAX, f64::MIN), |(l, h), x| (l.min(*x), h.max(*x)));
    if m > 0.0 { (hi - lo) / m } else { 0.0 }
}
