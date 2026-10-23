//! CPU cost of a Submitter operation, without a device: QD1 random 4 KiB
//! plain writes and page reads on the simulator (power-safe device, so no
//! cached copies), N ops, best of 5 rounds (v0.5 E4 C12: the region map's
//! runs cost a single-run region nothing measurable).
//!
//! Usage: `cargo run --release -p store-io-engine --example submitter_cpu [ops]`

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::cast_precision_loss)]

use std::path::Path;
use std::time::Instant;

use store_io_engine::completion::Done;
use store_io_engine::{Store, StoreOptions, SubmitterConfig, WriteMode};
use store_io_sim::{SimConfig, SimPlatform};

fn main() {
    let ops: u64 = std::env::args()
        .nth(1)
        .map_or(200_000, |a| a.parse().unwrap());
    let p = SimPlatform::new(SimConfig {
        runaway_writes: u64::MAX,
        ..SimConfig::power_safe(1)
    });
    let s = Store::create(p, Path::new("/db"), StoreOptions::default()).unwrap();
    let pages = s.provision_page_region("p", 16 << 20).unwrap();
    let mut sub = s
        .submitter(SubmitterConfig::default().with_depth(1))
        .unwrap();
    let blocks = (16u64 << 20) / 4096;
    for (name, write) in [("write", true), ("read", false)] {
        let mut best = f64::MAX;
        for _ in 0..5 {
            let mut x = 12345u64;
            let t = Instant::now();
            for i in 0..ops {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let pos = pages.pos((x % blocks) * 4096).unwrap();
                let buf = sub.take_buf(4096).unwrap();
                let r = if write {
                    sub.write(&pages, pos, buf, WriteMode::Plain, i)
                } else {
                    sub.read_page(&pages, pos, buf, i)
                };
                assert!(r.is_ok());
                for c in sub.wait(1).unwrap() {
                    match c.outcome.unwrap() {
                        Done::Read { .. } | Done::Written(_) => {}
                        other => panic!("{other:?}"),
                    }
                }
            }
            best = best.min(t.elapsed().as_secs_f64());
        }
        println!(
            "{name}: {:.0} ns an operation (best of 5, {ops} ops)",
            best * 1e9 / ops as f64
        );
    }
}
