//! What format-2 growth costs (v0.5 E4 C12), on this OS's backend:
//!
//! 1. provisioning `size` bytes, against provisioning one block and growing
//!    it to `size` beyond its capacity (one extent), and against a region
//!    capped at `size` provisioned with one block and grown within its cap;
//! 2. a capped provisioning (Linux: the slack allocated unwritten; Windows
//!    and macOS: the whole cap written);
//! 3. `space()` latency while a large growth holds the metadata lock;
//! 4. appends from `threads` threads to a region grown to `size` from 64 KiB
//!    (its append ring sized for 64 KiB) against one provisioned at `size`.
//!
//! Usage: `growth_cost <dir> <size MiB> [threads] [secs]` (the directory is
//! emptied, if it is missing, empty or holds only a store's container; it
//! needs about 11 x size of free space). `BUFFERED` set (any value):
//! appends not waited durable. Not built on macOS (no backend yet).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::clone_on_copy,
    clippy::cast_precision_loss
)]

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
mod cost {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use store_io_engine::{CONTAINER, RegionOptions, Store, StoreOptions};
    use store_io_format::slot::FORMAT_2;

    #[cfg(windows)]
    fn platform() -> store_io_win::WinPlatform {
        store_io_win::WinPlatform::new()
    }

    #[cfg(target_os = "linux")]
    fn platform() -> store_io_posix::PosixPlatform {
        store_io_posix::PosixPlatform::with_tier(store_io_platform::LinuxTier::Auto).unwrap()
    }

    #[cfg(windows)]
    type P = store_io_win::WinPlatform;
    #[cfg(target_os = "linux")]
    type P = store_io_posix::PosixPlatform;

    fn fresh(dir: &Path) -> (Store<P>, StoreOptions) {
        if let Ok(mut entries) = std::fs::read_dir(dir) {
            let ours = entries.all(|e| e.is_ok_and(|e| e.file_name() == CONTAINER));
            assert!(
                ours,
                "{} holds more than a store: not emptied",
                dir.display()
            );
        }
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        let mut opts = StoreOptions {
            container_format: FORMAT_2,
            region_capacity: 256,
            ..StoreOptions::default()
        };
        let p = platform();
        let s = match Store::create(p.clone(), dir, opts.clone()) {
            Ok(s) => s,
            Err(_) => {
                // A device without the evidence for default trust (a VM disk).
                opts.trust.override_refusal = true;
                Store::create(p.clone(), dir, opts.clone()).unwrap()
            }
        };
        (s, opts)
    }

    fn mbs(bytes: u64, secs: f64) -> f64 {
        bytes as f64 / secs / 1e6
    }

    /// Appends of one block from `threads` threads for `secs`: appends a
    /// second (more threads than a 64 KiB region's ring of 4 KiB blocks).
    /// A failed append (the region full, say) fails the measurement.
    fn appends(s: &Store<P>, name: &str, threads: usize, secs: f64) -> f64 {
        let a = s.append_region(name).unwrap();
        let block = s.report().block_size as usize;
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicU64::new(0));
        let t = Instant::now();
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                let (a, stop, done) = (a.clone(), stop.clone(), done.clone());
                std::thread::spawn(move || {
                    let data = vec![7u8; block];
                    let durable = std::env::var_os("BUFFERED").is_none();
                    while !stop.load(Ordering::Relaxed) {
                        let r = if durable {
                            a.append_durable(&data).map(|_| ())
                        } else {
                            a.append(&data).map(|_| ())
                        };
                        match r {
                            Ok(()) => {
                                let _ = done.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => panic!("an append failed: {e:?}"),
                        }
                    }
                })
            })
            .collect();
        std::thread::sleep(Duration::from_secs_f64(secs));
        stop.store(true, Ordering::Relaxed);
        for w in workers {
            w.join().unwrap();
        }
        done.load(Ordering::Relaxed) as f64 / t.elapsed().as_secs_f64()
    }

    pub(crate) fn main() {
        let args: Vec<String> = std::env::args().collect();
        let dir = Path::new(&args[1]);
        let size: u64 = args[2].parse::<u64>().unwrap() << 20;
        let threads: usize = args.get(3).map_or(64, |t| t.parse().unwrap());
        let secs: f64 = args.get(4).map_or(3.0, |t| t.parse().unwrap());
        let (s, _opts) = fresh(dir);
        let block = s.report().block_size;
        println!("platform {:?}, block {block}", s.report().evidence.platform);

        // 1. Provisioning against growth.
        let t = Instant::now();
        s.provision_page_region("prov", size).unwrap();
        let prov = t.elapsed().as_secs_f64();
        println!(
            "provision {} MiB: {prov:.3} s ({:.0} MB/s)",
            size >> 20,
            mbs(size, prov)
        );
        let g = s.provision_page_region("grow", block).unwrap();
        let t = Instant::now();
        g.grow(size).unwrap();
        let beyond = t.elapsed().as_secs_f64();
        println!(
            "grow beyond capacity to {} MiB: {beyond:.3} s ({:.0} MB/s), capacity {} MiB",
            size >> 20,
            mbs(size, beyond),
            g.capacity() >> 20
        );
        let t = Instant::now();
        let c = s
            .provision_page_region_with("cap", RegionOptions::default().size(block).max_size(size))
            .unwrap();
        let capped = t.elapsed().as_secs_f64();
        println!("provision capped at {} MiB: {capped:.3} s", size >> 20);
        let t = Instant::now();
        c.grow(size).unwrap();
        let within = t.elapsed().as_secs_f64();
        println!(
            "grow within capacity to {} MiB: {within:.3} s ({:.0} MB/s)",
            size >> 20,
            mbs(size, within)
        );

        // 3. space() while a growth holds the metadata lock.
        let big = s.provision_page_region("big", block).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worst = Arc::new(AtomicU64::new(0));
        let probe = {
            let (s, stop, worst) = (s.clone(), stop.clone(), worst.clone());
            std::thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let t = Instant::now();
                    let _ = s.space();
                    let us = t.elapsed().as_micros() as u64;
                    let _ = worst.fetch_max(us, Ordering::Relaxed);
                    n += 1;
                    std::thread::sleep(Duration::from_micros(200));
                }
                n
            })
        };
        let t = Instant::now();
        big.grow(4 * size).unwrap();
        let el = t.elapsed().as_secs_f64();
        stop.store(true, Ordering::Relaxed);
        let calls = probe.join().unwrap();
        println!(
            "grow beyond to {} MiB: {el:.3} s; space() worst {:.1} ms over {calls} calls",
            (4 * size) >> 20,
            worst.load(Ordering::Relaxed) as f64 / 1e3
        );

        // 4. Appends: grown from 64 KiB against provisioned at size (fresh
        // regions each round; one that fills fails the run). `BUFFERED` set: appends not
        // waited durable (where the ring bounds what is in flight).
        for round in 0..2 {
            let (small, large) = (format!("small{round}"), format!("large{round}"));
            s.provision_append_region(&small, 64 << 10).unwrap();
            s.append_region(&small).unwrap().grow(size).unwrap();
            s.provision_append_region(&large, size).unwrap();
            // Alternated, so neither always runs after the other's fill.
            let (grown, full) = if round % 2 == 0 {
                let g = appends(&s, &small, threads, secs);
                (g, appends(&s, &large, threads, secs))
            } else {
                let f = appends(&s, &large, threads, secs);
                (appends(&s, &small, threads, secs), f)
            };
            println!(
                "appends, {threads} threads, round {round}: grown from 64 KiB {grown:.0}/s, provisioned at size {full:.0}/s ({:.2}x)",
                grown / full
            );
        }
        drop(s);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
fn main() {
    cost::main();
}

/// No backend here.
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn main() {}
