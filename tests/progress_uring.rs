//! R5.13's progress scenarios on a parallel io_uring queue (Linux): the
//! simulator's inline queue never has reads in flight, so the calls that
//! hold pieces of their own while they are refused are run here, on a real
//! device, with reads and writes really in flight.
//!
//! Each call runs **paced** (commits active: a `CommitHold` on a store of
//! the same device; the device group is seen with at most the call's one
//! piece in flight), **unpaced** (commits quiet throughout; several pieces
//! seen in flight, at most `background_unpaced`, set below the scan's
//! depth so its cap binds), and **switched** from unpaced to paced (the
//! hold taken once the call has pieces in flight, the call still running
//! after it), and must complete with its data intact (every 8-byte word
//! carries its own offset, so a piece read or written at the wrong place
//! is caught):
//!
//! - `copy_store` (its reader and its fill one call: the reader drains its
//!   scan's reads before it blocks on a full feed, the fill writes one
//!   piece at a time; the target on the source's device, so the two share
//!   the call's record);
//! - a caller's scan (background I/O of its own: a refused read reaps the
//!   scan's own reads first).
//!
//! Each scenario runs under a watchdog: a call waiting on itself aborts
//! the run in two minutes, naming the scenario, never hanging CI (and
//! never leaving a stuck call's pieces to mislead the scenarios after it).
//! The scenarios run one at a time (pacing is process-wide per device: one
//! scenario's hold would pace another's unpaced run). Run by
//! `conformance-linux` with `STORE_IO_EXPECT_URING=1`, so a missing io_uring
//! fails there, never skips.

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    unused_results,
    unused_must_use
)]

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use store_io::{
    CommitHold, CopyOptions, IoInterface, LinuxTier, ScanItem, Store, StoreOptions, Trust,
    copy_store,
};

const MIB: u64 = 1 << 20;
/// The source's append region: many pieces (96 at the default 256 KiB),
/// so a full feed and the switch are met.
const LOG: u64 = 24 * MIB;
/// The source's page region.
const PAGES: u64 = 8 * MIB;
/// Pieces per issuer while commits are quiet: below a scan's depth (8),
/// so an unpaced scan meets its cap.
const UNPACED: u64 = 4;
/// A call that has not completed by then waits on itself.
const WATCHDOG: Duration = Duration::from_secs(120);

/// io_uring; commits quiet 50 ms after the last (every store of the
/// device: a group's window is its members' longest), so the unpaced runs
/// need no long sleep; `UNPACED` pieces while quiet.
fn opts() -> StoreOptions {
    StoreOptions {
        trust: Trust {
            override_refusal: true,
            ..Trust::default()
        },
        linux_tier: LinuxTier::Uring,
        commit_quiet: Duration::from_millis(50),
        background_unpaced: UNPACED as u32,
        ..StoreOptions::default()
    }
}

/// One scenario at a time (the module's docs); a failed one does not
/// stop the rest.
fn serial() -> MutexGuard<'static, ()> {
    static ONE: Mutex<()> = Mutex::new(());
    ONE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A fresh directory on the test device, removed when dropped.
struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let base = std::env::var_os("STORE_IO_TEST_DIR")
            .or_else(|| std::env::var_os("HOME"))
            .map_or_else(std::env::temp_dir, PathBuf::from);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!(
            ".store-io-progress-{}-{tag}-{n}",
            std::process::id()
        ));
        let _gone = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _cleanup = std::fs::remove_dir_all(&self.0);
    }
}

/// A store on io_uring at `path`, or `None` where io_uring is refused
/// (a skip, unless `STORE_IO_EXPECT_URING` says it works here).
fn create(path: &Path) -> Option<Store> {
    match Store::create(path, opts()) {
        Ok(s) => {
            assert_eq!(
                s.report().io.interface,
                IoInterface::IoUring,
                "{:?}",
                s.report().io
            );
            Some(s)
        }
        Err(e) => {
            assert!(
                std::env::var_os("STORE_IO_EXPECT_URING").is_none(),
                "io_uring refused ({e}) where STORE_IO_EXPECT_URING says it works"
            );
            eprintln!("io_uring refused here ({e}): the progress scenarios are skipped");
            None
        }
    }
}

/// The byte at `i` of a region seeded `seed`: each 8-byte word is its own
/// offset (scrambled by an odd multiplier, a bijection), so no two words
/// of a region match.
fn byte(i: u64, seed: u8) -> u8 {
    let word = (i / 8).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (word >> ((i % 8) * 8)) as u8 ^ seed
}

fn chunk(at: u64, len: u64, seed: u8) -> Vec<u8> {
    (at..at + len).map(|i| byte(i, seed)).collect()
}

/// Fills `s`: an append region "log" of `LOG` bytes and a page region
/// "pages" of `PAGES`, every byte durable.
fn fill(s: &Store) {
    let log = s.provision_append_region("log", LOG).unwrap();
    let mut last = None;
    for at in (0..LOG).step_by(MIB as usize) {
        last = Some(log.append(&chunk(at, MIB, 0x10)).unwrap());
    }
    log.sync_through(&last.unwrap()).unwrap();
    let pages = s.provision_page_region("pages", PAGES).unwrap();
    for at in (0..PAGES).step_by(MIB as usize) {
        pages
            .write_durable(pages.pos(at).unwrap(), &chunk(at, MIB, 0x20))
            .unwrap();
    }
}

/// `s` holds what [`fill`] wrote.
fn check(s: &Store) {
    let log = s.append_region("log").unwrap();
    assert_eq!(log.len(), LOG);
    let mut back = vec![0u8; MIB as usize];
    for at in (0..LOG).step_by(MIB as usize) {
        assert_eq!(log.read(at, &mut back).unwrap(), MIB as usize);
        assert!(back == chunk(at, MIB, 0x10), "log at {at}");
    }
    let pages = s.page_region("pages").unwrap();
    for at in (0..PAGES).step_by(MIB as usize) {
        assert_eq!(
            pages.read(pages.pos(at).unwrap(), &mut back).unwrap(),
            MIB as usize
        );
        assert!(back == chunk(at, MIB, 0x20), "pages at {at}");
    }
}

/// When the call is paced.
#[derive(Clone, Copy, Debug)]
enum Mode {
    /// Commits active throughout.
    Paced,
    /// Commits quiet throughout.
    Unpaced,
    /// Quiet at the start; active from a point the call has pieces in
    /// flight.
    Switch,
}

/// `holder`'s device group with its commits quiet (none for
/// `commit_quiet`).
fn wait_quiet(holder: &Store) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while holder.qos().active {
        assert!(Instant::now() < deadline, "commits never went quiet");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Runs `f` on a thread of its own and waits for it under the watchdog,
/// calling `meanwhile` (with a test of whether `f` is still running) once
/// it has started. A panic in `f` is the test's; a call not complete in
/// time aborts the run (its stuck pieces would mislead what follows).
fn within<T: Send + 'static>(
    name: &str,
    f: impl FnOnce() -> T + Send + 'static,
    meanwhile: impl FnOnce(&dyn Fn() -> bool),
) -> T {
    let (tx, rx) = mpsc::channel();
    let t = std::thread::Builder::new()
        .name(format!("progress-{name}"))
        .spawn(move || {
            let _sent = tx.send(f());
        })
        .unwrap();
    meanwhile(&|| !t.is_finished());
    match rx.recv_timeout(WATCHDOG) {
        Ok(v) => {
            t.join().unwrap();
            v
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            eprintln!("{name}: not complete within {WATCHDOG:?}: a call waiting on itself");
            std::process::abort()
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => match t.join() {
            Err(p) => std::panic::resume_unwind(p),
            Ok(()) => panic!("{name}: ended with no result"),
        },
    }
}

fn copy_in(mode: Mode) {
    let _one = serial();
    let name = format!("copy-{mode:?}");
    let dir = Dir::new(&name);
    // A store of the device that commits nothing: its holds pace the copy,
    // and its group's count shows the copy's pieces in flight.
    let Some(holder) = create(&dir.join("holder")) else {
        return;
    };
    let src = create(&dir.join("src")).unwrap();
    fill(&src);
    drop(src);
    let mut hold: Option<CommitHold> = None;
    match mode {
        Mode::Paced => {
            hold = Some(holder.commit_hold());
            assert!(holder.qos().active);
        }
        Mode::Unpaced | Mode::Switch => wait_quiet(&holder),
    }
    let (from, to) = (dir.join("src"), dir.join("dst"));
    // The group's pieces in flight, sampled while the copy runs: the most
    // before the switch (all of them but in Switch mode) and after it.
    let (mut before, mut after) = (0u64, 0u64);
    let report = within(
        &name,
        move || {
            copy_store(
                from,
                to,
                CopyOptions {
                    store: opts(),
                    ..CopyOptions::default()
                },
            )
        },
        |running| {
            while running() {
                let n = holder.qos().in_flight;
                if hold.is_some() && matches!(mode, Mode::Switch) {
                    after = after.max(n);
                } else {
                    before = before.max(n);
                }
                if matches!(mode, Mode::Switch) && hold.is_none() && n > 0 {
                    assert!(!holder.qos().active, "{name}: paced before the switch");
                    hold = Some(holder.commit_hold());
                    assert!(running(), "{name}: the copy ended at the switch");
                    eprintln!("{name}: switched to paced with {n} piece(s) in flight");
                }
                std::thread::yield_now();
            }
        },
    )
    .unwrap();
    eprintln!("{name}: at most {before} piece(s) in flight, {after} after the switch");
    // One piece of the call while paced, and one more a refused admission
    // of its other thread charges while it is checked (`qos.rs`).
    match mode {
        Mode::Paced => assert!((1..=2).contains(&before), "{name}: {before} in flight"),
        Mode::Unpaced => {
            assert!(
                (2..=UNPACED + 1).contains(&before),
                "{name}: {before} in flight"
            );
            assert!(!holder.qos().active, "{name}: commits went active");
        }
        Mode::Switch => {
            assert!(hold.is_some(), "{name}: the switch was never reached");
            assert!(after <= UNPACED + 1, "{name}: {after} in flight");
        }
    }
    drop(hold);
    assert_eq!(report.regions, 2, "{report:?}");
    assert_eq!(report.bytes, LOG + PAGES, "{report:?}");
    let dst = Store::open_with(dir.join("dst"), opts()).unwrap();
    check(&dst);
}

fn scan_in(mode: Mode) {
    let _one = serial();
    let name = format!("scan-{mode:?}");
    let dir = Dir::new(&name);
    let Some(s) = create(&dir.join("s")) else {
        return;
    };
    fill(&s);
    let mut hold = None;
    match mode {
        Mode::Paced => hold = Some(s.commit_hold()),
        Mode::Unpaced | Mode::Switch => wait_quiet(&s),
    }
    let s2 = s.clone();
    let n = name.clone();
    let (sum, read, most, switched) = within(
        &name,
        move || {
            let log = s2.append_region("log").unwrap();
            let (mut next, mut most) = (0u64, 0u64);
            let mut held = None;
            let sum = log.scan(0, |item| {
                if let ScanItem::Data { offset, bytes } = item {
                    assert_eq!(offset, next, "delivered in order");
                    assert!(
                        bytes == chunk(offset, bytes.len() as u64, 0x10).as_slice(),
                        "the scan's bytes at {offset}"
                    );
                    next += bytes.len() as u64;
                    // Only this thread admits the group's pieces: the scan's
                    // own reads still in flight past this one.
                    let in_flight = s2.qos().in_flight;
                    most = most.max(in_flight);
                    // The switch, with reads of the scan in flight.
                    if matches!(mode, Mode::Switch)
                        && held.is_none()
                        && next >= LOG / 3
                        && in_flight > 0
                    {
                        assert!(!s2.qos().active, "{n}: paced before the switch");
                        held = Some(s2.commit_hold());
                        assert!(s2.qos().active);
                    }
                }
                ControlFlow::Continue(())
            });
            (sum, next, most, held.is_some())
        },
        |_| {},
    );
    eprintln!("{name}: at most {most} read(s) in flight at a delivery");
    match mode {
        Mode::Paced => assert!(most <= 1, "{name}: {most} in flight"),
        Mode::Unpaced => {
            assert!((2..=UNPACED).contains(&most), "{name}: {most} in flight");
            assert!(!s.qos().active, "{name}: commits went active");
        }
        Mode::Switch => assert!(switched, "{name}: the switch was never reached"),
    }
    drop(hold);
    let sum = sum.unwrap();
    assert_eq!(sum.unreadable, 0);
    assert_eq!(read, LOG, "every byte delivered");
}

#[test]
fn test_a_paced_copy_completes() {
    copy_in(Mode::Paced);
}

#[test]
fn test_an_unpaced_copy_completes() {
    copy_in(Mode::Unpaced);
}

#[test]
fn test_a_copy_switched_from_unpaced_to_paced_completes() {
    copy_in(Mode::Switch);
}

#[test]
fn test_a_paced_scan_completes() {
    scan_in(Mode::Paced);
}

#[test]
fn test_an_unpaced_scan_completes() {
    scan_in(Mode::Unpaced);
}

#[test]
fn test_a_scan_switched_from_unpaced_to_paced_completes() {
    scan_in(Mode::Switch);
}
