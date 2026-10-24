//! The inline fast path (G1 P1): on a queue that runs every operation inside
//! `submit` (Linux's synchronous tier; the simulator with
//! `SimConfig::inline`), an operation made while nothing is in flight is
//! performed at once (`Queue::perform`), with no op-table row and no
//! completion. Each scenario here runs on the slow path and on the fast
//! path and must end the same: the same result, the same poisoning, the
//! same bytes on the media after a crash.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use store_io_core::error::Error;
use store_io_engine::{Store, StoreOptions};
use store_io_sim::{CrashMode, SimConfig, SimPlatform};

const DIR: &str = "/db";
/// The smallest pool the store allows: 128 KiB buffers, so a write of
/// several pieces is cheap to make.
const CHUNK: usize = 1 << 17;
const B: usize = 4096;

fn opts() -> StoreOptions {
    StoreOptions {
        buffers_per_class: 8,
        max_io: CHUNK,
        ..StoreOptions::default()
    }
}

/// `inline` set as asked (whatever `STORE_IO_SIM_INLINE` says).
fn platform(seed: u64, inline: bool) -> SimPlatform {
    SimPlatform::new(SimConfig {
        inline,
        ..SimConfig::volatile(seed)
    })
}

fn record(i: u32, len: usize) -> Vec<u8> {
    (0..len)
        .map(|j| (i as usize * 131 + j * 7 + (j >> 9)) as u8)
        .collect()
}

/// What a scenario ended with: the result's kind, the poisoning, and the
/// whole of region "p" after the store is dropped, power fails (every
/// cached write lost) and the store is opened again.
fn ending(p: &SimPlatform, s: Store<SimPlatform>, r: &Result<(), Error>) -> String {
    let kind = match r {
        Ok(()) => "ok".to_owned(),
        Err(e) => format!("{e:?}")
            .split([' ', '{', '('])
            .next()
            .unwrap()
            .to_owned(),
    };
    let poisoned = s.poisoned().is_some();
    std::mem::forget(s);
    p.with_world(|w| {
        *w.faults_mut() = store_io_sim::FaultPlan::default();
        w.crash(&CrashMode::LoseAll);
    });
    let s = Store::open(p.clone(), Path::new(DIR), opts()).unwrap();
    let pages = s.page_region("p").unwrap();
    let mut back = vec![0u8; 2 << 20];
    pages.read(pages.pos(0).unwrap(), &mut back).unwrap();
    let sum = back
        .iter()
        .fold(0u64, |h, &b| h.wrapping_mul(31).wrapping_add(u64::from(b)));
    format!("{kind} poisoned={poisoned} bytes={sum:x}")
}

/// Runs `scenario` (given the platform and region "p", 2 MiB, its first
/// 4 KiB written durably) on the slow path and the fast path (where it
/// waits on no queue): both end the same, and that ending is returned.
fn both(
    seed: u64,
    scenario: impl Fn(&SimPlatform, &store_io_engine::PageRegion<SimPlatform>) -> Result<(), Error>,
) -> String {
    let mut endings = Vec::new();
    for inline in [false, true] {
        let p = platform(seed, inline);
        let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
        let pages = s.provision_page_region("p", 2 << 20).unwrap();
        pages
            .write_durable(pages.pos(0).unwrap(), &record(0, B))
            .unwrap();
        let waits = p.with_world(|w| w.waits());
        let r = scenario(&p, &pages);
        if inline {
            assert_eq!(p.with_world(|w| w.waits()), waits, "performed at once");
        }
        drop(pages);
        endings.push(ending(&p, s, &r));
    }
    assert_eq!(endings[0], endings[1], "slow path, then fast path");
    endings.pop().unwrap()
}

/// A durable write, one piece and several (nine: more pieces than the pool
/// has buffers to spare, so some spares are never taken): acknowledged, and
/// on the media after power fails.
#[test]
fn test_durable_writes_end_the_same_on_both_paths() {
    for len in [B, 3 * CHUNK, 9 * CHUNK] {
        let e = both(1500, |_, pages| {
            pages
                .write_durable(pages.pos(0).unwrap(), &record(1, len))
                .map(drop)
        });
        assert!(e.starts_with("ok poisoned=false"), "{e}");
    }
}

/// A write refused before any I/O (the platform refuses the submission):
/// `Io`, nothing poisoned, the media as it was; the store writes again.
#[test]
fn test_a_refused_write_ends_the_same_on_both_paths() {
    let e = both(1501, |p, pages| {
        p.with_world(|w| w.faults_mut().fail_submit = Some((w.write_submits() + 1, 5)));
        let r = pages.write_durable(pages.pos(0).unwrap(), &record(2, B));
        p.with_world(|w| w.faults_mut().fail_submit = None);
        assert!(matches!(r, Err(Error::Io { .. })), "{r:?}");
        pages
            .write_durable(pages.pos(B as u64).unwrap(), &record(3, B))
            .map(drop)?;
        r.map(drop)
    });
    assert!(e.starts_with("Io poisoned=false"), "{e}");
    // Refused at its second piece, the first already written: unknown.
    let e = both(1506, |p, pages| {
        p.with_world(|w| w.faults_mut().fail_submit = Some((w.write_submits() + 2, 5)));
        pages
            .write_durable(pages.pos(0).unwrap(), &record(2, 3 * CHUNK))
            .map(drop)
    });
    assert!(e.starts_with("DurabilityUnknown poisoned=true"), "{e}");
}

/// A write failing at completion (EIO), a short transfer, a later piece of
/// a write of several failing, and a failed flush: unknown, poisoned, the
/// bytes acknowledged before kept.
#[test]
fn test_failed_writes_and_flushes_end_the_same_on_both_paths() {
    let cases: [(&str, u64, usize); 4] = [
        ("eio", 1, B),
        ("short", 1, B),
        ("eio", 2, 3 * CHUNK),
        ("flush", 1, B),
    ];
    for (k, (fault, nth, len)) in cases.into_iter().enumerate() {
        let e = both(1502 + k as u64, |p, pages| {
            p.with_world(|w| match fault {
                "eio" => w.faults_mut().fail_write = Some(w.writes() + nth),
                "short" => w.faults_mut().short_write = Some(w.writes() + nth),
                _ => w.faults_mut().fail_flush = Some(w.flushes() + nth),
            });
            pages
                .write_durable(pages.pos(0).unwrap(), &record(4, len))
                .map(drop)
        });
        assert!(
            e.starts_with("DurabilityUnknown poisoned=true"),
            "{fault} {nth}: {e}"
        );
    }
}

/// On the fast path a durable write waits on no queue (it is performed at
/// once); on the slow path it does.
#[test]
fn test_the_fast_path_waits_on_no_queue() {
    for inline in [false, true] {
        let p = platform(1510, inline);
        let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
        let pages = s.provision_page_region("p", 1 << 20).unwrap();
        let waits = p.with_world(|w| w.waits());
        pages
            .write_durable(pages.pos(0).unwrap(), &record(5, B))
            .unwrap();
        let mut back = vec![0u8; B];
        pages.read(pages.pos(0).unwrap(), &mut back).unwrap();
        assert_eq!(back, record(5, B));
        let waited = p.with_world(|w| w.waits()) - waits;
        assert_eq!(waited == 0, inline, "{waited} waits, inline {inline}");
    }
}

/// An operation performed at once is outstanding while it runs (A1 C6: a
/// hung device stays visible), and nothing is once it returns.
#[test]
fn test_an_operation_performed_at_once_is_outstanding_while_it_runs() {
    let p = platform(1511, true);
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let pages = s.provision_page_region("p", 1 << 20).unwrap();
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
    let waits = p.with_world(|w| w.waits());
    pages
        .write_durable(pages.pos(0).unwrap(), &record(6, B))
        .unwrap();
    p.with_world(|w| w.set_submit_probe(None));
    assert_eq!(p.with_world(|w| w.waits()), waits, "performed at once");
    assert!(seen.load(Ordering::Relaxed) >= 2, "the write and the flush");
    assert_eq!(idle.load(Ordering::Relaxed), 0);
    assert_eq!(s.oldest_outstanding(), None);
}
