//! W5: sequential bandwidth.
//!
//! - Write: 1 MiB appends (not durable), then one barrier at the end, into a
//!   1 GiB region (store-io `AppendRegion::append` + `sync_through`), against
//!   raw sequential unbuffered 1 MiB writes + one data flush into a ready
//!   1 GiB file, and fsys journal 1 MiB appends + one `sync_through`.
//! - Read: the 1 GiB just written, sequentially, via `AppendRegion::read` in
//!   1 MiB and 8 MiB requests, against raw unbuffered reads of the same
//!   request sizes (one call per request).

use std::time::{Duration, Instant};

use super::{Ctx, Snap, Step, fserr, ioerr, record_delta, remove, sioerr, time_boxed};
use crate::aligned::AlignedBuf;
use crate::fsys_cmp::{self, Journal};
use crate::json::Json;
use crate::raw::RawFile;
use crate::report::{Better, Comparison, Point, Section, System, Table, Verify};
use crate::sio::{Plat, SioStore};
use crate::stats::Samples;
use store_io_buf::{BufPool, PoolConfig};
use store_io_engine::AppendRegion;

/// Region / file size.
const SPAN: u64 = 1 << 30;
/// Write size.
const CHUNK: usize = 1 << 20;
/// Read request sizes.
const READS: [usize; 2] = [1 << 20, 8 << 20];
/// Payload stream.
const STREAM: u64 = 5;

/// Runs W5.
pub fn run(ctx: &Ctx) -> Section {
    let mut sec = Section::new(
        "sequential",
        "W5: sequential bandwidth (1 MiB writes, 1/8 MiB reads)",
    );
    sec.method = vec![
        "Write: one thread writes 1 MiB chunks back to back (not durable), then one durability barrier; MB/s = bytes / (time inside the write calls + the barrier); payload generation between writes is excluded (the wall clock is recorded as `wall_s` in the JSON). No warm-up (the region is written once); the point stops at 1 GiB or at the time box.".to_owned(),
        format!("store-io: `AppendRegion::append(1 MiB)` into a fresh 1 GiB append region, then `sync_through(last ticket)`. raw: copy into an aligned buffer + {} per chunk into a ready 1 GiB file, then one data flush. fsys: journal `append(1 MiB)` + one `sync_through` on a fresh journal (buffered and direct, window off; fsys extends its file as it appends, store-io and raw overwrite preallocated space).", if cfg!(windows) { "WriteFile" } else { "pwritev2" }),
        format!("Read: the bytes just written, front to back, repeated until the time box: store-io `AppendRegion::read(offset, &mut out)` with 1 MiB and 8 MiB `out`; raw {} with the same request sizes. Latency is per request; MB/s = bytes / time inside the read calls (the content spot-check between requests is excluded).", crate::raw::READ_PRIMITIVE),
        "fsys read: not measured. fsys 1.1.3 has no unbuffered positioned read (`Handle::read_at` opens the file buffered on every call, and `JournalReader` replays through a buffered reader), so it would measure the page cache, not the device.".to_owned(),
        "MB = 10^6 bytes.".to_owned(),
    ];
    let mut pts = Vec::new();

    // store-io: one store holds the written region for the read points.
    match ctx.store("w5") {
        Ok((store, dir)) => {
            let mut wp = Point::new(
                System::StoreIo,
                "AppendRegion::append(1 MiB) x N + sync_through",
            )
            .param("chunk_bytes", Json::Num(CHUNK as f64));
            match store_write(ctx, &store, &mut wp) {
                Ok((wal, written)) => {
                    pts.push(wp);
                    for &req in &READS {
                        pts.push(store_read(ctx, &wal, written, req));
                    }
                }
                Err(e) => {
                    wp.error = Some(e);
                    pts.push(wp);
                }
            }
            drop(store);
            if let Some(e) = remove(&dir) {
                sec.anomalies.push(e);
            }
        }
        Err(e) => {
            let mut p = Point::new(
                System::StoreIo,
                "AppendRegion::append(1 MiB) x N + sync_through",
            );
            p.error = Some(e);
            pts.push(p);
        }
    }

    let raw_path = ctx.path("w5-raw.dat");
    let mut wp = Point::new(System::Raw, "1 MiB writes + one data flush")
        .param("chunk_bytes", Json::Num(CHUNK as f64));
    match raw_write(ctx, &raw_path, &mut wp) {
        Ok((f, written)) => {
            pts.push(wp);
            for &req in &READS {
                pts.push(raw_read(ctx, &f, written, req));
            }
            pts.push(raw_read_pool(ctx, &f, written, CHUNK));
            pts.push(raw_read_pool_copy(ctx, &f, written, CHUNK));
            pts.push(raw_read_shifted(ctx, &f, written, CHUNK));
        }
        Err(e) => {
            wp.error = Some(e);
            pts.push(wp);
        }
    }
    if let Some(e) = remove(&raw_path) {
        sec.anomalies.push(e);
    }
    for j in [Journal::WindowOff, Journal::DirectWindowOff] {
        pts.push(fsys_write(ctx, j));
    }

    let mut t = Table::new(
        "Results:",
        &[
            "system",
            "variant",
            "request",
            "MB/s",
            "bytes",
            "seconds",
            "barrier ms",
            "request p50 µs",
            "request p99 µs",
            "request max µs",
            "write calls/request",
        ],
    );
    for p in &pts {
        t.row(vec![
            p.system.name().to_owned(),
            p.variant.clone(),
            p.cell("request_bytes", 0),
            p.cell("mb_per_s", 1),
            p.cell("bytes", 0),
            p.cell("seconds", 3),
            p.cell("barrier_ms", 2),
            p.cell("p50_us", 1),
            p.cell("p99_us", 1),
            p.cell("max_us", 1),
            p.cell("write_calls_per_op", 2),
        ]);
    }
    sec.tables.push(t);

    let get = |sys: System, v: &str, req: Option<usize>| {
        pts.iter()
            .find(|p| {
                p.system == sys
                    && p.variant.contains(v)
                    && req.is_none_or(|r| p.get("request_bytes") == Some(r as f64))
            })
            .and_then(|p| p.get("mb_per_s"))
    };
    let fsys_best = [Journal::WindowOff, Journal::DirectWindowOff]
        .iter()
        .filter_map(|j| get(System::Fsys, j.label(), None))
        .fold(None, |acc: Option<f64>, x| {
            Some(acc.map_or(x, |a| a.max(x)))
        });
    sec.comparisons.push(Comparison {
        case: "sequential 1 MiB writes, one barrier at the end".to_owned(),
        metric: "MB/s".to_owned(),
        better: Better::Higher,
        store_io: get(System::StoreIo, "append", None),
        raw: get(System::Raw, "writes", None),
        fsys: fsys_best,
        note: "fsys: the better of its buffered and direct journals".to_owned(),
    });
    for &req in &READS {
        sec.comparisons.push(Comparison {
            case: format!("sequential read, {} MiB requests", req >> 20),
            metric: "MB/s".to_owned(),
            better: Better::Higher,
            store_io: get(System::StoreIo, "read", Some(req)),
            raw: get(System::Raw, "read", Some(req)),
            fsys: None,
            note: "fsys: no comparable unbuffered read (see method)".to_owned(),
        });
    }
    sec.points = pts;
    sec.collect_point_anomalies();
    sec
}

/// Outcome of a sequential write loop.
struct Written {
    lat: Samples,
    bytes: u64,
    /// Time inside the write calls plus the barrier.
    total: Duration,
    barrier: Duration,
    /// Wall clock from the first write to the barrier's return (includes
    /// payload generation between writes).
    wall: Duration,
}

fn record_write(p: &mut Point, w: &Written, delta: &super::Delta) {
    let secs = w.total.as_secs_f64();
    p.metric("request_bytes", Some(CHUNK as f64));
    p.metric(
        "mb_per_s",
        (secs > 0.0).then(|| w.bytes as f64 / secs / 1e6),
    );
    p.metric("bytes", Some(w.bytes as f64));
    p.metric("seconds", Some(secs));
    p.metric("barrier_ms", Some(w.barrier.as_secs_f64() * 1e3));
    p.metric("wall_s", Some(w.wall.as_secs_f64()));
    p.metric(
        "mb_per_s_before_barrier",
        Some(w.bytes as f64 / (secs - w.barrier.as_secs_f64()).max(1e-9) / 1e6),
    );
    p.latency(w.lat.summary().as_ref());
    record_delta(p, delta, (w.bytes / CHUNK as u64).max(1));
}

fn store_write(
    ctx: &Ctx,
    store: &SioStore,
    p: &mut Point,
) -> Result<(AppendRegion<Plat>, u64), String> {
    let wal = super::provision_append(store, "seq", SPAN, p)?;
    let mut src = vec![0u8; CHUNK];
    let mut lat = Samples::with_capacity(1024);
    let before = Snap::store(store);
    let deadline = Duration::from_secs_f64(ctx.secs);
    let start = Instant::now();
    let mut last = None;
    let mut i = 0u64;
    while wal.tail() + CHUNK as u64 <= wal.len() && start.elapsed() < deadline {
        ctx.pool.fill(STREAM, i, &mut src);
        let t = Instant::now();
        let tk = wal.append(&src).map_err(|e| sioerr("append", &e))?;
        lat.push(t.elapsed());
        if tk.pos().offset() != i * CHUNK as u64 {
            return Err(format!(
                "append {i} landed at {} instead of {}",
                tk.pos().offset(),
                i * CHUNK as u64
            ));
        }
        last = Some(tk);
        i += 1;
    }
    let tb = Instant::now();
    let tk = last.ok_or_else(|| "no append completed".to_owned())?;
    let rc = wal
        .sync_through(&tk)
        .map_err(|e| sioerr("sync_through", &e))?;
    let barrier = tb.elapsed();
    let wall = start.elapsed();
    let total = lat.total() + barrier;
    let after = Snap::store(store);
    let bytes = i * CHUNK as u64;
    if rc.durable_through() < bytes {
        p.verify = Verify::Failed(format!(
            "receipt durable through {} < {bytes} bytes written",
            rc.durable_through()
        ));
    }
    record_write(
        p,
        &Written {
            lat,
            bytes,
            total,
            barrier,
            wall,
        },
        &super::Delta::between(&before, &after),
    );
    // Untimed integrity pass over everything written.
    if !matches!(p.verify, Verify::Failed(_)) {
        let mut out = vec![0u8; 8 << 20];
        let mut checked = 0u64;
        while checked < bytes {
            let len = ((bytes - checked) as usize).min(out.len());
            let got = wal
                .read(checked, &mut out[..len])
                .map_err(|e| sioerr("AppendRegion::read", &e))?;
            if got != len {
                p.verify = Verify::Failed(format!("short read at {checked}: {got} of {len}"));
                break;
            }
            for (k, c) in out[..len].chunks(CHUNK).enumerate() {
                let idx = checked / CHUNK as u64 + k as u64;
                if !ctx.pool.matches(STREAM, idx, c) {
                    p.verify = Verify::Failed(format!("chunk {idx} reads back different bytes"));
                    return Ok((wal, bytes));
                }
            }
            checked += len as u64;
        }
        if !matches!(p.verify, Verify::Failed(_)) {
            p.verify = Verify::Passed(format!(
                "all {} MiB read back through AppendRegion::read and compared byte for byte",
                bytes >> 20
            ));
        }
    }
    Ok((wal, bytes))
}

fn store_read(ctx: &Ctx, wal: &AppendRegion<Plat>, written: u64, req: usize) -> Point {
    let mut p = Point::new(
        System::StoreIo,
        format!("AppendRegion::read, {} MiB requests", req >> 20),
    )
    .param("request_bytes", Json::Num(req as f64));
    let per_pass = written / req as u64;
    if per_pass == 0 {
        p.error = Some("nothing written to read".to_owned());
        return p;
    }
    let mut out = vec![0u8; req];
    let mut bad: Option<String> = None;
    let r = time_boxed(ctx, &Snap::os, |i| {
        let off = (i % per_pass) * req as u64;
        let t = Instant::now();
        let n = wal
            .read(off, &mut out)
            .map_err(|e| sioerr("AppendRegion::read", &e))?;
        let d = t.elapsed();
        if n != req {
            return Err(format!("short read at {off}: {n} of {req}"));
        }
        // Spot-check the first chunk of every request (cheap, outside the timer).
        if bad.is_none() && !ctx.pool.matches(STREAM, off / CHUNK as u64, &out[..CHUNK]) {
            bad = Some(format!("chunk at {off} reads back different bytes"));
        }
        Ok(Step::Op(d))
    });
    match r {
        Ok(run) => {
            record_read(&mut p, &run, req);
            p.verify = match bad {
                Some(b) => Verify::Failed(b),
                None => Verify::Passed(format!(
                    "first 1 MiB of each of {} requests compared against what was written",
                    run.total_ops
                )),
            };
        }
        Err(e) => p.error = Some(e),
    }
    p
}

fn record_read(p: &mut Point, run: &super::Run, req: usize) {
    super::record_run(p, run, "requests_per_s");
    p.metric("mb_per_s", run.rate().map(|r| r * req as f64 / 1e6));
    p.metric("bytes", Some(run.ops as f64 * req as f64));
    p.metric("seconds", Some(run.lat.total().as_secs_f64()));
}

fn raw_write(ctx: &Ctx, path: &std::path::Path, p: &mut Point) -> Result<(RawFile, u64), String> {
    let (f, prep) = RawFile::create_ready(path, SPAN).map_err(|e| ioerr("raw create", &e))?;
    p.notes.push(format!(
        "file made ready (zero-filled unbuffered 1 MiB writes + flushed) in {:.2} s ({:.0} MB/s); compare store-io's provisioning note",
        prep.as_secs_f64(),
        SPAN as f64 / prep.as_secs_f64() / 1e6
    ));
    let mut src = vec![0u8; CHUNK];
    let mut abuf = AlignedBuf::zeroed(CHUNK);
    let mut lat = Samples::with_capacity(1024);
    let before = Snap::os();
    let deadline = Duration::from_secs_f64(ctx.secs);
    let start = Instant::now();
    let mut i = 0u64;
    while (i + 1) * CHUNK as u64 <= SPAN && start.elapsed() < deadline {
        ctx.pool.fill(STREAM, i, &mut src);
        let t = Instant::now();
        abuf.as_mut_slice().copy_from_slice(&src);
        f.write_at(abuf.as_slice(), i * CHUNK as u64)
            .map_err(|e| ioerr("raw write", &e))?;
        lat.push(t.elapsed());
        i += 1;
    }
    let tb = Instant::now();
    f.flush_data().map_err(|e| ioerr("raw flush", &e))?;
    let barrier = tb.elapsed();
    let wall = start.elapsed();
    let total = lat.total() + barrier;
    let after = Snap::os();
    let bytes = i * CHUNK as u64;
    record_write(
        p,
        &Written {
            lat,
            bytes,
            total,
            barrier,
            wall,
        },
        &super::Delta::between(&before, &after),
    );
    let mut out = AlignedBuf::zeroed(8 << 20);
    let mut checked = 0u64;
    while checked < bytes {
        let len = ((bytes - checked) as usize).min(out.len());
        let got = f
            .read_at(&mut out.as_mut_slice()[..len], checked)
            .map_err(|e| ioerr("raw read", &e))?;
        if got != len {
            p.verify = Verify::Failed(format!("short raw read at {checked}"));
            return Ok((f, bytes));
        }
        for (k, c) in out.as_slice()[..len].chunks(CHUNK).enumerate() {
            let idx = checked / CHUNK as u64 + k as u64;
            if !ctx.pool.matches(STREAM, idx, c) {
                p.verify = Verify::Failed(format!("raw chunk {idx} reads back different bytes"));
                return Ok((f, bytes));
            }
        }
        checked += len as u64;
    }
    p.verify = Verify::Passed(format!(
        "all {} MiB read back unbuffered and compared byte for byte",
        bytes >> 20
    ));
    Ok((f, bytes))
}

fn raw_read(ctx: &Ctx, f: &RawFile, written: u64, req: usize) -> Point {
    let p = Point::new(
        System::Raw,
        format!("unbuffered read, {} MiB requests", req >> 20),
    );
    let mut out = AlignedBuf::zeroed(req);
    raw_read_into(ctx, f, written, req, out.as_mut_slice(), p)
}

/// Diagnostic: the raw read primitive, but into a buffer taken from a
/// `store-io-buf` pool configured as the store configures its own (4 KiB
/// alignment, power-of-two classes up to the request size). If this is as
/// fast as `raw_read`, a store-io read's extra cost is in the engine path,
/// not in the pool's memory.
fn raw_read_pool(ctx: &Ctx, f: &RawFile, written: u64, req: usize) -> Point {
    let mut p = Point::new(
        System::Raw,
        format!(
            "diagnostic: unbuffered read into a store-io-buf pool buffer, {} MiB requests",
            req >> 20
        ),
    );
    let pool = match BufPool::new(&PoolConfig::uniform(4096, 4096, req, 4)) {
        Ok(pool) => pool,
        Err(e) => {
            p.error = Some(format!("BufPool::new: {e:?}"));
            return p;
        }
    };
    let mut buf = match pool.take(req) {
        Ok(b) => b,
        Err(e) => {
            p.error = Some(format!("BufPool::take: {e:?}"));
            return p;
        }
    };
    p.notes
        .push("not a comparison row: separates pool-memory cost from engine-path cost".to_owned());
    raw_read_into(ctx, f, written, req, buf.as_mut_slice(), p)
}

fn raw_read_into(
    ctx: &Ctx,
    f: &RawFile,
    written: u64,
    req: usize,
    out: &mut [u8],
    p: Point,
) -> Point {
    let mut p = p.param("request_bytes", Json::Num(req as f64));
    let per_pass = written / req as u64;
    if per_pass == 0 {
        p.error = Some("nothing written to read".to_owned());
        return p;
    }
    let mut bad: Option<String> = None;
    let r = time_boxed(ctx, &Snap::os, |i| {
        let off = (i % per_pass) * req as u64;
        let t = Instant::now();
        let n = f.read_at(out, off).map_err(|e| ioerr("raw read", &e))?;
        let d = t.elapsed();
        if n != req {
            return Err(format!("short raw read at {off}: {n} of {req}"));
        }
        if bad.is_none() && !ctx.pool.matches(STREAM, off / CHUNK as u64, &out[..CHUNK]) {
            bad = Some(format!("raw chunk at {off} reads back different bytes"));
        }
        Ok(Step::Op(d))
    });
    match r {
        Ok(run) => {
            record_read(&mut p, &run, req);
            p.verify = match bad {
                Some(b) => Verify::Failed(b),
                None => Verify::Passed(format!(
                    "first 1 MiB of each of {} requests compared against what was written",
                    run.total_ops
                )),
            };
        }
        Err(e) => p.error = Some(e),
    }
    p
}

fn fsys_write(ctx: &Ctx, j: Journal) -> Point {
    let mut p = Point::new(
        System::Fsys,
        format!("{}: append(1 MiB) x N + sync_through", j.label()),
    )
    .param("chunk_bytes", Json::Num(CHUNK as f64));
    let path = ctx.path("w5-fsys.wal");
    let r = (|| -> Result<(), String> {
        let journal = ctx
            .fsys
            .journal_with(&path, j.options())
            .map_err(|e| fserr("journal_with", &e))?;
        let mut src = vec![0u8; CHUNK];
        let mut lat = Samples::with_capacity(1024);
        let before = Snap::os();
        let deadline = Duration::from_secs_f64(ctx.secs);
        let start = Instant::now();
        let mut i = 0u64;
        let mut last = None;
        while (i + 1) * CHUNK as u64 <= SPAN && start.elapsed() < deadline {
            ctx.pool.fill(STREAM, i, &mut src);
            let t = Instant::now();
            last = Some(journal.append(&src).map_err(|e| fserr("append", &e))?);
            lat.push(t.elapsed());
            i += 1;
        }
        let tb = Instant::now();
        let lsn = last.ok_or_else(|| "no append completed".to_owned())?;
        journal
            .sync_through(lsn)
            .map_err(|e| fserr("sync_through", &e))?;
        let barrier = tb.elapsed();
        let wall = start.elapsed();
        let total = lat.total() + barrier;
        let after = Snap::os();
        record_write(
            &mut p,
            &Written {
                lat,
                bytes: i * CHUNK as u64,
                total,
                barrier,
                wall,
            },
            &super::Delta::between(&before, &after),
        );
        journal.close().map_err(|e| fserr("close", &e))?;
        p.verify = fsys_cmp::verify_journal(&ctx.pool, &path, i);
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

/// Diagnostic: the raw read primitive into a pool buffer, then the copy
/// into a separate caller buffer that `AppendRegion::read` performs (the
/// bounce copy). Both are inside the timed section.
fn raw_read_pool_copy(ctx: &Ctx, f: &RawFile, written: u64, req: usize) -> Point {
    let mut p = Point::new(
        System::Raw,
        format!(
            "diagnostic: unbuffered read into a pool buffer + copy into a caller Vec, {} MiB requests",
            req >> 20
        ),
    )
    .param("request_bytes", Json::Num(req as f64));
    p.notes.push(
        "not a comparison row: reproduces store-io's read path (device read into a pooled buffer, then a copy into the caller's buffer)".to_owned(),
    );
    let per_pass = written / req as u64;
    let pool = match BufPool::new(&PoolConfig::uniform(4096, 4096, req, 4)) {
        Ok(pool) => pool,
        Err(e) => {
            p.error = Some(format!("BufPool::new: {e:?}"));
            return p;
        }
    };
    let mut buf = match pool.take(req) {
        Ok(b) => b,
        Err(e) => {
            p.error = Some(format!("BufPool::take: {e:?}"));
            return p;
        }
    };
    if per_pass == 0 {
        p.error = Some("nothing written to read".to_owned());
        return p;
    }
    let mut out = vec![0u8; req];
    let mut bad: Option<String> = None;
    let r = time_boxed(ctx, &Snap::os, |i| {
        let off = (i % per_pass) * req as u64;
        let t = Instant::now();
        let n = f
            .read_at(buf.as_mut_slice(), off)
            .map_err(|e| ioerr("raw read", &e))?;
        out.copy_from_slice(buf.as_slice());
        let d = t.elapsed();
        if n != req {
            return Err(format!("short raw read at {off}: {n} of {req}"));
        }
        if bad.is_none() && !ctx.pool.matches(STREAM, off / CHUNK as u64, &out[..CHUNK]) {
            bad = Some(format!("raw chunk at {off} reads back different bytes"));
        }
        Ok(Step::Op(d))
    });
    match r {
        Ok(run) => {
            record_read(&mut p, &run, req);
            p.verify = match bad {
                Some(b) => Verify::Failed(b),
                None => Verify::Passed(format!(
                    "first 1 MiB of each of {} requests compared against what was written",
                    run.total_ops
                )),
            };
        }
        Err(e) => p.error = Some(e),
    }
    p
}

/// Where the first region's data starts in a fresh store with 4 KiB blocks:
/// volume slots (2 x 4 KiB) + table slots (2 x 16 KiB) + the region's two
/// header blocks (2 x 4 KiB) = 48 KiB (`crates/store-io-engine/src/layout.rs`,
/// `data_start` and `extent`).
const STORE_DATA_SHIFT: u64 = 48 << 10;

/// Diagnostic: the raw read primitive at file offsets shifted by
/// [`STORE_DATA_SHIFT`], i.e. with the same alignment relative to 1 MiB
/// that store-io's first region has. Content is not checked (the shifted
/// requests straddle written chunks); the unshifted rows check it.
fn raw_read_shifted(ctx: &Ctx, f: &RawFile, written: u64, req: usize) -> Point {
    let mut p = Point::new(
        System::Raw,
        format!(
            "diagnostic: unbuffered read at offsets + 48 KiB (store-io's data alignment), {} MiB requests",
            req >> 20
        ),
    )
    .param("request_bytes", Json::Num(req as f64));
    p.notes.push(
        "not a comparison row: tests whether store-io's region data offset (48 KiB past a 1 MiB boundary) costs bandwidth on this stack".to_owned(),
    );
    let per_pass = written.saturating_sub(STORE_DATA_SHIFT) / req as u64;
    if per_pass == 0 {
        p.error = Some("nothing written to read".to_owned());
        return p;
    }
    let mut out = AlignedBuf::zeroed(req);
    let r = time_boxed(ctx, &Snap::os, |i| {
        let off = STORE_DATA_SHIFT + (i % per_pass) * req as u64;
        let t = Instant::now();
        let n = f
            .read_at(out.as_mut_slice(), off)
            .map_err(|e| ioerr("raw read", &e))?;
        let d = t.elapsed();
        if n != req {
            return Err(format!("short raw read at {off}: {n} of {req}"));
        }
        Ok(Step::Op(d))
    });
    match r {
        Ok(run) => {
            record_read(&mut p, &run, req);
            p.verify = Verify::NotChecked(
                "shifted requests straddle written chunks; the unshifted rows check content"
                    .to_owned(),
            );
        }
        Err(e) => p.error = Some(e),
    }
    p
}
