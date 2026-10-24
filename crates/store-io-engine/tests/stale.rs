//! Operations abandoned on a queue (its wait failed, or a scan visitor
//! panicked with reads in flight) never affect a later operation on that
//! queue (D-097): every holder starts on an empty queue, and a failed wait is
//! a failure of the queue, never a verdict on the media.
//!
//! Without that, a later read could return another read's bytes, a later
//! batch could be acknowledged before its own writes completed, a queue
//! filled with abandoned operations refused everything forever, and an open
//! could take a failed wait for an unreadable slot and fall back to an older
//! region table.
//!
//! One shared queue (`queues: 1`), so every call after the failure reuses
//! the queue the failed call abandoned; completions arrive out of order, as
//! on IOCP, over several seeds.

#![allow(clippy::unwrap_used, unused_results, unused_must_use)]

use std::ops::ControlFlow;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use store_io_core::error::Error;
use store_io_engine::{PageRegion, ScanItem, Store, StoreOptions};
use store_io_sim::{SimConfig, SimPlatform};

const MIB: usize = 1 << 20;
/// The simulator's EIO.
const EIO: i32 = 5;

fn platform(seed: u64) -> SimPlatform {
    let mut cfg = SimConfig::volatile(seed);
    cfg.reorder = true;
    // Failed waits (also with `STORE_IO_SIM_INLINE`): every operation
    // submitted and waited for.
    cfg.inline = false;
    SimPlatform::new(cfg)
}

/// Block `i`'s contents: its index (little-endian) in the first 8 bytes,
/// then `i as u8`, so no two blocks of the region are alike.
fn block(i: usize, bs: usize) -> Vec<u8> {
    let mut b = vec![i as u8; bs];
    b[..8].copy_from_slice(&(i as u64).to_le_bytes());
    b
}

fn setup(
    seed: u64,
    depth: u32,
) -> (
    SimPlatform,
    Store<SimPlatform>,
    PageRegion<SimPlatform>,
    usize,
) {
    let p = platform(seed);
    let opts = StoreOptions {
        queues: 1,
        queue_depth: depth,
        ..StoreOptions::default()
    };
    let s = Store::create(p.clone(), Path::new("/db"), opts).unwrap();
    let pages = s.provision_page_region("pages", 8 * MIB as u64).unwrap();
    let bs = pages.block_size() as usize;
    for start in (0..8 * MIB / bs).step_by(32) {
        let mut batch = pages.batch();
        for i in start..start + 32 {
            batch
                .write(pages.pos((i * bs) as u64).unwrap(), &block(i, bs))
                .unwrap();
        }
        batch.commit().unwrap();
    }
    (p, s, pages, bs)
}

/// Fails the next queue wait.
fn fail_next_wait(p: &SimPlatform) {
    p.with_world(|w| {
        let next = w.waits() + 1;
        w.faults_mut().fail_wait = Some((next, EIO));
    });
}

fn expect_blocks(bs: usize, first: usize, out: &[u8]) {
    for (k, got) in out.chunks(bs).enumerate() {
        assert!(
            got == block(first + k, bs).as_slice(),
            "block {} holds another block's bytes",
            first + k
        );
    }
}

/// A large read whose wait fails, leaving reads in flight on the queue.
fn abandon_a_large_read(p: &SimPlatform, pages: &PageRegion<SimPlatform>) {
    fail_next_wait(p);
    let mut out = vec![0u8; 3 * MIB];
    assert!(matches!(
        pages.read(pages.pos(0).unwrap(), &mut out),
        Err(Error::Io { .. })
    ));
}

/// A single read whose wait failed reports `Io` (not a media error), and the
/// next single reads on the same queue get their own bytes.
#[test]
fn test_a_read_after_a_failed_wait_gets_its_own_bytes() {
    for seed in 0..8 {
        let (p, _s, pages, bs) = setup(seed, 64);
        fail_next_wait(&p);
        let mut out = vec![0u8; bs];
        let err = pages
            .read(pages.pos(3 * bs as u64).unwrap(), &mut out)
            .unwrap_err();
        assert!(matches!(err, Error::Io { .. }), "{err:?}");
        for i in [5usize, 6, 7] {
            let n = pages
                .read(pages.pos((i * bs) as u64).unwrap(), &mut out)
                .unwrap();
            assert_eq!(n, bs);
            expect_blocks(bs, i, &out);
        }
    }
}

/// After a large read abandoned reads in flight, the next large read on the
/// same queue delivers exactly its own bytes.
#[test]
fn test_a_large_read_after_a_failed_wait_gets_its_own_bytes() {
    for seed in 0..8 {
        let (p, _s, pages, bs) = setup(100 + seed, 64);
        abandon_a_large_read(&p, &pages);
        let mut out = vec![0u8; 3 * MIB];
        let from = 4 * MIB;
        let n = pages
            .read(pages.pos(from as u64).unwrap(), &mut out)
            .unwrap();
        assert_eq!(n, out.len());
        expect_blocks(bs, from / bs, &out);
    }
}

/// After reads were abandoned, a batch on the same queue is acknowledged
/// only once its own writes completed: every byte reads back.
#[test]
fn test_a_batch_after_a_failed_wait_counts_only_its_own_writes() {
    for seed in 0..8 {
        let (p, _s, pages, bs) = setup(200 + seed, 64);
        abandon_a_large_read(&p, &pages);
        let mut batch = pages.batch();
        for i in 0..16usize {
            batch
                .write(
                    pages.pos((i * 2 * bs) as u64).unwrap(),
                    &block(5000 + i, bs),
                )
                .unwrap();
        }
        batch.commit().unwrap();
        let mut one = vec![0u8; bs];
        for i in 0..16usize {
            pages
                .read(pages.pos((i * 2 * bs) as u64).unwrap(), &mut one)
                .unwrap();
            assert!(
                one == block(5000 + i, bs),
                "write {i} was acknowledged before it landed"
            );
        }
    }
}

/// A queue filled entirely by abandoned operations is cleared by its next
/// holder: reads and batches work again (they were refused as "queue full"
/// forever).
#[test]
fn test_a_queue_full_of_abandoned_operations_recovers() {
    for depth in [1u32, 3] {
        let (p, _s, pages, bs) = setup(300 + u64::from(depth), depth);
        abandon_a_large_read(&p, &pages);
        let mut one = vec![0u8; bs];
        pages
            .read(pages.pos(9 * bs as u64).unwrap(), &mut one)
            .unwrap();
        expect_blocks(bs, 9, &one);
        let mut out = vec![0u8; 3 * MIB];
        pages
            .read(pages.pos(MIB as u64).unwrap(), &mut out)
            .unwrap();
        expect_blocks(bs, MIB / bs, &out);
        let mut batch = pages.batch();
        for i in 0..2usize {
            batch
                .write(pages.pos((i * bs) as u64).unwrap(), &block(7000 + i, bs))
                .unwrap();
        }
        batch.commit().unwrap();
    }
}

/// A scan visitor that panics leaves its reads in flight on the queue (on
/// every platform); the next large read still gets exactly its own bytes.
#[test]
fn test_a_panicking_scan_visitor_leaves_nothing_behind() {
    for seed in 0..8 {
        let (_p, _s, pages, bs) = setup(400 + seed, 64);
        let panicked = catch_unwind(AssertUnwindSafe(|| {
            pages.scan(0, |item| {
                if let ScanItem::Data { .. } = item {
                    panic!("visitor fails");
                }
                ControlFlow::Continue(())
            })
        }));
        assert!(panicked.is_err());
        let mut out = vec![0u8; 3 * MIB];
        let from = 4 * MIB;
        pages
            .read(pages.pos(from as u64).unwrap(), &mut out)
            .unwrap();
        expect_blocks(bs, from / bs, &out);
    }
}

/// An open whose queue wait fails at any step either fails (`Io` for a
/// read, `DurabilityUnknown` for a write) or opens the complete state: it never takes the failed wait for an
/// unreadable slot and falls back to an older region table (a later
/// provisioning would then overwrite the newer one and lose a region).
#[test]
fn test_an_open_with_a_failed_wait_never_opens_an_older_state() {
    let p = platform(500);
    // A small table (format 2's default of 65,536 regions reads 388 pages:
    // a wait failing at each of them is the same case 388 times).
    let opts = || StoreOptions {
        queues: 1,
        region_capacity: 400,
        ..StoreOptions::default()
    };
    let s = Store::create(p.clone(), Path::new("/db"), opts()).unwrap();
    for name in ["r0", "r1", "r2"] {
        s.provision_page_region(name, MIB as u64).unwrap();
    }
    drop(s);
    let mut opened = 0;
    for k in 1..=80u64 {
        p.with_world(|w| {
            let at = w.waits() + k;
            w.faults_mut().fail_wait = Some((at, EIO));
        });
        let r = Store::open(p.clone(), Path::new("/db"), opts());
        p.with_world(|w| w.faults_mut().fail_wait = None);
        match r {
            Ok(s) => {
                opened += 1;
                let mut names: Vec<String> = s.regions().into_iter().map(|(n, _)| n).collect();
                names.sort();
                assert_eq!(names, ["r0", "r1", "r2"], "the wait failed at step {k}");
            }
            // A failed read is `Io`; an abandoned write (the owner
            // generation's slot) is `DurabilityUnknown`, fail-stop. Neither
            // may stand in for an older state.
            Err(Error::Io { .. } | Error::DurabilityUnknown { .. }) => {}
            Err(e) => panic!("the wait failed at step {k}: {e:?}"),
        }
    }
    assert!(opened > 0, "no open got past every failure");
    let s = Store::open(p, Path::new("/db"), opts()).unwrap();
    assert_eq!(s.regions().len(), 3);
}
