//! Gate G8b (Linux, run by CI under strace): the submitter's submit, reap
//! and wait make no futex call and no timed wait in steady state.
//!
//! `hotpath_window` warms a submitter up, then runs plain random writes 32
//! deep for two seconds between two marker system calls (a `statx` of
//! `/store-io-hotpath-begin` and `-end`, which do not exist). CI records the
//! run with `strace -f` and `dev/tools/hotpath-check.py` counts, on the
//! thread that made the markers and between them, every futex call and
//! every timed wait (`nanosleep`, `clock_nanosleep`, `timerfd_*`,
//! `setitimer`, a futex with a timeout), and requires at least one
//! `io_uring_enter` there (the window really did I/O on a ring). Uncontended
//! locks make no system call and are not seen by this check. Plain writes
//! only: durable writes wake the store's flush thread, which this window
//! does not cover.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, unused_results, unused_must_use)]

use std::time::{Duration, Instant};

use store_io::engine::{SubmitterConfig, WriteMode};
use store_io::{IoInterface, LinuxTier, Store, StoreOptions, Trust};

#[test]
#[ignore = "run by CI under strace (dev/tools/hotpath-check.py)"]
fn hotpath_window() {
    let base = std::env::var_os("STORE_IO_TEST_DIR")
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(std::env::temp_dir, std::path::PathBuf::from)
        .join(format!(".store-io-hotpath-{}", std::process::id()));
    let _gone = std::fs::remove_dir_all(&base);
    // io_uring, explicitly: a refusal fails the test (as "no io_uring"),
    // never a silent synchronous run.
    let opts = StoreOptions {
        trust: Trust {
            override_refusal: true,
            ..Trust::default()
        },
        linux_tier: LinuxTier::Uring,
        ..StoreOptions::default()
    };
    let store = Store::create(&base, opts).unwrap();
    assert_eq!(
        store.report().io.interface,
        IoInterface::IoUring,
        "{:?}",
        store.report().io
    );
    let pages = store.provision_page_region("p", 64 << 20).unwrap();
    let blk = pages.block_size();
    let slots = (64 << 20) / blk;
    let mut sub = store
        .submitter(SubmitterConfig::default().with_depth(32))
        .unwrap();
    let mut x = 1u64;
    let mut run = |sub: &mut store_io::Submitter, until: Instant| {
        let mut live = 0usize;
        let mut tag = 0u64;
        while Instant::now() < until || live > 0 {
            while live < 32 && Instant::now() < until {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let mut b = sub.take_buf(blk as usize).unwrap();
                b.as_mut_slice().fill(tag as u8);
                sub.write(
                    &pages,
                    pages.pos((x % slots) * blk).unwrap(),
                    b,
                    WriteMode::Plain,
                    tag,
                )
                .map_err(|r| r.error)
                .unwrap();
                tag += 1;
                live += 1;
            }
            if live > 0 {
                for c in sub.wait(1).unwrap() {
                    live -= 1;
                    // A failed write would make the window prove nothing.
                    let _written = c.outcome.unwrap();
                }
            }
        }
    };
    run(&mut sub, Instant::now() + Duration::from_millis(500));
    let _begin = std::fs::metadata("/store-io-hotpath-begin");
    run(&mut sub, Instant::now() + Duration::from_secs(2));
    let _end = std::fs::metadata("/store-io-hotpath-end");
    drop(sub);
    drop((pages, store));
    let _cleanup = std::fs::remove_dir_all(&base);
}
