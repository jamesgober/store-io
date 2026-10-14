//! Durable submitter writes and barriers (D-099) on the simulator: a
//! durable completion survives a crash, flushes are shared, a failed flush
//! poisons, power-safe stores need no flush, barriers check their tickets,
//! and drop waits for pending flushes.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use store_io_core::error::{Error, NotWrittenCause};
use store_io_engine::completion::{Completion, Done};
use store_io_engine::{AppendRegion, Store, StoreOptions, Submitter, SubmitterConfig, WriteMode};
use store_io_sim::{CrashMode, SimConfig, SimPlatform};

const MIB: u64 = 1 << 20;
const DIR: &str = "/db";

fn opts() -> StoreOptions {
    StoreOptions {
        queues: 2,
        ..StoreOptions::default()
    }
}

fn platform(cfg: SimConfig) -> (SimPlatform, Store<SimPlatform>) {
    let p = SimPlatform::new(cfg);
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    (p, s)
}

fn sub(s: &Store<SimPlatform>, depth: u32) -> Submitter<SimPlatform> {
    s.submitter(SubmitterConfig::default().with_depth(depth))
        .unwrap()
}

fn data(i: u64, len: usize) -> Vec<u8> {
    (0..len)
        .map(|k| (k as u8).wrapping_mul(11) ^ (i as u8).wrapping_mul(37) ^ 0x3C)
        .collect()
}

fn append(
    sub: &mut Submitter<SimPlatform>,
    wal: &AppendRegion<SimPlatform>,
    bytes: &[u8],
    mode: WriteMode,
    tag: u64,
    done: &mut Vec<Completion>,
) -> u64 {
    let mut buf = sub.take_buf(bytes.len()).unwrap();
    buf.as_mut_capacity().fill(0xEE);
    buf.as_mut_slice().copy_from_slice(bytes);
    loop {
        match sub.append(wal, buf, mode, tag) {
            Ok(pos) => return pos.offset(),
            Err(r) => {
                assert!(
                    matches!(
                        r.error,
                        Error::NotWritten {
                            cause: NotWrittenCause::QueueFull,
                            ..
                        }
                    ),
                    "{:?}",
                    r.error
                );
                buf = r.buf.unwrap();
                done.extend(sub.wait(1).unwrap());
            }
        }
    }
}

/// Waits for every operation, failing fast instead of hanging.
fn drain(sub: &mut Submitter<SimPlatform>, done: &mut Vec<Completion>) {
    let mut rounds = 0;
    while sub.in_flight() > 0 {
        rounds += 1;
        assert!(rounds < 100_000, "operations never completed");
        done.extend(sub.wait(sub.in_flight()).unwrap());
    }
    done.extend(sub.reap());
}

fn read_back(s: &Store<SimPlatform>, at: u64, len: usize) -> Vec<u8> {
    let mut out = vec![0; len];
    let n = s.append_region("wal").unwrap().read(at, &mut out).unwrap();
    out.truncate(n);
    out
}

/// Durable appends out of order, on a flush-required store: every
/// completion carries a receipt covering its ticket, flushes are shared,
/// and every byte survives a crash that loses everything not flushed.
#[test]
fn test_durable_appends_survive_a_crash_and_share_flushes() {
    for seed in 0..4 {
        let (p, s) = platform(SimConfig {
            reorder: true,
            ..SimConfig::volatile(1000 + seed)
        });
        let wal = s.provision_append_region("wal", 4 * MIB).unwrap();
        let mut sub = sub(&s, 16);
        let flushes = p.with_world(|w| w.flushes());
        let mut done = Vec::new();
        let mut placed = Vec::new();
        for i in 0..64u64 {
            let bytes = data(i, 100 + (i as usize * 313) % 9000);
            let at = append(&mut sub, &wal, &bytes, WriteMode::Durable, i, &mut done);
            placed.push((at, bytes));
        }
        drain(&mut sub, &mut done);
        assert_eq!(done.len(), 64);
        let mut prefixes = 0;
        for c in &done {
            let Ok(Done::Written(w)) = &c.outcome else {
                panic!("seed {seed}: {:?}", c.outcome)
            };
            let receipt = w
                .receipt
                .as_ref()
                .expect("a durable write without a receipt");
            assert!(
                receipt.covers(&w.ticket),
                "seed {seed}: receipt does not cover"
            );
            // A prefix receipt reaches at least the write's end; any other
            // covers exactly the write.
            if receipt.is_prefix() {
                assert!(
                    receipt.durable_through() >= w.ticket.end(),
                    "seed {seed}: short prefix"
                );
                prefixes += 1;
            } else {
                assert_eq!(receipt.durable_through(), w.ticket.end());
            }
        }
        assert!(prefixes > 0, "seed {seed}: no prefix receipt");
        let used = p.with_world(|w| w.flushes()) - flushes;
        assert!(
            (1..64).contains(&used),
            "seed {seed}: {used} flushes for 64 writes"
        );
        drop((sub, wal, s));
        p.with_world(|w| w.crash(&CrashMode::LoseAll));
        let s = Store::open(p.clone(), Path::new(DIR), opts()).unwrap();
        for (i, (at, bytes)) in placed.iter().enumerate() {
            let back = read_back(&s, *at, bytes.len());
            assert!(back == *bytes, "seed {seed}: durable append {i} lost");
        }
    }
}

/// Durable page writes the same way, and plain ones carry no receipt.
#[test]
fn test_durable_page_writes_carry_receipts_plain_ones_do_not() {
    let (p, s) = platform(SimConfig::volatile(1010));
    let pages = s.provision_page_region("pages", MIB).unwrap();
    let bs = pages.block_size() as usize;
    let mut sub = sub(&s, 8);
    for i in 0..8u64 {
        let mut buf = sub.take_buf(bs).unwrap();
        buf.as_mut_slice().copy_from_slice(&data(i, bs));
        let mode = if i % 2 == 0 {
            WriteMode::Durable
        } else {
            WriteMode::Plain
        };
        sub.write(&pages, pages.pos(i * bs as u64).unwrap(), buf, mode, i)
            .unwrap();
    }
    let mut done = Vec::new();
    drain(&mut sub, &mut done);
    for c in &done {
        let Ok(Done::Written(w)) = &c.outcome else {
            panic!("{:?}", c.outcome)
        };
        assert_eq!(w.receipt.is_some(), c.tag % 2 == 0, "tag {}", c.tag);
    }
    drop((sub, pages, s));
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    let s = Store::open(p.clone(), Path::new(DIR), opts()).unwrap();
    let pages = s.page_region("pages").unwrap();
    for i in (0..8u64).step_by(2) {
        let mut out = vec![0; bs];
        pages
            .read(pages.pos(i * bs as u64).unwrap(), &mut out)
            .unwrap();
        assert!(out == data(i, bs), "durable page {i} lost");
    }
}

/// A failed flush: the durable writes it covered are `DurabilityUnknown`
/// (or `Poisoned`), never receipted; the store is poisoned.
#[test]
fn test_a_failed_flush_gives_no_receipt_and_poisons() {
    let (p, s) = platform(SimConfig::volatile(1020));
    let wal = s.provision_append_region("wal", MIB).unwrap();
    let mut sub = sub(&s, 8);
    p.with_world(|w| {
        let next = w.flushes() + 1;
        w.faults_mut().fail_flush = Some(next);
    });
    let mut done = Vec::new();
    for i in 0..4 {
        append(
            &mut sub,
            &wal,
            &data(i, 500),
            WriteMode::Durable,
            i,
            &mut done,
        );
    }
    drain(&mut sub, &mut done);
    assert_eq!(done.len(), 4);
    for c in &done {
        assert!(
            matches!(
                c.outcome,
                Err(Error::DurabilityUnknown { .. } | Error::Poisoned { .. })
            ),
            "{:?}",
            c.outcome
        );
    }
    assert!(s.poisoned().is_some());
    let buf = sub.take_buf(10).unwrap();
    let r = sub.append(&wal, buf, WriteMode::Durable, 9).unwrap_err();
    assert!(matches!(r.error, Error::Poisoned { .. }));
}

/// A power-safe store: durable writes are receipted at completion with no
/// flush at all.
#[test]
fn test_power_safe_durable_writes_need_no_flush() {
    let (p, s) = platform(SimConfig::power_safe(1030));
    let wal = s.provision_append_region("wal", MIB).unwrap();
    let mut sub = sub(&s, 16);
    let flushes = p.with_world(|w| w.flushes());
    let mut done = Vec::new();
    for i in 0..8 {
        append(
            &mut sub,
            &wal,
            &data(i, 700),
            WriteMode::Durable,
            i,
            &mut done,
        );
    }
    sub.barrier_all(99).unwrap();
    drain(&mut sub, &mut done);
    assert_eq!(
        p.with_world(|w| w.flushes()),
        flushes,
        "a power-safe store flushed"
    );
    for c in &done {
        match &c.outcome {
            Ok(Done::Written(w)) => assert!(w.receipt.as_ref().unwrap().covers(&w.ticket)),
            Ok(Done::Flushed) => assert_eq!(c.tag, 99),
            other => panic!("{other:?}"),
        }
    }
}

/// `barrier(ticket)` receipts that ticket's range; `barrier_all` makes
/// every earlier write durable; both survive a crash. A ticket of another
/// open, or of a recycled generation, is refused.
#[test]
fn test_barriers() {
    let (p, s) = platform(SimConfig::volatile(1040));
    let wal = s.provision_append_region("wal", MIB).unwrap();
    let mut sub = sub(&s, 8);
    let mut done = Vec::new();
    let a = append(
        &mut sub,
        &wal,
        &data(1, 300),
        WriteMode::Plain,
        1,
        &mut done,
    );
    drain(&mut sub, &mut done);
    let Ok(Done::Written(w)) = &done[0].outcome else {
        panic!()
    };
    sub.barrier(&w.ticket, 2).unwrap();
    // A blocking append, then a store-wide barrier from the submitter.
    let t = wal.append(&data(3, 400)).unwrap();
    sub.barrier_all(3).unwrap();
    let mut more = Vec::new();
    drain(&mut sub, &mut more);
    for c in &more {
        match (&c.outcome, c.tag) {
            (Ok(Done::Synced(r)), 2) => assert!(r.covers(&w.ticket)),
            (Ok(Done::Flushed), 3) => {}
            other => panic!("{other:?}"),
        }
    }
    // A ticket from another open of a store is never covered.
    let (_q, other) = platform(SimConfig::volatile(1041));
    let theirs = other.provision_append_region("wal", MIB).unwrap();
    let foreign = theirs.append(b"x").unwrap();
    let r = sub.barrier(&foreign, 4).unwrap_err();
    assert!(matches!(
        r.error,
        Error::NotWritten {
            cause: NotWrittenCause::ForeignPosition,
            ..
        }
    ));
    let pos = t.pos();
    drop((sub, wal, s));
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    let s = Store::open(p.clone(), Path::new(DIR), opts()).unwrap();
    assert!(
        read_back(&s, a, 300) == data(1, 300),
        "barrier(ticket) lost"
    );
    assert!(
        read_back(&s, pos.offset(), 400) == data(3, 400),
        "barrier_all lost"
    );
    // A recycled generation's ticket.
    let wal = s.append_region("wal").unwrap();
    wal.resume_at(MIB / 2).unwrap();
    let old = wal.append(b"old").unwrap();
    wal.recycle().unwrap();
    let mut sub2 = s.submitter(SubmitterConfig::default()).unwrap();
    let r = sub2.barrier(&old, 5).unwrap_err();
    assert!(matches!(
        r.error,
        Error::NotWritten {
            cause: NotWrittenCause::StaleGeneration,
            ..
        }
    ));
}

/// Several threads with their own submitters and blocking barriers at once
/// share flushes, and every durable write is receipted.
#[test]
fn test_submitters_and_blocking_barriers_share_flushes() {
    let (p, s) = platform(SimConfig::volatile(1050));
    let flushes = p.with_world(|w| w.flushes());
    let workers: Vec<_> = (0..4u64)
        .map(|t| {
            let s = s.clone();
            std::thread::spawn(move || {
                let wal = s.provision_append_region(&format!("wal{t}"), MIB).unwrap();
                // Blocking appends on a region of their own: a blocking sync
                // past this thread's own unreaped appends would wait for
                // itself (see `Submitter`).
                let blocking = s.provision_append_region(&format!("blk{t}"), MIB).unwrap();
                let mut sub = sub(&s, 8);
                let mut done = Vec::new();
                for i in 0..50 {
                    append(
                        &mut sub,
                        &wal,
                        &data(i, 200),
                        WriteMode::Durable,
                        i,
                        &mut done,
                    );
                    if i % 10 == 0 {
                        blocking
                            .sync_through(&blocking.append(b"blocking").unwrap())
                            .unwrap();
                    }
                }
                drain(&mut sub, &mut done);
                done.iter()
                    .all(|c| matches!(&c.outcome, Ok(Done::Written(w)) if w.receipt.as_ref().is_some_and(|r| r.covers(&w.ticket))))
            })
        })
        .collect();
    for w in workers {
        assert!(w.join().unwrap(), "a durable write without a receipt");
    }
    let used = p.with_world(|w| w.flushes()) - flushes;
    assert!(
        used < 4 * 55,
        "{used} flushes for 220 durable writes and barriers"
    );
}

/// Dropping a submitter with flushes pending waits for them: the writes are
/// durable afterwards, nothing is poisoned, and the store can be dropped
/// (its flush thread stops) and reopened.
#[test]
fn test_drop_waits_for_pending_flushes() {
    let (p, s) = platform(SimConfig::volatile(1060));
    let wal = s.provision_append_region("wal", MIB).unwrap();
    let mut sub = sub(&s, 8);
    let mut placed = Vec::new();
    let mut done = Vec::new();
    for i in 0..8 {
        let bytes = data(i, 600);
        placed.push((
            append(&mut sub, &wal, &bytes, WriteMode::Durable, i, &mut done),
            bytes,
        ));
    }
    drop(sub);
    assert!(s.poisoned().is_none());
    // Lose everything not flushed now, with the store still open: only a
    // drop that waited for the flushes leaves the writes on media.
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    let (tx, rx) = mpsc::channel();
    let dropper = std::thread::spawn(move || {
        drop((wal, s));
        tx.send(()).unwrap();
    });
    rx.recv_timeout(Duration::from_secs(20))
        .expect("dropping the store hung (flush thread not stopped)");
    dropper.join().unwrap();
    let s = Store::open(p.clone(), Path::new(DIR), opts()).unwrap();
    for (i, (at, bytes)) in placed.iter().enumerate() {
        assert!(read_back(&s, *at, bytes.len()) == *bytes, "write {i} lost");
    }
}

/// A barrier for a ticket an earlier flush already covers completes at once
/// (no thread hop, no new flush); barriers on a poisoned store fail; a
/// released region's ticket and a read-only store are refused.
#[test]
fn test_barrier_fast_path_and_refusals() {
    let (p, s) = platform(SimConfig::volatile(1070));
    let wal = s.provision_append_region("wal", MIB).unwrap();
    let mut sub = sub(&s, 8);
    let t = wal.append(b"covered").unwrap();
    wal.sync_through(&t).unwrap();
    let flushes = p.with_world(|w| w.flushes());
    sub.barrier(&t, 1).unwrap();
    let c = sub
        .reap()
        .next()
        .expect("an already-covered barrier did not complete at once");
    assert!(
        matches!(&c.outcome, Ok(Done::Synced(r)) if r.covers(&t)),
        "{:?}",
        c.outcome
    );
    assert_eq!(
        p.with_world(|w| w.flushes()),
        flushes,
        "an already-covered barrier flushed"
    );
    // Released: its tickets are refused.
    let gone = s.provision_page_region("gone", MIB).unwrap();
    let pt = gone.write(gone.pos(0).unwrap(), &vec![1; 4096]).unwrap();
    gone.release().unwrap();
    let r = sub.barrier(&pt, 2).unwrap_err();
    assert!(matches!(
        r.error,
        Error::NotWritten {
            cause: NotWrittenCause::NotReady,
            ..
        }
    ));
    // Poisoned: barriers complete with `Poisoned`.
    let t2 = wal.append(b"late").unwrap();
    s.poison();
    sub.barrier(&t2, 3).unwrap();
    sub.barrier_all(4).unwrap();
    let mut done = Vec::new();
    drain(&mut sub, &mut done);
    assert_eq!(done.len(), 2);
    assert!(
        done.iter()
            .all(|c| matches!(c.outcome, Err(Error::Poisoned { .. })))
    );
    // Read-only: nothing to flush, refused.
    drop((sub, wal, s));
    let s = Store::open_readonly(p.clone(), Path::new(DIR), opts()).unwrap();
    let mut ro = s.submitter(SubmitterConfig::default()).unwrap();
    let r = ro.barrier_all(5).unwrap_err();
    assert!(matches!(
        r.error,
        Error::NotWritten {
            cause: NotWrittenCause::ReadOnly,
            ..
        }
    ));
}

/// Right after a durable write's outcome is taken, the store's last handle
/// can be dropped and the store reopened at once: the flush thread never
/// holds the store when it hands outcomes over.
#[test]
fn test_a_store_reopens_right_after_its_last_handle_drops() {
    let p = SimPlatform::new(SimConfig::volatile(1080));
    Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    for i in 0..50u64 {
        let s = Store::open(p.clone(), Path::new(DIR), opts()).unwrap();
        let wal = match s.append_region("wal") {
            Ok(w) => w,
            Err(_) => s.provision_append_region("wal", MIB).unwrap(),
        };
        let _ = wal.resume_at(wal.tail());
        let mut sub = sub(&s, 4);
        let mut done = Vec::new();
        append(
            &mut sub,
            &wal,
            &data(i, 64),
            WriteMode::Durable,
            i,
            &mut done,
        );
        drain(&mut sub, &mut done);
        assert!(
            matches!(&done[0].outcome, Ok(Done::Written(_))),
            "{:?}",
            done[0].outcome
        );
        drop((sub, wal, s));
    }
}
