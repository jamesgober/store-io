//! Device pacing wired into the store (PLANNING R5.4; V3c1): every store
//! joins its device's group when it opens; a commit-class barrier's entry,
//! and on a power-safe store a `dsync` write, marks the group's commits
//! active; a hold keeps them active; the pacing options are checked.

#![allow(clippy::unwrap_used, unused_results)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use store_io_core::IoClass;
use store_io_core::error::{Error, NotWrittenCause};
use store_io_core::identity::{DeviceId, DeviceIdentity, DeviceNode};
use store_io_engine::completion::{Completion, Rejected};
use store_io_engine::{
    CONTAINER, CopyOptions, Store, StoreOptions, SubmitterConfig, WriteMode, copy_store,
};
use store_io_platform::Platform;
use store_io_sim::world::TraceEvent;
use store_io_sim::{DeviceKind, SimConfig, SimPlatform};

const SECOND: u64 = 1_000_000_000;

/// A world of two devices (the second `second`, with `/b` on it), each
/// given a complete identity of its own and its own file-system device.
fn two_devices(seed: u64, second: DeviceKind) -> SimPlatform {
    let cfg = match second {
        DeviceKind::PowerSafe => SimConfig::power_safe(seed),
        DeviceKind::VolatileCache => SimConfig::volatile(seed),
    };
    let p = SimPlatform::new(cfg.with_device(second));
    p.with_world(|w| w.place("/b", 1));
    // Any store's evidence, as the simulator's probe reports it.
    let ev = Store::create(p.clone(), Path::new("/probe"), StoreOptions::default())
        .unwrap()
        .report()
        .evidence
        .clone();
    for d in 0..2 {
        let mut e = ev.clone();
        e.device.identity = DeviceIdentity {
            devices: vec![DeviceId::node(DeviceNode::Sim { device: d as u32 })],
            complete: true,
        };
        e.fs.device = Some((p.identity_scope() << 16) | d as u64);
        p.with_world(|w| w.set_device_evidence(d, Some(e)));
    }
    p
}

fn create(p: &SimPlatform, dir: &str) -> Store<SimPlatform> {
    Store::create(p.clone(), Path::new(dir), StoreOptions::default()).unwrap()
}

/// A durable append and its barrier, in a region of its own (never one a
/// test's background writes go to: its `sync_through` would wait for them,
/// and they are reaped only by the test's own thread).
fn commit(s: &Store<SimPlatform>) {
    let r = match s.append_region("commits") {
        Ok(r) => r,
        Err(_) => s.provision_append_region("commits", 1 << 20).unwrap(),
    };
    let t = r.append(&[7u8; 4096]).unwrap();
    let _receipt = r.sync_through(&t).unwrap();
}

/// The byte at `i` of an operation seeded `seed`: every 4 KiB block of a
/// 1 MiB operation differs, so a piece landing at another offset (or two
/// swapped) never reads back right.
fn byte_at(i: usize, seed: u8) -> u8 {
    ((i / 4096) as u8).wrapping_mul(37) ^ (i as u8) ^ seed
}

fn pattern(buf: &mut [u8], seed: u8) {
    for (i, b) in buf.iter_mut().enumerate() {
        *b = byte_at(i, seed);
    }
}

fn is_pattern(back: &[u8], seed: u8) -> bool {
    back.iter().enumerate().all(|(i, &b)| b == byte_at(i, seed))
}

fn advance(p: &SimPlatform, ns: u64) {
    p.with_world(|w| w.advance_clock_ns(ns));
}

#[test]
fn test_a_commit_paces_its_device_and_not_another() {
    let p = two_devices(1, DeviceKind::VolatileCache);
    let (a, a2, b) = (create(&p, "/a"), create(&p, "/a2"), create(&p, "/b"));
    // Give every store's seed time to pass (none is unknown: no seed).
    advance(&p, 6 * SECOND);
    assert!(!a.qos().active && !b.qos().active);
    commit(&a);
    assert!(a.qos().active && a2.qos().active, "the device's stores");
    assert!(!b.qos().active, "another device is quiet");
    assert_eq!(a.qos().members, 2);
    assert!(!a.qos().unknown);
    advance(&p, 6 * SECOND);
    assert!(!a2.qos().active, "past commit_quiet");
}

#[test]
fn test_a_read_only_open_joins_its_device_s_group() {
    let p = two_devices(2, DeviceKind::VolatileCache);
    let a = create(&p, "/a");
    commit(&a);
    drop(a);
    let ro = Store::open_readonly(p.clone(), Path::new("/a"), StoreOptions::default()).unwrap();
    let w = create(&p, "/a3");
    advance(&p, 6 * SECOND);
    commit(&w);
    assert!(
        ro.qos().active,
        "a read-only store is paced by its device too"
    );
}

#[test]
fn test_a_power_safe_store_s_dsync_writes_mark_activity() {
    let p = two_devices(3, DeviceKind::PowerSafe);
    let a = create(&p, "/a");
    advance(&p, 6 * SECOND);
    assert!(!a.qos().active);
    // No barrier flushes on a power-safe store: the write itself counts.
    let r = a.provision_append_region("log", 1 << 20).unwrap();
    advance(&p, 6 * SECOND);
    let _t = r.append(&[1u8; 4096]).unwrap();
    assert!(
        a.qos().active,
        "a dsync write is a commit on a power-safe store"
    );
}

#[test]
fn test_a_power_safe_store_s_page_writes_mark_activity() {
    // A one-block page write: one write, performed at once on an inline
    // queue (`STORE_IO_SIM_INLINE`), else through the pump.
    let p = two_devices(11, DeviceKind::PowerSafe);
    let a = create(&p, "/a");
    let pages = a.provision_page_region("pages", 1 << 20).unwrap();
    advance(&p, 6 * SECOND);
    assert!(!a.qos().active);
    let _t = pages.write(pages.pos(0).unwrap(), &[3u8; 4096]).unwrap();
    assert!(a.qos().active, "a dsync page write is a commit");
}

#[test]
fn test_a_power_safe_store_s_submitter_writes_mark_activity() {
    let p = two_devices(9, DeviceKind::PowerSafe);
    let a = create(&p, "/a");
    let r = a.provision_append_region("log", 1 << 20).unwrap();
    let mut sub = a.submitter(SubmitterConfig::default()).unwrap();
    advance(&p, 6 * SECOND);
    assert!(!a.qos().active);
    let buf = sub.take_buf(4096).unwrap();
    sub.append(&r, buf, WriteMode::Durable, 1).unwrap();
    assert!(a.qos().active, "a Submitter's dsync write is a commit");
    let _done = sub.wait(1).unwrap().count();
}

#[test]
fn test_a_store_on_a_memory_file_system_is_never_paced() {
    let p = two_devices(10, DeviceKind::VolatileCache);
    // Made outside the world's lock (a store's I/O takes it).
    let mut ev = Store::create(p.clone(), Path::new("/m0"), StoreOptions::default())
        .unwrap()
        .report()
        .evidence
        .clone();
    ev.fs.kind = store_io_core::evidence::FsKind::Tmpfs;
    p.with_world(|w| w.set_device_evidence(1, Some(ev)));
    // tmpfs is no durable device: created under the labelled override.
    let mut o = StoreOptions::default();
    o.trust.override_refusal = true;
    let m = Store::create(p.clone(), Path::new("/b/m"), o).unwrap();
    let a = create(&p, "/a");
    advance(&p, 6 * SECOND);
    commit(&m);
    let q = m.qos();
    assert!(!q.paced && !q.active && q.members == 1, "{q:?}");
    assert!(!a.qos().active, "a memory store's commits pace nobody");
}

#[test]
fn test_a_hold_keeps_the_device_active_until_dropped() {
    let p = two_devices(4, DeviceKind::VolatileCache);
    let (a, a2) = (create(&p, "/a"), create(&p, "/a2"));
    advance(&p, 6 * SECOND);
    let h = a.commit_hold();
    assert!(a2.qos().active);
    advance(&p, 600 * SECOND);
    assert!(a2.qos().active, "a hold needs no commit and never expires");
    drop(h);
    assert!(!a2.qos().active);
}

#[test]
fn test_stores_of_two_worlds_never_share_a_group() {
    let p = two_devices(5, DeviceKind::VolatileCache);
    let q = two_devices(6, DeviceKind::VolatileCache);
    let (a, x) = (create(&p, "/a"), create(&q, "/a"));
    advance(&p, 6 * SECOND);
    advance(&q, 6 * SECOND);
    commit(&a);
    assert!(
        a.qos().active && !x.qos().active,
        "identical identities, other world"
    );
    assert_eq!(x.qos().members, 1);
}

#[test]
fn test_stores_with_unknown_identities_are_paced_by_their_world() {
    // The simulator's default: no identity, no file-system device read.
    let p = SimPlatform::new(SimConfig::volatile(7));
    let a = create(&p, "/a");
    assert!(a.qos().unknown, "{:?}", a.report().evidence.device.identity);
}

#[test]
fn test_pacing_options_out_of_range_are_refused() {
    let p = SimPlatform::new(SimConfig::volatile(8));
    let bad = [
        StoreOptions {
            background_io: 4095,
            ..StoreOptions::default()
        },
        StoreOptions {
            background_io: 6000,
            ..StoreOptions::default()
        },
        StoreOptions {
            background_io: 512 << 10,
            ..StoreOptions::default()
        },
        StoreOptions {
            background_io: 0,
            ..StoreOptions::default()
        },
        StoreOptions {
            background_unpaced: 0,
            ..StoreOptions::default()
        },
        StoreOptions {
            background_unpaced: 65,
            ..StoreOptions::default()
        },
        StoreOptions {
            commit_quiet: Duration::from_micros(999),
            ..StoreOptions::default()
        },
    ];
    for opts in bad {
        let e = Store::create(p.clone(), Path::new("/x"), opts.clone()).map(|_| ());
        assert!(
            matches!(
                e,
                Err(Error::NotWritten {
                    cause: NotWrittenCause::InvalidOptions,
                    ..
                })
            ),
            "{opts:?}: {e:?}"
        );
    }
    let ok = StoreOptions {
        background_io: 64 << 10,
        background_unpaced: 64,
        commit_quiet: Duration::from_millis(1),
        background_pacing: false,
        ..StoreOptions::default()
    };
    let s = Store::create(p.clone(), Path::new("/y"), ok.clone()).unwrap();
    assert!(!s.qos().paced);
    // A store whose largest I/O is under the default piece is not refused:
    // it splits at its largest I/O.
    let small = StoreOptions {
        max_io: 128 << 10,
        ..StoreOptions::default()
    };
    let z = Store::create(p.clone(), Path::new("/z"), small).unwrap();
    assert_eq!(z.qos().background_io, 128 << 10, "split at its largest I/O");
    // A piece is whole blocks, at least one: 4 KiB on 16 KiB blocks is one
    // block, never a partial-block I/O.
    let big_blocks = StoreOptions {
        block_size: Some(16384),
        background_io: 4096,
        ..StoreOptions::default()
    };
    let b = Store::create(p.clone(), Path::new("/bb"), big_blocks).unwrap();
    assert_eq!(b.report().block_size, 16384);
    assert_eq!(b.qos().background_io, 16384);
    let d = Store::create(p.clone(), Path::new("/d"), StoreOptions::default()).unwrap();
    assert_eq!(d.qos().background_io, 256 << 10);
    drop(s);
    assert!(Store::open(p.clone(), Path::new("/y"), ok).is_ok());
    let bad_open = StoreOptions {
        background_unpaced: 0,
        ..StoreOptions::default()
    };
    assert!(Store::open(p.clone(), Path::new("/y"), bad_open.clone()).is_err());
    assert!(Store::open_readonly(p, Path::new("/y"), bad_open).is_err());
}

// ------------------------------------------------- the background Submitter

fn background(s: &Store<SimPlatform>) -> store_io_engine::Submitter<SimPlatform> {
    s.submitter(SubmitterConfig::default().with_class(IoClass::Background))
        .unwrap()
}

/// Every completion of `sub`, waited for until none is in flight.
fn drain(sub: &mut store_io_engine::Submitter<SimPlatform>) -> Vec<Completion> {
    let mut out = Vec::new();
    while sub.in_flight() > 0 {
        out.extend(sub.wait(1).unwrap());
    }
    out.extend(sub.reap());
    out
}

fn is_paced(r: &Result<store_io_engine::RegionPos, Rejected>) -> bool {
    matches!(
        r,
        Err(Rejected {
            error: Error::NotWritten {
                cause: NotWrittenCause::Paced,
                ..
            },
            buf: Some(_),
        })
    )
}

#[test]
fn test_a_background_submitter_is_paced_while_commits_are_active() {
    let p = two_devices(20, DeviceKind::VolatileCache);
    let a = create(&p, "/a");
    let log = a.provision_append_region("log", 4 << 20).unwrap();
    let mut sub = background(&a);
    commit(&a);
    assert!(a.qos().active);
    let first = sub
        .append(&log, sub.take_buf(4096).unwrap(), WriteMode::Plain, 1)
        .unwrap();
    // One piece in flight while commits are active: the next is refused
    // before anything is reserved, its buffer given back.
    let second = sub.append(&log, sub.take_buf(4096).unwrap(), WriteMode::Plain, 2);
    assert!(is_paced(&second), "{second:?}");
    assert_eq!(a.qos().in_flight, 1);
    let _done = drain(&mut sub);
    assert_eq!(
        a.qos().in_flight,
        0,
        "a completed piece's charge is released"
    );
    let next = sub
        .append(&log, sub.take_buf(4096).unwrap(), WriteMode::Plain, 3)
        .unwrap();
    assert_eq!(
        next.offset(),
        first.offset() + 4096,
        "the refusal reserved nothing"
    );
    let _done = drain(&mut sub);
}

#[test]
fn test_a_background_submitter_runs_unpaced_while_commits_are_quiet() {
    let p = two_devices(21, DeviceKind::VolatileCache);
    let a = create(&p, "/a");
    let log = a.provision_append_region("log", 4 << 20).unwrap();
    let mut sub = background(&a);
    advance(&p, 6 * SECOND);
    for t in 0..8 {
        sub.append(&log, sub.take_buf(4096).unwrap(), WriteMode::Plain, t)
            .unwrap();
    }
    let ninth = sub.append(&log, sub.take_buf(4096).unwrap(), WriteMode::Plain, 9);
    assert!(is_paced(&ninth), "background_unpaced bounds it: {ninth:?}");
    let _done = drain(&mut sub);
}

#[test]
fn test_a_background_write_goes_out_in_pieces() {
    let p = two_devices(22, DeviceKind::VolatileCache);
    let a = create(&p, "/a");
    let log = a.provision_append_region("log", 4 << 20).unwrap();
    let mut sub = background(&a);
    commit(&a);
    let mut buf = sub.take_buf(1 << 20).unwrap();
    pattern(buf.as_mut_slice(), 0x5a);
    let before = p.with_world(|w| w.write_submits());
    let pos = sub.append(&log, buf, WriteMode::Plain, 1).unwrap();
    assert_eq!(
        p.with_world(|w| w.write_submits()) - before,
        1,
        "one piece at once"
    );
    let done = drain(&mut sub);
    assert_eq!(done.len(), 1);
    assert!(done[0].outcome.is_ok(), "{:?}", done[0].outcome);
    // 1 MiB in 256 KiB pieces, each after the last.
    assert_eq!(p.with_world(|w| w.write_submits()) - before, 4);
    assert_eq!(a.qos().in_flight, 0);
    let mut back = vec![0u8; 1 << 20];
    log.read(pos.offset(), &mut back).unwrap();
    assert!(is_pattern(&back, 0x5a), "the pieces in order");
}

#[test]
fn test_a_background_submitter_s_barriers_and_writes_mark_no_activity() {
    let p = two_devices(23, DeviceKind::VolatileCache);
    let a = create(&p, "/a");
    let log = a.provision_append_region("log", 4 << 20).unwrap();
    let mut sub = background(&a);
    advance(&p, 6 * SECOND);
    sub.append(&log, sub.take_buf(4096).unwrap(), WriteMode::Durable, 1)
        .unwrap();
    let done = drain(&mut sub);
    assert!(done.iter().all(|c| c.outcome.is_ok()), "{done:?}");
    assert!(
        !a.qos().active,
        "a background durable write's flush is no commit"
    );
    // A commit-class submitter's flush is.
    let mut commit_sub = a.submitter(SubmitterConfig::default()).unwrap();
    commit_sub
        .append(
            &log,
            commit_sub.take_buf(4096).unwrap(),
            WriteMode::Durable,
            2,
        )
        .unwrap();
    let _done = drain(&mut commit_sub);
    assert!(a.qos().active);
}

#[test]
fn test_a_dropped_background_submitter_finishes_its_write() {
    let p = two_devices(24, DeviceKind::VolatileCache);
    let a = create(&p, "/a");
    let log = a.provision_append_region("log", 4 << 20).unwrap();
    let mut sub = background(&a);
    commit(&a);
    let mut buf = sub.take_buf(1 << 20).unwrap();
    pattern(buf.as_mut_slice(), 0x6b);
    let before = p.with_world(|w| w.write_submits());
    let pos = sub.append(&log, buf, WriteMode::Plain, 1).unwrap();
    // Dropped at once, its later pieces never submitted yet: the drop
    // submits them under pacing and drains (a write never left unfinished).
    drop(sub);
    assert_eq!(p.with_world(|w| w.write_submits()) - before, 4);
    assert!(a.poisoned().is_none(), "no hole was left");
    let mut back = vec![0u8; 1 << 20];
    log.read(pos.offset(), &mut back).unwrap();
    assert!(is_pattern(&back, 0x6b));
}

#[test]
fn test_a_poison_stops_a_background_write_s_pieces() {
    let p = two_devices(25, DeviceKind::VolatileCache);
    let a = create(&p, "/a");
    let log = a.provision_append_region("log", 4 << 20).unwrap();
    let mut sub = background(&a);
    commit(&a);
    let before = p.with_world(|w| w.write_submits());
    let buf = sub.take_buf(1 << 20).unwrap();
    sub.append(&log, buf, WriteMode::Plain, 1).unwrap();
    a.poison();
    let done = drain(&mut sub);
    assert_eq!(done.len(), 1);
    assert!(
        matches!(done[0].outcome, Err(Error::Poisoned { .. })),
        "{:?}",
        done[0].outcome
    );
    assert_eq!(
        p.with_world(|w| w.write_submits()) - before,
        1,
        "no piece after the poison"
    );
}

#[test]
fn test_a_background_submitter_holds_at_most_half_a_buffer_class() {
    let p = two_devices(26, DeviceKind::VolatileCache);
    let opts = StoreOptions {
        buffers_per_class: 8,
        ..StoreOptions::default()
    };
    let a = Store::create(p.clone(), Path::new("/a"), opts).unwrap();
    let sub = background(&a);
    // Background takes move up the classes as each class's half is held,
    // until every class's half is.
    let mut held = Vec::new();
    while let Ok(b) = sub.take_buf(4096) {
        held.push(b);
        assert!(held.len() < 10_000, "the background takes never stop");
    }
    // Exactly half of every class (8 each): 4 of each size, never more.
    let mut per = std::collections::BTreeMap::<usize, usize>::new();
    for b in &held {
        *per.entry(b.capacity()).or_default() += 1;
    }
    assert!(!per.is_empty());
    assert!(per.values().all(|&n| n == 4), "{per:?}");
    // The other half is the commits': a commit-class submitter takes the
    // other four of every class (largest first, so each take is of exactly
    // its class).
    let commit_sub = a.submitter(SubmitterConfig::default()).unwrap();
    let mut commits = Vec::new();
    for &size in per.keys().rev() {
        for _ in 0..4 {
            let b = commit_sub.take_buf(size).unwrap();
            assert_eq!(b.capacity(), size);
            commits.push(b);
        }
    }
    drop(held);
}

#[test]
fn test_a_read_class_submitter_s_writes_are_commits() {
    let p = two_devices(27, DeviceKind::PowerSafe);
    let a = create(&p, "/a");
    let log = a.provision_append_region("log", 4 << 20).unwrap();
    let mut sub = a
        .submitter(SubmitterConfig::default().with_class(IoClass::Read))
        .unwrap();
    advance(&p, 6 * SECOND);
    sub.append(&log, sub.take_buf(4096).unwrap(), WriteMode::Plain, 1)
        .unwrap();
    assert!(a.qos().active, "its writes are commit-class");
    let _done = drain(&mut sub);
    // A background one's are not.
    let mut bg = background(&a);
    advance(&p, 6 * SECOND);
    bg.append(&log, bg.take_buf(4096).unwrap(), WriteMode::Plain, 2)
        .unwrap();
    assert!(
        !a.qos().active,
        "a background power-safe write is no commit"
    );
    let _done = drain(&mut bg);
}

/// Two large background writes in flight when commits become active: their
/// later pieces wait their turn (parked), and every byte lands; a wait or a
/// drop never ends with a piece left behind.
#[test]
fn test_background_pieces_wait_their_turn_once_commits_are_active() {
    for drop_it in [false, true] {
        let p = two_devices(28 + u64::from(drop_it), DeviceKind::VolatileCache);
        let a = create(&p, "/a");
        let log = a.provision_append_region("log", 8 << 20).unwrap();
        let mut sub = background(&a);
        advance(&p, 6 * SECOND);
        let mut at = Vec::new();
        for (i, fill) in [0x11u8, 0x22].into_iter().enumerate() {
            let mut buf = sub.take_buf(1 << 20).unwrap();
            pattern(buf.as_mut_slice(), fill);
            at.push((
                sub.append(&log, buf, WriteMode::Plain, i as u64).unwrap(),
                fill,
            ));
        }
        commit(&a);
        let before = p.with_world(|w| w.write_submits());
        if drop_it {
            drop(sub);
            assert!(a.poisoned().is_none(), "a parked piece was dropped");
        } else {
            let done = drain(&mut sub);
            assert_eq!(done.len(), 2, "a wait ended with a piece left behind");
            assert!(done.iter().all(|c| c.outcome.is_ok()), "{done:?}");
        }
        assert_eq!(
            p.with_world(|w| w.write_submits()) - before,
            6,
            "the six pieces left"
        );
        for (pos, fill) in at {
            let mut back = vec![0u8; 1 << 20];
            log.read(pos.offset(), &mut back).unwrap();
            assert!(is_pattern(&back, fill), "a write's pieces out of place");
        }
    }
}

/// Two background appends of 1 MiB issued while commits are quiet; then
/// commits. One write submitter step per reap (the simulator completes
/// whatever is submitted).
/// What [`two_appends_then_commits`] leaves: the world, the store, the
/// region, the submitter and each write's position and fill.
struct TwoAppends {
    p: SimPlatform,
    a: Store<SimPlatform>,
    log: store_io_engine::AppendRegion<SimPlatform>,
    sub: store_io_engine::Submitter<SimPlatform>,
    at: Vec<(store_io_engine::RegionPos, u8)>,
}

fn two_appends_then_commits(seed: u64) -> TwoAppends {
    let p = two_devices(seed, DeviceKind::VolatileCache);
    let a = create(&p, "/a");
    let log = a.provision_append_region("log", 8 << 20).unwrap();
    let mut sub = background(&a);
    advance(&p, 6 * SECOND);
    let mut at = Vec::new();
    for (t, fill) in [(1u64, 0x31u8), (2, 0x32)] {
        let mut buf = sub.take_buf(1 << 20).unwrap();
        pattern(buf.as_mut_slice(), fill);
        at.push((sub.append(&log, buf, WriteMode::Plain, t).unwrap(), fill));
    }
    commit(&a);
    TwoAppends { p, a, log, sub, at }
}

/// Their later pieces take turns, oldest first, never two in flight while
/// commits are active; an operation whose next piece waits is still in
/// flight; every byte lands.
#[test]
fn test_background_pieces_take_turns_one_in_flight_at_a_time() {
    let TwoAppends {
        p,
        a,
        log,
        mut sub,
        at,
    } = two_appends_then_commits(30);
    let mut order = Vec::new();
    let mut steps = 0;
    while sub.in_flight() > 0 {
        steps += 1;
        assert!(steps < 64, "the pieces never finished");
        let before = p.with_world(|w| w.write_submits());
        for c in sub.reap() {
            assert!(c.outcome.is_ok(), "{:?}", c.outcome);
            order.push(c.tag);
        }
        let sent = p.with_world(|w| w.write_submits()) - before;
        assert!(
            sent <= 1,
            "two pieces at once while commits are active (step {steps})"
        );
        assert!(a.qos().in_flight <= 1);
        if steps == 1 {
            assert_eq!(
                sub.in_flight(),
                2,
                "a waiting piece's operation is in flight"
            );
        }
    }
    assert_eq!(
        order,
        [1, 2],
        "the older write first: its pieces never passed"
    );
    for (pos, fill) in at {
        let mut back = vec![0u8; 1 << 20];
        log.read(pos.offset(), &mut back).unwrap();
        assert!(is_pattern(&back, fill), "a write's pieces out of place");
    }
}

/// A poison stops a piece that pacing parked, as it stops a continuation.
#[test]
fn test_a_poison_stops_a_parked_piece() {
    let TwoAppends { p, a, mut sub, .. } = two_appends_then_commits(31);
    let _first = sub.reap().count();
    assert_eq!(sub.in_flight(), 2, "one piece in flight, one parked");
    a.poison();
    let before = p.with_world(|w| w.write_submits());
    let done = drain(&mut sub);
    assert_eq!(done.len(), 2);
    assert!(
        done.iter()
            .all(|c| matches!(c.outcome, Err(Error::Poisoned { .. }))),
        "{done:?}"
    );
    assert_eq!(
        p.with_world(|w| w.write_submits()) - before,
        0,
        "no piece after the poison"
    );
}

/// A drop whose wait fails submits no parked piece (fail-stop): the store
/// is poisoned instead, a write being left unwritten.
#[test]
fn test_a_drop_whose_wait_fails_submits_no_parked_piece() {
    let TwoAppends { p, a, mut sub, .. } = two_appends_then_commits(32);
    let _first = sub.reap().count();
    assert_eq!(sub.in_flight(), 2, "one piece in flight, one parked");
    p.with_world(|w| {
        let next = w.waits() + 1;
        w.faults_mut().fail_wait = Some((next, 5));
    });
    let before = p.with_world(|w| w.write_submits());
    drop(sub);
    assert_eq!(
        p.with_world(|w| w.write_submits()) - before,
        0,
        "a piece submitted after the drain failed"
    );
    assert!(
        a.poisoned().is_some(),
        "a write left unwritten poisons the store"
    );
}

/// A background submitter's reads are paced and split as its writes are
/// (R5.4: the class is the submitter's).
#[test]
fn test_a_background_submitter_s_reads_are_paced_and_split() {
    let p = two_devices(33, DeviceKind::VolatileCache);
    let a = create(&p, "/a");
    let log = a.provision_append_region("log", 4 << 20).unwrap();
    let mut sub = background(&a);
    advance(&p, 6 * SECOND);
    let mut buf = sub.take_buf(1 << 20).unwrap();
    pattern(buf.as_mut_slice(), 0x7c);
    let pos = sub.append(&log, buf, WriteMode::Plain, 1).unwrap();
    let _done = drain(&mut sub);
    commit(&a);
    // A piece in flight while commits are active: a read is refused, its
    // buffer given back.
    sub.append(&log, sub.take_buf(4096).unwrap(), WriteMode::Plain, 2)
        .unwrap();
    let refused = sub.read_append(&log, pos.offset(), sub.take_buf(1 << 20).unwrap(), 3);
    assert!(
        matches!(
            refused,
            Err(Rejected {
                error: Error::NotWritten {
                    cause: NotWrittenCause::Paced,
                    ..
                },
                buf: Some(_),
            })
        ),
        "{refused:?}"
    );
    drop(refused);
    let _done = drain(&mut sub);
    let before = p.with_world(|w| w.reads());
    sub.read_append(&log, pos.offset(), sub.take_buf(1 << 20).unwrap(), 4)
        .unwrap();
    let done = drain(&mut sub);
    assert_eq!(
        p.with_world(|w| w.reads()) - before,
        4,
        "1 MiB in 256 KiB pieces"
    );
    let read = done.iter().find(|c| c.tag == 4).unwrap();
    assert!(read.outcome.is_ok(), "{:?}", read.outcome);
    let bytes = read.buf.as_ref().unwrap().as_slice();
    assert_eq!(bytes.len(), 1 << 20);
    assert!(is_pattern(bytes, 0x7c), "a read's pieces out of place");
}

/// A drop whose wait fails submits no next piece of an operation in flight
/// either (fail-stop): one write, its first piece in flight.
#[test]
fn test_a_drop_whose_wait_fails_submits_no_next_piece() {
    let p = two_devices(34, DeviceKind::VolatileCache);
    let a = create(&p, "/a");
    let log = a.provision_append_region("log", 4 << 20).unwrap();
    let mut sub = background(&a);
    sub.append(&log, sub.take_buf(1 << 20).unwrap(), WriteMode::Plain, 1)
        .unwrap();
    assert_eq!(sub.in_flight(), 1, "its first piece in flight");
    p.with_world(|w| {
        let next = w.waits() + 1;
        w.faults_mut().fail_wait = Some((next, 5));
    });
    let before = p.with_world(|w| w.write_submits());
    drop(sub);
    assert_eq!(
        p.with_world(|w| w.write_submits()) - before,
        0,
        "a piece submitted after the drain failed"
    );
    assert!(
        a.poisoned().is_some(),
        "a write left unwritten poisons the store"
    );
}

// ------------------------------------------------- a copy's target (V3d1)

/// A source store in `/a` (device 0) whose 4 MiB append region holds
/// `pattern(.., 0x40)`, closed so a copy can open it.
fn copy_source(p: &SimPlatform) {
    let a = create(p, "/a");
    let log = a.provision_append_region("log", 4 << 20).unwrap();
    let mut data = vec![0u8; 4 << 20];
    pattern(&mut data, 0x40);
    let mut last = None;
    for c in data.chunks(1 << 20) {
        last = Some(log.append(c).unwrap());
    }
    let _receipt = log.sync_through(&last.unwrap()).unwrap();
}

fn copy_to_c(p: &SimPlatform) {
    copy_store(
        p.clone(),
        Path::new("/a"),
        Path::new("/c"),
        CopyOptions::default(),
    )
    .unwrap();
}

/// The lengths of the writes to `dir`'s container in the trace.
fn write_lens(p: &SimPlatform, dir: &str) -> Vec<u64> {
    p.with_world(|w| {
        let f = w.file_id(&PathBuf::from(dir), CONTAINER).unwrap();
        w.trace()
            .iter()
            .filter_map(|e| match e {
                TraceEvent::Write { file, len, .. } if *file == f => Some(*len),
                _ => None,
            })
            .collect()
    })
}

/// Samples `s`'s group's pieces in flight at every submission until the
/// returned guard's samples are taken.
fn sample_in_flight(p: &SimPlatform, s: &Store<SimPlatform>) -> Arc<Mutex<Vec<u64>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (out, s) = (Arc::clone(&seen), s.clone());
    p.with_world(|w| {
        w.set_submit_probe(Some(Arc::new(move || {
            out.lock().unwrap().push(s.qos().in_flight);
        })))
    });
    seen
}

/// A copy's target is the call's own (R5.4): created, filled and closed
/// in background pieces of at most `background_io`, each admitted
/// (charged to the device group) before it is submitted, here while
/// commits are active; the copy is whole.
#[test]
fn test_a_copy_writes_its_target_in_admitted_background_pieces() {
    let p = two_devices(40, DeviceKind::VolatileCache);
    copy_source(&p);
    let w = create(&p, "/w");
    let _hold = w.commit_hold();
    let before = p.with_world(|w| w.write_submits());
    let seen = sample_in_flight(&p, &w);
    copy_to_c(&p);
    p.with_world(|w| w.set_submit_probe(None));
    let writes = p.with_world(|w| w.write_submits()) - before;
    let seen = seen.lock().unwrap().clone();
    // Every write of the copy is charged when it is submitted (a read of
    // the source's, on the reader's thread, may be sampled during one too).
    let charged = seen.iter().filter(|&&n| n >= 1).count() as u64;
    assert!(charged >= writes, "{charged} of {writes} writes admitted");
    assert!(
        seen.iter().all(|&n| n <= 1),
        "one piece at a time: {seen:?}"
    );
    let lens = write_lens(&p, "/c");
    assert!(
        lens.iter().all(|&n| n <= 256 << 10),
        "a piece over background_io: {lens:?}"
    );
    assert!(
        lens.iter().filter(|&&n| n == 256 << 10).count() >= 16,
        "the region's 4 MiB in 256 KiB pieces: {lens:?}"
    );
    let c = Store::open(p.clone(), Path::new("/c"), StoreOptions::default()).unwrap();
    let log = c.append_region("log").unwrap();
    let mut back = vec![0u8; 4 << 20];
    log.read(0, &mut back).unwrap();
    assert!(is_pattern(&back, 0x40), "the copy is whole");
}

/// A copy's own metadata and flushes (its target's creation, provisioning,
/// table and clean close) mark no activity, on a flushing device and on a
/// power-safe one (whose `dsync` slot writes are its flushes).
#[test]
fn test_a_copy_marks_no_activity() {
    for (seed, kind) in [(41, DeviceKind::VolatileCache), (42, DeviceKind::PowerSafe)] {
        let p = two_devices(seed, kind);
        copy_source(&p);
        let w = create(&p, "/w");
        advance(&p, 6 * SECOND);
        assert!(!w.qos().active);
        copy_to_c(&p);
        assert!(
            !w.qos().active,
            "a copy marked its device active ({kind:?})"
        );
    }
}

/// On a power-safe store a slot commit's `dsync` write is its flush: it
/// marks the device's commits active.
#[test]
fn test_a_power_safe_store_s_slot_commits_mark_activity() {
    let p = two_devices(43, DeviceKind::PowerSafe);
    let a = create(&p, "/a");
    let slot = a.provision_slot("obj").unwrap();
    advance(&p, 6 * SECOND);
    assert!(!a.qos().active);
    let _receipt = slot.commit(b"object").unwrap();
    assert!(a.qos().active, "a slot commit is a commit");
}

/// A caller's own provisioning is commit-class, as before: its fill in
/// 1 MiB pieces, none charged.
#[test]
fn test_a_caller_s_own_provisioning_is_unpaced() {
    let p = two_devices(44, DeviceKind::VolatileCache);
    let a = create(&p, "/a");
    let _hold = a.commit_hold();
    let seen = sample_in_flight(&p, &a);
    let _log = a.provision_append_region("log", 4 << 20).unwrap();
    p.with_world(|w| w.set_submit_probe(None));
    assert!(
        seen.lock().unwrap().iter().all(|&n| n == 0),
        "a commit-class fill was charged"
    );
    assert!(
        write_lens(&p, "/a").contains(&(1 << 20)),
        "its fill in 1 MiB pieces"
    );
}
