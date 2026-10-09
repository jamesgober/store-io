//! W3: caller batching: N records, one durability barrier.
//!
//! For N in {1, 10, 100, 1000} and record sizes {64 B, 512 B, 4 KiB}:
//!
//! - store-io (a): N × `AppendRegion::append` (each padded to a block, as
//!   the API documents), then one `sync_through` of the last ticket.
//! - store-io (b): an `AppendBatch` (reused across commits): N × `append`
//!   (records packed back to back), then `commit()`.
//! - raw (a'): N × (copy into a zero-padded block, one write), one flush:
//!   the same I/O shape as (a).
//! - raw (b'): pack the N records into one aligned buffer, pad to a block,
//!   one write, one flush: the same I/O shape as (b) at its best.
//! - fsys: N × `append` then one `sync_through`, and `append_batch` then one
//!   `sync_through` (window off: an explicit sync per commit is what the
//!   window would otherwise delay).

use std::time::Instant;

use super::{
    Ctx, Rec, Run, Snap, Step, fserr, ioerr, region_bytes, remove, sioerr, time_boxed_capped,
    verify_appends,
};
use crate::aligned::AlignedBuf;
use crate::fsys_cmp::{self, Journal};
use crate::json::Json;
use crate::raw::RawFile;
use crate::report::{Better, Comparison, Point, Section, System, Table, Verify};
use crate::sio::SioStore;

/// Records per commit.
pub const COUNTS: [usize; 4] = [1, 10, 100, 1000];
/// Record sizes.
pub const SIZES: [usize; 3] = [64, 512, 4096];
/// Size of the raw file.
const SPAN: u64 = 1 << 30;
/// Read-back checks per point.
const CHECKS: usize = 1024;
/// A point ends at the time box or after this many measured commits,
/// whichever comes first (72 points must fit the run budget; 3000 samples
/// still support p99.9).
pub const MAX_COMMITS: u64 = 3000;
/// Largest append region a point provisions.
const REGION_CAP: u64 = 2 << 30;
/// Block size assumed for sizing (the store's actual block size is used for
/// every I/O; this only sizes regions).
const BLOCK: u64 = 4096;

fn stream_of(n: usize, size: usize) -> u64 {
    3_000_000 + n as u64 * 10_000 + size as u64
}

/// Runs W3.
pub fn run(ctx: &Ctx) -> Section {
    let mut sec = Section::new("caller-batch", "W3: caller batch (N records, one barrier)");
    sec.method = vec![
        format!("N in {COUNTS:?}, record sizes {SIZES:?} bytes; one commit = N records made durable by one barrier; commits run back to back on one thread. Each point ends at the time box or after {MAX_COMMITS} measured commits, whichever comes first."),
        "store-io (a) `N x append + sync_through`: each append is its own write, padded to a block (documented behaviour). (b) `AppendBatch`: records packed back to back into pooled buffers, `commit()` = one reservation, the fewest writes (largest pooled buffer 1 MiB), one barrier. A fresh store per (N, size).".to_owned(),
        format!("raw (a') N writes of one zero-padded block each, then one flush; (b') the N records packed into one aligned buffer padded to a block, one write, one flush. Primitive: {}. A ready 1 GiB file, offsets cycling.", crate::raw::PRIMITIVE),
        "fsys: `append` x N + `sync_through(last lsn)`, and `append_batch(&records)` + `sync_through`, on a fresh journal per point, window off.".to_owned(),
        "records/s = commits/s x N; payload MB/s counts caller bytes only (10^6 bytes); `write calls/commit` and `bytes written/commit` come from the OS counters and show the device writes each design issues per commit.".to_owned(),
    ];
    let raw_path = ctx.path("w3-raw.dat");
    let raw_file = RawFile::create_ready(&raw_path, SPAN);
    let mut pts = Vec::new();
    for &n in &COUNTS {
        for &size in &SIZES {
            let (a, b) = store_points(ctx, n, size);
            pts.push(a);
            pts.push(b);
            match &raw_file {
                Ok((f, _)) => {
                    pts.push(raw_point(ctx, f, n, size, false));
                    pts.push(raw_point(ctx, f, n, size, true));
                }
                Err(e) => {
                    let mut p = Point::new(System::Raw, "raw").param("n", Json::Num(n as f64));
                    p.error = Some(ioerr("raw create", e));
                    pts.push(p);
                }
            }
            pts.push(fsys_point(ctx, n, size, false));
            pts.push(fsys_point(ctx, n, size, true));
        }
    }
    drop(raw_file);
    if let Some(e) = remove(&raw_path) {
        sec.anomalies.push(e);
    }

    let mut t = Table::new(
        "Results:",
        &[
            "N",
            "record B",
            "system",
            "variant",
            "commits/s",
            "records/s",
            "payload MB/s",
            "commit p50 µs",
            "commit p99 µs",
            "write calls/commit",
            "bytes written/commit",
            "flushes/commit",
        ],
    );
    for p in &pts {
        t.row(vec![
            p.cell("n", 0),
            p.cell("record_bytes", 0),
            p.system.name().to_owned(),
            p.variant.clone(),
            p.cell("commits_per_s", 0),
            p.cell("records_per_s", 0),
            p.cell("payload_mb_per_s", 2),
            p.cell("p50_us", 1),
            p.cell("p99_us", 1),
            p.cell("write_calls_per_op", 2),
            p.cell("write_bytes_per_op", 0),
            p.cell("flushes_per_op", 2),
        ]);
    }
    sec.tables.push(t);

    for &n in &COUNTS {
        for &size in &SIZES {
            let get = |sys: System, v: &str| {
                pts.iter()
                    .find(|p| {
                        p.system == sys
                            && p.variant.starts_with(v)
                            && p.get("n") == Some(n as f64)
                            && p.get("record_bytes") == Some(size as f64)
                    })
                    .and_then(|p| p.get("records_per_s"))
            };
            sec.comparisons.push(Comparison {
                case: format!("batch of {n} x {size} B, one barrier"),
                metric: "records/s".to_owned(),
                better: Better::Higher,
                store_io: get(System::StoreIo, "(b)"),
                raw: get(System::Raw, "(b')"),
                fsys: get(System::Fsys, "append_batch"),
                note: "AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through".to_owned(),
            });
            sec.comparisons.push(Comparison {
                case: format!("{n} x append({size} B) + one sync_through"),
                metric: "records/s".to_owned(),
                better: Better::Higher,
                store_io: get(System::StoreIo, "(a)"),
                raw: get(System::Raw, "(a')"),
                fsys: get(System::Fsys, "append x N"),
                note: "N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through".to_owned(),
            });
        }
    }
    sec.points = pts;
    sec.collect_point_anomalies();
    sec
}

/// Records a commit loop: rate is commits/s; records/s and MB/s derived.
fn record(p: &mut Point, run: &Run, n: usize, size: usize) {
    super::record_run(p, run, "commits_per_s");
    let c = run.rate();
    p.metric("records_per_s", c.map(|c| c * n as f64));
    p.metric("payload_mb_per_s", c.map(|c| c * (n * size) as f64 / 1e6));
}

fn point(sys: System, variant: &str, n: usize, size: usize) -> Point {
    Point::new(sys, variant)
        .param("n", Json::Num(n as f64))
        .param("record_bytes", Json::Num(size as f64))
}

fn store_points(ctx: &Ctx, n: usize, size: usize) -> (Point, Point) {
    let mut a = point(
        System::StoreIo,
        "(a) N x AppendRegion::append + sync_through",
        n,
        size,
    );
    let mut b = point(
        System::StoreIo,
        "(b) AppendBatch: N x append + commit",
        n,
        size,
    );
    match ctx.store(&format!("w3-{n}-{size}")) {
        Ok((store, dir)) => {
            if let Err(e) = store_unbatched(ctx, &store, n, size, &mut a) {
                a.error = Some(e);
            }
            if let Err(e) = store_batched(ctx, &store, n, size, &mut b) {
                b.error = Some(e);
            }
            drop(store);
            if let Some(e) = remove(&dir) {
                b.notes.push(e);
            }
        }
        Err(e) => {
            a.error = Some(e.clone());
            b.error = Some(e);
        }
    }
    (a, b)
}

fn store_unbatched(
    ctx: &Ctx,
    store: &SioStore,
    n: usize,
    size: usize,
    p: &mut Point,
) -> Result<(), String> {
    let stream = stream_of(n, size);
    let per_commit = n as u64 * size.div_ceil(BLOCK as usize) as u64 * BLOCK;
    let bytes = region_bytes(ctx, per_commit, 5_000.0, REGION_CAP);
    let wal = super::provision_append(store, "a", bytes, p)?;
    let block = wal.block_size();
    let need = n as u64 * (size as u64).div_ceil(block) * block;
    let mut bufs: Vec<Vec<u8>> = (0..n).map(|_| vec![0u8; size]).collect();
    let mut recs = Vec::new();
    let mut offs = vec![0u64; n];
    let run = time_boxed_capped(ctx, MAX_COMMITS, &|| Snap::store(store), |c| {
        if wal.tail() + need > wal.len() {
            return Ok(Step::Full);
        }
        for (k, b) in bufs.iter_mut().enumerate() {
            ctx.pool.fill(stream, c * n as u64 + k as u64, b);
        }
        let t = Instant::now();
        let mut last = None;
        for (k, b) in bufs.iter().enumerate() {
            let tk = wal.append(b).map_err(|e| sioerr("append", &e))?;
            offs[k] = tk.pos().offset();
            last = Some(tk);
        }
        if let Some(tk) = last {
            let _rc = wal
                .sync_through(&tk)
                .map_err(|e| sioerr("sync_through", &e))?;
        }
        let d = t.elapsed();
        for (k, off) in offs.iter().enumerate() {
            recs.push(Rec {
                off: *off,
                stream,
                index: c * n as u64 + k as u64,
                len: size,
                aligned: true,
            });
        }
        Ok(Step::Op(d))
    })?;
    record(p, &run, n, size);
    p.verify = verify_appends(&wal, &ctx.pool, &recs, CHECKS);
    Ok(())
}

fn store_batched(
    ctx: &Ctx,
    store: &SioStore,
    n: usize,
    size: usize,
    p: &mut Point,
) -> Result<(), String> {
    let stream = stream_of(n, size) + 1;
    let per_commit = ((n * size) as u64).div_ceil(BLOCK) * BLOCK;
    let bytes = region_bytes(ctx, per_commit, 5_000.0, REGION_CAP);
    let wal = super::provision_append(store, "b", bytes, p)?;
    let block = wal.block_size();
    let need = ((n * size) as u64).div_ceil(block) * block;
    let mut bufs: Vec<Vec<u8>> = (0..n).map(|_| vec![0u8; size]).collect();
    let mut batch = wal.batch();
    let mut recs = Vec::new();
    let mut rel = vec![0u64; n];
    let mut short = 0u64;
    let run = time_boxed_capped(ctx, MAX_COMMITS, &|| Snap::store(store), |c| {
        if wal.tail() + need > wal.len() {
            return Ok(Step::Full);
        }
        for (k, b) in bufs.iter_mut().enumerate() {
            ctx.pool.fill(stream, c * n as u64 + k as u64, b);
        }
        let t = Instant::now();
        for (k, b) in bufs.iter().enumerate() {
            rel[k] = batch
                .append(b)
                .map_err(|e| sioerr("AppendBatch::append", &e))?;
        }
        let (pos, rc) = batch
            .commit()
            .map_err(|e| sioerr("AppendBatch::commit", &e))?;
        let d = t.elapsed();
        if rc.durable_through() < pos.offset() + (n * size) as u64 {
            short += 1;
        }
        for (k, r) in rel.iter().enumerate() {
            recs.push(Rec {
                off: pos.offset() + r,
                stream,
                index: c * n as u64 + k as u64,
                len: size,
                aligned: false,
            });
        }
        Ok(Step::Op(d))
    })?;
    record(p, &run, n, size);
    p.verify = verify_appends(&wal, &ctx.pool, &recs, CHECKS);
    if short > 0 {
        p.verify = Verify::Failed(format!("{short} commit receipts did not cover their batch"));
    }
    Ok(())
}

fn raw_point(ctx: &Ctx, f: &RawFile, n: usize, size: usize, packed: bool) -> Point {
    let variant = if packed {
        "(b') packed: one write of N records + one flush"
    } else {
        "(a') N writes of one padded block + one flush"
    };
    let mut p = point(System::Raw, variant, n, size);
    let stream = stream_of(n, size) + if packed { 3 } else { 2 };
    let block = BLOCK as usize;
    let per_rec = size.div_ceil(block) * block;
    let commit_len = if packed {
        (n * size).div_ceil(block) * block
    } else {
        n * per_rec
    };
    let mut bufs: Vec<Vec<u8>> = (0..n).map(|_| vec![0u8; size]).collect();
    let mut abuf = AlignedBuf::zeroed(commit_len);
    let commits_in_file = SPAN / commit_len as u64;
    let mut last_commit = None;
    let r = time_boxed_capped(ctx, MAX_COMMITS, &Snap::os, |c| {
        for (k, b) in bufs.iter_mut().enumerate() {
            ctx.pool.fill(stream, c * n as u64 + k as u64, b);
        }
        let base = (c % commits_in_file) * commit_len as u64;
        let t = Instant::now();
        if packed {
            let dst = abuf.as_mut_slice();
            for (k, b) in bufs.iter().enumerate() {
                dst[k * size..(k + 1) * size].copy_from_slice(b);
            }
            dst[n * size..commit_len].fill(0);
            f.write_at(&abuf.as_slice()[..commit_len], base)
                .map_err(|e| ioerr("raw write", &e))?;
        } else {
            for (k, b) in bufs.iter().enumerate() {
                let dst = &mut abuf.as_mut_slice()[k * per_rec..(k + 1) * per_rec];
                dst[..size].copy_from_slice(b);
                dst[size..].fill(0);
                f.write_at(dst, base + (k * per_rec) as u64)
                    .map_err(|e| ioerr("raw write", &e))?;
            }
        }
        f.flush_data().map_err(|e| ioerr("raw flush", &e))?;
        let d = t.elapsed();
        last_commit = Some(c);
        Ok(Step::Op(d))
    });
    match r {
        Ok(run) => {
            record(&mut p, &run, n, size);
            p.verify = match last_commit {
                Some(c) => verify_raw_commit(
                    ctx,
                    f,
                    c,
                    n,
                    size,
                    packed,
                    commit_len,
                    commits_in_file,
                    stream,
                ),
                None => Verify::NotChecked("no commit".to_owned()),
            };
        }
        Err(e) => p.error = Some(e),
    }
    p
}

#[allow(clippy::too_many_arguments)]
fn verify_raw_commit(
    ctx: &Ctx,
    f: &RawFile,
    c: u64,
    n: usize,
    size: usize,
    packed: bool,
    commit_len: usize,
    commits_in_file: u64,
    stream: u64,
) -> Verify {
    let base = (c % commits_in_file) * commit_len as u64;
    let mut out = AlignedBuf::zeroed(commit_len);
    match f.read_at(out.as_mut_slice(), base) {
        Ok(x) if x == commit_len => {}
        Ok(x) => return Verify::Failed(format!("short raw read {x} of {commit_len}")),
        Err(e) => return Verify::Failed(ioerr("raw read", &e)),
    }
    let per_rec = if packed {
        size
    } else {
        size.div_ceil(BLOCK as usize) * BLOCK as usize
    };
    for k in 0..n {
        let got = &out.as_slice()[k * per_rec..k * per_rec + size];
        if !ctx.pool.matches(stream, c * n as u64 + k as u64, got) {
            return Verify::Failed(format!(
                "last commit, record {k} reads back different bytes"
            ));
        }
    }
    Verify::Passed(format!(
        "all {n} records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle)"
    ))
}

fn fsys_point(ctx: &Ctx, n: usize, size: usize, batch: bool) -> Point {
    let variant = if batch {
        "append_batch(N) + sync_through (window off)"
    } else {
        "append x N + sync_through (window off)"
    };
    let mut p = point(System::Fsys, variant, n, size);
    let stream = stream_of(n, size) + if batch { 5 } else { 4 };
    let path = ctx.path("w3-fsys.wal");
    let r = (|| -> Result<(), String> {
        let j = ctx
            .fsys
            .journal_with(&path, Journal::WindowOff.options())
            .map_err(|e| fserr("journal_with", &e))?;
        let mut bufs: Vec<Vec<u8>> = (0..n).map(|_| vec![0u8; size]).collect();
        let run = time_boxed_capped(ctx, MAX_COMMITS, &Snap::os, |c| {
            for (k, b) in bufs.iter_mut().enumerate() {
                ctx.pool.fill(stream, c * n as u64 + k as u64, b);
            }
            let refs: Vec<&[u8]> = if batch {
                bufs.iter().map(Vec::as_slice).collect()
            } else {
                Vec::new()
            };
            let t = Instant::now();
            let lsn = if batch {
                j.append_batch(&refs)
                    .map_err(|e| fserr("append_batch", &e))?
            } else {
                let mut lsn = None;
                for b in &bufs {
                    lsn = Some(j.append(b).map_err(|e| fserr("append", &e))?);
                }
                lsn.ok_or_else(|| "empty commit".to_owned())?
            };
            j.sync_through(lsn).map_err(|e| fserr("sync_through", &e))?;
            Ok(Step::Op(t.elapsed()))
        })?;
        record(&mut p, &run, n, size);
        j.close().map_err(|e| fserr("close", &e))?;
        p.verify = fsys_cmp::verify_journal(&ctx.pool, &path, run.total_ops * n as u64);
        Ok(())
    })();
    if let Err(e) = r {
        p.error = Some(e);
    }
    if let Some(e) = remove(&path) {
        p.notes.push(e);
    }
    p
}
