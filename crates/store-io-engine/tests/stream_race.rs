//! A read stream's gate checks against a recycle or release (R5.5): at a
//! read's submission, at its delivery and after a bisection, each through
//! a `test_hook` step (the `sim-hooks` feature). The hook sees every thread
//! of the process, so these tests run one at a time, in a binary of their
//! own.

// Test setup unwraps (a failure is the test failing) and ignores values a
// given test does not inspect.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, ThreadId};
use std::time::Duration;

use store_io_core::IoClass;
use store_io_core::error::{Error, NotWrittenCause};
use store_io_engine::test_hook::{self, Step};
use store_io_engine::{CONTAINER, PageRegion, Store, StoreOptions, StreamOptions};
use store_io_sim::{FaultPlan, SimConfig, SimPlatform};

const DIR: &str = "/db";
const CHUNK: usize = 1 << 17;

/// One at a time: the hook sees every thread of the process.
static SERIAL: Mutex<()> = Mutex::new(());

#[derive(Debug)]
struct Race {
    streamer: Option<ThreadId>,
    park_at: Option<Step>,
    parked: bool,
    release: bool,
    /// The streamer's `StreamSubmit` steps.
    submits: u32,
}

static STATE: Mutex<Race> = Mutex::new(Race {
    streamer: None,
    park_at: None,
    parked: false,
    release: false,
    submits: 0,
});
static CV: Condvar = Condvar::new();

fn lock() -> MutexGuard<'static, Race> {
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

fn hook(s: Step) {
    let mut g = lock();
    if g.streamer != Some(thread::current().id()) {
        return;
    }
    if s == Step::StreamSubmit {
        g.submits += 1;
    }
    if g.park_at == Some(s) && !g.parked {
        g.parked = true;
        CV.notify_all();
        let (_g, timeout) = CV
            .wait_timeout_while(g, Duration::from_secs(10), |g| !g.release)
            .unwrap_or_else(PoisonError::into_inner);
        assert!(!timeout.timed_out(), "never released");
    }
}

/// The hook installed for a test, the state fresh; removed when dropped.
struct Hooked(#[allow(dead_code)] MutexGuard<'static, ()>);

impl Hooked {
    fn new(park_at: Option<Step>) -> Self {
        let serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        *lock() = Race {
            streamer: None,
            park_at,
            parked: false,
            release: false,
            submits: 0,
        };
        test_hook::install(Some(hook));
        Self(serial)
    }
}

impl Drop for Hooked {
    fn drop(&mut self) {
        test_hook::install(None);
        let mut g = lock();
        g.release = true;
        CV.notify_all();
    }
}

fn store(seed: u64) -> (SimPlatform, Store<SimPlatform>) {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    let o = StoreOptions {
        queues: 2,
        buffers_per_class: 64,
        max_io: CHUNK,
        ..StoreOptions::default()
    };
    let s = Store::create(p.clone(), Path::new(DIR), o).unwrap();
    (p, s)
}

fn page_region(s: &Store<SimPlatform>, len: u64) -> PageRegion<SimPlatform> {
    let r = s.provision_page_region("pages", len).unwrap();
    let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
    for (i, c) in data.chunks(CHUNK).enumerate() {
        r.write_durable(r.pos((i * CHUNK) as u64).unwrap(), c)
            .unwrap();
    }
    r
}

fn cause(e: &Error) -> Option<NotWrittenCause> {
    match e {
        Error::NotWritten { cause, .. } => Some(*cause),
        _ => None,
    }
}

/// What the streamer's first `next()` came to: a chunk, an error's cause,
/// or the stream's end.
#[derive(Debug, PartialEq, Eq)]
enum First {
    Chunk,
    Cause(Option<NotWrittenCause>),
    Corruption,
    End,
}

/// A stream over `r` made and stepped once on a thread of its own (a
/// stream is `!Send`), parked at the hook's step while this thread
/// recycles (or releases) `r`; what its first `next()` came to.
fn race(r: PageRegion<SimPlatform>, len: u64, release: bool) -> First {
    let r2 = r.clone();
    let streamer = thread::spawn(move || {
        lock().streamer = Some(thread::current().id());
        let read = StreamOptions::default().class(IoClass::Read);
        let mut stream = r2.read_stream_with(0..len, read).unwrap();
        match stream.next() {
            Some(Ok(_)) => First::Chunk,
            Some(Err(Error::Corruption { .. })) => First::Corruption,
            Some(Err(e)) => First::Cause(cause(&e)),
            None => First::End,
        }
    });
    {
        let g = lock();
        let (g, timeout) = CV
            .wait_timeout_while(g, Duration::from_secs(10), |g| !g.parked)
            .unwrap_or_else(PoisonError::into_inner);
        assert!(!timeout.timed_out(), "the stream never parked");
        drop(g);
    }
    // The stream holds no pass: neither waits on it.
    if release {
        r.release().unwrap();
    } else {
        let _r = r.recycle().unwrap();
    }
    {
        let mut g = lock();
        g.release = true;
        CV.notify_all();
    }
    streamer.join().unwrap()
}

/// A recycle (or release) after a read completed and before its delivery:
/// nothing of it is delivered.
#[test]
fn test_a_retire_before_delivery_ends_the_stream_with_nothing_delivered() {
    for (seed, release, want) in [
        (1, false, NotWrittenCause::StaleGeneration),
        (2, true, NotWrittenCause::NotReady),
    ] {
        let _h = Hooked::new(Some(Step::StreamCompleted));
        let (_p, s) = store(seed);
        let len = 4 * CHUNK as u64;
        let r = page_region(&s, len);
        assert_eq!(race(r, len, release), First::Cause(Some(want)));
    }
}

/// A media error in a region of one piece, its bisection's reads done,
/// and a recycle (or release) before its items are queued: none of them
/// (good bytes or the bad range) is delivered.
#[test]
fn test_a_retire_inside_a_bisection_delivers_none_of_its_items() {
    for (seed, release, want) in [
        (3, false, NotWrittenCause::StaleGeneration),
        (4, true, NotWrittenCause::NotReady),
    ] {
        let _h = Hooked::new(Some(Step::StreamBisected));
        let (p, s) = store(seed);
        let len = CHUNK as u64;
        let r = page_region(&s, len);
        let needle: Vec<u8> = (0..64u64).map(|i| (i % 251) as u8).collect();
        p.with_world(|w| {
            let media = w.media_of(&PathBuf::from(DIR), CONTAINER).unwrap();
            let base = media
                .windows(64)
                .position(|win| win == &needle[..])
                .unwrap() as u64;
            let file = w.file_id(&PathBuf::from(DIR), CONTAINER).unwrap();
            *w.faults_mut() = FaultPlan {
                bad_ranges: vec![(file, base + 8192, base + 12288)],
                ..FaultPlan::default()
            };
        });
        assert_eq!(race(r, len, release), First::Cause(Some(want)));
    }
}

/// A recycle between two `next()` calls: the next call submits no read.
#[test]
fn test_a_retire_between_calls_submits_no_read() {
    let _h = Hooked::new(None);
    lock().streamer = Some(thread::current().id());
    let (_p, s) = store(5);
    let len = 32 * CHUNK as u64;
    let r = page_region(&s, len);
    let read = StreamOptions::default().class(IoClass::Read).depth(2);
    let mut stream = r.read_stream_with(0..len, read).unwrap();
    let first = stream.next().unwrap().unwrap();
    drop(first);
    let before = lock().submits;
    assert!(before > 0);
    let _r = r.recycle().unwrap();
    let e = stream.next().unwrap().unwrap_err();
    assert_eq!(cause(&e), Some(NotWrittenCause::StaleGeneration));
    assert_eq!(lock().submits, before, "a read submitted after the retire");
}
