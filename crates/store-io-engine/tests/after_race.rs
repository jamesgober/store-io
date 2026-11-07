//! The dependent-write race (R5.3, R5.13): a recycle of a dependency's
//! region at every step of a dependent write, on every path (a blocking
//! append, a page write performed at once or pumped, a `Submitter`'s append
//! and page write). The steps are reached through the engine's `test_hook`
//! (the `sim-hooks` feature) and the simulator's step probe, parking the
//! writer while another thread recycles.
//!
//! What must hold: never a poisoned store; never a ticket for a write
//! reserved after the dependency's drain completed; a refusal only with
//! nothing written. And the order behind it: the passes are held from the
//! check through the reservation (a drain meanwhile does not complete),
//! never before the lane, never while waiting for room in the append
//! window, never through the write's issue (a drain then completes).
//!
//! Every check is one-sided in time: a drain that must stay blocked gets
//! [`HELD`] to complete wrongly, a drain that must complete gets [`LONG`].
//! The hook is process-wide, so the tests run one at a time.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle, ThreadId};
use std::time::{Duration, Instant};

use store_io_core::error::{Error, NotWrittenCause};
use store_io_engine::completion::Done;
use store_io_engine::test_hook::{self, Step};
use store_io_engine::{
    AppendRegion, DurableReceipt, PageRegion, Store, StoreOptions, SubmitterConfig, WriteMode,
};
use store_io_sim::{SimConfig, SimPlatform};

const DIR: &str = "/db";
const B: usize = 4096;
const MIB: u64 = 1 << 20;
/// How long a drain that must stay blocked is given to complete wrongly.
const HELD: Duration = Duration::from_millis(200);
/// How long anything that must happen is waited for.
const LONG: Duration = Duration::from_secs(10);

/// One at a time: the hook sees every thread of the process.
static SERIAL: Mutex<()> = Mutex::new(());

#[derive(Debug)]
struct Race {
    writer: Option<ThreadId>,
    park_at: Option<Step>,
    parked: bool,
    release: bool,
    /// The writer's steps seen, one bit each.
    seen: u32,
    draining: bool,
    drained: bool,
    /// At the writer's `Reserved`: whether the drain had completed.
    reserved_after_drain: Option<bool>,
    /// A drain completed while the writer held its passes.
    held_violation: bool,
    /// Park the writer at its first queue operation after `Reserved`.
    issue_check: bool,
    armed: bool,
    at_issue: bool,
    lane_holder: Option<ThreadId>,
    holder_parked: bool,
    release_holder: bool,
}

const fn fresh() -> Race {
    Race {
        writer: None,
        park_at: None,
        parked: false,
        release: false,
        seen: 0,
        draining: false,
        drained: false,
        reserved_after_drain: None,
        held_violation: false,
        issue_check: false,
        armed: false,
        at_issue: false,
        lane_holder: None,
        holder_parked: false,
        release_holder: false,
    }
}

static STATE: Mutex<Race> = Mutex::new(fresh());
static CV: Condvar = Condvar::new();

fn lock() -> MutexGuard<'static, Race> {
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

fn set(f: impl FnOnce(&mut Race)) {
    f(&mut lock());
    CV.notify_all();
}

fn bit(s: Step) -> u32 {
    1 << match s {
        Step::BeforeLane => 0,
        Step::BeforePasses => 1,
        Step::BeforeReserve => 2,
        Step::Reserved => 3,
        Step::RecycleDraining => 4,
        Step::RecycleDrained => 5,
        _ => 6,
    }
}

/// Waits up to `limit` for `f`; whether it holds.
fn wait_until(limit: Duration, f: impl Fn(&Race) -> bool) -> bool {
    let g = lock();
    let (g, _) = CV
        .wait_timeout_while(g, limit, |g| !f(g))
        .unwrap_or_else(PoisonError::into_inner);
    f(&g)
}

fn hook(s: Step) {
    let mut g = lock();
    match s {
        Step::RecycleDraining => g.draining = true,
        Step::RecycleDrained => {
            g.drained = true;
            g.release = true;
        }
        _ if g.writer == Some(thread::current().id()) => {
            g.seen |= bit(s);
            if s == Step::Reserved {
                g.reserved_after_drain = Some(g.drained);
                g.armed = g.issue_check;
            }
            CV.notify_all();
            // The passes are held from here to the reservation: a drain
            // started meanwhile must not complete.
            if matches!(s, Step::BeforeReserve | Step::Reserved) && g.draining && !g.drained {
                g = CV
                    .wait_timeout_while(g, HELD, |g| !g.drained)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
                if g.drained {
                    g.held_violation = true;
                }
            }
            if g.park_at == Some(s) {
                g.parked = true;
                CV.notify_all();
                g = CV
                    .wait_while(g, |g| !g.release)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
        _ => {}
    }
    drop(g);
    CV.notify_all();
}

/// The simulator's step probe: parks the writer at its write's issue, or
/// the lane holder at its own write.
fn probe() {
    let me = thread::current().id();
    let mut g = lock();
    if g.armed && g.writer == Some(me) {
        g.armed = false;
        g.at_issue = true;
        CV.notify_all();
        let _g = CV
            .wait_while(g, |g| !g.release)
            .unwrap_or_else(PoisonError::into_inner);
    } else if g.lane_holder == Some(me) && !g.holder_parked {
        g.holder_parked = true;
        CV.notify_all();
        let _g = CV
            .wait_while(g, |g| !g.release_holder)
            .unwrap_or_else(PoisonError::into_inner);
    }
}

/// The hook and the probe installed, the state reset; on drop (a failing
/// assertion included) everything parked is released and both removed.
struct Hooked {
    p: SimPlatform,
    _serial: MutexGuard<'static, ()>,
}

impl Hooked {
    fn install(p: &SimPlatform, setup: impl FnOnce(&mut Race)) -> Self {
        let serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        set(|g| {
            *g = fresh();
            setup(g);
        });
        test_hook::install(Some(hook as fn(Step)));
        p.with_world(|w| {
            w.set_step_probe(Some(Arc::new(|| {
                probe();
                false
            })));
        });
        Self {
            p: p.clone(),
            _serial: serial,
        }
    }
}

impl Drop for Hooked {
    fn drop(&mut self) {
        set(|g| {
            g.release = true;
            g.release_holder = true;
        });
        test_hook::install(None);
        self.p.with_world(|w| w.set_step_probe(None));
    }
}

#[derive(Debug, Clone, Copy)]
enum Way {
    Append,
    PagePerform,
    PagePump,
    SubmitterAppend,
    SubmitterWrite,
}

const WAYS: [Way; 5] = [
    Way::Append,
    Way::PagePerform,
    Way::PagePump,
    Way::SubmitterAppend,
    Way::SubmitterWrite,
];

/// A store with a dependency `dep` (an append region holding one durable
/// record, whose receipt is `d`), and the regions the dependent writes go
/// to. One queue, so every write of the store shares one lane.
struct Fixture {
    p: SimPlatform,
    s: Store<SimPlatform>,
    dep: AppendRegion<SimPlatform>,
    d: DurableReceipt,
    log: AppendRegion<SimPlatform>,
    other: AppendRegion<SimPlatform>,
    pages: PageRegion<SimPlatform>,
}

fn fixture(seed: u64, append_window: Option<u64>) -> Fixture {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    let mut opts = StoreOptions {
        queues: 1,
        max_io: 128 << 10,
        ..StoreOptions::default()
    };
    if let Some(w) = append_window {
        opts.append_window = w;
    }
    let s = Store::create(p.clone(), Path::new(DIR), opts).unwrap();
    let dep = s.provision_append_region("dep", MIB).unwrap();
    let (_at, d) = dep.append_durable(b"dependency").unwrap();
    let log = s.provision_append_region("log", 4 * MIB).unwrap();
    let other = s.provision_append_region("other", MIB).unwrap();
    let pages = s.provision_page_region("pages", 4 * MIB).unwrap();
    Fixture {
        p,
        s,
        dep,
        d,
        log,
        other,
        pages,
    }
}

/// The dependent write of `way`, on this thread, as the writer.
fn write(fx: &Fixture, way: Way) -> Result<(), Error> {
    set(|g| g.writer = Some(thread::current().id()));
    let deps = [fx.d];
    let at = fx.pages.pos(0).unwrap();
    match way {
        Way::Append => fx.log.append_after(&deps, &[1u8; B]).map(drop),
        Way::PagePerform => fx.pages.write_after(&deps, at, &[2u8; B]).map(drop),
        Way::PagePump => fx
            .pages
            .write_after(&deps, at, &vec![3u8; 512 << 10])
            .map(drop),
        Way::SubmitterAppend | Way::SubmitterWrite => {
            let mut sub = fx.s.submitter(SubmitterConfig::default()).unwrap();
            let mut buf = sub.take_buf(B).unwrap();
            buf.as_mut_slice().fill(4);
            let done = match way {
                Way::SubmitterAppend => sub
                    .append_after(&fx.log, &deps, buf, WriteMode::Plain, 1)
                    .map(drop),
                _ => sub.write_after(&fx.pages, &deps, at, buf, WriteMode::Plain, 1),
            };
            if let Err(rejected) = done {
                assert!(rejected.buf.is_some(), "a refusal keeps the buffer");
                return Err(rejected.error);
            }
            let c = sub.wait(1).unwrap().next().unwrap();
            match c.outcome {
                Ok(Done::Written(_)) => Ok(()),
                Ok(other) => panic!("{other:?}"),
                Err(e) => Err(e),
            }
        }
    }
}

fn spawn_writer(fx: &Arc<Fixture>, way: Way) -> JoinHandle<Result<(), Error>> {
    let fx = Arc::clone(fx);
    thread::spawn(move || write(&fx, way))
}

fn spawn_recycler(fx: &Arc<Fixture>) -> JoinHandle<Result<(), Error>> {
    spawn_retire(fx, false)
}

/// Recycles the dependency's region on another thread, or releases it.
fn spawn_retire(fx: &Arc<Fixture>, release: bool) -> JoinHandle<Result<(), Error>> {
    let dep = fx.dep.clone();
    thread::spawn(move || {
        if release {
            dep.release()
        } else {
            dep.recycle().map(drop)
        }
    })
}

fn is_stale(r: &Result<(), Error>) -> bool {
    is_cause(r, NotWrittenCause::StaleGeneration)
}

fn is_cause(r: &Result<(), Error>, want: NotWrittenCause) -> bool {
    matches!(r, Err(Error::NotWritten { cause, .. }) if *cause == want)
}

/// The writer parked at `step`, then the dependency recycled (or
/// released): before the passes the write is refused (the drain completes
/// first: `StaleGeneration`, or `NotReady` for a release), with them it is
/// written (the drain waits for them); never poisoned.
fn step_race(seed: u64, way: Way, step: Step, release: bool) {
    let fx = Arc::new(fixture(seed, None));
    let hooked = Hooked::install(&fx.p, |g| g.park_at = Some(step));
    let tail = fx.log.tail();
    let writer = spawn_writer(&fx, way);
    let parked = wait_until(LONG, |g| g.parked);
    let recycler = spawn_retire(&fx, release);
    let draining = wait_until(LONG, |g| g.draining);
    let holds = matches!(step, Step::BeforeReserve | Step::Reserved);
    // A drain that must complete gets LONG; one that must not, HELD.
    let drained_while_parked = wait_until(if holds { HELD } else { LONG }, |g| g.drained);
    set(|g| g.release = true);
    let wrote = writer.join().unwrap();
    let recycled = recycler.join().unwrap();
    let g = lock();
    let ctx = format!("{way:?} at {step:?} (release {release}): {g:?}, wrote {wrote:?}");
    assert!(parked && draining, "{ctx}");
    assert!(recycled.is_ok(), "{ctx}: recycle {recycled:?}");
    assert!(fx.s.poisoned().is_none(), "{ctx}");
    assert!(
        !g.held_violation,
        "{ctx}: a drain completed under held passes"
    );
    assert_eq!(drained_while_parked, !holds, "{ctx}");
    if holds {
        assert!(wrote.is_ok(), "{ctx}");
        assert_eq!(g.reserved_after_drain, Some(false), "{ctx}");
    } else {
        let want = if release {
            NotWrittenCause::NotReady
        } else {
            NotWrittenCause::StaleGeneration
        };
        assert!(is_cause(&wrote, want), "{ctx}");
        assert_eq!(g.reserved_after_drain, None, "{ctx}: reserved");
        if matches!(way, Way::Append | Way::SubmitterAppend) {
            assert_eq!(fx.log.tail(), tail, "{ctx}: nothing reserved");
        }
    }
    drop(g);
    drop(hooked);
}

#[test]
fn test_a_recycle_at_every_step_of_every_dependent_write() {
    let steps = [
        Step::BeforeLane,
        Step::BeforePasses,
        Step::BeforeReserve,
        Step::Reserved,
    ];
    let mut seed = 100;
    for way in WAYS {
        for step in steps {
            seed += 1;
            step_race(seed, way, step, false);
        }
    }
}

/// A release of the dependency's region instead: refused `NotReady` before
/// the passes, written with them.
#[test]
fn test_a_release_before_or_after_the_passes() {
    let mut seed = 150;
    for way in WAYS {
        for step in [Step::BeforePasses, Step::BeforeReserve] {
            seed += 1;
            step_race(seed, way, step, true);
        }
    }
}

/// The page pump really is the multi-chunk branch: several write submits.
#[test]
fn test_the_pumped_page_write_is_several_writes() {
    let fx = fixture(90, None);
    let before = fx.p.with_world(|w| w.write_submits());
    fx.pages
        .write_after(&[fx.d], fx.pages.pos(0).unwrap(), &vec![3u8; 512 << 10])
        .unwrap();
    assert!(fx.p.with_world(|w| w.write_submits()) - before > 1);
}

/// The writer parked at its write's issue: its passes are gone, so a
/// recycle of the dependency drains (never held through the I/O).
#[test]
fn test_no_pass_is_held_through_the_write() {
    let mut seed = 200;
    for way in WAYS {
        seed += 1;
        let fx = Arc::new(fixture(seed, None));
        let hooked = Hooked::install(&fx.p, |g| g.issue_check = true);
        let writer = spawn_writer(&fx, way);
        let at_issue = wait_until(LONG, |g| g.at_issue);
        let recycler = spawn_recycler(&fx);
        let drained = wait_until(LONG, |g| g.drained);
        set(|g| g.release = true);
        let wrote = writer.join().unwrap();
        let recycled = recycler.join().unwrap();
        let ctx = format!("{way:?}: {:?}, wrote {wrote:?}", *lock());
        assert!(at_issue, "{ctx}");
        assert!(drained, "{ctx}: the drain waited on the write");
        assert!(wrote.is_ok() && recycled.is_ok(), "{ctx}");
        assert!(fx.s.poisoned().is_none(), "{ctx}");
        drop(hooked);
    }
}

/// A dependent write waiting for its lane holds no pass: a recycle of the
/// dependency drains meanwhile, and the write, once it has the lane, is
/// refused with nothing reserved.
#[test]
fn test_no_pass_is_taken_before_the_lane() {
    let mut seed = 300;
    for way in [Way::Append, Way::PagePerform, Way::PagePump] {
        seed += 1;
        let fx = Arc::new(fixture(seed, None));
        let hooked = Hooked::install(&fx.p, |_| {});
        let holder = {
            let fx = Arc::clone(&fx);
            thread::spawn(move || {
                set(|g| g.lane_holder = Some(thread::current().id()));
                fx.other.append(&[5u8; B]).map(drop)
            })
        };
        let holding = wait_until(LONG, |g| g.holder_parked);
        let tail = fx.log.tail();
        let writer = spawn_writer(&fx, way);
        let at_lane = wait_until(LONG, |g| g.seen & bit(Step::BeforeLane) != 0);
        // Long enough for passes taken out of order to be taken.
        let passes_early = wait_until(HELD, |g| g.seen & bit(Step::BeforeReserve) != 0);
        let recycler = spawn_recycler(&fx);
        let drained = wait_until(LONG, |g| g.drained);
        set(|g| g.release_holder = true);
        let held = holder.join().unwrap();
        let wrote = writer.join().unwrap();
        let recycled = recycler.join().unwrap();
        let ctx = format!("{way:?}: {:?}, wrote {wrote:?}", *lock());
        assert!(holding && at_lane, "{ctx}");
        assert!(!passes_early, "{ctx}: passes taken before the lane");
        assert!(
            drained,
            "{ctx}: the drain waited on a write waiting for its lane"
        );
        assert!(held.is_ok() && recycled.is_ok(), "{ctx}");
        assert!(is_stale(&wrote), "{ctx}");
        assert_eq!(fx.log.tail(), tail, "{ctx}: nothing reserved");
        assert!(fx.s.poisoned().is_none(), "{ctx}");
        drop(hooked);
    }
}

/// A dependent append waiting for room in its region's window holds no
/// pass: a recycle of the dependency drains meanwhile, and the append's
/// retry is refused with nothing reserved.
#[test]
fn test_no_pass_is_held_while_waiting_for_room() {
    let fx = Arc::new(fixture(400, Some(2 * B as u64)));
    let hooked = Hooked::install(&fx.p, |_| {});
    // This thread's unreaped appends fill the window.
    let mut sub = fx.s.submitter(SubmitterConfig::default()).unwrap();
    let mut queued = 0;
    loop {
        let buf = sub.take_buf(B).unwrap();
        match sub.append(&fx.log, buf, WriteMode::Plain, queued) {
            Ok(_) => queued += 1,
            Err(r) => {
                assert!(
                    matches!(
                        r.error,
                        Error::NotWritten {
                            cause: NotWrittenCause::TooManyInFlight,
                            ..
                        }
                    ),
                    "{:?}",
                    r.error
                );
                break;
            }
        }
    }
    let tail = fx.log.tail();
    let writer = spawn_writer(&fx, Way::Append);
    let deadline = Instant::now() + LONG;
    while fx.log.waiting_on_appends() == 0 && Instant::now() < deadline {
        thread::yield_now();
    }
    let waiting = fx.log.waiting_on_appends() > 0;
    let recycler = spawn_recycler(&fx);
    let drained = wait_until(LONG, |g| g.drained);
    let mut reaped = 0;
    while reaped < queued {
        reaped += sub.wait(1).unwrap().count() as u64;
    }
    let wrote = writer.join().unwrap();
    let recycled = recycler.join().unwrap();
    let ctx = format!("{:?}, wrote {wrote:?}", *lock());
    assert!(waiting, "{ctx}");
    assert!(
        drained,
        "{ctx}: the drain waited on an append waiting for room"
    );
    assert!(recycled.is_ok(), "{ctx}");
    assert!(is_stale(&wrote), "{ctx}");
    assert_eq!(fx.log.tail(), tail, "{ctx}: nothing reserved");
    assert!(fx.s.poisoned().is_none(), "{ctx}");
    drop(sub);
    drop(hooked);
}
