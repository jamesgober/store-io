//! Crash enumeration inside the writable open (v0.5 E7 C3, spec §9): the
//! open's own writes (the flush of a dead owner's cached writes, the heal of
//! a torn metadata write, the replacement of a damaged volume copy, the
//! volume record that marks the store open) crashed at every step with
//! every fate of what is cached, from each state that makes the open write.
//! The next open reaches the verdict an uninterrupted open reaches (checked
//! against the spec once), the data intact; once the open has returned, its
//! work is durable.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use,
    // Each enumeration reports how much it covered (E7 plan §3b).
    clippy::print_stderr
)]

mod format2_util;

use std::cell::RefCell;
use std::path::Path;

use format2_util::audit::audit;
use format2_util::craft::{lose_page, tear_newer_copy_of};
use format2_util::enumerate::{Case, Outcomes, enumerate_with, summary};
use format2_util::*;
use store_io_engine::{Healed, Store, StoreReport};
use store_io_sim::{SimConfig, SimPlatform};

const B: u64 = 4096;

/// A store with a page region holding a block of 5s and a slot holding
/// "object", closed cleanly.
fn stored(seed: u64) -> SimPlatform {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    let s = create(&p);
    let r = s.provision_page_region("p", 8 * B).unwrap();
    r.write_durable(r.pos(0).unwrap(), &[5; B as usize])
        .unwrap();
    s.provision_slot("s").unwrap().commit(b"object").unwrap();
    drop((r, s));
    p
}

/// The same, the power failing with it open (no clean close).
fn stopped(seed: u64) -> SimPlatform {
    let p = stored(seed);
    crash_open(&p, open(&p));
    p
}

/// What an open decided: the regions and their lengths, stale and lost
/// pages ([`summary`]), the freeze, read-only, the unavailable regions and
/// every per-region finding.
fn verdict(s: &Store<SimPlatform>) -> String {
    let r = s.report();
    let m = &r.metadata;
    format!(
        "{} frozen {} read-only {} unavailable {:?} invalid {:?} extra {:?} superseded {:?} \
         shares {:?} agreement {:?} lost headers {:?} truncated {}",
        summary(s),
        m.frozen,
        r.read_only,
        r.unavailable.iter().map(|u| u.id).collect::<Vec<_>>(),
        m.invalid_entries,
        m.extra_extents,
        m.superseded,
        m.shares_reserved,
        m.stale_page_agreement,
        m.lost_header_writes,
        m.truncated
    )
}

/// One state the open writes in, and what the spec says of it.
struct State {
    name: &'static str,
    seed: u64,
    /// Makes the media the open finds.
    damaged: fn(u64) -> SimPlatform,
    /// The spec's verdict, asserted on an uninterrupted open's report.
    spec: fn(&StoreReport),
    /// The region and the slot hold what [`stored`] put there.
    data: bool,
    /// The open writes nothing (the media after the crash, and after the
    /// next open, as the open found it).
    writes_nothing: bool,
    /// The outcomes reached (see [`judge`]), and the one once the open has
    /// returned.
    outcomes: &'static [&'static str],
    after: &'static str,
}

/// The next open after a crash inside one: its verdict the reference's, its
/// owner generation the reference's (the crashed open's record lost) or the
/// next (it landed). The outcome: "pending" (a heal or a damaged volume
/// copy still to replace), "unrecorded" (nothing of the open's own record
/// durable) or "recorded".
fn judge(
    s: &Store<SimPlatform>,
    reference: &(String, u64),
    name: &str,
    case: &Case,
) -> &'static str {
    assert_eq!(verdict(s), reference.0, "{name} {case:?}");
    let r = s.report();
    let generation = r.owner_generation;
    assert!(
        [reference.1, reference.1 + 1].contains(&generation),
        "{name} {case:?}: owner generation {generation}, the reference's {}",
        reference.1
    );
    if r.metadata.healed.is_some() || r.metadata.volume_stale {
        "pending"
    } else if generation == reference.1 {
        "unrecorded"
    } else {
        "recorded"
    }
}

fn every_crash_of_an_open(state: &State) {
    let name = state.name;
    let reference = {
        let p = (state.damaged)(state.seed);
        let s = open(&p);
        (state.spec)(s.report());
        (verdict(&s), s.report().owner_generation)
    };
    let seen = Outcomes::default();
    // The media the open found (this run's: a store's ids are random).
    let found = RefCell::new(Vec::new());
    let covered = enumerate_with(
        state.seed,
        false,
        || ((state.damaged)(state.seed), RefCell::new(None)),
        |p, s| {
            *found.borrow_mut() = media(p);
            // Kept open: the power fails with it open.
            *s.borrow_mut() = Store::open(p.clone(), Path::new(DIR), opts()).ok();
        },
        |p, case| {
            if state.writes_nothing {
                assert!(media(p) == *found.borrow(), "{name} wrote: {case:?}");
            }
            let s = Store::open(p.clone(), Path::new(DIR), opts())
                .unwrap_or_else(|e| panic!("{name} {case:?}: {e:?}"));
            seen.saw(case, judge(&s, &reference, name, case));
            if state.data {
                let r = s.page_region("p").unwrap();
                let mut back = vec![0u8; B as usize];
                r.read(r.pos(0).unwrap(), &mut back).unwrap();
                assert!(back == [5; B as usize], "{name} {case:?}");
                assert_eq!(
                    s.slot("s").unwrap().read().as_deref(),
                    Some(&b"object"[..]),
                    "{name} {case:?}"
                );
            }
            let checked = !s.report().metadata.frozen && !s.report().read_only;
            drop(s);
            if state.writes_nothing {
                // Its writes, durable or still cached.
                let written = p.with_world(|w| {
                    let mut all = w.clone();
                    all.crash(&store_io_sim::CrashMode::KeepAll);
                    all.media_of(&DIR.into(), store_io_engine::CONTAINER)
                        .unwrap()
                        .to_vec()
                });
                assert!(written == *found.borrow(), "{name}: the next open wrote");
            }
            if checked {
                audit(p);
            }
        },
    );
    eprintln!("open {name}: {covered:?}");
    assert!(covered.points > 1, "{name}: no crash point");
    seen.expect(state.outcomes, state.after);
}

fn fine(r: &StoreReport) {
    let m = &r.metadata;
    assert!(!m.frozen && !r.read_only && m.healed.is_none() && !m.volume_stale);
}

/// After a clean close: the open's record that the store is open landed,
/// or not (the clean close stands).
#[test]
fn test_every_crash_of_an_open_after_a_clean_close() {
    every_crash_of_an_open(&State {
        name: "clean",
        seed: 1800,
        damaged: stored,
        spec: |r| {
            fine(r);
            assert!(r.metadata.clean_close);
        },
        data: true,
        writes_nothing: false,
        outcomes: &["recorded", "unrecorded"],
        after: "recorded",
    });
}

/// After a stop with the store open: unclean either way.
#[test]
fn test_every_crash_of_an_open_after_an_unclean_stop() {
    every_crash_of_an_open(&State {
        name: "stopped",
        seed: 1801,
        damaged: stopped,
        spec: |r| {
            fine(r);
            assert!(!r.metadata.clean_close);
        },
        data: true,
        writes_nothing: false,
        outcomes: &["recorded", "unrecorded"],
        after: "recorded",
    });
}

/// A torn clean-close record: the open heals it (spec §9); a crash in the
/// heal leaves it to heal again, never a store that does not open, and one
/// after the heal may still lose the open's own record.
#[test]
fn test_every_crash_of_an_open_healing_the_volume_record() {
    every_crash_of_an_open(&State {
        name: "torn volume record",
        seed: 1802,
        damaged: |seed| {
            let p = stored(seed);
            tear(&p, newest_volume_slot(&p));
            p
        },
        spec: |r| {
            assert_eq!(r.metadata.healed, Some(Healed::VolumeRecord));
            assert!(!r.metadata.frozen && !r.read_only && !r.metadata.clean_close);
        },
        data: true,
        writes_nothing: false,
        outcomes: &["pending", "recorded", "unrecorded"],
        after: "recorded",
    });
}

/// A torn write of the page holding the regions, left by a stop: the open
/// heals it, the regions whole.
#[test]
fn test_every_crash_of_an_open_healing_a_page_of_regions() {
    every_crash_of_an_open(&State {
        name: "torn page",
        seed: 1803,
        damaged: |seed| {
            let p = stopped(seed);
            tear_newer_copy_of(&p, 0);
            p
        },
        spec: |r| {
            assert_eq!(r.metadata.healed, Some(Healed::Page(0)));
            assert!(r.metadata.stale_pages.is_empty() && !r.metadata.frozen && !r.read_only);
        },
        data: true,
        writes_nothing: false,
        outcomes: &["pending", "recorded", "unrecorded"],
        after: "recorded",
    });
}

/// A damaged newer volume copy (not a tear): the open writes over it.
#[test]
fn test_every_crash_of_an_open_replacing_a_damaged_volume_copy() {
    every_crash_of_an_open(&State {
        name: "damaged volume copy",
        seed: 1804,
        damaged: |seed| {
            let p = SimPlatform::new(SimConfig::volatile(seed));
            crash_open(&p, create(&p));
            put(&p, block(&p), &volume_copy_b(&p, 3, true));
            p
        },
        spec: |r| {
            assert!(r.metadata.volume_stale);
            assert!(!r.metadata.frozen && !r.read_only && r.metadata.healed.is_none());
        },
        data: false,
        writes_nothing: false,
        outcomes: &["pending", "recorded"],
        after: "recorded",
    });
}

/// A stale page (after a clean close: damage, not a tear): frozen, nothing
/// healed, whatever the crash.
#[test]
fn test_every_crash_of_an_open_of_a_stale_page() {
    every_crash_of_an_open(&State {
        name: "stale page",
        seed: 1805,
        damaged: |seed| {
            let p = stored(seed);
            tear_newer_copy_of(&p, 0);
            p
        },
        spec: |r| {
            assert_eq!(r.metadata.stale_pages, [0]);
            assert!(r.metadata.frozen && !r.read_only && r.metadata.healed.is_none());
        },
        data: true,
        writes_nothing: false,
        outcomes: &["recorded", "unrecorded"],
        after: "recorded",
    });
}

/// A lost page (no usable copy): frozen, whatever the crash.
#[test]
fn test_every_crash_of_an_open_of_a_lost_page() {
    every_crash_of_an_open(&State {
        name: "lost page",
        seed: 1806,
        damaged: |seed| {
            let p = stored(seed);
            lose_page(&p, 2);
            p
        },
        spec: |r| {
            assert_eq!(r.metadata.lost_pages, [2]);
            assert!(r.metadata.frozen && !r.read_only);
        },
        data: true,
        writes_nothing: false,
        outcomes: &["recorded", "unrecorded"],
        after: "recorded",
    });
}

/// A stop's torn write of page 1 (one a writable open heals) and, with
/// `foreign`, a copy of page 0 naming a read-only feature this build does
/// not know.
fn torn_beside(seed: u64, foreign: bool) -> SimPlatform {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    crash_open(&p, create(&p));
    if foreign {
        let mut slot = page_copy_b(&p, 0, false);
        slot[20] = 0x80;
        rehash(&mut slot);
        put(&p, page_at(&p, 0, 1), &slot);
    }
    put(&p, page_at(&p, 1, 1), &page_copy_b(&p, 1, true));
    p
}

/// The foreign read-only feature beside the torn page: opened read only,
/// nothing written (the torn copy left as found), whatever the crash.
#[test]
fn test_every_crash_of_an_open_made_read_only() {
    // Without the feature, the open heals the page.
    let p = torn_beside(1807, false);
    assert_eq!(open(&p).report().metadata.healed, Some(Healed::Page(1)));
    every_crash_of_an_open(&State {
        name: "read-only feature",
        seed: 1807,
        damaged: |seed| torn_beside(seed, true),
        spec: |r| {
            assert!(r.read_only);
            assert_eq!(
                r.metadata.read_only_reason,
                Some("the metadata carries a feature this version may read but not write")
            );
        },
        data: false,
        writes_nothing: true,
        outcomes: &["unrecorded"],
        after: "unrecorded",
    });
}

/// A dead owner (its process gone, the machine up) left a completed write
/// in the cache: the open makes it durable, so a crash before the open's
/// first flush keeps or loses it, and after it, keeps it. (Any flush of the
/// open makes it durable, the drain or its own record's: this does not pin
/// the drain; the provisioning below does.)
#[test]
fn test_every_crash_of_an_open_after_a_dead_owner() {
    let seed = 1808;
    let damaged = || {
        let p = stored(seed);
        let s = open(&p);
        let r = s.page_region("p").unwrap();
        r.write(r.pos(0).unwrap(), &[6; B as usize]).unwrap();
        // The process dies: its locks and handles gone, its write cached.
        p.with_world(store_io_sim::World::kill);
        drop((r, s));
        assert_eq!(p.with_world(|w| w.cached_extents().len()), 1);
        p
    };
    let reference = {
        let p = damaged();
        let s = open(&p);
        fine(s.report());
        (verdict(&s), s.report().owner_generation)
    };
    let seen = Outcomes::default();
    let covered = enumerate_with(
        seed,
        false,
        || (damaged(), RefCell::new(None)),
        |p, s| {
            *s.borrow_mut() = Store::open(p.clone(), Path::new(DIR), opts()).ok();
        },
        |p, case| {
            let s = open(p);
            judge(&s, &reference, "dead owner", case);
            let r = s.page_region("p").unwrap();
            let mut back = vec![0u8; B as usize];
            r.read(r.pos(0).unwrap(), &mut back).unwrap();
            if back == [6; B as usize] {
                seen.saw(case, "kept");
            } else {
                assert!(back == [5; B as usize], "{case:?}");
                seen.saw(case, "lost");
            }
            drop((r, s));
            audit(p);
        },
    );
    eprintln!("open dead owner: {covered:?}");
    seen.expect(&["kept", "lost"], "kept");
}

/// A dead owner stopped (its process frozen, then gone, the machine up)
/// between its clean close's volume record and the flush that would make
/// it durable: its newest record is in the cache only, the open's own
/// record due over the older copy, on 512-byte sectors (each write torn at
/// every sector). The next open finds the store as the dead close left it
/// or as the open recorded it, never older: the owner generation the
/// reference's or the next. (The open's drain of a dead owner's cache does
/// not show here: a record's bytes fit its first sector, so a prefix tear
/// never loses it. A cached table page, which spans sectors, shows it: the
/// provisioning below.)
#[test]
fn test_every_crash_of_an_open_after_an_owner_dead_before_its_flush() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let seed = 1809;
    let config = || SimConfig {
        logical_block: 512,
        ..SimConfig::volatile(seed)
    };
    // The step of the dead owner's close to stop at: the first at which its
    // volume record is cached (found once; the same in every run).
    let freeze = |k: usize| {
        let p = SimPlatform::new(config());
        drop(create(&p));
        let s = open(&p);
        let steps = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&steps);
        p.with_world(|w| {
            w.set_step_probe(Some(Arc::new(move || {
                c.fetch_add(1, Ordering::Relaxed) + 1 == k
            })));
        });
        drop(s);
        p.with_world(|w| {
            w.set_step_probe(None);
            w.kill();
            w.restore_power();
        });
        let record_cached = p.with_world(|w| {
            w.cached_extents()
                .iter()
                .any(|&(_, at, _)| at == 0 || at == block(&p))
        });
        (p, record_cached, steps.load(Ordering::Relaxed) >= k)
    };
    let k = (1..)
        .find(|&k| {
            let (_, cached, stopped) = freeze(k);
            assert!(stopped, "the close ended without its record cached");
            cached
        })
        .unwrap();
    let reference = {
        let (p, _, _) = freeze(k);
        let s = open(&p);
        fine(s.report());
        assert!(s.report().metadata.clean_close, "the drained close counts");
        (verdict(&s), s.report().owner_generation)
    };
    let seen = Outcomes::default();
    let covered = enumerate_with(
        seed,
        false,
        || (freeze(k).0, RefCell::new(None)),
        |p, s| {
            *s.borrow_mut() = Store::open(p.clone(), Path::new(DIR), opts()).ok();
        },
        |p, case| {
            let s = Store::open(p.clone(), Path::new(DIR), opts())
                .unwrap_or_else(|e| panic!("{case:?}: {e:?}"));
            seen.saw(
                case,
                judge(&s, &reference, "owner dead before its flush", case),
            );
            drop(s);
            audit(p);
        },
    );
    eprintln!("open owner dead before its flush (step {k}): {covered:?}");
    assert!(covered.cases > covered.points);
    seen.expect(&["recorded", "unrecorded"], "recorded");
}

/// A dead owner stopped (frozen, then gone, the machine up) between a
/// provisioning's table page write and the flush after it: the page's
/// newest copy is in the cache only, and spans several 512-byte sectors
/// (eight regions on it), so a crash can tear it. The open drains it before
/// reading any slot. Read from the cache instead, the open's own record
/// would be numbered past it, and a crash losing or tearing it would leave
/// the store recorded open without the region, or the tear taken for
/// damage (the page stale, the store frozen). So: the region present (as
/// an uninterrupted open finds it) or absent, never frozen; recorded only
/// with it.
#[test]
fn test_every_crash_of_an_open_after_an_owner_dead_inside_a_provisioning() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let seed = 1810;
    let config = || SimConfig {
        logical_block: 512,
        ..SimConfig::volatile(seed)
    };
    let freeze = |k: usize| {
        let p = SimPlatform::new(config());
        let s = create(&p);
        for n in ["a", "b", "c", "d", "e", "f", "g"] {
            s.provision_slot(n).unwrap();
        }
        drop(s);
        let s = open(&p);
        let steps = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&steps);
        p.with_world(|w| {
            w.set_step_probe(Some(Arc::new(move || {
                c.fetch_add(1, Ordering::Relaxed) + 1 == k
            })));
        });
        s.provision_slot("r");
        p.with_world(|w| {
            w.set_step_probe(None);
            w.kill();
            w.restore_power();
        });
        drop(s);
        let page = [page_at(&p, 0, 0), page_at(&p, 0, 1)];
        let cached = p.with_world(|w| {
            w.cached_extents()
                .iter()
                .any(|&(_, at, blocks)| page.contains(&at) && blocks > 1)
        });
        (p, cached, steps.load(Ordering::Relaxed) >= k)
    };
    // The step to stop at: the first with the page's copy cached.
    let k = (1..)
        .find(|&k| {
            let (_, cached, stopped) = freeze(k);
            assert!(stopped, "the provisioning ended without its page cached");
            cached
        })
        .unwrap();
    let with = {
        let (p, _, _) = freeze(k);
        let s = open(&p);
        fine(s.report());
        assert!(s.slot("r").is_ok(), "the drained page names the region");
        (verdict(&s), s.report().owner_generation)
    };
    let without = {
        let (p, _, _) = freeze(k);
        p.with_world(|w| w.crash(&store_io_sim::CrashMode::LoseAll));
        let s = open(&p);
        assert!(s.slot("r").is_err());
        verdict(&s)
    };
    let seen = Outcomes::default();
    let covered = enumerate_with(
        seed,
        false,
        || (freeze(k).0, RefCell::new(None)),
        |p, s| {
            *s.borrow_mut() = Store::open(p.clone(), Path::new(DIR), opts()).ok();
        },
        |p, case| {
            let s = Store::open(p.clone(), Path::new(DIR), opts())
                .unwrap_or_else(|e| panic!("{case:?}: {e:?}"));
            let v = verdict(&s);
            let generation = s.report().owner_generation;
            assert!(
                [with.1, with.1 + 1].contains(&generation),
                "{case:?}: owner generation {generation}"
            );
            let outcome = if s.report().metadata.healed.is_some() {
                // The heal rewrites the older winner, the crashed open's
                // record never landed.
                assert_eq!(v, without, "{case:?}");
                assert_eq!(generation, with.1, "healed after a record: {case:?}");
                "pending"
            } else if v == with.0 {
                if generation == with.1 {
                    "kept"
                } else {
                    "recorded"
                }
            } else {
                assert_eq!(v, without, "{case:?}");
                assert_eq!(generation, with.1, "recorded without the region: {case:?}");
                "lost"
            };
            seen.saw(case, outcome);
            drop(s);
            audit(p);
        },
    );
    eprintln!("open owner dead inside a provisioning (step {k}): {covered:?}");
    assert!(covered.cases > covered.points);
    seen.expect(&["kept", "lost", "pending", "recorded"], "recorded");
}
