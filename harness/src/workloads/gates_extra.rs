//! The gate measurements no workload makes: durable writes kept N deep
//! through the `Submitter`, operations per CPU-second, allocations on the
//! hot paths, and commit latency under background load. Each one checks
//! its data, as every workload does.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use store_io_engine::completion::Done;
use store_io_engine::{PageRegion, StoreOptions, SubmitterConfig, WriteMode};

use super::{Ctx, KIB4, sample, sioerr, stamp, with_store, with_store_using};
use crate::alloc_count;
use crate::data::Rng;
use crate::report::{Point, System, Verify};
use crate::sio::Plat;
use crate::stats::Samples;

/// Region size for these measurements.
const SPAN: u64 = 256 << 20;
const PAGES: u64 = SPAN / KIB4 as u64;
/// Pages read back after a write measurement.
const CHECKS: usize = 4096;
/// G10 needs this many latency samples on each side for a stable p99.
const QOS_SAMPLES: usize = 1000;
/// ... but measures one side for at most this long.
const QOS_MAX_SECS: f64 = 60.0;
/// G10's log size.
const WAL: u64 = 512 << 20;

/// CPU time this process has used so far (user and system), seconds.
#[cfg(target_os = "linux")]
fn cpu_s() -> f64 {
    // SAFETY: getrusage(2) fills the struct it is given.
    let mut u: libc::rusage = unsafe { core::mem::zeroed() };
    // SAFETY: as above; RUSAGE_SELF covers every thread of the process.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut u) };
    if rc != 0 {
        return f64::NAN;
    }
    let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    tv(u.ru_utime) + tv(u.ru_stime)
}

/// CPU time this process has used so far (user and system), seconds.
#[cfg(windows)]
fn cpu_s() -> f64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut c, mut e, mut k, mut u) = (zero, zero, zero, zero);
    // SAFETY: the pseudo-handle of this process, and four FILETIMEs the
    // call fills.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &raw mut c,
            &raw mut e,
            &raw mut k,
            &raw mut u,
        )
    };
    if ok == 0 {
        return f64::NAN;
    }
    // 100 ns units.
    let t = |f: FILETIME| {
        ((u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime)) as f64 / 1e7
    };
    t(k) + t(u)
}

/// CPU time: not measured on this system.
#[cfg(not(any(target_os = "linux", windows)))]
fn cpu_s() -> f64 {
    f64::NAN
}

/// One thread keeps `qd` random 4 KiB operations in flight through a
/// submitter for the measured window. Writes are `mode`; `None` reads
/// instead. Returns ops/s, ops per CPU-second, and allocations per
/// operation in the window.
///
/// Integrity (patterns from [`stamp`]): a page is never in flight twice, so
/// each page's latest write is known, and every write carries its page and
/// sequence number in every word. Every write completion must carry its own
/// page's ticket (and, for a durable write, a receipt); afterwards a sample
/// of the written pages reads back as their latest write. For reads the
/// region is first written with each page's own pattern; every read must
/// return the whole page with its page's first word (the whole page during
/// warm-up). Stamping costs the measured window one multiply-xor per word.
fn submitter_loop(ctx: &Ctx, qd: u32, mode: Option<WriteMode>, name: &str) -> Point {
    submitter_loop_with(ctx, &ctx.opts, qd, mode, name)
}

/// As [`submitter_loop`], on a store made with `opts`.
fn submitter_loop_with(
    ctx: &Ctx,
    opts: &StoreOptions,
    qd: u32,
    mode: Option<WriteMode>,
    name: &str,
) -> Point {
    let variant = match mode {
        Some(WriteMode::Durable) => format!("Submitter durable random write QD{qd}"),
        Some(_) => format!("Submitter plain random write QD{qd}"),
        None => format!("Submitter random read QD{qd}"),
    };
    let variant = if opts.register_buffers {
        format!("{variant} (registered)")
    } else {
        variant
    };
    let p = Point::new(System::StoreIo, variant);
    with_store_using(ctx, name, opts, p, |store, p| {
        let pages = store
            .provision_page_region(name, SPAN)
            .map_err(|e| sioerr("provision", &e))?;
        if mode.is_none() {
            lay_out(&pages)?;
        }
        let mut sub = store
            .submitter(SubmitterConfig::default().with_depth(qd))
            .map_err(|e| sioerr("submitter", &e))?;
        if opts.register_buffers {
            p.notes.push(format!("io: {}", store.io_path().reason));
        }
        let mut rng = Rng::new(7_000_000 + u64::from(qd));
        let n = qd as usize;
        let mut free: Vec<usize> = (0..n).collect();
        // Per page, the sequence number of its latest write (0: none); per
        // slot, the page it has in flight. All allocated before the window.
        let mut last = vec![0u64; PAGES as usize];
        let mut busy = vec![false; PAGES as usize];
        let mut page_of = vec![0u64; n];
        let (mut writes, mut compared) = (0u64, 0u64);
        let mut bad: Option<String> = None;
        let warm_end = Instant::now() + Duration::from_secs_f64(ctx.warmup);
        let mut measuring = false;
        let (mut ops, mut t0, mut c0, mut a0) = (0u64, Instant::now(), 0.0, 0u64);
        let stop = warm_end + Duration::from_secs_f64(ctx.secs);
        loop {
            let now = Instant::now();
            if !measuring && now >= warm_end {
                measuring = true;
                t0 = now;
                c0 = cpu_s();
                a0 = alloc_count::count();
                ops = 0;
            }
            if now < stop {
                while let Some(slot) = free.pop() {
                    let page = loop {
                        let pg = rng.below(PAGES);
                        if !busy[pg as usize] {
                            break pg;
                        }
                    };
                    busy[page as usize] = true;
                    page_of[slot] = page;
                    let pos = pages
                        .pos(page * KIB4 as u64)
                        .map_err(|e| sioerr("pos", &e))?;
                    let mut buf = sub.take_buf(KIB4).map_err(|e| sioerr("take_buf", &e))?;
                    let r = match mode {
                        Some(m) => {
                            writes += 1;
                            stamp::stamp(buf.as_mut_slice(), page, writes);
                            last[page as usize] = writes;
                            sub.write(&pages, pos, buf, m, slot as u64)
                        }
                        None => sub.read_page(&pages, pos, buf, slot as u64),
                    };
                    r.map_err(|r| sioerr("submit", &r.error))?;
                }
            }
            if free.len() == n {
                break;
            }
            let mut got = 0;
            for c in sub.wait(1).map_err(|e| sioerr("wait", &e))? {
                got += 1;
                let slot = c.tag as usize;
                let page = page_of[slot];
                busy[page as usize] = false;
                free.push(slot);
                if measuring {
                    ops += 1;
                }
                let at = page * KIB4 as u64;
                match c.outcome.map_err(|e| sioerr("completion", &e))? {
                    Done::Written(w) => {
                        if w.ticket.pos().offset() != at || w.ticket.end() != at + KIB4 as u64 {
                            bad = Some(format!(
                                "a write to page {page} completed with another range's ticket"
                            ));
                        } else if mode == Some(WriteMode::Durable) && w.receipt.is_none() {
                            bad = Some(format!(
                                "a durable write to page {page} completed without a receipt"
                            ));
                        }
                    }
                    Done::Read { len } => {
                        let bytes = c.buf.as_ref().map_or(&[][..], |b| b.as_slice());
                        let right = if measuring {
                            stamp::heads(bytes, page, 0)
                        } else {
                            compared += 1;
                            stamp::holds(bytes, page, 0)
                        };
                        if len != KIB4 || !right {
                            bad = Some(format!(
                                "a read of page {page} returned {len} bytes not holding that page"
                            ));
                        }
                    }
                    other => return Err(format!("unexpected {other:?}")),
                }
            }
            if got == 0 && sub.in_flight() == 0 {
                return Err("a completion was lost".to_owned());
            }
        }
        let secs = t0.elapsed().as_secs_f64();
        let cpu = cpu_s() - c0;
        let allocs = alloc_count::count() - a0;
        p.metric("ops_per_s", Some(ops as f64 / secs));
        p.metric("ops_per_cpu_s", (cpu > 0.0).then(|| ops as f64 / cpu));
        p.metric(
            "allocs_per_op",
            (ops > 0).then(|| allocs as f64 / ops as f64),
        );
        p.metric("allocs", Some(allocs as f64));
        drop(sub);
        p.verify = match bad {
            Some(why) => Verify::Failed(why),
            None if mode.is_some() => check_pages(
                &pages,
                &last,
                "every completion carried its own page's ticket; ",
            ),
            None if compared == 0 => {
                Verify::Failed("no read completed during warm-up to check".to_owned())
            }
            None => Verify::Passed(format!(
                "every read returned its whole page, its first word that page's; {compared} warm-up reads checked word for word"
            )),
        };
        Ok(())
    })
}

/// Writes every page of `pages` with its own pattern (sequence 0).
fn lay_out(pages: &PageRegion<Plat>) -> Result<(), String> {
    const MIB: u64 = 1 << 20;
    let per = MIB / KIB4 as u64;
    let mut chunk = vec![0u8; MIB as usize];
    for i in 0..SPAN / MIB {
        stamp::stamp(&mut chunk, i * per, 0);
        let pos = pages.pos(i * MIB).map_err(|e| sioerr("pos", &e))?;
        let _t = pages
            .write(pos, &chunk)
            .map_err(|e| sioerr("lay out", &e))?;
    }
    Ok(())
}

/// Reads back a sample of the pages `last` marks as written and checks each
/// holds its latest write, word for word.
fn check_pages(pages: &PageRegion<Plat>, last: &[u64], what: &str) -> Verify {
    let written: Vec<usize> = (0..last.len()).filter(|&i| last[i] != 0).collect();
    if written.is_empty() {
        return Verify::Failed("no page was written".to_owned());
    }
    let idx = sample(written.len(), CHECKS);
    let mut out = vec![0u8; KIB4];
    for &k in &idx {
        let page = written[k];
        let read = pages
            .pos((page * KIB4) as u64)
            .and_then(|pos| pages.read(pos, &mut out));
        match read {
            Ok(n) if n == KIB4 && stamp::holds(&out, page as u64, last[page]) => {}
            Ok(_) => {
                return Verify::Failed(format!(
                    "page {page} does not hold its latest write ({})",
                    last[page]
                ));
            }
            Err(e) => return Verify::Failed(sioerr("PageRegion::read", &e)),
        }
    }
    Verify::Passed(format!(
        "{what}{} of {} written pages read back and checked word for word against their latest write",
        idx.len(),
        written.len()
    ))
}

/// G4: durable random writes kept `qd` deep (each completion durable).
pub fn durable_qd(ctx: &Ctx, qd: u32) -> Point {
    submitter_loop(ctx, qd, Some(WriteMode::Durable), "g4")
}

/// G5 and G8: random reads 128 deep (ops per CPU-second; allocations).
pub fn read_qd128(ctx: &Ctx) -> Point {
    submitter_loop(ctx, 128, None, "g5")
}

/// G8: plain random writes 32 deep (allocations on the write path).
pub fn write_qd32(ctx: &Ctx) -> Point {
    submitter_loop(ctx, 32, Some(WriteMode::Plain), "g8")
}

/// G8: allocations per blocking durable page write, after warm-up; the
/// window is at least as long as the warm-up, so a buffer growing on the
/// hot path cannot hide in it. Every write is stamped with its page and
/// sequence; afterwards a sample of the written pages reads back.
pub fn blocking_allocs(ctx: &Ctx) -> Point {
    let p = Point::new(System::StoreIo, "PageRegion::write_durable (allocations)");
    with_store(ctx, "g8b", p, |store, p| {
        let pages = store
            .provision_page_region("g8b", SPAN)
            .map_err(|e| sioerr("provision", &e))?;
        let mut buf = vec![0u8; KIB4];
        let mut last = vec![0u64; PAGES as usize];
        let mut rng = Rng::new(8_000_000);
        let mut seq = 0u64;
        let mut write = |rng: &mut Rng, last: &mut [u64]| -> Result<(), String> {
            let page = rng.below(PAGES);
            seq += 1;
            stamp::stamp(&mut buf, page, seq);
            let pos = pages
                .pos(page * KIB4 as u64)
                .map_err(|e| sioerr("pos", &e))?;
            let _r = pages
                .write_durable(pos, &buf)
                .map_err(|e| sioerr("write", &e))?;
            last[page as usize] = seq;
            Ok(())
        };
        let warm_end = Instant::now() + Duration::from_secs_f64(ctx.warmup);
        while Instant::now() < warm_end {
            write(&mut rng, &mut last)?;
        }
        let (a0, t0) = (alloc_count::count(), Instant::now());
        let mut ops = 0u64;
        while t0.elapsed().as_secs_f64() < ctx.secs.max(ctx.warmup) {
            write(&mut rng, &mut last)?;
            ops += 1;
        }
        let allocs = alloc_count::count() - a0;
        p.metric(
            "allocs_per_op",
            (ops > 0).then(|| allocs as f64 / ops as f64),
        );
        p.metric("allocs", Some(allocs as f64));
        p.metric("ops", Some(ops as f64));
        p.verify = check_pages(&pages, &last, "");
        Ok(())
    })
}

/// G10: durable 4 KiB appends one at a time, p99 alone and with another
/// thread writing 1 MiB plain writes 8 deep to another region of the store.
/// Each side runs for the measured time and until it has [`QOS_SAMPLES`]
/// latencies (at most [`QOS_MAX_SECS`]), so the p99 is never a handful of
/// samples on a slow device.
///
/// Integrity: every append and every background chunk is stamped with its
/// own block and sequence ([`stamp`]); afterwards a sample of the appends
/// and every written background chunk (its latest write) read back and are
/// checked word for word.
pub fn qos(ctx: &Ctx) -> Point {
    let p = Point::new(
        System::StoreIo,
        "append_durable p99: idle vs background writer",
    );
    with_store(ctx, "g10", p, |store, p| {
        let wal = store
            .provision_append_region("wal", WAL)
            .map_err(|e| sioerr("provision wal", &e))?;
        let bulk = store
            .provision_page_region("bulk", SPAN)
            .map_err(|e| sioerr("provision bulk", &e))?;
        let mut rec = vec![0u8; KIB4];
        // (region offset, append number) of every append.
        let mut appended: Vec<(u64, u64)> = Vec::with_capacity(4 * QOS_SAMPLES);
        let mut measure = |label: &str| -> Result<(Option<f64>, usize), String> {
            let mut lat = Samples::with_capacity(1 << 16);
            let t0 = Instant::now();
            let mut n = 0usize;
            loop {
                let e = t0.elapsed().as_secs_f64();
                // Each side may fill at most half the log.
                let full = n as u64 >= WAL / KIB4 as u64 / 2;
                if full || e >= QOS_MAX_SECS || (e >= ctx.secs && n >= QOS_SAMPLES) {
                    break;
                }
                let k = appended.len() as u64;
                stamp::stamp(&mut rec, k, APPEND_SEQ);
                let t = Instant::now();
                let (pos, _r) = wal.append_durable(&rec).map_err(|e| sioerr(label, &e))?;
                lat.push(t.elapsed());
                appended.push((pos.offset(), k));
                n += 1;
            }
            Ok((lat.summary().and_then(|s| s.p99), n))
        };
        let (idle, n_idle) = measure("append (idle)")?;
        let stop = Arc::new(AtomicBool::new(false));
        let bg = {
            let stop = Arc::clone(&stop);
            let store = store.clone();
            let bulk = bulk.clone();
            std::thread::spawn(move || -> Result<u64, String> {
                let mut sub = store
                    .submitter(SubmitterConfig::default().with_depth(8))
                    .map_err(|e| sioerr("bg submitter", &e))?;
                // Consecutive chunks, at most 8 in flight, a chunk never in
                // flight twice (write `i` is sequence `i + 1`).
                let (mut i, mut live) = (0u64, 0usize);
                let mut busy = vec![false; SLOTS as usize];
                while !stop.load(Ordering::Relaxed) || live > 0 {
                    while live < 8 && !stop.load(Ordering::Relaxed) && !busy[(i % SLOTS) as usize] {
                        let s = i % SLOTS;
                        busy[s as usize] = true;
                        let pos = bulk.pos(s * CHUNK).map_err(|e| sioerr("bg pos", &e))?;
                        let mut b = sub
                            .take_buf(CHUNK as usize)
                            .map_err(|e| sioerr("bg buf", &e))?;
                        stamp::stamp(b.as_mut_slice(), s * PER_CHUNK, i + 1);
                        sub.write(&bulk, pos, b, WriteMode::Plain, i)
                            .map_err(|r| sioerr("bg write", &r.error))?;
                        i += 1;
                        live += 1;
                    }
                    if live > 0 {
                        for c in sub.wait(1).map_err(|e| sioerr("bg wait", &e))? {
                            live -= 1;
                            busy[(c.tag % SLOTS) as usize] = false;
                            let _w = c.outcome.map_err(|e| sioerr("bg completion", &e))?;
                        }
                    }
                }
                Ok(i)
            })
        };
        std::thread::sleep(Duration::from_millis(300));
        let loaded = measure("append (loaded)");
        stop.store(true, Ordering::Relaxed);
        let issued = bg
            .join()
            .map_err(|_| "background writer panicked".to_owned())??;
        let (loaded, n_loaded) = loaded?;
        p.metric("p99_idle_us", idle);
        p.metric("p99_loaded_us", loaded);
        p.metric("p99_ratio", idle.zip(loaded).map(|(i, l)| l / i));
        p.metric("background_writes", Some(issued as f64));
        p.notes.push(format!(
            "{n_idle} idle and {n_loaded} loaded durable appends"
        ));
        p.verify = check_qos(&wal, &bulk, &appended, issued);
        Ok(())
    })
}

/// G10's background chunk size, chunks in its region, and blocks per chunk.
const CHUNK: u64 = 1 << 20;
const SLOTS: u64 = SPAN / CHUNK;
const PER_CHUNK: u64 = CHUNK / KIB4 as u64;
/// The sequence number of every append's pattern (each append its own block
/// number: its append number); never a background write's (`i + 1`).
const APPEND_SEQ: u64 = u64::MAX;

/// G10's read-back: a sample of the appends, then each written background
/// chunk (its latest write: the largest `i < issued` with `i % SLOTS == s`).
fn check_qos(
    wal: &store_io_engine::AppendRegion<Plat>,
    bulk: &PageRegion<Plat>,
    appended: &[(u64, u64)],
    issued: u64,
) -> Verify {
    let idx = sample(appended.len(), CHECKS);
    let mut out = vec![0u8; KIB4];
    for &k in &idx {
        let (at, n) = appended[k];
        match wal.read(at, &mut out) {
            Ok(r) if r == KIB4 && stamp::holds(&out, n, APPEND_SEQ) => {}
            Ok(_) => return Verify::Failed(format!("append {n} at {at} reads back wrong")),
            Err(e) => return Verify::Failed(sioerr("AppendRegion::read", &e)),
        }
    }
    let mut big = vec![0u8; CHUNK as usize];
    let mut chunks = 0u64;
    for s in 0..SLOTS.min(issued) {
        let i = s + (issued - 1 - s) / SLOTS * SLOTS;
        let read = bulk.pos(s * CHUNK).and_then(|pos| bulk.read(pos, &mut big));
        match read {
            Ok(n) if n == big.len() && stamp::holds(&big, s * PER_CHUNK, i + 1) => chunks += 1,
            Ok(_) => {
                return Verify::Failed(format!(
                    "background chunk {s} does not hold its latest write ({i})"
                ));
            }
            Err(e) => return Verify::Failed(sioerr("PageRegion::read", &e)),
        }
    }
    Verify::Passed(format!(
        "{} of {} durable appends and all {chunks} written background chunks read back and checked word for word",
        idx.len(),
        appended.len()
    ))
}

/// 3d (D-078): submitter random 4 KiB writes and reads 32 deep with the
/// buffer arena and the file registered and without, alternated three times
/// each, on stores whose arena fits a default locked-memory limit (64 KiB
/// largest ordinary buffer, no large buffers: about 8 MiB), so the
/// buffers can really be registered. Returns the points in run order:
/// write off, write on, read off, read on, three times.
pub fn registration(ctx: &Ctx) -> Vec<Point> {
    let base = StoreOptions {
        max_io: 64 << 10,
        large_buffers: 0,
        buffers_per_class: 64,
        ..ctx.opts.clone()
    };
    let mut pts = Vec::new();
    for _ in 0..3 {
        for mode in [Some(WriteMode::Plain), None] {
            for on in [false, true] {
                let opts = StoreOptions {
                    register_buffers: on,
                    ..base.clone()
                };
                pts.push(submitter_loop_with(ctx, &opts, 32, mode, "g3d"));
            }
        }
    }
    pts
}
