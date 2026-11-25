//! G20's measurement (R5.5, FILEMAP's gates row): a `Read`-class
//! [`PageRegion::read_stream`](store_io_engine::PageRegion::read_stream)
//! over a whole region, front to back, repeated for the measured time,
//! alternating with fio's best 1 MiB sequential read (its QD8 and QD1
//! reads, the better of the two each round) in [`paired::ROUNDS`] rounds,
//! the side first alternating, judged on the rounds' ratios ([`paired`]:
//! a runner's speed drifts between runs, and each round's ratio judges
//! store-io, not the drift). A `Background`-class stream's figure is
//! published beside it, never gated.

use std::time::{Duration, Instant};

use store_io_core::IoClass;
use store_io_engine::{PageRegion, StreamOptions};
use store_io_platform::Platform;

use super::paired::{self, median};
use super::stamp::{self, BLOCK};
use super::{Ctx, sioerr, with_store};
use crate::report::{Point, System, Verify};

/// The region streamed.
const SPAN: u64 = 256 << 20;
/// Runs of the published `Background` stream.
const QUIET: usize = 3;
const MIB: u64 = 1 << 20;

/// Streams a written region repeatedly; MB/s over the time inside the
/// streams, alternating with `fio` (one round of the baseline: its QD8
/// and QD1 reads, in MB/s, the better of the two taken) in
/// [`paired::ROUNDS`] rounds: the rounds' ratios as [`paired::record`]
/// names them, `mb_per_s` and `fio_mb_per_s` the medians of each side,
/// `spread` the larger relative range of the two; `background_mb_per_s` a
/// `Background` stream's median of [`QUIET`] runs, measured after. Every
/// block holds its own pattern ([`stamp`]): the measured streams check each
/// chunk's offset (in order, no gap) and each block's first word; one more
/// stream afterwards checks every word.
pub fn stream(ctx: &Ctx, fio: &mut dyn FnMut() -> Result<(f64, f64), String>) -> Point {
    let p = Point::new(System::StoreIo, "PageRegion::read_stream, Read class");
    with_store(ctx, "g20", p, |store, p| {
        let pages = store
            .provision_page_region("g20", SPAN)
            .map_err(|e| sioerr("provision", &e))?;
        let mut chunk = vec![0u8; MIB as usize];
        for i in 0..SPAN / MIB {
            stamp::stamp(&mut chunk, i * MIB / BLOCK as u64, 0);
            let pos = pages.pos(i * MIB).map_err(|e| sioerr("pos", &e))?;
            let _t = pages.write(pos, &chunk).map_err(|e| sioerr("write", &e))?;
        }
        let read = StreamOptions::default().class(IoClass::Read);
        let background = StreamOptions::default();
        let warm_end = Instant::now() + Duration::from_secs_f64(ctx.warmup);
        while Instant::now() < warm_end {
            if let (_, Some(why)) = pass(&pages, read, false)? {
                p.verify = Verify::Failed(why);
                return Ok(());
            }
        }
        let (mut ours, mut theirs, mut passes) = (Vec::new(), Vec::new(), 0u64);
        let mut rounds = Vec::new();
        for i in 0..paired::ROUNDS {
            let first = paired::ours_first(i);
            for ours_turn in [first, !first] {
                if ours_turn {
                    match timed(ctx, &pages, read, &mut passes)? {
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
        // The gated figures first: the published `Background` run below
        // never loses them.
        let (m, f) = (median(&ours), median(&theirs));
        p.metric("mb_per_s", Some(m));
        p.metric("fio_mb_per_s", Some(f));
        p.metric("spread", Some(range(&ours, m).max(range(&theirs, f))));
        paired::record(p, &ours, &theirs);
        p.metric("streams", Some(passes as f64));
        p.notes.push(format!(
            "alternated with fio (store-io first in the 2nd, 4th, ... rounds): Read stream {ours:.0?} MB/s, fio's best {theirs:.0?} MB/s (QD8, QD1 each round: {rounds:.0?})"
        ));
        // Published, not gated, after the rounds: a refusal for buffers
        // (`PoolExhausted`, in the error's text) is noted and leaves the
        // gate to the `Read` stream; any other error fails the point, as do
        // wrong bytes (a stream's integrity failure).
        let mut quiet = Vec::new();
        for _ in 0..QUIET {
            match timed(ctx, &pages, background, &mut passes) {
                Ok(Ok(mb)) => quiet.push(mb),
                Ok(Err(why)) => {
                    p.verify = Verify::Failed(format!("the Background stream: {why}"));
                    return Ok(());
                }
                Err(why) if why.contains("PoolExhausted") => {
                    p.notes
                        .push(format!("the Background stream not measured: {why}"));
                    break;
                }
                Err(why) => return Err(format!("the Background stream: {why}")),
            }
        }
        if !quiet.is_empty() {
            let b = median(&quiet);
            p.metric("background_mb_per_s", Some(b));
            // Against fio's median: comparable from run to run, though
            // measured after the rounds, not alternated.
            p.metric("background_ratio", (f > 0.0).then(|| b / f));
            p.notes.push(format!(
                "the Background stream, after the rounds: {quiet:.0?} MB/s"
            ));
        }
        p.verify = match pass(&pages, read, true)? {
            (_, Some(why)) => Verify::Failed(why),
            (_, None) => Verify::Passed(format!(
                "every measured stream delivered all {} MiB in order, each block's first word checked; a final stream checked every word",
                SPAN / MIB
            )),
        };
        Ok(())
    })
}

/// Streams for the measured time: MB/s over the time inside the streams,
/// or why a stream's bytes were wrong.
fn timed<P: Platform>(
    ctx: &Ctx,
    pages: &PageRegion<P>,
    opts: StreamOptions,
    passes: &mut u64,
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
        *passes += 1;
    }
    Ok(Ok(bytes as f64 / inside.as_secs_f64().max(1e-9) / 1e6))
}

/// One stream over the whole region: the bytes delivered, and why they
/// were wrong (`full` checks every word, else each block's first).
fn pass<P: Platform>(
    pages: &PageRegion<P>,
    opts: StreamOptions,
    full: bool,
) -> Result<(u64, Option<String>), String>
where
    P::Queue: Send,
{
    let stream = pages
        .read_stream_with(0..SPAN, opts)
        .map_err(|e| sioerr("read_stream", &e))?;
    let mut expect = 0u64;
    for item in stream {
        let c = match item {
            Ok(c) => c,
            Err(e) => return Ok((expect, Some(format!("an item at {expect}: {e}")))),
        };
        let (offset, bytes) = (c.offset(), c.bytes());
        if offset != expect || offset % BLOCK as u64 != 0 || bytes.len() % BLOCK != 0 {
            return Ok((
                expect,
                Some(format!(
                    "a chunk at {offset} of {}, expected {expect}",
                    bytes.len()
                )),
            ));
        }
        let first = offset / BLOCK as u64;
        let right = if full {
            stamp::holds(bytes, first, 0)
        } else {
            stamp::heads(bytes, first, 0)
        };
        if !right {
            return Ok((
                expect,
                Some(format!(
                    "[{offset}, {}) does not hold its blocks",
                    offset + bytes.len() as u64
                )),
            ));
        }
        expect = offset + bytes.len() as u64;
    }
    if expect != SPAN {
        return Ok((
            expect,
            Some(format!("the stream ended at {expect} of {SPAN}")),
        ));
    }
    Ok((expect, None))
}

/// A set's range relative to `m`.
fn range(v: &[f64], m: f64) -> f64 {
    let (lo, hi) = v
        .iter()
        .fold((f64::MAX, f64::MIN), |(l, h), x| (l.min(*x), h.max(*x)));
    if m > 0.0 { (hi - lo) / m } else { 0.0 }
}
