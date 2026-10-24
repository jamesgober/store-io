//! Crash enumeration (v0.5 E7): an operation crashed before every step it
//! makes that can change what a crash leaves (each submission, flush,
//! directory sync, rename, ...: the simulator's step probe) and after it
//! returns, each time with every fate of the writes still cached (kept or
//! lost in every combination, and torn), optionally with the file system's
//! metadata (names, lengths) settled first; each state judged by the caller,
//! and its first open by a second after power fails during the first.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use store_io_engine::Store;
use store_io_sim::{CrashMode, Fate, FileId, SimPlatform, World};

use super::{DIR, opts};

/// One crash state: the step the power failed before (`None`: after the
/// operation returned), each cached write's fate, oldest first, and whether
/// the file system's metadata was settled first.
#[derive(Debug, Clone)]
pub(crate) struct Case {
    pub(crate) point: Option<usize>,
    pub(crate) fates: Vec<Fate>,
    pub(crate) settled: bool,
}

/// How much an enumeration covered.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Covered {
    /// Crash points (steps, and the end).
    pub(crate) points: usize,
    /// Crash states judged.
    pub(crate) cases: usize,
    /// Points whose cached writes were too many to combine exhaustively.
    pub(crate) sampled: usize,
}

/// The outcomes an enumeration's checks reached, by name: an outcome the
/// spec allows that no crash state reaches has not been tested, and once
/// the operation has returned only its own outcome may follow.
#[derive(Debug, Default)]
pub(crate) struct Outcomes {
    all: std::cell::RefCell<std::collections::BTreeSet<&'static str>>,
    returned: std::cell::RefCell<std::collections::BTreeSet<&'static str>>,
}

impl Outcomes {
    pub(crate) fn saw(&self, case: &Case, outcome: &'static str) {
        self.all.borrow_mut().insert(outcome);
        if case.point.is_none() {
            self.returned.borrow_mut().insert(outcome);
        }
    }

    /// Every one of `all` was reached, and nothing else; after the
    /// operation returned, only `after`.
    pub(crate) fn expect(&self, all: &[&'static str], after: &'static str) {
        let want: std::collections::BTreeSet<&str> = all.iter().copied().collect();
        assert_eq!(*self.all.borrow(), want, "the outcomes reached");
        assert_eq!(
            self.returned.borrow().iter().copied().collect::<Vec<_>>(),
            [after],
            "the outcomes once the operation returned"
        );
    }
}

/// Exhaustive up to this many cached writes; sampled past it.
const EXHAUSTIVE: usize = 10;

/// Every fate of `blocks.len()` cached writes this enumeration judges:
/// every kept/lost combination (sampled past [`EXHAUSTIVE`]: each alone,
/// all but each, every prefix, and 64 seeded combinations), then each write
/// torn at a block boundary (every one for writes of at most 8 blocks, else
/// the first, the middle and the last) with the others kept, and with them
/// lost; each once. Returns them and whether it sampled.
pub(crate) fn fates(blocks: &[usize], seed: u64) -> (Vec<Vec<Fate>>, bool) {
    let n = blocks.len();
    let all = |f: Fate| vec![f; n];
    let mut out: Vec<Vec<Fate>> = Vec::new();
    let sampled = n > EXHAUSTIVE;
    if sampled {
        out.push(all(Fate::Lost));
        out.push(all(Fate::Kept));
        for i in 0..n {
            let mut alone = all(Fate::Lost);
            alone[i] = Fate::Kept;
            out.push(alone);
            let mut but = all(Fate::Kept);
            but[i] = Fate::Lost;
            out.push(but);
            let mut prefix = all(Fate::Lost);
            prefix[..=i].fill(Fate::Kept);
            out.push(prefix);
        }
        let mut x = seed | 1;
        for _ in 0..64 {
            let mut f = all(Fate::Lost);
            for slot in &mut f {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                if x & 1 == 1 {
                    *slot = Fate::Kept;
                }
            }
            out.push(f);
        }
    } else {
        for mask in 0u32..(1 << n) {
            out.push(
                (0..n)
                    .map(|i| {
                        if mask & (1 << i) != 0 {
                            Fate::Kept
                        } else {
                            Fate::Lost
                        }
                    })
                    .collect(),
            );
        }
    }
    for (i, &b) in blocks.iter().enumerate() {
        let tears: Vec<usize> = if b <= 8 {
            (1..b).collect()
        } else {
            vec![1, b / 2, b - 1]
        };
        for t in tears {
            for rest in [Fate::Kept, Fate::Lost] {
                let mut f = all(rest);
                f[i] = Fate::Torn(t);
                out.push(f);
            }
        }
    }
    // Each once (the sample repeats some: the whole prefix is all kept).
    let mut seen = std::collections::HashSet::new();
    out.retain(|f| seen.insert(f.clone()));
    (out, sampled)
}

/// What two opens must agree on: the regions, each one's length, and the
/// stale and lost pages.
fn summary(s: &Store<SimPlatform>) -> String {
    let mut regions = s.regions();
    regions.sort_by(|a, b| a.0.cmp(&b.0));
    let lens: Vec<u64> = regions
        .iter()
        .map(|(name, kind)| match kind {
            store_io_format::meta::RegionKind::Page => {
                s.page_region(name).map_or(u64::MAX, |r| r.len())
            }
            store_io_format::meta::RegionKind::Append => {
                s.append_region(name).map_or(u64::MAX, |r| r.len())
            }
            store_io_format::meta::RegionKind::Slot => s
                .slot(name)
                .ok()
                .and_then(|x| x.read())
                .map_or(0, |o| o.len() as u64),
        })
        .collect();
    let m = &s.report().metadata;
    format!(
        "{regions:?} {lens:?} stale {:?} lost {:?}",
        m.stale_pages, m.lost_pages
    )
}

/// The steps an operation may make after the power cut before it is taken
/// for one retrying a dead device for ever.
const RUN_ON: usize = 100_000;

/// The store in `dir` as a crash left it: its first writable open, and the
/// open after power fails with that one open, agree ([`summary`]); the
/// media is left as it was found. Nothing to judge when it does not open.
pub(crate) fn agree(p: &SimPlatform, dir: &str, case: &Case) {
    let crashed = p.with_world(|w| w.clone());
    if let Ok(a) = Store::open(p.clone(), std::path::Path::new(dir), opts()) {
        let first = summary(&a);
        p.with_world(|w| w.crash(&CrashMode::LoseAll));
        drop(a);
        let b = Store::open(p.clone(), std::path::Path::new(dir), opts())
            .unwrap_or_else(|e| panic!("the open after a crash during the first: {e:?} {case:?}"));
        assert_eq!(
            summary(&b),
            first,
            "the open after a crash during the first disagrees: {case:?}"
        );
    }
    p.with_world(|w| *w = crashed);
}

/// Crashes `op` (run on what `setup` makes, the same each time) before
/// every step it makes and once after it returns, each time with every
/// fate [`fates`] gives the writes then cached; after each crash `check`
/// judges the media (it may open, read, audit), and, from the same crashed
/// media, a first writable open must agree ([`summary`]) with the open
/// after power fails with the first still open.
pub(crate) fn enumerate(
    seed: u64,
    setup: impl Fn() -> (SimPlatform, Store<SimPlatform>),
    op: impl Fn(&Store<SimPlatform>),
    check: impl Fn(&SimPlatform, &Case),
) -> Covered {
    enumerate_with(seed, false, setup, |_, s| op(s), check)
}

/// As [`enumerate`], judging every state also with the file system's
/// metadata settled (see [`enumerate_with`]).
pub(crate) fn enumerate_settled(
    seed: u64,
    setup: impl Fn() -> (SimPlatform, Store<SimPlatform>),
    op: impl Fn(&Store<SimPlatform>),
    check: impl Fn(&SimPlatform, &Case),
) -> Covered {
    enumerate_with(seed, true, setup, |_, s| op(s), check)
}

/// As [`enumerate`], over any state `setup` makes beside the platform (a
/// store, several, or none; dropped with the power off, so nothing it does
/// reaches the media): `op` gets both. The power is cut at a point (the
/// simulator's step probe): the operation runs on against a dead device,
/// every later step failing with EIO, as a process would until the crash,
/// and returns through its own error paths. With `settle`, every state
/// is judged twice: as the store left it, and with the file system's
/// metadata (names, lengths) made durable at the crash, as a journaling file
/// system may have.
pub(crate) fn enumerate_with<T>(
    seed: u64,
    settle: bool,
    setup: impl Fn() -> (SimPlatform, T),
    op: impl Fn(&SimPlatform, &T),
    check: impl Fn(&SimPlatform, &Case),
) -> Covered {
    let mut covered = Covered::default();
    // Runs the operation with the power cut before step `point` (`None`: not
    // cut) and crashes with `fates` (all lost when `None`), the metadata
    // settled first with `settled`; returns the platform, the writes cached
    // at the crash (file, offset, blocks), and whether the operation made
    // fewer steps than the point (the point is past its end).
    let run = |point: Option<usize>,
               fates: Option<&[Fate]>,
               settled: bool|
     -> (SimPlatform, Vec<(FileId, u64, usize)>, bool) {
        let (p, state) = setup();
        let count = Arc::new(AtomicUsize::new(0));
        if let Some(k) = point {
            let c = Arc::clone(&count);
            p.with_world(|w| {
                w.set_step_probe(Some(Arc::new(move || {
                    let n = c.fetch_add(1, Ordering::Relaxed) + 1;
                    // An operation retrying a dead device for ever.
                    assert!(n < k + RUN_ON, "{n} steps after the power cut");
                    n == k
                })));
            });
        }
        op(&p, &state);
        p.with_world(|w| w.set_step_probe(None));
        let past_end = point.is_some_and(|k| count.load(Ordering::Relaxed) < k);
        let cached = p.with_world(|w| w.cached_extents());
        // The power fails (at the end too) before the state is dropped:
        // nothing its drop does reaches the media.
        p.with_world(World::cut_power);
        drop(state);
        p.with_world(|w| {
            if settled {
                w.settle_metadata();
            }
            w.crash(&CrashMode::Exact(fates.unwrap_or_default().to_vec()));
        });
        (p, cached, past_end)
    };
    let mut k = 1usize;
    loop {
        // The point (a step, or the end): its cached writes first.
        let point = Some(k);
        let (_, cached, past_end) = run(point, None, false);
        let at = if past_end { None } else { point };
        covered.points += 1;
        let blocks: Vec<usize> = cached.iter().map(|c| c.2).collect();
        let (all, sampled) = fates(&blocks, seed ^ k as u64);
        covered.sampled += usize::from(sampled);
        for settled in [false, true].into_iter().take(1 + usize::from(settle)) {
            for f in &all {
                let (p, again, _) = run(at, Some(f), settled);
                assert_eq!(
                    again, cached,
                    "the operation ran differently (point {at:?})"
                );
                let case = Case {
                    point: at,
                    fates: f.clone(),
                    settled,
                };
                let crashed = p.with_world(|w| w.clone());
                check(&p, &case);
                p.with_world(|w| *w = crashed);
                agree(&p, DIR, &case);
                covered.cases += 1;
            }
        }
        if past_end {
            break;
        }
        k += 1;
        assert!(k < 10_000, "an operation without end");
    }
    covered
}
