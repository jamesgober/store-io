//! G6's measurement: sequential 1 MiB appends kept 8 deep through a
//! `Submitter`, from the store's pool buffers (no copy: the engine layer's
//! sequential writer), then one `barrier_all`, against fio's best
//! sequential write. The simple API's `append(&[u8])` copies the caller's
//! bytes into an aligned buffer first; at memory speed that copy costs as
//! much as the write, so it is reported (W5), not gated.

use std::time::{Duration, Instant};

use store_io_core::error::{Error, NotWrittenCause};
use store_io_engine::completion::Done;
use store_io_engine::{AppendRegion, SubmitterConfig, WriteMode};

use super::stamp::{self, BLOCK};
use super::{Ctx, sample, sioerr, with_store};
use crate::report::{Point, System, Verify};
use crate::sio::Plat;

/// The log, recycled each time it fills.
const REGION: u64 = 1 << 30;
const CHUNK: u64 = 1 << 20;
const QD: usize = 8;
/// Appends read back after the run.
const CHECKS: usize = 256;

/// Fills the region with 1 MiB appends 8 deep from pool buffers until the
/// window ends, recycling it whenever it fills; MB/s over the measured
/// window plus one final `barrier_all` (fio's `--end_fsync`).
///
/// Integrity: append `k` of a generation must land at `k` MiB (one writer,
/// whole-block appends), every block of it is stamped with its block number
/// and the generation ([`stamp`]), and afterwards a sample of the last
/// generation's appends reads back word for word.
pub fn seq_write(ctx: &Ctx) -> Point {
    let p = Point::new(
        System::StoreIo,
        "Submitter append(1 MiB) x QD8 + barrier_all",
    );
    with_store(ctx, "g6", p, |store, p| {
        let mut wal = store
            .provision_append_region("g6", REGION)
            .map_err(|e| sioerr("provision", &e))?;
        let mut sub = store
            .submitter(SubmitterConfig::default().with_depth(QD as u32))
            .map_err(|e| sioerr("submitter", &e))?;
        let per = CHUNK / BLOCK as u64;
        let warm_end = Instant::now() + Duration::from_secs_f64(ctx.warmup);
        let stop = warm_end + Duration::from_secs_f64(ctx.secs);
        let (mut generation, mut k, mut live) = (1u64, 0u64, 0usize);
        let (mut measuring, mut t0, mut bytes) = (false, Instant::now(), 0u64);
        // The region is full: no more appends this generation (kept until
        // the recycle, so a drain does not stamp and offer a buffer again).
        let mut full = false;
        loop {
            let now = Instant::now();
            if !measuring && now >= warm_end {
                (measuring, t0, bytes) = (true, now, 0);
            }
            // A fresh generation always gets one append, so the read-back
            // has something to check even when time ran out mid-recycle.
            while live < QD && (now < stop || k == 0) && !full {
                let mut b = sub
                    .take_buf(CHUNK as usize)
                    .map_err(|e| sioerr("take_buf", &e))?;
                stamp::stamp(b.as_mut_slice(), k * per, generation);
                match sub.append(&wal, b, WriteMode::Plain, k) {
                    Ok(pos) if pos.offset() == k * CHUNK => {
                        k += 1;
                        live += 1;
                    }
                    Ok(pos) => {
                        p.verify = Verify::Failed(format!(
                            "append {k} of generation {generation} landed at {}, not {}",
                            pos.offset(),
                            k * CHUNK
                        ));
                        return Ok(());
                    }
                    Err(r)
                        if matches!(
                            r.error,
                            Error::NotWritten {
                                cause: NotWrittenCause::RegionFull,
                                ..
                            }
                        ) =>
                    {
                        full = true;
                        break;
                    }
                    Err(r) => return Err(sioerr("append", &r.error)),
                }
            }
            if live == 0 && (now >= stop || !full) {
                break;
            }
            if live > 0 {
                for c in sub.wait(1).map_err(|e| sioerr("wait", &e))? {
                    live -= 1;
                    match c.outcome.map_err(|e| sioerr("completion", &e))? {
                        Done::Written(_) => {
                            if measuring {
                                bytes += CHUNK;
                            }
                        }
                        other => return Err(format!("unexpected {other:?}")),
                    }
                }
            }
            if full && live == 0 {
                if Instant::now() >= stop {
                    // Over: keep this generation for the read-back.
                    break;
                }
                // Every append of this generation is reaped: a new one.
                wal = wal.recycle().map_err(|e| sioerr("recycle", &e))?;
                (generation, k, full) = (generation + 1, 0, false);
            }
        }
        sub.barrier_all(u64::MAX)
            .map_err(|r| sioerr("barrier_all", &r.error))?;
        // Until the barrier itself completes (a wait may end early).
        let mut flushed = false;
        while !flushed {
            for c in sub.wait(1).map_err(|e| sioerr("barrier wait", &e))? {
                match c.outcome.map_err(|e| sioerr("barrier", &e))? {
                    Done::Flushed if c.tag == u64::MAX => flushed = true,
                    other => return Err(format!("unexpected {other:?} (tag {})", c.tag)),
                }
            }
        }
        let secs = t0.elapsed().as_secs_f64();
        p.metric("mb_per_s", (secs > 0.0).then(|| bytes as f64 / secs / 1e6));
        p.metric("generations", Some(generation as f64));
        drop(sub);
        p.verify = check(&wal, k, generation, per);
        Ok(())
    })
}

/// Reads back a sample of the last generation's `k` appends.
fn check(wal: &AppendRegion<Plat>, k: u64, generation: u64, per: u64) -> Verify {
    if k == 0 {
        return Verify::Failed("the last generation holds no append".to_owned());
    }
    let mut out = vec![0u8; CHUNK as usize];
    let idx = sample(k as usize, CHECKS);
    for &i in &idx {
        let i = i as u64;
        match wal.read(i * CHUNK, &mut out) {
            Ok(n) if n == out.len() && stamp::holds(&out, i * per, generation) => {}
            Ok(_) => {
                return Verify::Failed(format!(
                    "append {i} of generation {generation} reads back wrong"
                ));
            }
            Err(e) => return Verify::Failed(sioerr("AppendRegion::read", &e)),
        }
    }
    Verify::Passed(format!(
        "every append landed where expected; {} of the last generation's {k} appends read back word for word",
        idx.len()
    ))
}
