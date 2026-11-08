//! Device pacing wired into the store (PLANNING R5.4; V3c1): every store
//! joins its device's group when it opens; a commit-class barrier's entry,
//! and on a power-safe store a `dsync` write, marks the group's commits
//! active; a hold keeps them active; the pacing options are checked.

#![allow(clippy::unwrap_used, unused_results)]

use std::path::Path;
use std::time::Duration;

use store_io_core::error::{Error, NotWrittenCause};
use store_io_core::identity::{DeviceId, DeviceIdentity, DeviceNode};
use store_io_engine::{Store, StoreOptions, SubmitterConfig, WriteMode};
use store_io_platform::Platform;
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

/// A durable append and its barrier.
fn commit(s: &Store<SimPlatform>) {
    let r = match s.append_region("log") {
        Ok(r) => r,
        Err(_) => s.provision_append_region("log", 1 << 20).unwrap(),
    };
    let t = r.append(&[7u8; 4096]).unwrap();
    let _receipt = r.sync_through(&t).unwrap();
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
