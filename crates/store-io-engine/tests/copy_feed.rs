//! A copy's reader never blocks on its full feed holding reads in flight
//! (R5.4's progress rule): the fill may be waiting on the call's record
//! for one of them, so the reader drains its scan's reads first. And the
//! reader is a piece of the copy's call: on one device group it shares the
//! fill's record, each read admitted before it is submitted.
//!
//! The two meet: the reader, its feed full and about to block, waits in
//! the hook until the fill is parked at the top of a piece (holding none of
//! the call's pieces), whichever gets there first; the device group must
//! then have nothing in flight. Its feed fills whatever the timing: once
//! the fill parks, the reader runs alone.
//!
//! One test in its own binary: the test hook it installs is process-wide,
//! so its cases run one after another.

#![allow(clippy::unwrap_used, unused_results)]

use std::ops::ControlFlow;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use store_io_engine::test_hook::{self, Step};
use store_io_engine::{CopyOptions, ScanItem, Store, StoreOptions, copy_store};
use store_io_sim::{SimConfig, SimPlatform};

const SECOND: u64 = 1_000_000_000;
/// The source's region: 16 of the reader's 256 KiB reads, twice its feed
/// (8 pieces).
const LEN: usize = 4 << 20;
/// The copy's reader thread.
const READER: &str = "store-io-copy";

#[derive(Default)]
struct Race {
    /// The thread whose fill pieces are counted (the copy's caller).
    fill: Option<ThreadId>,
    pieces: u32,
    /// The fill is parked at the top of a piece.
    parked: bool,
    /// The reader has reported its full feed and the two have met.
    met: bool,
    /// The group's pieces in flight at each of the reader's submissions.
    reads: Vec<u64>,
    /// The group's pieces in flight when they met.
    blocking: Vec<u64>,
}

static RACE: Mutex<Option<Race>> = Mutex::new(None);
static CV: Condvar = Condvar::new();
/// The group's pieces in flight, read through a store of the device.
static SAMPLE: Mutex<Option<Store<SimPlatform>>> = Mutex::new(None);

fn lock() -> MutexGuard<'static, Option<Race>> {
    RACE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How long either side waits for the other before failing the copy (a
/// reader that ended or stopped early never reports its full feed; a fill
/// refused for the reader's undrained reads never parks).
const MEET: Duration = Duration::from_secs(20);

/// Waits on the race until `ready` holds, failing after [`MEET`].
fn wait_until(
    mut g: MutexGuard<'static, Option<Race>>,
    what: &str,
    ready: impl Fn(&Race) -> bool,
) -> MutexGuard<'static, Option<Race>> {
    let end = Instant::now() + MEET;
    while g.as_ref().is_some_and(|r| !ready(r)) {
        let left = end.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "{what} never came");
        g = CV
            .wait_timeout(g, left)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
    g
}

fn in_flight() -> u64 {
    SAMPLE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map_or(0, |s| s.qos().in_flight)
}

fn hook(s: Step) {
    match s {
        Step::FillPiece => {
            let mut g = lock();
            let Some(r) = g.as_mut() else { return };
            if r.fill != Some(std::thread::current().id()) || r.met {
                return;
            }
            r.pieces += 1;
            // From the second piece on (the first content request started
            // the reader): parked here, the fill holds none of the call's
            // pieces, until the two have met.
            if r.pieces >= 2 {
                r.parked = true;
                CV.notify_all();
                g = wait_until(g, "the reader's full feed", |r| r.met);
                if let Some(r) = g.as_mut() {
                    r.parked = false;
                }
            }
        }
        Step::FeedBlocking => {
            let mut g = lock();
            if g.as_ref().is_none_or(|r| r.met) {
                return;
            }
            // The fill gets to the top of a piece without the reader: its
            // content is in the full feed.
            g = wait_until(g, "the fill parked", |r| r.parked);
            let n = in_flight();
            if let Some(r) = g.as_mut() {
                r.blocking.push(n);
                r.met = true;
            }
            CV.notify_all();
            drop(g);
            // Fails the copy at once (the reader's panic is the caller's)
            // rather than leaving the fill waiting on the record.
            assert_eq!(n, 0, "the reader blocks holding reads in flight");
        }
        _ => {}
    }
}

fn byte_at(i: usize) -> u8 {
    ((i / 4096) as u8).wrapping_mul(37) ^ (i as u8) ^ 0x5a
}

/// `/a` with one region of `LEN` bytes, durable; closed.
fn source(p: &SimPlatform) {
    let a = Store::create(p.clone(), Path::new("/a"), StoreOptions::default()).unwrap();
    let log = a.provision_append_region("log", LEN as u64).unwrap();
    let data: Vec<u8> = (0..LEN).map(byte_at).collect();
    let mut last = None;
    for c in data.chunks(1 << 20) {
        last = Some(log.append(c).unwrap());
    }
    let _receipt = log.sync_through(&last.unwrap()).unwrap();
}

/// Copies `/a` to `to` under the hook; returns the race's record.
fn copy_met(p: &SimPlatform, to: &str) -> Race {
    *lock() = Some(Race {
        fill: Some(std::thread::current().id()),
        ..Race::default()
    });
    p.with_world(|w| {
        w.set_submit_probe(Some(Arc::new(|| {
            if std::thread::current().name() == Some(READER) {
                let n = in_flight();
                if let Some(r) = lock().as_mut() {
                    r.reads.push(n);
                }
            }
        })))
    });
    copy_store(
        p.clone(),
        Path::new("/a"),
        Path::new(to),
        CopyOptions::default(),
    )
    .unwrap();
    p.with_world(|w| w.set_submit_probe(None));
    lock().take().unwrap()
}

/// The copy at `to` holds the source's bytes.
fn intact(p: &SimPlatform, to: &str) -> bool {
    let s = Store::open_readonly(p.clone(), Path::new(to), StoreOptions::default()).unwrap();
    let log = s.append_region("log").unwrap();
    let mut back = Vec::new();
    log.scan(0, |item| {
        if let ScanItem::Data { bytes, .. } = item {
            back.extend_from_slice(bytes);
        }
        ControlFlow::Continue(())
    })
    .unwrap();
    back.len() == LEN && back.iter().enumerate().all(|(i, &b)| b == byte_at(i))
}

#[test]
fn test_a_copy_s_reader_drains_before_it_blocks_on_a_full_feed() {
    let p = SimPlatform::new(SimConfig::volatile(1701));
    source(&p);
    // A store of the device: the samples read its group (the original
    // kept here, so no hook or probe ever drops its last handle).
    let w = Store::create(p.clone(), Path::new("/w"), StoreOptions::default()).unwrap();
    *SAMPLE.lock().unwrap() = Some(w.clone());
    test_hook::install(Some(hook as fn(_)));

    // Paced (a hold keeps commits active): each of the reader's reads is
    // the call's one piece (the fill's record: never beside a fill piece).
    // A sample may count one more: the fill's admission, charged
    // tentatively while it is checked and undone when refused.
    let hold = w.commit_hold();
    let paced = copy_met(&p, "/c1");
    drop(hold);
    assert_eq!(paced.blocking, vec![0], "drained before blocking");
    assert!(paced.reads.len() >= 16, "{:?}", paced.reads);
    assert!(
        paced.reads.iter().all(|&n| (1..=2).contains(&n)),
        "one admitted piece at a time: {:?}",
        paced.reads
    );
    assert!(intact(&p, "/c1"));

    // Unpaced (commits quiet): several reads in flight, each admitted,
    // and none when the reader blocks.
    p.with_world(|w| w.advance_clock_ns(10 * SECOND));
    assert!(!w.qos().active);
    let unpaced = copy_met(&p, "/c2");
    assert_eq!(unpaced.blocking, vec![0], "drained before blocking");
    assert!(
        unpaced.reads.iter().all(|&n| (1..=8).contains(&n)),
        "{:?}",
        unpaced.reads
    );
    assert!(unpaced.reads.iter().any(|&n| n > 1), "{:?}", unpaced.reads);
    assert!(intact(&p, "/c2"));

    test_hook::install(None);
    *SAMPLE.lock().unwrap() = None;
    drop(w);
}
