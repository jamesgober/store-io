//! What `copy_store` costs: a source store holding one append region of
//! `size` MiB (written in 256 KiB appends, made durable), copied `reps`
//! times into a fresh target once its writes are quiet (`PACED` set: at
//! once, paced), each copy timed; prints each copy's MB/s and the median.
//! Builds unchanged against v0.5 and v0.6, so the two can be compared on
//! one machine (D-156).
//!
//! Usage: `copy_cost <dir> <size MiB> [reps]` (the directory is emptied if
//! it holds only `src` and `dst`; it needs about 2 x size of free space).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::clone_on_copy,
    clippy::cast_precision_loss
)]

#[cfg(any(windows, target_os = "linux"))]
mod cost {
    use std::path::Path;
    use std::time::Instant;

    use store_io_engine::{CopyOptions, Store, StoreOptions, copy_store};
    use store_io_format::slot::FORMAT_2;

    #[cfg(windows)]
    fn platform() -> store_io_win::WinPlatform {
        store_io_win::WinPlatform::new()
    }

    #[cfg(target_os = "linux")]
    fn platform() -> store_io_posix::PosixPlatform {
        store_io_posix::PosixPlatform::with_tier(store_io_platform::LinuxTier::Auto).unwrap()
    }

    fn opts() -> StoreOptions {
        let mut o = StoreOptions {
            container_format: FORMAT_2,
            ..StoreOptions::default()
        };
        // A device without the evidence for default trust (a VM disk).
        o.trust.override_refusal = true;
        o
    }

    pub fn main() {
        let args: Vec<String> = std::env::args().collect();
        let dir = Path::new(
            args.get(1)
                .expect("usage: copy_cost <dir> <size MiB> [reps]"),
        );
        let size: u64 = args.get(2).map_or(512, |s| s.parse().unwrap()) << 20;
        let reps: usize = args.get(3).map_or(5, |s| s.parse().unwrap());
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries {
                let n = e.unwrap().file_name();
                assert!(
                    n == "src" || n == "dst",
                    "{} holds more: not emptied",
                    dir.display()
                );
            }
        }
        let _ = std::fs::remove_dir_all(dir);
        let (src, dst) = (dir.join("src"), dir.join("dst"));
        std::fs::create_dir_all(&src).unwrap();
        let p = platform();
        {
            let s = Store::create(p.clone(), &src, opts()).unwrap();
            let r = s.provision_append_region("data", size).unwrap();
            let piece = 256usize << 10;
            let mut block = vec![0u8; piece];
            let mut last = None;
            for i in 0..size / piece as u64 {
                for (k, w) in block.chunks_mut(8).enumerate() {
                    w.copy_from_slice(&(i * piece as u64 + 8 * k as u64).to_le_bytes());
                }
                last = Some(r.append(&block).unwrap());
            }
            let _r = r.sync_through(&last.unwrap()).unwrap();
        }
        // The source's writes are commits: v0.6 paces background work on
        // the device until they have been quiet for `commit_quiet` (5 s by
        // default). `PACED` set: copy at once, paced.
        if std::env::var_os("PACED").is_none() {
            std::thread::sleep(std::time::Duration::from_secs(6));
        }
        let mut rates = Vec::new();
        for rep in 0..reps {
            let _ = std::fs::remove_dir_all(&dst);
            let t = Instant::now();
            let _report = copy_store(
                p.clone(),
                &src,
                &dst,
                CopyOptions {
                    store: opts(),
                    ..CopyOptions::default()
                },
            )
            .unwrap();
            let secs = t.elapsed().as_secs_f64();
            let mbs = size as f64 / secs / 1e6;
            println!(
                "copy {rep}: {} MiB in {secs:.3} s, {mbs:.0} MB/s",
                size >> 20
            );
            rates.push(mbs);
        }
        rates.sort_by(f64::total_cmp);
        println!("copy median {:.0} MB/s over {reps}", rates[rates.len() / 2]);
        let _ = std::fs::remove_dir_all(dir);
    }
}

fn main() {
    #[cfg(any(windows, target_os = "linux"))]
    cost::main();
}
