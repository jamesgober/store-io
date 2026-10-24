//! Format-2 provisioning and growth beyond capacity fill outside the
//! metadata lock (v0.5 E4 C13): while one fills, other provisionings,
//! growths, reservations and space reports run; its name, table slot and
//! space are held until it commits or fails; its commit takes the page as
//! it is then.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

mod format2_util;

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use format2_util::audit::{Found, audit};
use format2_util::*;
use store_io_core::error::{Error, Named, NotWrittenCause};
use store_io_engine::Store;
use store_io_sim::{CrashMode, SimConfig, SimPlatform};

const B: u64 = 4096;
const SIZE: u64 = 16 * B;
const EXT: u64 = 1 << 20;

fn fresh(seed: u64) -> SimPlatform {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    drop(create(&p));
    p
}

fn found(p: &SimPlatform, name: &str) -> Found {
    audit(p).into_iter().find(|f| f.name == name).unwrap()
}

/// Runs `f` on a thread of its own, failing the test after 20 s (were an
/// operation ever to wait for one that is held, the test would hang).
fn within<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _sent = tx.send(f());
    });
    match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(v) => v,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("an operation waited for the one held")
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            panic!("the operation failed (its panic above)")
        }
    }
}

/// Holds the first write submitted after `arm` until `release`: the
/// operation that submits it stops mid-fill, outside the metadata lock.
#[derive(Default)]
struct Hold {
    state: Mutex<(bool, bool, bool)>,
    cv: Condvar,
}

impl Hold {
    fn install(p: &SimPlatform) -> Arc<Self> {
        let h = Arc::new(Self::default());
        let probe = Arc::clone(&h);
        p.with_world(|w| {
            w.set_submit_probe(Some(Arc::new(move || {
                let mut st = probe.state.lock().unwrap();
                if st.0 {
                    // Armed: this write waits; later ones pass.
                    st.0 = false;
                    st.1 = true;
                    probe.cv.notify_all();
                    while !st.2 {
                        st = probe.cv.wait(st).unwrap();
                    }
                }
            })));
        });
        h
    }

    fn arm(&self) {
        *self.state.lock().unwrap() = (true, false, false);
    }

    /// Waits until the armed write is held (at most 10 s).
    fn held(&self) {
        let st = self.state.lock().unwrap();
        let (st, t) = self
            .cv
            .wait_timeout_while(st, Duration::from_secs(10), |s| !s.1)
            .unwrap();
        assert!(!t.timed_out() && st.1, "the operation never wrote");
    }

    fn release(&self) {
        self.state.lock().unwrap().2 = true;
        self.cv.notify_all();
    }
}

/// While a growth beyond capacity fills: the space report counts its
/// extent in use, a reservation, another provisioning and another
/// region's growth all complete; then the growth commits, its page as it
/// was by then (the other region's entry kept).
#[test]
fn test_a_growth_filling_blocks_no_one() {
    let p = fresh(901);
    let s = open(&p);
    s.provision_page_region("p", SIZE).unwrap();
    s.provision_page_region("q", SIZE).unwrap();
    s.provision_page_region("gone", SIZE).unwrap();
    // Without "gone", released during the growth.
    let regions = s.space().regions - SIZE - 2 * B;
    let hold = Hold::install(&p);
    hold.arm();
    let grower = {
        let s = s.clone();
        std::thread::spawn(move || s.page_region("p").unwrap().grow(SIZE + B))
    };
    hold.held();
    // The growth is mid-fill: everyone else proceeds, a release of a region
    // on the same page too.
    let s2 = s.clone();
    within(move || {
        assert_eq!(
            s2.space().regions,
            regions + SIZE + 2 * B + EXT,
            "the marked extent in use"
        );
        let r = s2.reserve(4 * B, 0).unwrap();
        drop(r);
        s2.provision_page_region("other", SIZE).unwrap();
        s2.page_region("q").unwrap().grow(SIZE + B).unwrap();
        s2.page_region("gone").unwrap().release().unwrap();
    });
    hold.release();
    assert_eq!(grower.join().unwrap().unwrap(), SIZE + B);
    let f = found(&p, "p");
    assert_eq!((f.data_size, f.extents.len()), (SIZE + B, 2));
    assert_eq!(
        found(&p, "other").data_size,
        SIZE,
        "the page kept the other entry"
    );
    assert_eq!(found(&p, "q").extents.len(), 2);
    assert!(
        audit(&p).iter().all(|f| f.name != "gone"),
        "the release kept"
    );
    assert_eq!(s.space().regions, regions + 2 * EXT + SIZE + 2 * B);
    drop(s);
    let s = open(&p);
    assert!(s.report().unavailable.is_empty());
    assert_eq!(s.page_region("p").unwrap().len(), SIZE + B);
    assert_eq!(s.page_region("other").unwrap().len(), SIZE);
}

/// While a provisioning fills: its name is taken, its table slot held (the
/// next provisioning takes the next), its space marked; both commit, in
/// either order, each its own entry.
#[test]
fn test_a_provisioning_filling_holds_its_name_and_slot() {
    let p = fresh(902);
    let s = open(&p);
    let hold = Hold::install(&p);
    hold.arm();
    let first = {
        let s = s.clone();
        std::thread::spawn(move || s.provision_page_region("a", SIZE).map(|_| ()))
    };
    hold.held();
    let s2 = s.clone();
    within(move || {
        let e = s2.provision_page_region("a", SIZE).map(|_| ()).unwrap_err();
        assert!(
            matches!(
                e,
                Error::AlreadyExists {
                    what: Named::Region
                }
            ),
            "{e:?}"
        );
        s2.provision_page_region("b", SIZE).unwrap();
    });
    hold.release();
    first.join().unwrap().unwrap();
    let (a, b) = (found(&p, "a"), found(&p, "b"));
    assert_ne!(a.id, b.id, "two slots");
    assert!(a.id < b.id, "the first claim took the first seq");
    drop(s);
    let s = open(&p);
    assert!(s.report().unavailable.is_empty());
    s.page_region("a").unwrap();
    s.page_region("b").unwrap();
}

/// A page's last slot held by a provisioning filling: the next one goes to
/// the next page.
#[test]
fn test_a_held_last_slot_sends_the_next_to_the_next_page() {
    let p = fresh(903);
    let s = open(&p);
    let per_page = store_io_format::meta2::entries_per_page(LOG2_TABLE) as u64;
    for i in 0..per_page - 1 {
        s.provision_slot(&format!("s{i}")).unwrap();
    }
    let hold = Hold::install(&p);
    hold.arm();
    let last = {
        let s = s.clone();
        std::thread::spawn(move || s.provision_page_region("last", SIZE).map(|_| ()))
    };
    hold.held();
    let s2 = s.clone();
    within(move || s2.provision_page_region("next", SIZE).map(|_| ()).unwrap());
    hold.release();
    last.join().unwrap().unwrap();
    assert_eq!(found(&p, "last").id >> 32, 0, "page 0's last slot");
    assert_eq!(found(&p, "next").id >> 32, 1, "page 1");
}

/// A provisioning refused before its commit (its fill does not verify)
/// gives back everything it held: its name, its table slot, its space and
/// its tag's charge.
#[test]
fn test_a_refused_provisioning_gives_back_its_holds() {
    let p = fresh(904);
    let s = open(&p);
    let space = s.space();
    p.with_world(|w| w.faults_mut().cached_pages = true);
    let e = s.provision_page_region("a", SIZE).map(|_| ()).unwrap_err();
    assert!(
        matches!(
            e,
            Error::NotWritten {
                cause: NotWrittenCause::NotReady,
                ..
            }
        ),
        "{e:?}"
    );
    p.with_world(|w| w.faults_mut().cached_pages = false);
    let mut now = s.space();
    assert_eq!(
        now.released,
        space.released + (now.container - space.container)
    );
    now.container = space.container;
    now.released = space.released;
    // (A tag the charge named stays listed, at zero.)
    now.tags
        .retain(|t| (t.logical, t.physical, t.reserved) != (0, 0, 0) || t.cap.is_some());
    assert_eq!(now, space, "the space and the tags");
    // From a reservation: refused, the reservation keeps its bytes.
    let mut r = s.reserve(SIZE + 2 * B, 0).unwrap();
    p.with_world(|w| w.faults_mut().cached_pages = true);
    assert!(r.provision_page_region("a", SIZE).is_err());
    p.with_world(|w| w.faults_mut().cached_pages = false);
    assert_eq!(r.remaining(), SIZE + 2 * B, "refunded");
    drop(r);
    // The name and the slot again.
    s.provision_page_region("a", SIZE).unwrap();
    assert_eq!(found(&p, "a").id, 0, "seq 0 again");
    audit(&p);
    drop(s);
    assert!(
        Store::open(p.clone(), Path::new(DIR), opts())
            .unwrap()
            .report()
            .unavailable
            .is_empty()
    );
}

/// Many threads provisioning one name at once: exactly one of them gets
/// it, in every round, and the store opens with no region unavailable (a
/// second one of a name would be settled out at the open, its data lost).
#[test]
fn test_one_name_one_region_under_contention() {
    let p = fresh(905);
    let s = open(&p);
    for round in 0..40 {
        let name = format!("n{round}");
        let won: usize = (0..6)
            .map(|_| {
                let (s, name) = (s.clone(), name.clone());
                std::thread::spawn(move || s.provision_slot(&name).is_ok())
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| usize::from(t.join().unwrap()))
            .sum();
        assert_eq!(won, 1, "round {round}");
    }
    drop(s);
    let s = open(&p);
    assert!(
        s.report().unavailable.is_empty(),
        "{:?}",
        s.report().unavailable
    );
}

/// A provisioning refused at each of its three write submissions in turn
/// (the fill, the header, the page write: the commit) before they reach
/// the device gives everything back, unpoisoned: the name, the table slot,
/// the space and the tag's charge.
#[test]
fn test_each_refused_write_gives_back_the_holds() {
    for k in 1..=3u64 {
        let p = fresh(906);
        let s = open(&p);
        let space = s.space();
        p.with_world(|w| w.faults_mut().fail_submit = Some((w.write_submits() + k, 5)));
        let e = s.provision_page_region("a", SIZE).map(|_| ()).unwrap_err();
        p.with_world(|w| w.faults_mut().fail_submit = None);
        assert!(s.poisoned().is_none(), "k {k}: {e:?}");
        let mut now = s.space();
        assert_eq!(
            now.released,
            space.released + (now.container - space.container)
        );
        now.container = space.container;
        now.released = space.released;
        now.tags
            .retain(|t| (t.logical, t.physical, t.reserved) != (0, 0, 0));
        assert_eq!(now, space, "k {k}: {e:?}");
        s.provision_page_region("a", SIZE).unwrap();
        assert_eq!(found(&p, "a").id, 0, "k {k}: the slot again");
        drop(s);
        assert!(open(&p).report().unavailable.is_empty(), "k {k}");
    }
}

/// A reservation's provisioning that fails mid-fill returns its bytes, and
/// the container still holds every reservation past the tail: another
/// region provisioned meanwhile and the failure leave the reservations
/// whole, so a later provisioning from one never needs the device to grow.
#[test]
fn test_a_failed_reserved_provisioning_keeps_reservations_whole() {
    let p = fresh(907);
    let s = open(&p);
    let ext = SIZE + 2 * B;
    let mut x = s.reserve(ext, 0).unwrap();
    // Larger than the hole A's failure leaves: it can only go to the tail.
    let mut y = s.reserve(2 * ext, 0).unwrap();
    let hold = Hold::install(&p);
    hold.arm();
    let a = std::thread::spawn(move || {
        let r = x.provision_page_region("a", SIZE).map(|_| ());
        (r, x)
    });
    hold.held();
    let s2 = s.clone();
    within(move || s2.provision_page_region("b", SIZE).map(|_| ()).unwrap());
    // A's fill will not verify: it fails before its commit.
    p.with_world(|w| w.faults_mut().cached_pages = true);
    hold.release();
    let (r, mut x) = a.join().unwrap();
    p.with_world(|w| w.faults_mut().cached_pages = false);
    assert!(r.is_err());
    assert_eq!(x.remaining(), ext, "the reservation keeps its bytes");
    // The device full: no file may grow further. A provisioning from a
    // reservation still fits.
    p.with_world(|w| w.faults_mut().capacity = Some(w.allocated_bytes()));
    // Both reservations alive: each provisions its whole size.
    y.provision_page_region("c", 2 * ext - 2 * B).unwrap();
    x.provision_page_region("a", SIZE).unwrap();
    drop((x, y));
    // Every reservation gone, its bytes in regions now or given back.
    assert_eq!(s.space().reserved, 0);
    audit(&p);
}

/// Power fails at each write submission of a provisioning that fills while
/// another provisioning commits: the next open finds the other region,
/// and the first whole or not at all.
#[test]
fn test_a_power_failure_beside_a_concurrent_provisioning() {
    let mut k = 1u64;
    loop {
        let p = fresh(908);
        let s = open(&p);
        let hold = Hold::install(&p);
        hold.arm();
        let a = {
            let s = s.clone();
            std::thread::spawn(move || {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    s.provision_page_region("a", SIZE).map(|_| ())
                }))
                .is_ok_and(|r| r.is_ok())
            })
        };
        hold.held();
        let s2 = s.clone();
        within(move || s2.provision_page_region("b", SIZE).map(|_| ()).unwrap());
        // From here, the k-th write submission (A's) loses power.
        let n = Arc::new(AtomicU64::new(0));
        p.with_world(|w| {
            let n = Arc::clone(&n);
            w.set_submit_probe(Some(Arc::new(move || {
                if n.fetch_add(1, Ordering::SeqCst) + 1 == k {
                    panic!("power fails");
                }
            })));
        });
        hold.release();
        let finished = a.join().unwrap();
        std::mem::forget(s);
        p.with_world(|w| {
            w.set_submit_probe(None);
            w.crash(&CrashMode::Random);
        });
        let s = open(&p);
        assert!(s.report().unavailable.is_empty(), "k {k}");
        s.page_region("b").unwrap();
        if let Ok(a) = s.page_region("a") {
            assert_eq!(a.len(), SIZE, "k {k}");
        }
        audit(&p);
        if finished {
            break;
        }
        k += 1;
        assert!(k < 32, "the provisioning never finished");
    }
}
