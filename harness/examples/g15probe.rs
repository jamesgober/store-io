//! G15 probe (Linux): `n` threads looping a durable 4 KiB write for a time,
//! on store-io (one shared append region) or raw (each thread its own
//! range, `pwritev2` then `fdatasync`, as G15's fio control flushing after
//! every write, though in order within each range); or the machine's
//! cross-thread wake.
//!
//! Usage: `g15probe <dir> <store|raw|wake> <writers> <secs>
//! <flushes_in_flight>`. Prints one line: ops/s, flushes per op, process
//! CPU (user + system) per op in µs, and voluntary and involuntary context
//! switches per op; for `wake` (the other arguments ignored, `dir` left
//! alone), one wake's time one way in µs. `dir` must be missing or empty:
//! the probe creates it and removes it when done.
//! Any panic aborts the process, so a writer that fails before the others
//! start can never leave them waiting.
//!
//! D-138 (2026-10) ran it on CI runners over null_blk, under `perf`
//! (`sched:sched_switch` and CPU samples, as root): eight store-io writers
//! made 0.62-0.64x the durable writes a second of eight raw writers on
//! every runner CPU, their sleeps 69% in the
//! barrier's wait and 20% in the flush; its `wake` mode times the hand-off
//! G15's wake control (`gates_g15::wake_us`) times.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::cast_precision_loss
)]

#[cfg(target_os = "linux")]
fn main() {
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use store_io_engine::{Store, StoreOptions};

    fn usage() -> (f64, i64, i64) {
        // SAFETY: a zeroed rusage is valid, and getrusage fills it.
        let mut u: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) }, 0);
        let t = |v: libc::timeval| v.tv_sec as f64 * 1e6 + v.tv_usec as f64;
        (t(u.ru_utime) + t(u.ru_stime), u.ru_nvcsw, u.ru_nivcsw)
    }

    // A writer that panics before the barrier would leave every other
    // thread waiting on it: report the panic and end the process.
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(info);
        std::process::abort();
    }));
    let args: Vec<String> = std::env::args().collect();
    let dir = Path::new(&args[1]);
    let mode = args[2].as_str();
    if mode == "wake" {
        // Two threads handing a turn back and forth through park/unpark
        // (a futex): the time of one wake, one way, over the second half
        // of the round trips.
        let turn = AtomicU64::new(0);
        let trips = 100_000u64;
        let main = std::thread::current();
        let mut timed = Duration::ZERO;
        std::thread::scope(|sc| {
            let other = sc.spawn(|| {
                for i in 0..2 * trips {
                    while turn.load(Ordering::Acquire) != 2 * i + 1 {
                        std::thread::park();
                    }
                    turn.store(2 * i + 2, Ordering::Release);
                    main.unpark();
                }
            });
            let mut t0 = Instant::now();
            for i in 0..2 * trips {
                if i == trips {
                    t0 = Instant::now();
                }
                turn.store(2 * i + 1, Ordering::Release);
                other.thread().unpark();
                while turn.load(Ordering::Acquire) != 2 * i + 2 {
                    std::thread::park();
                }
            }
            timed = t0.elapsed();
        });
        let one_way = timed.as_secs_f64() * 1e6 / (2 * trips) as f64;
        println!("mode=wake one_way_us={one_way:.2}");
        return;
    }
    assert!(mode == "store" || mode == "raw", "mode: store, raw or wake");
    let n: usize = args[3].parse().unwrap();
    assert!(n > 0, "at least one writer");
    let secs: f64 = args[4].parse().unwrap();
    let fif: u8 = args[5].parse().unwrap();
    const K: usize = 4096;
    const SPAN: u64 = 1536 << 20;
    // Never someone else's directory: only one missing or empty.
    if let Ok(mut entries) = std::fs::read_dir(dir) {
        assert!(entries.next().is_none(), "{} is not empty", dir.display());
    }
    std::fs::create_dir_all(dir).unwrap();
    let ops = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    let warm = Duration::from_millis(200);
    let window = Duration::from_secs_f64(secs);
    let (u0, u1, elapsed, flushes);
    match mode {
        "store" => {
            let p = store_io_posix::PosixPlatform::with_tier(store_io_platform::LinuxTier::Uring)
                .unwrap();
            let mut opts = StoreOptions {
                linux_tier: store_io_platform::LinuxTier::Uring,
                flushes_in_flight: fif,
                ..StoreOptions::default()
            };
            opts.trust.override_refusal = true;
            let s = Store::create(p, dir, opts).unwrap();
            let wal = s.provision_append_region("g15", SPAN).unwrap();
            let margin = (n as u64 + 1) * K as u64;
            let counting = AtomicBool::new(false);
            let (r0, r1, e, f) = std::thread::scope(|sc| {
                for _ in 0..n {
                    sc.spawn(|| {
                        let buf = vec![7u8; K];
                        while !stop.load(Ordering::Relaxed) {
                            if wal.tail() + margin > wal.len() {
                                break;
                            }
                            let _ = wal.append_durable(&buf).unwrap();
                            if counting.load(Ordering::Relaxed) {
                                ops.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    });
                }
                std::thread::sleep(warm);
                let d0 = s.domain_stats().flushes;
                let r0 = usage();
                counting.store(true, Ordering::Relaxed);
                let t0 = Instant::now();
                while t0.elapsed() < window && wal.tail() + margin <= wal.len() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                counting.store(false, Ordering::Relaxed);
                let e = t0.elapsed();
                let r1 = usage();
                let f = s.domain_stats().flushes - d0;
                stop.store(true, Ordering::Relaxed);
                (r0, r1, e, f)
            });
            u0 = r0;
            u1 = r1;
            elapsed = e;
            flushes = f;
        }
        _ => {
            use std::os::unix::ffi::OsStrExt;
            use std::sync::Barrier;
            let path = std::ffi::CString::new(dir.join("raw.dat").as_os_str().as_bytes()).unwrap();
            let per = (256u64 << 20) / n as u64 / K as u64 * K as u64;
            // SAFETY: plain libc calls on a descriptor kept open for the
            // threads' whole use.
            let fd = unsafe {
                let fd = libc::open(
                    path.as_ptr(),
                    libc::O_RDWR | libc::O_CREAT | libc::O_DIRECT,
                    0o644,
                );
                assert!(fd >= 0);
                assert_eq!(libc::posix_fallocate(fd, 0, 256 << 20), 0);
                fd
            };
            let counting = AtomicBool::new(false);
            // Every thread's range written once before the warm-up starts.
            let ready = Barrier::new(n + 1);
            let (r0, r1, e) = std::thread::scope(|sc| {
                for t in 0..n {
                    let (ops, stop, counting, ready) = (&ops, &stop, &counting, &ready);
                    sc.spawn(move || {
                        let mut buf: *mut libc::c_void = std::ptr::null_mut();
                        // SAFETY: an aligned buffer this thread owns; the
                        // descriptor outlives the scope.
                        unsafe {
                            assert_eq!(libc::posix_memalign(&mut buf, 4096, K), 0);
                            std::ptr::write_bytes(buf.cast::<u8>(), 7, K);
                            // Written once first: no write lands on an
                            // unwritten extent, whose conversion the flush
                            // would then have to commit.
                            let mut off = 0;
                            while off < per {
                                let iov = libc::iovec {
                                    iov_base: buf,
                                    iov_len: K,
                                };
                                let at = (t as u64 * per + off) as i64;
                                assert_eq!(libc::pwritev2(fd, &iov, 1, at, 0), K as isize);
                                off += K as u64;
                            }
                            ready.wait();
                            let mut i = 0u64;
                            while !stop.load(Ordering::Relaxed) {
                                let iov = libc::iovec {
                                    iov_base: buf,
                                    iov_len: K,
                                };
                                let at = (t as u64 * per + (i * K as u64) % per) as i64;
                                assert_eq!(libc::pwritev2(fd, &iov, 1, at, 0), K as isize);
                                assert_eq!(libc::fdatasync(fd), 0);
                                i += 1;
                                if counting.load(Ordering::Relaxed) {
                                    ops.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            libc::free(buf);
                        }
                    });
                }
                ready.wait();
                std::thread::sleep(warm);
                let r0 = usage();
                counting.store(true, Ordering::Relaxed);
                let t0 = Instant::now();
                std::thread::sleep(window);
                counting.store(false, Ordering::Relaxed);
                let e = t0.elapsed();
                let r1 = usage();
                stop.store(true, Ordering::Relaxed);
                (r0, r1, e)
            });
            // SAFETY: every thread has finished with it.
            unsafe { libc::close(fd) };
            u0 = r0;
            u1 = r1;
            elapsed = e;
            // One `fdatasync` per write, by construction (the kernel may
            // merge concurrent ones into fewer device flushes).
            flushes = ops.load(Ordering::Relaxed);
        }
    }
    let k = ops.load(Ordering::Relaxed).max(1) as f64;
    println!(
        "mode={mode} writers={n} fif={fif} ops_s={:.0} flushes_per_op={:.3} cpu_us_per_op={:.2} vcsw_per_op={:.2} ivcsw_per_op={:.2} secs={:.2}",
        k / elapsed.as_secs_f64(),
        flushes as f64 / k,
        (u1.0 - u0.0) / k,
        (u1.1 - u0.1) as f64 / k,
        (u1.2 - u0.2) as f64 / k,
        elapsed.as_secs_f64()
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(not(target_os = "linux"))]
fn main() {}
