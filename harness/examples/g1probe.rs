//! G1 probe (Linux): QD1 4 KiB durable writes, one side at a time, each
//! split in two halves, to find where store-io's time over the raw
//! primitive goes (G1's gate: within the raw primitive + 2 µs).
//!
//! Usage: `g1probe <dir> <store|raw|rawmmap|rawpool> <sync|uring> <n>`: `n`
//! writes after 2,000 unmeasured ones. `store` writes through
//! `PageRegion::write` then `sync_through` (what `write_durable` does) on a
//! 256 MiB page region; `raw` copies into an aligned buffer and calls
//! `pwritev2` on an `O_DIRECT` descriptor, then `fdatasync`, as G1's raw
//! side does (`rawmmap`: the buffer in an anonymous mapping set up as
//! store-io's arena is; `rawpool`: a buffer from store-io's pool, taken per
//! write). Prints the p50 of the whole and of each half (µs), this thread's
//! CPU time in each half, and the process's user and system CPU time and
//! context switches per write over the measured writes only.
//!
//! The G1 work (2026-10: the inline fast path, `Queue::perform`; G1 itself
//! is `src/workloads/gates_g1.rs`) ran it on CI runners, alternated with
//! the raw side, and under
//! `valgrind --tool=callgrind` at two write counts (1,000 and 4,000: the
//! difference over 3,000 is the instructions per write): sampling profilers
//! see almost nothing in CI's virtual machines.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::cast_precision_loss
)]

#[cfg(target_os = "linux")]
fn main() {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use store_io_engine::{Store, StoreOptions};

    fn cpu() -> Duration {
        // SAFETY: clock_gettime fills the struct it is given.
        let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) }, 0);
        Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
    }

    fn usage() -> (f64, f64, i64, i64) {
        // SAFETY: getrusage fills the struct it is given.
        let mut u: libc::rusage = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) }, 0);
        let t = |v: libc::timeval| v.tv_sec as f64 * 1e6 + v.tv_usec as f64;
        (t(u.ru_utime), t(u.ru_stime), u.ru_nvcsw, u.ru_nivcsw)
    }

    let args: Vec<String> = std::env::args().collect();
    let dir = Path::new(&args[1]);
    let mode = args[2].as_str();
    let tier = match args[3].as_str() {
        "sync" => store_io_platform::LinuxTier::Sync,
        _ => store_io_platform::LinuxTier::Uring,
    };
    let n: usize = args[4].parse().unwrap();
    const WARM: u64 = 2_000;
    const SPAN: u64 = 256 << 20;
    const K: usize = 4096;
    let slots = SPAN / K as u64;
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    let (mut whole, mut first, mut second) = (
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
    );
    // This thread's CPU time (user and system) in each half.
    let (mut cpu1, mut cpu2) = (Vec::with_capacity(n), Vec::with_capacity(n));
    let mut src = vec![0u8; K];
    let mut before = usage();
    match mode {
        "store" => {
            let p = store_io_posix::PosixPlatform::with_tier(tier).unwrap();
            // As the harness: the documented override for a device class
            // the runner cannot prove (a hypervisor's disk).
            let mut opts = StoreOptions {
                linux_tier: tier,
                ..StoreOptions::default()
            };
            opts.trust.override_refusal = true;
            let s = Store::create(p, dir, opts).unwrap();
            let pages = s.provision_page_region("g1", SPAN).unwrap();
            for i in 0..WARM + n as u64 {
                if i == WARM {
                    before = usage();
                }
                src[..8].copy_from_slice(&i.to_le_bytes());
                let pos = pages.pos((i % slots) * K as u64).unwrap();
                let c0 = cpu();
                let t0 = Instant::now();
                let w = pages.write(pos, &src).unwrap();
                let t1 = Instant::now();
                let c1 = cpu();
                let _rc = pages.sync_through(&w).unwrap();
                let t2 = Instant::now();
                let c2 = cpu();
                if i >= WARM {
                    cpu1.push(c1 - c0);
                    cpu2.push(c2 - c1);
                    whole.push(t2 - t0);
                    first.push(t1 - t0);
                    second.push(t2 - t1);
                }
            }
        }
        _ => {
            use std::os::unix::ffi::OsStrExt;
            let path = std::ffi::CString::new(dir.join("raw.dat").as_os_str().as_bytes()).unwrap();
            // SAFETY: plain libc calls on a descriptor this function owns and
            // a buffer it keeps alive for every call.
            unsafe {
                let fd = libc::open(
                    path.as_ptr(),
                    libc::O_RDWR | libc::O_CREAT | libc::O_DIRECT,
                    0o644,
                );
                assert!(fd >= 0);
                assert_eq!(libc::posix_fallocate(fd, 0, SPAN as libc::off_t), 0);
                let mut buf: *mut libc::c_void = std::ptr::null_mut();
                assert_eq!(libc::posix_memalign(&mut buf, 4096, K), 0);
                // `rawmmap`: the buffer in an anonymous mapping set up as
                // store-io's arena is (MADV_DONTFORK, MADV_DONTDUMP).
                if mode == "rawmmap" {
                    let len = 64usize << 20;
                    let m = libc::mmap(
                        std::ptr::null_mut(),
                        len,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                        -1,
                        0,
                    );
                    assert!(m != libc::MAP_FAILED);
                    let _ = libc::madvise(m, len, libc::MADV_DONTDUMP);
                    let _ = libc::madvise(m, len, libc::MADV_DONTFORK);
                    buf = m;
                }
                // `rawpool`: a buffer from store-io's pool, taken per write.
                let pool = store_io_buf::BufPool::new(&store_io_buf::PoolConfig::uniform(
                    4096,
                    4096,
                    1 << 20,
                    64,
                ))
                .unwrap();
                // Written once in full, as `RawFile::create_ready` leaves it.
                for s in 0..slots {
                    let iov = libc::iovec {
                        iov_base: buf,
                        iov_len: K,
                    };
                    assert_eq!(
                        libc::pwritev2(fd, &iov, 1, (s * K as u64) as i64, 0),
                        K as isize
                    );
                }
                assert_eq!(libc::fdatasync(fd), 0);
                for i in 0..WARM + n as u64 {
                    if i == WARM {
                        before = usage();
                    }
                    src[..8].copy_from_slice(&i.to_le_bytes());
                    let c0 = cpu();
                    let t0 = Instant::now();
                    let mut held = None;
                    let target = if mode == "rawpool" {
                        let mut b = pool.take(K).unwrap();
                        let ptr = b.as_mut_ptr().cast::<libc::c_void>();
                        held = Some(b);
                        ptr
                    } else {
                        buf
                    };
                    std::ptr::copy_nonoverlapping(src.as_ptr(), target.cast::<u8>(), K);
                    let iov = libc::iovec {
                        iov_base: target,
                        iov_len: K,
                    };
                    let off = ((i % slots) * K as u64) as i64;
                    assert_eq!(libc::pwritev2(fd, &iov, 1, off, 0), K as isize);
                    drop(held);
                    let t1 = Instant::now();
                    let c1 = cpu();
                    assert_eq!(libc::fdatasync(fd), 0);
                    let t2 = Instant::now();
                    let c2 = cpu();
                    if i >= WARM {
                        cpu1.push(c1 - c0);
                        cpu2.push(c2 - c1);
                        whole.push(t2 - t0);
                        first.push(t1 - t0);
                        second.push(t2 - t1);
                    }
                }
                libc::close(fd);
            }
        }
    }
    let after = usage();
    let per = |a: f64, b: f64| (a - b) / n as f64;
    let p50 = |d: &mut Vec<Duration>| {
        d.sort_unstable();
        d[d.len() / 2].as_secs_f64() * 1e6
    };
    println!(
        "{mode:5} {:5}: p50 {:6.2} = write {:6.2} + flush {:6.2} µs | thread CPU p50: write {:5.2} flush {:5.2} µs | per write: user {:5.2} sys {:5.2} µs, csw {:.2} vol {:.2} invol",
        args[3],
        p50(&mut whole),
        p50(&mut first),
        p50(&mut second),
        p50(&mut cpu1),
        p50(&mut cpu2),
        per(after.0, before.0),
        per(after.1, before.1),
        per(after.2 as f64, before.2 as f64),
        per(after.3 as f64, before.3 as f64),
    );
}

#[cfg(not(target_os = "linux"))]
fn main() {}
