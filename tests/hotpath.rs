//! Gate G8b (Linux, run by CI under strace): the submitter's submit, reap
//! and wait make no futex call and no timed wait in steady state, whatever
//! pacing it meets.
//!
//! `hotpath_window` warms a submitter up, then runs plain random writes 32
//! deep for two seconds between two marker system calls (a `statx` of
//! `/store-io-hotpath-begin<label>` and `-end<label>`, which do not exist),
//! in three windows:
//!
//! - `""`: a commit-class submitter, with an idle background `Submitter`
//!   registered on the store (an idle issuer adds nothing to the commit
//!   path, R5.4);
//! - `"-bg"`: a background submitter while commits are quiet: unpaced, up
//!   to `background_unpaced` pieces in flight, a write over them refused
//!   `Paced` and resubmitted once one is reaped (every admission and
//!   release of the pacing on the path);
//! - `"-bg-paced"`: the same with a `CommitHold` live: paced, one piece in
//!   flight.
//!
//! CI records the run with `strace -f` and `dev/tools/hotpath-check.py`
//! counts, on the thread that made each window's markers and between them,
//! every futex call and every timed wait (`nanosleep`, `clock_nanosleep`,
//! `timerfd_*`, `setitimer`, a futex with a timeout), and requires at least
//! one `io_uring_enter` there (the window really did I/O on a ring) and
//! every window present. Uncontended locks make no system call and are not
//! seen by this check. Plain writes only: durable writes wake the store's
//! flush thread, which this window does not cover.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, unused_results, unused_must_use)]

use std::time::{Duration, Instant};

use store_io::engine::{SubmitterConfig, WriteMode};
use store_io::{
    Error, IoClass, IoInterface, LinuxTier, NotWrittenCause, PageRegion, Store, StoreOptions,
    Submitter, Trust,
};

const DEPTH: usize = 32;
/// The region written.
const SIZE: u64 = 64 << 20;

/// Random plain writes `DEPTH` deep over `pages` until `until`, then every
/// write reaped. A background write refused `Paced` keeps its buffer and is
/// submitted again once a write of its own is reaped (it always has one in
/// flight: an issuer with none is always admitted).
fn run(sub: &mut Submitter, pages: &PageRegion, x: &mut u64, until: Instant) -> u64 {
    let blk = pages.block_size();
    let slots = SIZE / blk;
    let mut live = 0usize;
    let mut tag = 0u64;
    let mut refused = 0u64;
    let mut held = None;
    while Instant::now() < until || live > 0 {
        while live < DEPTH && Instant::now() < until {
            let mut b = match held.take() {
                Some(b) => b,
                None => sub.take_buf(blk as usize).unwrap(),
            };
            *x ^= *x << 13;
            *x ^= *x >> 7;
            *x ^= *x << 17;
            b.as_mut_slice().fill(tag as u8);
            let at = pages.pos((*x % slots) * blk).unwrap();
            match sub.write(pages, at, b, WriteMode::Plain, tag) {
                Ok(()) => {
                    tag += 1;
                    live += 1;
                }
                Err(r) => match r.error {
                    Error::NotWritten {
                        cause: NotWrittenCause::Paced,
                        ..
                    } if live > 0 => {
                        refused += 1;
                        held = r.buf;
                        break;
                    }
                    e => panic!("a write failed ({live} in flight): {e}"),
                },
            }
        }
        if live > 0 {
            for c in sub.wait(1).unwrap() {
                live -= 1;
                // A failed write would make the window prove nothing.
                let _written = c.outcome.unwrap();
            }
        }
    }
    refused
}

/// One measured window, labelled `label`: a warm-up, then two seconds
/// between the markers. Returns the writes refused `Paced` in it.
fn window(label: &str, sub: &mut Submitter, pages: &PageRegion, x: &mut u64) -> u64 {
    run(sub, pages, x, Instant::now() + Duration::from_millis(500));
    let _begin = std::fs::metadata(format!("/store-io-hotpath-begin{label}"));
    let refused = run(sub, pages, x, Instant::now() + Duration::from_secs(2));
    let _end = std::fs::metadata(format!("/store-io-hotpath-end{label}"));
    refused
}

#[test]
#[ignore = "run by CI under strace (dev/tools/hotpath-check.py)"]
fn hotpath_window() {
    let base = std::env::var_os("STORE_IO_TEST_DIR")
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(std::env::temp_dir, std::path::PathBuf::from)
        .join(format!(".store-io-hotpath-{}", std::process::id()));
    let _gone = std::fs::remove_dir_all(&base);
    // io_uring, explicitly: a refusal fails the test (as "no io_uring"),
    // never a silent synchronous run. Commits quiet 50 ms after the last
    // (none is made here but the store's creation and the region's
    // provisioning: the wait for quiet below covers both).
    let opts = StoreOptions {
        trust: Trust {
            override_refusal: true,
            ..Trust::default()
        },
        linux_tier: LinuxTier::Uring,
        commit_quiet: Duration::from_millis(50),
        ..StoreOptions::default()
    };
    let store = Store::create(&base, opts).unwrap();
    assert_eq!(
        store.report().io.interface,
        IoInterface::IoUring,
        "{:?}",
        store.report().io
    );
    let pages = store.provision_page_region("p", SIZE).unwrap();
    let mut x = 1u64;
    let commit = SubmitterConfig::default().with_depth(DEPTH as u32);
    let background = commit.with_class(IoClass::Background);
    // An idle background issuer registered beside the commit-class window.
    let idle = store.submitter(background).unwrap();
    let mut sub = store.submitter(commit).unwrap();
    assert_eq!(window("", &mut sub, &pages, &mut x), 0);
    drop((sub, idle));
    let deadline = Instant::now() + Duration::from_secs(10);
    while store.qos().active {
        assert!(Instant::now() < deadline, "commits never went quiet");
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut sub = store.submitter(background).unwrap();
    let unpaced = window("-bg", &mut sub, &pages, &mut x);
    assert!(!store.qos().active, "commits went active");
    let hold = store.commit_hold();
    let paced = window("-bg-paced", &mut sub, &pages, &mut x);
    assert!(store.qos().active);
    drop(hold);
    // Each background window met its pacing: writes over the cap refused.
    assert!(
        unpaced > 0 && paced > 0,
        "refused: {unpaced} unpaced, {paced} paced"
    );
    drop(sub);
    drop((pages, store));
    let _cleanup = std::fs::remove_dir_all(&base);
}
