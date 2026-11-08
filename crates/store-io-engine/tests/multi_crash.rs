//! Crashes across devices (R5.13): a pointer made durable on one device
//! after the data it names on another, with appends in flight beside it; a
//! ditto copy failing; a device lost during a set barrier. Every crash
//! state is opened, and no receipt or durable pointer may name bytes a
//! crash lost.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

mod format2_util;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use format2_util::enumerate::{Case, Outcomes, agree, enumerate_with};
use format2_util::{DIR, opts};
use store_io_engine::{AppendRegion, Store, StoreSet, SubmitterConfig, WriteMode, WriteTicket};
use store_io_sim::{CrashMode, DeviceKind, SimConfig, SimPlatform};

const B: usize = 4096;
const MIB: u64 = 1 << 20;
/// The second store's directory, on device 1 (the first's, `DIR`, is on 0).
const OTHER: &str = "/b";

/// Record `i` of the data region: one block of `i + 1`.
fn rec(i: usize) -> Vec<u8> {
    vec![i as u8 + 1; B]
}

/// The pointer record: one block of `0xA0` naming `target` in bytes 8..16.
fn pointer(target: u64) -> Vec<u8> {
    let mut v = vec![0xA0u8; B];
    v[8..16].copy_from_slice(&target.to_le_bytes());
    v
}

/// The pointer `block` names, if it is a whole pointer record.
fn pointer_in(block: &[u8]) -> Option<u64> {
    let target = u64::from_le_bytes(block[8..16].try_into().ok()?);
    (block == pointer(target).as_slice()).then_some(target)
}

/// Two devices, the second's evidence `with` (identities, when set), a
/// store on each.
fn two_stores(seed: u64) -> (SimPlatform, Store<SimPlatform>, Store<SimPlatform>) {
    let p = SimPlatform::new(SimConfig::volatile(seed).with_device(DeviceKind::VolatileCache));
    p.with_world(|w| w.place(OTHER, 1));
    let a = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let b = Store::create(p.clone(), Path::new(OTHER), opts()).unwrap();
    (p, a, b)
}

fn read_block(r: &AppendRegion<SimPlatform>, at: u64) -> Vec<u8> {
    let mut out = vec![0u8; B];
    let n = r.read(at, &mut out).unwrap_or(0);
    out.truncate(n);
    out
}

/// Where the data region's valid records end: the resume point a caller
/// scanning for its records would choose.
fn records_end(data: &AppendRegion<SimPlatform>) -> u64 {
    let mut i = 0;
    while read_block(data, (i * B) as u64) == rec(i) {
        i += 1;
    }
    (i * B) as u64
}

struct Pointed {
    a: Store<SimPlatform>,
    set: StoreSet<SimPlatform>,
    data: AppendRegion<SimPlatform>,
    ptr: AppendRegion<SimPlatform>,
}

/// Data on device 0, then a barrier, then a pointer to its durable prefix
/// on device 1 through the set's `append_after`, while a `Submitter` keeps
/// two more appends of the data region in flight and a blocking append
/// lands after the receipt: crashed before every step and after, with
/// every fate of the cached writes. A durable pointer's data is durable in
/// every state, and the resume point a scan finds lies past it, so a
/// resume never overwrites it. (The pointer's prefix receipt is what makes
/// this so; the set's `append_after` adds its checks, a plain append after
/// the same barrier would hold the same here.)
#[test]
fn test_a_pointer_across_devices_never_outlives_its_data() {
    let outcomes = Outcomes::default();
    let setup = || {
        let (p, a, b) = two_stores(200);
        let set = StoreSet::new([a.clone(), b.clone()]).unwrap();
        let data = a.provision_append_region("data", MIB).unwrap();
        let ptr = b.provision_append_region("ptr", MIB).unwrap();
        (p, Pointed { a, set, data, ptr })
    };
    let op = |_p: &SimPlatform, s: &Pointed| {
        let (Ok(_t0), Ok(t1)) = (s.data.append(&rec(0)), s.data.append(&rec(1))) else {
            return;
        };
        let Ok(mut sub) = s.a.submitter(SubmitterConfig::default()) else {
            return;
        };
        for i in 2..4 {
            let Ok(mut buf) = sub.take_buf(B) else {
                return;
            };
            buf.as_mut_slice().copy_from_slice(&rec(i));
            if sub
                .append(&s.data, buf, WriteMode::Plain, i as u64)
                .is_err()
            {
                return;
            }
        }
        let Ok(r) = s.set.barrier(&[&t1]) else {
            return;
        };
        let _later = s.data.append(&rec(4));
        let Ok(tp) = s
            .set
            .append_after(&s.ptr, &[r[0]], &pointer(r[0].durable_through()))
        else {
            return;
        };
        let _ = s.set.barrier(&[&tp]);
        let _ = sub.wait(2);
    };
    let check = |p: &SimPlatform, case: &Case| {
        let a = Store::open(p.clone(), Path::new(DIR), opts()).unwrap();
        let b = Store::open(p.clone(), Path::new(OTHER), opts()).unwrap();
        let data = a.append_region("data").unwrap();
        let ptr = b.append_region("ptr").unwrap();
        let end = records_end(&data);
        match pointer_in(&read_block(&ptr, 0)) {
            Some(target) => {
                assert!(
                    end >= target,
                    "a durable pointer to {target} with records only to {end}: {case:?}"
                );
                outcomes.saw(case, "the pointer and its data");
            }
            None => outcomes.saw(case, "no pointer"),
        }
        // The caller resumes where its records end and appends: what any
        // durable pointer names stays as it was.
        data.resume_at(end).unwrap();
        let before: Vec<Vec<u8>> = (0..end as usize / B)
            .map(|i| read_block(&data, (i * B) as u64))
            .collect();
        data.append_durable(&rec(end as usize / B)).unwrap();
        for (i, blk) in before.iter().enumerate() {
            assert_eq!(&read_block(&data, (i * B) as u64), blk, "{case:?}");
        }
        drop((data, ptr, a, b));
        agree(p, OTHER, case);
    };
    let covered = enumerate_with(200, false, setup, op, check);
    outcomes.expect(
        &["no pointer", "the pointer and its data"],
        "the pointer and its data",
    );
    assert!(covered.cases > covered.points, "{covered:?}");
}

/// Gives devices 0 and 1 of `p` complete identities of their own.
fn identify(p: &SimPlatform, s: &Store<SimPlatform>) {
    for d in 0..2u32 {
        let mut ev = s.report().evidence.clone();
        ev.device.identity = store_io_core::identity::DeviceIdentity {
            devices: vec![store_io_core::identity::DeviceId::node(
                store_io_core::identity::DeviceNode::Sim { device: d },
            )],
            complete: true,
        };
        p.with_world(|w| w.set_device_evidence(d as usize, Some(ev)));
    }
}

struct Copies {
    set: StoreSet<SimPlatform>,
    ra: AppendRegion<SimPlatform>,
    rb: AppendRegion<SimPlatform>,
}

/// Copy `i` of the ditto: two blocks of `i + 1`.
fn copy(i: usize) -> Vec<u8> {
    vec![i as u8 + 1; 2 * B]
}

/// A ditto of two-block copies whose second copy fails mid-write (its
/// device transfers one block of the two and reports it short: that store
/// is poisoned), then a barrier of the first: crashed before every step
/// and after, with every fate. Once the barrier returned, the first copy
/// is durable; the second store always opens (and its first open agrees
/// with the next), holding none of its copy or the block the device
/// transferred, never a whole copy nothing acknowledged.
#[test]
fn test_a_ditto_copy_failing_mid_write() {
    let outcomes = Outcomes::default();
    let setup = || {
        let (p, a, b) = two_stores(201);
        identify(&p, &a);
        let set = StoreSet::new([a.clone(), b.clone()]).unwrap();
        let ra = a.provision_append_region("log", MIB).unwrap();
        let rb = b.provision_append_region("log", MIB).unwrap();
        // Set order: a's copy is the next write, b's the one after.
        p.with_world(|w| w.faults_mut().short_write = Some(w.writes() + 2));
        (p, Copies { set, ra, rb })
    };
    let op = |p: &SimPlatform, s: &Copies| {
        let copies = s.set.ditto(&[&s.ra, &s.rb], &copy(5));
        // The fault is the ditto's alone: the opens judging each state
        // write too.
        p.with_world(|w| w.faults_mut().short_write = None);
        let Ok(t) = copies else {
            return;
        };
        assert!(t[1].is_err(), "the failing copy succeeded");
        if let Ok(ta) = &t[0] {
            let _ = s.set.barrier(&[ta]);
        }
    };
    let halves = std::cell::Cell::new(0usize);
    let check = |p: &SimPlatform, case: &Case| {
        let a = Store::open(p.clone(), Path::new(DIR), opts()).unwrap();
        let b = Store::open(p.clone(), Path::new(OTHER), opts()).unwrap();
        let (ra, rb) = (
            a.append_region("log").unwrap(),
            b.append_region("log").unwrap(),
        );
        let first = (read_block(&ra, 0), read_block(&ra, B as u64)) == (rec(5), rec(5));
        let second = (read_block(&rb, 0), read_block(&rb, B as u64));
        assert_ne!(second, (rec(5), rec(5)), "a whole second copy: {case:?}");
        if second.0 == rec(5) {
            halves.set(halves.get() + 1);
        }
        outcomes.saw(case, if first { "the first copy" } else { "no copy" });
        drop((ra, rb, a, b));
        agree(p, OTHER, case);
    };
    let covered = enumerate_with(201, false, setup, op, check);
    outcomes.expect(&["no copy", "the first copy"], "the first copy");
    assert!(halves.get() > 0, "no state kept the transferred block");
    assert!(covered.cases > covered.points, "{covered:?}");
}

/// Device 1 lost at every step of a set barrier over a ticket on each
/// device and one already durable on device 1: device 0's receipt stands,
/// the covered ticket keeps its receipt, and every receipt returned names
/// bytes a later crash of everything keeps. The flush threads are off, so
/// both flushes run on this thread in one order every run, and device 1 is
/// lost both before its flush (its ticket fails) and after (its receipt
/// stands).
#[test]
fn test_a_device_lost_during_a_set_barrier() {
    let mut k = 1usize;
    let (mut lost_seen, mut kept_seen) = (false, false);
    loop {
        let (p, a, b) = two_stores(202);
        let set = StoreSet::new([a.clone(), b.clone()]).unwrap();
        let (wa, wb) = (
            a.provision_append_region("log", MIB).unwrap(),
            b.provision_append_region("log", MIB).unwrap(),
        );
        let early = wb.append(&rec(0)).unwrap();
        wb.sync_through(&early).unwrap();
        let ta = wa.append(&rec(1)).unwrap();
        let tb = wb.append(&rec(2)).unwrap();
        let steps = Arc::new(AtomicUsize::new(0));
        {
            let (steps, q) = (Arc::clone(&steps), p.clone());
            p.with_world(|w| {
                w.set_step_probe(Some(Arc::new(move || {
                    if steps.fetch_add(1, Ordering::SeqCst) + 1 == k {
                        q.with_world(|w| w.crash_device(1, &CrashMode::LoseAll));
                    }
                    false
                })));
            });
        }
        let each = store_io_engine::test_hook::without_flush_threads(|| {
            set.barrier_each(&[&ta, &tb, &early]).unwrap()
        });
        p.with_world(|w| w.set_step_probe(None));
        let taken = steps.load(Ordering::SeqCst);
        assert!(each[0].as_ref().unwrap().covers(&ta), "k {k}");
        assert!(each[2].as_ref().unwrap().covers(&early), "k {k}");
        lost_seen |= each[1].is_err();
        kept_seen |= each[1].is_ok();
        // Each ticket, its record, and whether it is store a's.
        let held: Vec<(&WriteTicket, usize, bool)> =
            [(&ta, 1, true), (&tb, 2, false), (&early, 0, false)]
                .into_iter()
                .zip(&each)
                .filter(|(_, r)| r.is_ok())
                .map(|(t, _)| t)
                .collect();
        // A crash of everything: every receipt's bytes are there.
        p.with_world(|w| w.crash(&CrashMode::LoseAll));
        drop((set, wa, wb, a, b));
        let a = Store::open(p.clone(), Path::new(DIR), opts()).unwrap();
        let b = Store::open(p.clone(), Path::new(OTHER), opts()).unwrap();
        for (t, i, on_a) in held {
            let s = if on_a { &a } else { &b };
            let r = s.append_region("log").unwrap();
            assert_eq!(
                read_block(&r, t.pos().offset()),
                rec(i),
                "k {k}, record {i}"
            );
        }
        if k > taken {
            break;
        }
        k += 1;
    }
    assert!(lost_seen, "no point lost device 1 before its flush");
    assert!(kept_seen, "no point lost device 1 after its flush");
}
