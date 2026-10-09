//! Reservations, caps and per-tag accounting on the deterministic simulator.

// Test setup unwraps (a failure is the test failing) and ignores values a
// given test does not inspect.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::path::Path;
use std::sync::Arc;

use store_io_core::error::{Error, ReserveTag};
use store_io_engine::{Store, StoreOptions};
use store_io_sim::{FaultPlan, SimConfig, SimPlatform};

const DIR: &str = "/db";
const MIB: u64 = 1 << 20;

fn opts() -> StoreOptions {
    StoreOptions {
        queues: 2,
        buffers_per_class: 8,
        max_io: 1 << 17,
        ..StoreOptions::default()
    }
}

fn create(p: &SimPlatform) -> Store<SimPlatform> {
    Store::create(p.clone(), Path::new(DIR), opts()).unwrap()
}

fn no_space_for(r: &Result<impl std::fmt::Debug, Error>, tag: u32) -> bool {
    matches!(r, Err(Error::NoSpace { tag: ReserveTag::Caller(t), .. }) if *t == tag)
}

#[test]
fn test_a_reservation_grows_the_container_once_and_provisioning_takes_from_it() {
    let p = SimPlatform::new(SimConfig::volatile(51));
    let s = create(&p);
    let before = s.space();
    let mut r = s.reserve(4 * MIB, 3).unwrap();
    let grown = s.space();
    assert!(grown.container >= before.container + 4 * MIB);
    assert_eq!(grown.reserved, 4 * MIB);
    let extent = s.extent_of(MIB).unwrap();
    let wal = r.provision_append_region("wal", MIB).unwrap();
    wal.append_durable(b"tagged").unwrap();
    let after = s.space();
    // The reservation is charged the extent alone; the container may grow
    // only by the gap that aligns a large region's data (under 1 MiB).
    assert!(
        after.container < grown.container + MIB,
        "a reserved region allocated more than its alignment gap"
    );
    assert_eq!(r.remaining(), 4 * MIB - extent);
    assert_eq!(after.reserved, 4 * MIB - extent);
    let t3 = after.tags.iter().find(|t| t.tag == 3).unwrap();
    assert_eq!((t3.logical, t3.physical), (MIB, extent));
    assert_eq!(t3.reserved, 4 * MIB - extent);
    drop(r);
    assert_eq!(s.space().reserved, 0, "leftover returns on drop");
    // Tags are recorded in the table, so usage survives a reopen.
    drop((wal, s));
    let s = Store::open(p.clone(), Path::new(DIR), opts()).unwrap();
    let t3 = s.space().tags.into_iter().find(|t| t.tag == 3).unwrap();
    assert_eq!((t3.logical, t3.physical, t3.reserved), (MIB, extent, 0));
}

#[test]
fn test_a_reservation_survives_a_full_file_system() {
    // Reserve, then let unreserved provisioning fill the disk: it fails with
    // NoSpace before it can take reserved space, and the reservation still
    // provisions.
    let mut cfg = SimConfig::volatile(52);
    cfg.faults = FaultPlan {
        capacity: Some(24 * MIB),
        ..FaultPlan::default()
    };
    let p = SimPlatform::new(cfg);
    let s = create(&p);
    let mut r = s.reserve(4 * MIB, 9).unwrap();
    // Bounded: a broken capacity check must fail the test, not exhaust memory.
    let mut filled = 0;
    let err = loop {
        assert!(filled < 64, "the file system never filled");
        match s.provision_page_region(&format!("fill{filled}"), MIB) {
            Ok(_) => filled += 1,
            Err(e) => break e,
        }
    };
    assert!(filled > 0);
    assert!(
        matches!(
            err,
            Error::NoSpace {
                tag: ReserveTag::Caller(0),
                ..
            }
        ),
        "{err:?}"
    );
    for i in 0..3 {
        r.provision_page_region(&format!("mine{i}"), MIB).unwrap();
    }
    // Nothing was poisoned along the way.
    let pages = s.page_region("mine0").unwrap();
    pages
        .write_durable(pages.pos(0).unwrap(), &[1; 4096])
        .unwrap();
}

#[test]
fn test_caps_bound_each_tag_and_refuse_with_its_name() {
    let p = SimPlatform::new(SimConfig::volatile(53));
    let s = create(&p);
    s.set_cap(7, Some(3 * MIB));
    let before = s.space();
    assert!(no_space_for(&s.reserve(4 * MIB, 7), 7));
    assert_eq!(s.space(), before, "a refused reservation changes nothing");
    let mut r = s.reserve(2 * MIB, 7).unwrap();
    assert!(no_space_for(&s.reserve(2 * MIB, 7), 7));
    // Other tags are unaffected by tag 7's cap.
    s.reserve(8 * MIB, 8).unwrap();
    // A reservation too small for the region refuses, naming its tag.
    assert!(no_space_for(&r.provision_append_region("big", 4 * MIB), 7));
    r.provision_append_region("fits", MIB).unwrap();
    // The cap counts provisioned bytes too.
    drop(r);
    let used = s.space().tags.iter().find(|t| t.tag == 7).unwrap().physical;
    assert!(no_space_for(&s.reserve(3 * MIB - used + 1, 7), 7));
    s.reserve(3 * MIB - used, 7).unwrap();
    // Tag 0's cap applies to provisioning without a reservation.
    s.set_cap(0, Some(MIB));
    assert!(no_space_for(&s.provision_page_region("p", 2 * MIB), 0));
    s.set_cap(0, None);
    s.provision_page_region("p", 2 * MIB).unwrap();
}

#[test]
fn test_concurrent_reservations_never_exceed_the_cap() {
    let p = SimPlatform::new(SimConfig::volatile(54));
    let s = Arc::new(create(&p));
    s.set_cap(1, Some(10 * MIB));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let s = Arc::clone(&s);
            std::thread::spawn(move || {
                let mut held = Vec::new();
                loop {
                    // Bounded: a broken cap must fail, not exhaust memory.
                    assert!(held.len() <= 10, "the cap was exceeded");
                    match s.reserve(MIB, 1) {
                        Ok(r) => held.push(r),
                        Err(Error::NoSpace {
                            tag: ReserveTag::Caller(1),
                            ..
                        }) => break,
                        Err(e) => panic!("{e:?}"),
                    }
                }
                held
            })
        })
        .collect();
    let mut total = Vec::new();
    for h in handles {
        total.extend(h.join().unwrap());
    }
    assert_eq!(total.len(), 10);
    assert_eq!(s.space().reserved, 10 * MIB);
    drop(total);
    assert_eq!(s.space().reserved, 0);
}

#[test]
fn test_release_returns_a_tag_s_usage() {
    let p = SimPlatform::new(SimConfig::volatile(55));
    let s = create(&p);
    let mut r = s.reserve(4 * MIB, 4).unwrap();
    let pages = r.provision_page_region("pages", MIB).unwrap();
    drop(r);
    let tag = |s: &Store<SimPlatform>| s.space().tags.into_iter().find(|t| t.tag == 4).unwrap();
    assert!(tag(&s).physical > 0);
    let released_before = s.space().released;
    pages.release().unwrap();
    assert_eq!((tag(&s).logical, tag(&s).physical), (0, 0));
    assert!(s.space().released > released_before);
}

#[test]
fn test_read_only_stores_report_space_but_cannot_reserve() {
    let p = SimPlatform::new(SimConfig::volatile(56));
    let s = create(&p);
    s.provision_append_region("wal", MIB).unwrap();
    drop(s);
    let ro = Store::open_readonly(p.clone(), Path::new(DIR), opts()).unwrap();
    assert!(ro.space().regions > 0);
    assert!(matches!(ro.reserve(MIB, 0), Err(Error::NotWritten { .. })));
}
