//! The engine-layer `Submitter` (D-098) on the simulator: positions exact
//! and final at submission, tickets only from full completions, a write
//! failing after issue poisons, back-pressure gives the buffer back, drop
//! drains, recycle waits for unprocessed operations, and runs replay by
//! seed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use store_io_core::error::{CorruptionKind, Error, NotWrittenCause};
use store_io_engine::completion::{Completion, Done};
use store_io_engine::{
    AppendRegion, Outstanding, Store, StoreOptions, Submitter, SubmitterConfig, WriteMode,
    WriteTicket,
};
use store_io_sim::{SimConfig, SimPlatform};

const MIB: u64 = 1 << 20;
const EIO: i32 = 5;
const EINVAL: i32 = 22;

fn opts() -> StoreOptions {
    StoreOptions {
        queues: 1,
        ..StoreOptions::default()
    }
}

fn store(seed: u64, reorder: bool) -> (SimPlatform, Store<SimPlatform>) {
    let p = SimPlatform::new(SimConfig {
        reorder,
        ..SimConfig::volatile(seed)
    });
    let s = Store::create(p.clone(), Path::new("/db"), opts()).unwrap();
    (p, s)
}

fn sub(s: &Store<SimPlatform>, depth: u32) -> Submitter<SimPlatform> {
    s.submitter(SubmitterConfig::default().with_depth(depth))
        .unwrap()
}

/// A buffer of `bytes`, its whole capacity dirtied first: what a reused pool
/// buffer may hold, so padding that is not zeroed shows.
fn filled(sub: &Submitter<SimPlatform>, bytes: &[u8]) -> store_io_platform::IoBuf {
    let mut buf = sub.take_buf(bytes.len()).unwrap();
    buf.as_mut_capacity().fill(0xEE);
    buf.as_mut_slice().copy_from_slice(bytes);
    buf
}

/// `sync_through` on another thread, failing fast instead of hanging (an
/// append whose completion never completed its reservation).
fn sync_soon(
    wal: &AppendRegion<SimPlatform>,
    ticket: WriteTicket,
) -> store_io_engine::DurableReceipt {
    let (tx, rx) = mpsc::channel();
    let wal = wal.clone();
    std::thread::spawn(move || tx.send(wal.sync_through(&ticket)));
    rx.recv_timeout(Duration::from_secs(20))
        .expect("sync_through hung: an append's completion was not recorded")
        .unwrap()
}

fn data(i: u64, len: usize) -> Vec<u8> {
    (0..len)
        .map(|k| (k as u8).wrapping_mul(7) ^ (i as u8).wrapping_mul(31) ^ 0x5A)
        .collect()
}

/// Submits an append of `bytes`, taking completions while the submitter is
/// full; returns its position's offset.
fn append(
    sub: &mut Submitter<SimPlatform>,
    wal: &AppendRegion<SimPlatform>,
    bytes: &[u8],
    tag: u64,
    done: &mut Vec<Completion>,
) -> u64 {
    let mut buf = filled(sub, bytes);
    loop {
        match sub.append(wal, buf, WriteMode::Plain, tag) {
            Ok(pos) => return pos.offset(),
            Err(r) => {
                assert!(
                    is_cause(&r.error, NotWrittenCause::QueueFull),
                    "{:?}",
                    r.error
                );
                buf = r.buf.unwrap();
                done.extend(sub.wait(1).unwrap());
            }
        }
    }
}

fn drain(sub: &mut Submitter<SimPlatform>, done: &mut Vec<Completion>) {
    while sub.in_flight() > 0 {
        done.extend(sub.wait(sub.in_flight()).unwrap());
    }
    done.extend(sub.reap());
}

fn is_cause(e: &Error, want: NotWrittenCause) -> bool {
    matches!(e, Error::NotWritten { cause, .. } if *cause == want)
}

/// Out-of-order completions: every append lands at the position returned at
/// submission (back to back, block aligned), each completion's ticket names
/// exactly that range, and every byte reads back, zero-padded.
#[test]
fn test_out_of_order_appends_land_where_submission_said() {
    for seed in 0..6 {
        let (_p, s) = store(700 + seed, true);
        let wal = s.provision_append_region("wal", 4 * MIB).unwrap();
        let bs = wal.block_size();
        let mut sub = sub(&s, 8);
        let mut done = Vec::new();
        let mut placed = Vec::new();
        let mut next = 0u64;
        for i in 0..40u64 {
            let len = 1 + (i as usize * 977) % (3 * bs as usize);
            let at = append(&mut sub, &wal, &data(i, len), i, &mut done);
            assert_eq!(at, next, "seed {seed}: append {i} not back to back");
            next = (at + len as u64).div_ceil(bs) * bs;
            placed.push((at, len));
            if i % 5 == 0 {
                done.extend(sub.reap());
            }
        }
        drain(&mut sub, &mut done);
        assert_eq!(done.len(), 40);
        let mut last = None;
        for c in done {
            let (at, len) = placed[c.tag as usize];
            assert_eq!(
                c.buf.as_ref().unwrap().len() as u64,
                (len as u64).div_ceil(bs) * bs
            );
            let Ok(Done::Written(w)) = c.outcome else {
                panic!("seed {seed}: append {} failed: {:?}", c.tag, c.outcome);
            };
            assert_eq!(w.ticket.pos().offset(), at);
            assert_eq!(w.ticket.end(), (at + len as u64).div_ceil(bs) * bs);
            assert!(w.receipt.is_none());
            if c.tag == 39 {
                last = Some(w.ticket);
            }
        }
        // The last ticket's sync covers every append before it.
        let receipt = sync_soon(&wal, last.unwrap());
        assert_eq!(receipt.durable_through(), next);
        for (i, &(at, len)) in placed.iter().enumerate() {
            let padded = (len as u64).div_ceil(bs) * bs;
            let mut back = vec![0xEE; padded as usize];
            wal.read(at, &mut back).unwrap();
            assert_eq!(&back[..len], &data(i as u64, len)[..], "seed {seed}: {i}");
            assert!(back[len..].iter().all(|&b| b == 0), "padding not zero");
        }
    }
}

/// The same seed gives the same run: completion order and device trace.
#[test]
fn test_a_seed_replays_exactly() {
    let run = || {
        let (p, s) = store(720, true);
        let wal = s.provision_append_region("wal", 4 * MIB).unwrap();
        let pages = s.provision_page_region("pages", 4 * MIB).unwrap();
        let bs = pages.block_size() as usize;
        let mut sub = sub(&s, 6);
        let mut done = Vec::new();
        for i in 0..30u64 {
            if i % 3 == 0 {
                let mut buf = sub.take_buf(bs).unwrap();
                buf.as_mut_slice().copy_from_slice(&data(i, bs));
                let pos = pages.pos(i * bs as u64).unwrap();
                while let Err(r) = sub.write(&pages, pos, buf, WriteMode::Plain, i) {
                    buf = r.buf.unwrap();
                    done.extend(sub.wait(1).unwrap());
                }
            } else {
                append(&mut sub, &wal, &data(i, 100 + i as usize), i, &mut done);
            }
        }
        drain(&mut sub, &mut done);
        let order: Vec<u64> = done.iter().map(|c| c.tag).collect();
        (order, p.with_world(|w| w.trace_hash()))
    };
    let (a, ha) = run();
    let (b, hb) = run();
    assert_eq!(a, b);
    assert_eq!(ha, hb);
    assert_ne!(
        a,
        (0..30).collect::<Vec<_>>(),
        "completions were not reordered"
    );
}

/// A write failing after issue: its completion is `DurabilityUnknown`, the
/// store is poisoned, later writes are refused with their buffer, every
/// other completion still arrives, and reads keep working.
#[test]
fn test_a_write_failing_in_flight_poisons() {
    let (p, s) = store(730, false);
    let wal = s.provision_append_region("wal", 4 * MIB).unwrap();
    let mut sub = sub(&s, 8);
    let mut done = Vec::new();
    p.with_world(|w| {
        let third = w.writes() + 3;
        w.faults_mut().fail_write = Some(third);
    });
    for i in 0..6 {
        append(&mut sub, &wal, &data(i, 500), i, &mut done);
    }
    drain(&mut sub, &mut done);
    assert_eq!(done.len(), 6);
    let failed: Vec<_> = done.iter().filter(|c| c.outcome.is_err()).collect();
    assert_eq!(failed.len(), 1);
    assert!(
        matches!(failed[0].outcome, Err(Error::DurabilityUnknown { .. })),
        "{:?}",
        failed[0].outcome
    );
    assert!(s.poisoned().is_some());
    let buf = sub.take_buf(10).unwrap();
    let r = sub.append(&wal, buf, WriteMode::Plain, 99).unwrap_err();
    assert!(matches!(r.error, Error::Poisoned { .. }), "{:?}", r.error);
    assert!(r.buf.is_some());
    // A ticket from before the failure cannot be made durable past the hole.
    let Ok(Done::Written(w)) = &done[0].outcome else {
        panic!()
    };
    assert!(wal.sync_through(&w.ticket).is_err());
    let buf = sub.take_buf(4096).unwrap();
    sub.read_append(&wal, 0, buf, 7).unwrap();
    let c = sub.wait(1).unwrap().next().unwrap();
    assert_eq!(c.outcome, Ok(Done::Read { len: 4096 }));
    assert_eq!(&c.buf.unwrap().as_slice()[..500], &data(0, 500)[..]);
}

/// A panic between an append's reservation and its submission (here a
/// backend's `submit` panicking) poisons the store at once and wakes what
/// waits: a durable append past the hole, already waiting on it while the
/// submitter lives, returns `Poisoned` instead of waiting until the
/// submitter drops.
#[test]
fn test_a_panic_after_the_reservation_poisons_and_wakes_at_once() {
    let (p, s) = store(740, false);
    let wal = s.provision_append_region("wal", 4 * MIB).unwrap();
    let mut sub = sub(&s, 8);
    let buf = filled(&sub, &data(1, 500));
    let main = std::thread::current().id();
    let (tx, rx) = mpsc::channel();
    let later = wal.clone();
    let world = p.clone();
    let tx = std::sync::Mutex::new(Some(tx));
    p.with_world(|w| {
        w.set_submit_probe(Some(std::sync::Arc::new(move || {
            if std::thread::current().id() != main {
                return;
            }
            // With the hole reserved: a durable append past it, on another
            // thread, until its write has landed (it then waits on the hole).
            let Some(tx) = tx.lock().unwrap().take() else {
                return;
            };
            let landed = world.with_world(|w| w.writes()) + 1;
            let later = later.clone();
            std::thread::spawn(move || tx.send(later.append_durable(b"after").map(|_| ())));
            while world.with_world(|w| w.writes()) < landed {
                std::thread::yield_now();
            }
            std::thread::sleep(Duration::from_millis(50));
            panic!("a backend's submit panicked");
        })));
    });
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        sub.append(&wal, buf, WriteMode::Plain, 1)
    }));
    p.with_world(|w| w.set_submit_probe(None));
    assert!(r.is_err(), "the probe did not panic");
    assert!(s.poisoned().is_some(), "not poisoned");
    let got = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("a durable append waited on the hole");
    assert!(matches!(got, Err(Error::Poisoned { .. })), "{got:?}");
    drop(sub);
}

/// A short page write: `DurabilityUnknown`, poisoned, no ticket.
#[test]
fn test_a_short_write_is_unknown_and_poisons() {
    let (p, s) = store(731, false);
    let pages = s.provision_page_region("pages", MIB).unwrap();
    let bs = pages.block_size() as usize;
    let mut sub = sub(&s, 4);
    p.with_world(|w| {
        let next = w.writes() + 1;
        w.faults_mut().short_write = Some(next);
    });
    let mut buf = sub.take_buf(4 * bs).unwrap();
    buf.as_mut_slice().fill(3);
    sub.write(&pages, pages.pos(0).unwrap(), buf, WriteMode::Plain, 1)
        .unwrap();
    let c = sub.wait(1).unwrap().next().unwrap();
    assert!(matches!(c.outcome, Err(Error::DurabilityUnknown { .. })));
    assert!(s.poisoned().is_some());
}

/// A full submitter refuses with the buffer back and nothing reserved;
/// completions handed back but not taken still count; taking them frees
/// room.
#[test]
fn test_back_pressure_returns_the_buffer_and_reserves_nothing() {
    let (_p, s) = store(740, false);
    let wal = s.provision_append_region("wal", MIB).unwrap();
    let mut sub = sub(&s, 4);
    for i in 0..4 {
        let buf = sub.take_buf(10).unwrap();
        sub.append(&wal, buf, WriteMode::Plain, i).unwrap();
    }
    let tail = wal.tail();
    let buf = sub.take_buf(10).unwrap();
    let r = sub.append(&wal, buf, WriteMode::Plain, 4).unwrap_err();
    assert!(is_cause(&r.error, NotWrittenCause::QueueFull));
    assert_eq!(wal.tail(), tail, "a refused append reserved space");
    // Completions arrive but are left with the submitter: still full.
    drop(sub.wait(4).unwrap());
    assert_eq!(sub.in_flight(), 0);
    let r = sub
        .append(&wal, r.buf.unwrap(), WriteMode::Plain, 4)
        .unwrap_err();
    assert!(is_cause(&r.error, NotWrittenCause::QueueFull));
    assert_eq!(sub.reap().count(), 4);
    sub.append(&wal, r.buf.unwrap(), WriteMode::Plain, 4)
        .unwrap();
}

/// Another store's region or buffer is refused before anything is
/// reserved or written; so are an empty append and a misaligned read.
#[test]
fn test_foreign_regions_and_buffers_are_refused() {
    let (_p, s) = store(750, false);
    let (_q, other) = store(751, false);
    let wal = s.provision_append_region("wal", MIB).unwrap();
    let theirs = other.provision_append_region("wal", MIB).unwrap();
    let mut sub = sub(&s, 4);
    let other_sub = other.submitter(SubmitterConfig::default()).unwrap();
    // A depth past the cap is clamped, never an allocation of rows for it.
    drop(
        s.submitter(SubmitterConfig::default().with_depth(u32::MAX))
            .unwrap(),
    );
    let buf = sub.take_buf(10).unwrap();
    let r = sub.append(&theirs, buf, WriteMode::Plain, 0).unwrap_err();
    assert!(is_cause(&r.error, NotWrittenCause::ForeignPosition));
    let foreign = other_sub.take_buf(10).unwrap();
    let r = sub.append(&wal, foreign, WriteMode::Plain, 0).unwrap_err();
    assert!(is_cause(&r.error, NotWrittenCause::ForeignBuffer));
    let empty = sub.take_buf(0).unwrap();
    let r = sub.append(&wal, empty, WriteMode::Plain, 0).unwrap_err();
    assert!(is_cause(&r.error, NotWrittenCause::Empty));
    assert_eq!(wal.tail(), 0);
    assert_eq!(theirs.tail(), 0);
    let buf = sub.take_buf(4096).unwrap();
    let r = sub.read_append(&wal, 100, buf, 0).unwrap_err();
    assert!(is_cause(&r.error, NotWrittenCause::Misaligned));
    let buf = sub.take_buf(4096).unwrap();
    let r = sub.read_append(&wal, MIB, buf, 0).unwrap_err();
    assert!(is_cause(&r.error, NotWrittenCause::OutOfBounds));
    assert_eq!(sub.in_flight(), 0);
}

/// A reopened append region is not positioned: refused until `resume_at`.
#[test]
fn test_an_unpositioned_region_refuses_appends() {
    let p = SimPlatform::new(SimConfig::volatile(752));
    let s = Store::create(p.clone(), Path::new("/db"), opts()).unwrap();
    s.provision_append_region("wal", MIB).unwrap();
    drop(s);
    let s = Store::open(p.clone(), Path::new("/db"), opts()).unwrap();
    let wal = s.append_region("wal").unwrap();
    let mut sub = sub(&s, 4);
    let buf = sub.take_buf(10).unwrap();
    let r = sub.append(&wal, buf, WriteMode::Plain, 0).unwrap_err();
    assert!(is_cause(&r.error, NotWrittenCause::NotPositioned));
    wal.resume_at(0).unwrap();
    sub.append(&wal, r.buf.unwrap(), WriteMode::Plain, 0)
        .unwrap();
}

/// Dropping a submitter with operations in flight waits for them: every
/// buffer goes back to the pool, every reservation is completed (later
/// appends and syncs work), and every pass is released (recycle proceeds).
#[test]
fn test_drop_drains_and_releases_everything() {
    // Out of order, so a collection without waiting leaves some in flight.
    let p = SimPlatform::new(SimConfig {
        reorder: true,
        ..SimConfig::volatile(760)
    });
    let o = StoreOptions {
        buffers_per_class: 4,
        large_buffers: 4,
        ..opts()
    };
    let s = Store::create(p.clone(), Path::new("/db"), o).unwrap();
    let wal = s.provision_append_region("wal", 32 * MIB).unwrap();
    let mut sub = sub(&s, 8);
    let max = sub.max_buf();
    for i in 0..4 {
        let mut buf = sub.take_buf(max).unwrap();
        buf.as_mut_slice().fill(i as u8 + 1);
        sub.append(&wal, buf, WriteMode::Plain, i).unwrap();
    }
    assert!(is_cause(
        &sub.take_buf(max).unwrap_err(),
        NotWrittenCause::PoolExhausted
    ));
    drop(sub);
    let again = s.submitter(SubmitterConfig::default()).unwrap();
    let held: Vec<_> = (0..4).map(|_| again.take_buf(max).unwrap()).collect();
    drop(held);
    let t = wal.append(b"after").unwrap();
    assert_eq!(t.pos().offset(), 4 * max as u64);
    sync_soon(&wal, t);
    let mut back = vec![0; 4096];
    wal.read(3 * max as u64, &mut back).unwrap();
    assert!(back.iter().all(|&b| b == 4));
    wal.recycle().unwrap();
}

/// Recycle waits until the submitter's operation on the region is
/// processed; afterwards the old generation is refused.
#[test]
fn test_recycle_waits_for_unprocessed_operations() {
    let (_p, s) = store(770, false);
    let wal = s.provision_append_region("wal", MIB).unwrap();
    let mut sub = sub(&s, 4);
    let buf = sub.take_buf(10).unwrap();
    sub.append(&wal, buf, WriteMode::Plain, 0).unwrap();
    let (tx, rx) = mpsc::channel();
    let recycler = {
        let wal = wal.clone();
        std::thread::spawn(move || tx.send(wal.recycle()).unwrap())
    };
    assert!(
        rx.recv_timeout(Duration::from_millis(100)).is_err(),
        "recycle returned with an operation unprocessed"
    );
    let c = sub.wait(1).unwrap().next().unwrap();
    assert!(c.outcome.is_ok());
    let fresh = rx.recv_timeout(Duration::from_secs(30)).unwrap().unwrap();
    recycler.join().unwrap();
    let buf = sub.take_buf(10).unwrap();
    let r = sub.append(&wal, buf, WriteMode::Plain, 1).unwrap_err();
    assert!(is_cause(&r.error, NotWrittenCause::StaleGeneration));
    assert_eq!(
        sub.append(&fresh, r.buf.unwrap(), WriteMode::Plain, 1)
            .unwrap()
            .offset(),
        0
    );
}

/// Poisoning with writes in flight: they complete with their raw results,
/// new writes are refused, reads continue.
#[test]
fn test_poison_mid_flight() {
    let (_p, s) = store(780, false);
    let wal = s.provision_append_region("wal", MIB).unwrap();
    let mut sub = sub(&s, 8);
    for i in 0..3 {
        let buf = sub.take_buf(100).unwrap();
        sub.append(&wal, buf, WriteMode::Plain, i).unwrap();
    }
    s.poison();
    let done: Vec<_> = sub.wait(3).unwrap().collect();
    assert_eq!(done.len(), 3);
    assert!(done.iter().all(|c| c.outcome.is_ok()));
    let buf = sub.take_buf(100).unwrap();
    let r = sub.append(&wal, buf, WriteMode::Plain, 3).unwrap_err();
    assert!(matches!(r.error, Error::Poisoned { .. }));
    sub.read_append(&wal, 0, sub.take_buf(4096).unwrap(), 4)
        .unwrap();
    assert!(sub.wait(1).unwrap().next().unwrap().outcome.is_ok());
}

/// Page writes and reads round-trip; a ticket from a completion is made
/// durable by the blocking `sync_through`; media and other read failures
/// keep their classification; a stale position is refused.
#[test]
fn test_page_writes_and_reads_round_trip() {
    let (p, s) = store(790, true);
    let pages = s.provision_page_region("pages", MIB).unwrap();
    let bs = pages.block_size() as usize;
    let mut sub = sub(&s, 8);
    for i in 0..8u64 {
        let mut buf = sub.take_buf(2 * bs).unwrap();
        buf.as_mut_slice().copy_from_slice(&data(i, 2 * bs));
        sub.write(
            &pages,
            pages.pos(i * 2 * bs as u64).unwrap(),
            buf,
            WriteMode::Plain,
            i,
        )
        .unwrap();
    }
    let mut done = Vec::new();
    drain(&mut sub, &mut done);
    for c in &done {
        let Ok(Done::Written(w)) = &c.outcome else {
            panic!("{:?}", c.outcome)
        };
        assert_eq!(w.ticket.pos().offset(), c.tag * 2 * bs as u64);
        let r = pages.sync_through(&w.ticket).unwrap();
        assert!(r.covers(&w.ticket));
    }
    for i in 0..8u64 {
        let buf = sub.take_buf(2 * bs).unwrap();
        sub.read_page(&pages, pages.pos(i * 2 * bs as u64).unwrap(), buf, i)
            .unwrap();
    }
    let mut done = Vec::new();
    drain(&mut sub, &mut done);
    for c in done {
        assert_eq!(c.outcome, Ok(Done::Read { len: 2 * bs }));
        assert_eq!(c.buf.unwrap().as_slice(), &data(c.tag, 2 * bs)[..]);
    }
    // A read the device cannot do (not a media error) keeps its code.
    p.with_world(|w| {
        let next = w.reads() + 1;
        w.faults_mut().fail_read = Some((next, EINVAL));
    });
    let buf = sub.take_buf(bs).unwrap();
    sub.read_page(&pages, pages.pos(0).unwrap(), buf, 0)
        .unwrap();
    let c = sub.wait(1).unwrap().next().unwrap();
    assert!(
        matches!(c.outcome, Err(Error::Io { raw, .. }) if raw.code == EINVAL),
        "{:?}",
        c.outcome
    );
    // A latent sector error is a media error naming the read's range.
    let file = p.with_world(|w| {
        w.file_id(&"/db".into(), store_io_engine::CONTAINER)
            .unwrap()
    });
    let at = region_base(&p);
    p.with_world(|w| {
        w.faults_mut()
            .bad_ranges
            .push((file, at + bs as u64, at + 2 * bs as u64));
    });
    let buf = sub.take_buf(2 * bs).unwrap();
    sub.read_page(&pages, pages.pos(0).unwrap(), buf, 0)
        .unwrap();
    let c = sub.wait(1).unwrap().next().unwrap();
    assert!(
        matches!(
            c.outcome,
            Err(Error::Corruption {
                kind: CorruptionKind::MediaError,
                ..
            })
        ),
        "{:?}",
        c.outcome
    );
    // Page writes are checked as the blocking write checks them.
    let buf = sub.take_buf(bs).unwrap();
    let r = sub.write(
        &pages,
        pages.pos(MIB - bs as u64).unwrap(),
        buf,
        WriteMode::Plain,
        0,
    );
    assert!(r.is_ok());
    let buf = sub.take_buf(2 * bs).unwrap();
    let r = sub.write(
        &pages,
        pages.pos(MIB - bs as u64).unwrap(),
        buf,
        WriteMode::Plain,
        0,
    );
    assert!(is_cause(
        &r.unwrap_err().error,
        NotWrittenCause::OutOfBounds
    ));
    let buf = sub.take_buf(100).unwrap();
    let r = sub.write(&pages, pages.pos(0).unwrap(), buf, WriteMode::Plain, 0);
    assert!(is_cause(&r.unwrap_err().error, NotWrittenCause::Misaligned));
    drop(sub.wait(1).unwrap());
    let fresh = pages.recycle().unwrap();
    let buf = sub.take_buf(bs).unwrap();
    let r = sub
        .write(&fresh, pages.pos(0).unwrap(), buf, WriteMode::Plain, 0)
        .unwrap_err();
    assert!(is_cause(&r.error, NotWrittenCause::StaleGeneration));
    let buf = sub.take_buf(bs).unwrap();
    let r = sub
        .read_page(&fresh, pages.pos(0).unwrap(), buf, 0)
        .unwrap_err();
    assert!(is_cause(&r.error, NotWrittenCause::StaleGeneration));
}

/// Where the data written at `pages` offset 0 (`data(0, ..)`, durable)
/// starts in the container.
fn region_base(p: &SimPlatform) -> u64 {
    let marker = data(0, 32);
    p.with_world(|w| {
        let media = w
            .media_of(&"/db".into(), store_io_engine::CONTAINER)
            .unwrap();
        media.windows(32).position(|x| x == &marker[..]).unwrap() as u64
    })
}

/// A failed wait is `Io`: nothing is poisoned and the operations stay in
/// flight for a later wait.
#[test]
fn test_failed_waits() {
    let (p, s) = store(800, false);
    let wal = s.provision_append_region("wal", MIB).unwrap();
    let mut sub = sub(&s, 4);
    let buf = sub.take_buf(10).unwrap();
    sub.append(&wal, buf, WriteMode::Plain, 0).unwrap();
    p.with_world(|w| {
        let next = w.waits() + 1;
        w.faults_mut().fail_wait = Some((next, EIO));
    });
    assert!(matches!(sub.wait(1), Err(Error::Io { .. })));
    assert_eq!(sub.in_flight(), 1);
    assert!(s.poisoned().is_none());
    assert_eq!(sub.wait(1).unwrap().count(), 1);
    let buf = sub.take_buf(10).unwrap();
    sub.append(&wal, buf, WriteMode::Plain, 1).unwrap();
    assert_eq!(sub.wait(1).unwrap().count(), 1);
}

/// A drop whose wait fails processes whatever the queue still collects; a
/// write left in flight poisons the store (its range may never be
/// written), reads alone do not. Out-of-order completion decides, per seed,
/// how much the final collection gets.
#[test]
fn test_a_failed_drain_poisons_only_for_writes() {
    let (mut poisoned, mut clean) = (0, 0);
    for seed in 0..32 {
        for reads in [false, true] {
            let (p, s) = store(840 + seed, true);
            let wal = s.provision_append_region("wal", MIB).unwrap();
            let mut sub = sub(&s, 8);
            for i in 0..2 {
                if reads {
                    let buf = sub.take_buf(4096).unwrap();
                    sub.read_append(&wal, 0, buf, i).unwrap();
                } else {
                    sub.append(&wal, filled(&sub, &data(i, 100)), WriteMode::Plain, i)
                        .unwrap();
                }
            }
            p.with_world(|w| {
                let next = w.waits() + 1;
                w.faults_mut().fail_wait = Some((next, EIO));
            });
            drop(sub);
            if reads {
                assert!(s.poisoned().is_none(), "seed {seed}: reads poisoned");
            } else if s.poisoned().is_some() {
                poisoned += 1;
            } else {
                // Every append was collected: the prefix passed them all.
                let t = wal.append(b"next").unwrap();
                sync_soon(&wal, t);
                clean += 1;
            }
        }
    }
    assert!(
        poisoned > 0 && clean > 0,
        "{poisoned} poisoned, {clean} clean"
    );
}

/// A short append: `DurabilityUnknown`, poisoned, no ticket.
#[test]
fn test_a_short_append_is_unknown_and_poisons() {
    let (p, s) = store(732, false);
    let wal = s.provision_append_region("wal", MIB).unwrap();
    let mut sub = sub(&s, 4);
    p.with_world(|w| {
        let next = w.writes() + 1;
        w.faults_mut().short_write = Some(next);
    });
    sub.append(&wal, filled(&sub, &data(1, 3 * 4096)), WriteMode::Plain, 1)
        .unwrap();
    let c = sub.wait(1).unwrap().next().unwrap();
    assert!(
        matches!(c.outcome, Err(Error::DurabilityUnknown { .. })),
        "{:?}",
        c.outcome
    );
    assert!(s.poisoned().is_some());
}

/// A short read hands back only the bytes read: the buffer's length is set
/// to them.
#[test]
fn test_a_short_read_trims_the_buffer() {
    let (p, s) = store(733, false);
    let wal = s.provision_append_region("wal", MIB).unwrap();
    let mut sub = sub(&s, 4);
    p.with_world(|w| {
        let next = w.reads() + 1;
        w.faults_mut().short_read = Some(next);
    });
    sub.read_append(&wal, 0, filled(&sub, &[0x77; 4 * 4096]), 1)
        .unwrap();
    let c = sub.wait(1).unwrap().next().unwrap();
    let Ok(Done::Read { len }) = c.outcome else {
        panic!("{:?}", c.outcome)
    };
    assert!(len < 4 * 4096, "the read was not short");
    assert_eq!(c.buf.unwrap().len(), len);
}

/// A read-only store: writes are refused, reads work.
#[test]
fn test_a_read_only_store_reads_and_refuses_writes() {
    let p = SimPlatform::new(SimConfig::volatile(734));
    let s = Store::create(p.clone(), Path::new("/db"), opts()).unwrap();
    s.provision_append_region("wal", MIB)
        .unwrap()
        .append(b"kept")
        .unwrap();
    drop(s);
    let s = Store::open_readonly(p.clone(), Path::new("/db"), opts()).unwrap();
    let wal = s.append_region("wal").unwrap();
    let mut sub = sub(&s, 4);
    let r = sub
        .append(&wal, filled(&sub, b"x"), WriteMode::Plain, 0)
        .unwrap_err();
    assert!(
        is_cause(&r.error, NotWrittenCause::ReadOnly),
        "{:?}",
        r.error
    );
    sub.read_append(&wal, 0, sub.take_buf(4096).unwrap(), 1)
        .unwrap();
    let c = sub.wait(1).unwrap().next().unwrap();
    assert_eq!(&c.buf.unwrap().as_slice()[..4], b"kept");
}

/// Release waits for a submitter's unprocessed operation without holding
/// the metadata lock: the submitter's thread can still provision (which
/// takes it) before it reaps, and then the release completes.
#[test]
fn test_release_waiting_for_a_submitter_never_blocks_provisioning() {
    let (_p, s) = store(820, false);
    let wal = s.provision_append_region("wal", MIB).unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let owner = {
        let s = s.clone();
        let wal = wal.clone();
        std::thread::spawn(move || {
            let mut sub = sub(&s, 8);
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
            assert!(is_cause(
                &wal.read(0, &mut probe).unwrap_err(),
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

/// An append waiting for room in its window holds no lane. The window is
/// full of a submitter's appends, which complete only when its owner reaps
/// them; the owner's own call (a read, with the store's one shared lane)
/// still runs, then it reaps, and the waiting append lands after them.
/// Waiting with the lane held, the read and the append would wait for each
/// other forever (and so would any provisioning behind them).
#[test]
fn test_an_append_waiting_for_room_holds_no_lane() {
    let p = SimPlatform::new(SimConfig::volatile(830));
    let s = Store::create(
        p,
        Path::new("/db"),
        StoreOptions {
            append_window: 4 << 12,
            ..opts()
        },
    )
    .unwrap();
    let wal = s.provision_append_region("wal", MIB).unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let owner = {
        let wal = wal.clone();
        std::thread::spawn(move || {
            let mut sub = sub(&s, 64);
            // Fill the window, reaping nothing.
            let mut held = 0;
            loop {
                match sub.append(&wal, filled(&sub, b"ahead"), WriteMode::Plain, held) {
                    Ok(_) => held += 1,
                    Err(r) => {
                        assert!(is_cause(&r.error, NotWrittenCause::TooManyInFlight));
                        break;
                    }
                }
            }
            let waiter = {
                let wal = wal.clone();
                std::thread::spawn(move || wal.append(b"waits for room").unwrap())
            };
            while wal.waiting_on_appends() == 0 {
                std::thread::yield_now();
            }
            let mut back = [0u8; 4096];
            wal.read(0, &mut back).unwrap();
            assert_eq!(sub.wait(held as usize).unwrap().count(), held as usize);
            let ticket = waiter.join().unwrap();
            assert_eq!(ticket.pos().offset(), held * wal.block_size());
            done_tx.send(()).unwrap();
        })
    };
    done_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("an append waiting for room deadlocked the submitter's owner");
    owner.join().unwrap();
}

/// When the oldest operation finishes while a newer one is still in flight,
/// the age restarts for the newer one: it is a different operation.
#[test]
fn test_oldest_outstanding_moves_to_the_next_operation() {
    let mut seen = false;
    for seed in 0..64 {
        let (p, s) = store(900 + seed, true);
        let wal = s.provision_append_region("wal", MIB).unwrap();
        let mut sub = sub(&s, 4);
        sub.append(&wal, filled(&sub, b"a"), WriteMode::Plain, 0)
            .unwrap();
        assert!(s.oldest_outstanding().is_some());
        p.with_world(|w| w.advance_clock(5_000));
        sub.append(&wal, filled(&sub, b"b"), WriteMode::Plain, 1)
            .unwrap();
        let tags: Vec<u64> = sub.reap().map(|c| c.tag).collect();
        if tags == [0] {
            // The newer one was never the oldest before: first seen now.
            assert_eq!(
                s.oldest_outstanding(),
                Some(Outstanding {
                    age: Duration::ZERO,
                    flush: false,
                })
            );
            seen = true;
        } else {
            assert_eq!(
                s.oldest_outstanding().map(|o| o.age),
                (tags.is_empty() || tags == [1]).then_some(Duration::from_secs(5))
            );
        }
        drop(sub.wait(2).unwrap());
    }
    assert!(seen, "no seed completed the oldest operation alone");
}

/// Dropping a submitter waits for every operation (out of order, 8 in
/// flight, so a final collection without waiting would leave some): never
/// poisoned, every append recorded, and the submitter no longer counted.
#[test]
fn test_drop_waits_for_every_operation() {
    for seed in 0..4 {
        let (_p, s) = store(950 + seed, true);
        let wal = s.provision_append_region("wal", MIB).unwrap();
        let mut sub = sub(&s, 8);
        for i in 0..8 {
            sub.append(&wal, filled(&sub, &data(i, 300)), WriteMode::Plain, i)
                .unwrap();
        }
        assert!(s.oldest_outstanding().is_some());
        drop(sub);
        assert!(s.poisoned().is_none(), "seed {seed}");
        assert_eq!(s.oldest_outstanding(), None);
        let t = wal.append(b"after").unwrap();
        sync_soon(&wal, t);
    }
}

/// A submitter's operation in flight shows in `oldest_outstanding` and ages
/// with the clock until its completion is processed.
#[test]
fn test_oldest_outstanding_sees_submitted_operations() {
    let (p, s) = store(810, false);
    let wal = s.provision_append_region("wal", MIB).unwrap();
    let mut sub = sub(&s, 4);
    assert_eq!(s.oldest_outstanding(), None);
    sub.append(&wal, filled(&sub, b"a"), WriteMode::Plain, 0)
        .unwrap();
    let at = |ms| {
        Some(Outstanding {
            age: Duration::from_millis(ms),
            flush: false,
        })
    };
    assert_eq!(s.oldest_outstanding(), at(0));
    p.with_world(|w| w.advance_clock(3_000));
    sub.append(&wal, filled(&sub, b"b"), WriteMode::Plain, 1)
        .unwrap();
    // The newer operation does not hide the older one.
    assert_eq!(s.oldest_outstanding(), at(3_000));
    assert_eq!(sub.wait(2).unwrap().count(), 2);
    assert_eq!(s.oldest_outstanding(), None);
    sub.append(&wal, filled(&sub, b"c"), WriteMode::Plain, 2)
        .unwrap();
    p.with_world(|w| w.advance_clock(1_000));
    // A new operation is not dated from the old one.
    assert_eq!(s.oldest_outstanding(), at(0));
    drop(sub);
    assert_eq!(s.oldest_outstanding(), None);
}

/// A submitter operation shows as outstanding from before it is submitted
/// (A1 C3): a queue that does the I/O inside `submit` (the synchronous
/// tier) must not hide a hung device from `Store::oldest_outstanding`.
#[test]
fn test_an_operation_is_outstanding_while_it_is_being_submitted() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    let p = SimPlatform::new(SimConfig::volatile(810));
    let s = Store::create(p.clone(), Path::new("/db"), StoreOptions::default()).unwrap();
    let pages = s.provision_page_region("pages", 1 << 20).unwrap();
    let mut sub = s
        .submitter(SubmitterConfig::default().with_depth(4))
        .unwrap();
    let (seen, idle) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    {
        let (s, seen, idle) = (s.clone(), Arc::clone(&seen), Arc::clone(&idle));
        p.with_world(|w| {
            w.set_submit_probe(Some(Arc::new(move || {
                let n = if s.oldest_outstanding().is_some() {
                    &seen
                } else {
                    &idle
                };
                n.fetch_add(1, Ordering::Relaxed);
            })));
        });
    }
    let buf = sub.take_buf(4096).unwrap();
    sub.write(&pages, pages.pos(0).unwrap(), buf, WriteMode::Plain, 1)
        .map_err(|r| r.error)
        .unwrap();
    p.with_world(|w| w.set_submit_probe(None));
    assert_eq!(
        (seen.load(Ordering::Relaxed), idle.load(Ordering::Relaxed)),
        (1, 0),
        "the write was not outstanding while being submitted"
    );
    let done: Vec<_> = sub.wait(1).unwrap().collect();
    assert_eq!(done.len(), 1);
    drop(done);
    assert!(
        s.oldest_outstanding().is_none(),
        "idle again after the completion"
    );
}

/// A refused submission leaves nothing outstanding (A1 C3: the row begun
/// before `submit` is aborted).
#[test]
fn test_a_refused_submission_leaves_nothing_outstanding() {
    let p = SimPlatform::new(SimConfig::volatile(811));
    let s = Store::create(p.clone(), Path::new("/db"), StoreOptions::default()).unwrap();
    let pages = s.provision_page_region("pages", 1 << 20).unwrap();
    let mut sub = s
        .submitter(SubmitterConfig::default().with_depth(1))
        .unwrap();
    p.with_world(|w| {
        let next = w.write_submits() + 1;
        w.faults_mut().fail_submit = Some((next, 5));
    });
    let buf = sub.take_buf(4096).unwrap();
    let r = sub.write(&pages, pages.pos(0).unwrap(), buf, WriteMode::Plain, 1);
    let rejected = r.expect_err("the submission was refused");
    assert!(rejected.buf.is_some(), "the buffer comes back");
    assert_eq!(sub.in_flight(), 0);
    assert!(
        s.oldest_outstanding().is_none(),
        "the aborted row is not outstanding"
    );
    assert!(
        s.poisoned().is_none(),
        "a page write refused before any I/O poisons nothing"
    );
    // The row is usable again (depth 1: the retry needs the aborted row).
    let buf = sub.take_buf(4096).unwrap();
    sub.write(&pages, pages.pos(0).unwrap(), buf, WriteMode::Plain, 2)
        .map_err(|r| r.error)
        .unwrap();
    let done: Vec<_> = sub.wait(1).unwrap().collect();
    assert_eq!(done.len(), 1);
    assert!(done[0].outcome.is_ok());
    drop(done);
    // The table's count was restored: dropping the submitter finds every
    // row accounted for and poisons nothing.
    drop(sub);
    assert!(
        s.poisoned().is_none(),
        "an aborted row left the table inconsistent"
    );
}
