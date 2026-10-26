//! Format 1's write paths, still reachable on a store just created as
//! format 1 (from 0.5 a format-1 store opens read-only, v0.5 E5 C3): its
//! provisioning, reservations and release. Each test works on the handle
//! that created the store, and checks anything after a reopen read-only.
//! These are the format-1 counterparts of tests that ran format 1 before
//! format 2 became the default (`lifecycle`, `space`, `submitter`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use store_io_core::error::{Error, Named, NotWrittenCause, ReserveTag};
use store_io_engine::{CONTAINER, ScanItem, Store, StoreOptions, SubmitterConfig};
use store_io_format::slot::FORMAT_1;
use store_io_sim::{CrashMode, FaultPlan, SimConfig, SimPlatform};

const DIR: &str = "/db";
const MIB: u64 = 1 << 20;

fn opts() -> StoreOptions {
    StoreOptions {
        queues: 2,
        buffers_per_class: 8,
        max_io: 1 << 17,
        container_format: FORMAT_1,
        ..StoreOptions::default()
    }
}

fn create(p: &SimPlatform) -> Store<SimPlatform> {
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    assert_eq!(s.report().format, 1);
    s
}

fn not_written(e: &Result<impl std::fmt::Debug, Error>, want: NotWrittenCause) -> bool {
    matches!(e, Err(Error::NotWritten { cause, .. }) if *cause == want)
}

fn container_len(p: &SimPlatform) -> usize {
    p.with_world(|w| {
        w.media_of(&PathBuf::from(DIR), CONTAINER)
            .map_or(0, <[u8]>::len)
    })
}

/// A released region is gone from the store's regions, its name and space
/// free again: the space is reused (refilled, no released byte comes back),
/// and a read-only reopen finds the two regions left.
#[test]
fn test_release_frees_the_region_and_its_space_is_reused_clean() {
    let p = SimPlatform::new(SimConfig::volatile(1201));
    let s = create(&p);
    let a = s.provision_append_region("a", MIB).unwrap();
    let b = s.provision_page_region("b", MIB).unwrap();
    a.append_durable(&[0x77; 1 << 19]).unwrap();
    let held = a.clone();
    let len = container_len(&p);
    a.release().unwrap();
    assert!(not_written(&held.append(b"x"), NotWrittenCause::NotReady));
    assert!(matches!(s.append_region("a"), Err(Error::NotFound { .. })));
    assert_eq!(s.regions().len(), 1);
    b.write_durable(b.pos(0).unwrap(), &[1; 4096]).unwrap();
    let c = s.provision_append_region("a", MIB).unwrap();
    assert_eq!(container_len(&p), len, "released space was reused");
    let mut stale = false;
    c.scan(0, |item| {
        if let ScanItem::Data { bytes, .. } = item {
            stale |= bytes.contains(&0x77);
        }
        ControlFlow::Continue(())
    })
    .unwrap();
    assert!(!stale, "released data came back");
    assert_eq!(s.regions().len(), 2);
    drop((b, c, held, s));
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    let s = Store::open_readonly(p.clone(), Path::new(DIR), opts()).unwrap();
    assert_eq!(s.regions().len(), 2);
}

/// A name is unique among the table's ready regions: provisioning it again
/// is `AlreadyExists`, with nothing added; once released it can be used.
#[test]
fn test_a_ready_region_s_name_cannot_be_provisioned_again() {
    let p = SimPlatform::new(SimConfig::volatile(1202));
    let s = create(&p);
    let a = s.provision_page_region("a", MIB).unwrap();
    let len = container_len(&p);
    for r in [
        s.provision_page_region("a", MIB).map(drop),
        s.provision_append_region("a", MIB).map(drop),
    ] {
        assert!(
            matches!(
                r,
                Err(Error::AlreadyExists {
                    what: Named::Region
                })
            ),
            "{r:?}"
        );
    }
    assert_eq!(
        container_len(&p),
        len,
        "a refused provisioning grew the file"
    );
    assert_eq!(s.regions().len(), 1);
    a.release().unwrap();
    s.provision_append_region("a", MIB).unwrap();
    drop(s);
    let s = Store::open_readonly(p.clone(), Path::new(DIR), opts()).unwrap();
    assert_eq!(s.regions().len(), 1);
    assert!(s.append_region("a").is_ok());
}

/// A reservation grows the container once, and provisioning from it is
/// charged the extent alone.
#[test]
fn test_a_reservation_grows_the_container_once_and_provisioning_takes_from_it() {
    let p = SimPlatform::new(SimConfig::volatile(1203));
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
    drop((wal, s));
    let s = Store::open_readonly(p.clone(), Path::new(DIR), opts()).unwrap();
    let t3 = s.space().tags.into_iter().find(|t| t.tag == 3).unwrap();
    assert_eq!((t3.logical, t3.physical, t3.reserved), (MIB, extent, 0));
}

/// Reserve, then let unreserved provisioning fill the file system: it fails
/// with `NoSpace` before it can take reserved space, and the reservation
/// still provisions (unaligned, when aligning would need growth).
#[test]
fn test_a_reservation_survives_a_full_file_system() {
    let mut cfg = SimConfig::volatile(1204);
    cfg.faults = FaultPlan {
        capacity: Some(24 * MIB),
        ..FaultPlan::default()
    };
    let p = SimPlatform::new(cfg);
    let s = create(&p);
    let mut r = s.reserve(4 * MIB, 9).unwrap();
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
    let pages = s.page_region("mine0").unwrap();
    pages
        .write_durable(pages.pos(0).unwrap(), &[1; 4096])
        .unwrap();
}

/// A release waiting for a submitter's operation does not hold the metadata
/// lock: the submitter's thread can still provision (which takes it) before
/// it reaps, and then the release completes.
#[test]
fn test_release_waiting_for_a_submitter_never_blocks_provisioning() {
    let p = SimPlatform::new(SimConfig::volatile(1205));
    let s = Store::create(
        p,
        Path::new(DIR),
        StoreOptions {
            queues: 1,
            container_format: FORMAT_1,
            ..StoreOptions::default()
        },
    )
    .unwrap();
    let wal = s.provision_append_region("wal", MIB).unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let owner = {
        let s = s.clone();
        let wal = wal.clone();
        std::thread::spawn(move || {
            let mut sub = s
                .submitter(SubmitterConfig::default().with_depth(8))
                .unwrap();
            sub.read_append(&wal, 0, sub.take_buf(4096).unwrap(), 0)
                .unwrap();
            let (tx, rx) = mpsc::channel();
            let releaser = {
                let wal = wal.clone();
                std::thread::spawn(move || tx.send(wal.release()).unwrap())
            };
            // Until the region refuses new operations: it is retired, and
            // the release is waiting for the submitter's read.
            let mut probe = vec![0; 4096];
            while wal.read(0, &mut probe).is_ok() {
                std::thread::yield_now();
            }
            assert!(not_written(
                &wal.read(0, &mut probe),
                NotWrittenCause::NotReady
            ));
            assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
            s.provision_page_region("more", MIB).unwrap();
            assert_eq!(sub.wait(1).unwrap().count(), 1);
            rx.recv_timeout(Duration::from_secs(20)).unwrap().unwrap();
            releaser.join().unwrap();
            done_tx.send(()).unwrap();
        })
    };
    done_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("release and provisioning deadlocked");
    owner.join().unwrap();
    assert!(s.append_region("wal").is_err());
}

/// A release whose table flip (the commit) is durable but whose header
/// write is refused before it reaches the device is done: `Ok`, the store
/// not poisoned, the region gone and gone after reopening. Before, the
/// refusal came back as an error for a release that was committed.
#[test]
fn test_a_committed_release_whose_header_write_is_refused_is_done() {
    let p = SimPlatform::new(SimConfig::volatile(1209));
    let s = create(&p);
    let keep = s.provision_append_region("keep", MIB).unwrap();
    let gone = s.provision_append_region("gone", MIB).unwrap();
    // The table flip is the next submission; the header's the one after it.
    p.with_world(|w| w.faults_mut().fail_submit = Some((w.write_submits() + 2, 5)));
    let released = gone.release();
    p.with_world(|w| w.faults_mut().fail_submit = None);
    assert!(
        released.is_ok(),
        "a committed release reported {released:?}"
    );
    assert!(matches!(
        s.append_region("gone"),
        Err(Error::NotFound { .. })
    ));
    keep.append_durable(b"the store still writes").unwrap();
    drop((keep, s));
    let s = Store::open_readonly(p.clone(), Path::new(DIR), opts()).unwrap();
    assert!(matches!(
        s.append_region("gone"),
        Err(Error::NotFound { .. })
    ));
    assert_eq!(s.regions().len(), 1);
}
