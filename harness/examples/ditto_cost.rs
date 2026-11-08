//! What a ditto write costs beyond its copies' appends (v0.6 V2c, D-145):
//! `StoreSet::ditto` reads every named store's device identity afresh
//! (never cached: a volume moved online changes it), then appends one copy
//! per store. Measured here on this OS's backend, two stores in `dir` (one
//! device, so the ditto itself is refused: its parts are timed apart):
//!
//! 1. `StoreSet::distinct(0, 1)`: two identity reads, what a two-copy
//!    ditto adds before its writes;
//! 2. one blocking 4 KiB append, what each copy costs (not made durable:
//!    the barrier after a ditto flushes the stores in parallel).
//!
//! Usage: `ditto_cost <dir> [calls]`: the two stores are made in `dir/a`
//! and `dir/b`, and `dir` is removed afterwards (refused if it holds
//! anything else). Not built on macOS (no backend yet).

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

    use store_io_engine::{CONTAINER, Store, StoreOptions, StoreSet};
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

    fn fresh(dir: &Path) -> Store<P> {
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
            region_capacity: 16,
            ..StoreOptions::default()
        };
        let p = platform();
        match Store::create(p.clone(), dir, opts.clone()) {
            Ok(s) => s,
            Err(_) => {
                // A device without the evidence for default trust (a VM disk).
                opts.trust.override_refusal = true;
                Store::create(p, dir, opts).unwrap()
            }
        }
    }

    /// The median and the 99th percentile of `ns`, in microseconds.
    fn spread(mut ns: Vec<u64>) -> (f64, f64) {
        ns.sort_unstable();
        let at = |q: f64| ns[((ns.len() - 1) as f64 * q) as usize] as f64 / 1e3;
        (at(0.5), at(0.99))
    }

    pub fn main() {
        let mut args = std::env::args().skip(1);
        let dir = args.next().expect("usage: ditto_cost <dir> [calls]");
        let calls: usize = args.next().map_or(2000, |n| n.parse().unwrap());
        let root = Path::new(&dir);
        if let Ok(entries) = std::fs::read_dir(root) {
            for e in entries.flatten() {
                assert!(
                    e.file_name() == "a" || e.file_name() == "b",
                    "{dir} holds more than this measurement's stores"
                );
            }
        }
        let (a, b) = (fresh(&root.join("a")), fresh(&root.join("b")));
        let set = StoreSet::new([a.clone(), b.clone()]).unwrap();
        // Room for every append, warm-up included.
        let span = ((calls as u64 + 100) * 4096).next_multiple_of(1 << 20);
        let wal = a.provision_append_region("wal", span).unwrap();
        let block = vec![7u8; 4096];
        // Warm both paths first.
        for _ in 0..100 {
            let _ = set.distinct(0, 1);
            let _ticket = wal.append(&block).unwrap();
        }
        let mut identity = Vec::with_capacity(calls);
        let mut append = Vec::with_capacity(calls);
        for _ in 0..calls {
            let t = Instant::now();
            let _ = set.distinct(0, 1);
            identity.push(t.elapsed().as_nanos() as u64);
            let t = Instant::now();
            let _ticket = wal.append(&block).unwrap();
            append.push(t.elapsed().as_nanos() as u64);
        }
        let (i50, i99) = spread(identity);
        let (a50, a99) = spread(append);
        println!("ditto_cost on {}: {calls} calls", std::env::consts::OS);
        println!("  two identity reads (distinct): p50 {i50:.1} us, p99 {i99:.1} us");
        println!("  one 4 KiB append:              p50 {a50:.1} us, p99 {a99:.1} us");
        println!(
            "  a two-copy ditto before its barrier: about {:.1} us at the p50",
            i50 + 2.0 * a50
        );
        drop((wal, set, a, b));
        let _ = std::fs::remove_dir_all(root);
    }
}

fn main() {
    #[cfg(any(windows, target_os = "linux"))]
    cost::main();
}
