//! Writes of any size, the batch layer and exact receipts, on the
//! deterministic simulator.

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

use store_io_core::error::{Error, NotWrittenCause};
use store_io_engine::{Store, StoreOptions};
use store_io_sim::{CrashMode, SimConfig, SimPlatform};

const DIR: &str = "/db";
/// The smallest pool the store allows: 128 KiB buffers, so multi-piece
/// writes and multi-buffer batches are cheap to provoke.
const CHUNK: usize = 1 << 17;

fn opts() -> StoreOptions {
    StoreOptions {
        queues: 2,
        buffers_per_class: 8,
        append_ring: 256,
        max_io: CHUNK,
        ..StoreOptions::default()
    }
}

fn create_with(p: &SimPlatform, o: StoreOptions) -> Store<SimPlatform> {
    Store::create(p.clone(), Path::new(DIR), o).unwrap()
}

fn reopen(p: &SimPlatform) -> Store<SimPlatform> {
    Store::open(p.clone(), Path::new(DIR), opts()).unwrap()
}

fn record(i: u32, len: usize) -> Vec<u8> {
    (0..len)
        .map(|j| (i as usize * 131 + j * 7 + (j >> 9)) as u8)
        .collect()
}

fn counts(p: &SimPlatform) -> (u64, u64) {
    p.with_world(|w| (w.writes(), w.flushes()))
}

#[test]
fn test_append_of_many_buffers_round_trips_and_survives_a_crash() {
    let p = SimPlatform::new(SimConfig::volatile(11));
    let s = create_with(&p, opts());
    let wal = s.provision_append_region("wal", 4 << 20).unwrap();
    let data = record(1, 8 * CHUNK + 12_345);
    let (w0, f0) = counts(&p);
    let (pos, receipt) = wal.append_durable(&data).unwrap();
    let (w1, f1) = counts(&p);
    // One device write per pooled buffer, one flush for the whole append.
    assert_eq!(w1 - w0, data.len().div_ceil(CHUNK) as u64);
    assert_eq!(f1 - f0, 1);
    assert!(receipt.durable_through() >= pos.offset() + data.len() as u64);
    drop((wal, s));
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    let s = reopen(&p);
    let mut out = vec![0u8; data.len()];
    let n = s
        .append_region("wal")
        .unwrap()
        .read(pos.offset(), &mut out)
        .unwrap();
    assert_eq!(n, data.len());
    assert!(out == data, "large receipted append lost or altered");
}

#[test]
fn test_padding_and_lent_bytes_are_zero_even_in_reused_buffers() {
    // Pool buffers are reused in rotation. Dirty every buffer of every class
    // first; then short writes must not carry earlier bytes into their
    // padding, and lent slices must arrive zeroed.
    let p = SimPlatform::new(SimConfig::volatile(23));
    let s = create_with(
        &p,
        StoreOptions {
            buffers_per_class: 4,
            ..opts()
        },
    );
    let wal = s.provision_append_region("wal", 8 << 20).unwrap();
    let bs = wal.block_size() as usize;
    let mut len = bs;
    while len <= CHUNK {
        for _ in 0..4 {
            wal.append(&vec![0xFF; len]).unwrap();
            let mut batch = wal.batch();
            batch.append(&vec![0xFF; len]).unwrap();
            batch.commit().unwrap();
        }
        len *= 2;
    }
    let mut out = vec![0u8; bs];
    for i in 0..4u32 {
        let t = wal.append(&record(i, 10)).unwrap();
        wal.read(t.pos().offset(), &mut out).unwrap();
        assert_eq!(&out[..10], record(i, 10).as_slice());
        assert!(out[10..].iter().all(|&b| b == 0), "append padding");
        let mut batch = wal.batch();
        batch.append(&record(i, 7)).unwrap();
        batch
            .append_with(5, |buf| {
                assert!(buf.iter().all(|&b| b == 0), "lent bytes are zeroed");
                buf.copy_from_slice(&[9; 5]);
            })
            .unwrap();
        let (pos, _) = batch.commit().unwrap();
        wal.read(pos.offset(), &mut out).unwrap();
        assert_eq!(&out[7..12], &[9; 5]);
        assert!(out[12..].iter().all(|&b| b == 0), "batch padding");
    }
}

#[test]
fn test_appends_wait_rather_than_fail_when_the_ring_is_full() {
    // A ring of 4 blocks with appends of up to 40 blocks from 4 threads:
    // reservations routinely run past the ring, and must wait, not fail.
    let p = SimPlatform::new(SimConfig::volatile(12));
    let s = create_with(
        &p,
        StoreOptions {
            append_ring: 4,
            ..opts()
        },
    );
    let wal = s.provision_append_region("wal", 16 << 20).unwrap();
    let handles: Vec<_> = (0..4u32)
        .map(|t| {
            let wal = wal.clone();
            std::thread::spawn(move || {
                let mut mine = Vec::new();
                for i in 0..12u32 {
                    let len = 1 + ((t * 12 + i) as usize * 13_331) % (40 * 4096);
                    let rec = record(t * 100 + i, len);
                    let ticket = wal.append(&rec).unwrap();
                    mine.push((ticket.pos().offset(), rec, ticket));
                }
                mine
            })
        })
        .collect();
    let mut all = Vec::new();
    for h in handles {
        all.extend(h.join().unwrap());
    }
    let last = all.iter().max_by_key(|(_, _, t)| t.end()).unwrap();
    let receipt = wal.sync_through(&last.2).unwrap();
    for (off, rec, ticket) in &all {
        assert!(receipt.covers(ticket));
        let mut out = vec![0u8; rec.len()];
        wal.read(*off, &mut out).unwrap();
        assert!(&out == rec, "record at {off} altered");
    }
}

#[test]
fn test_batch_of_small_records_is_one_write_and_one_flush() {
    let p = SimPlatform::new(SimConfig::volatile(13));
    let s = create_with(&p, opts());
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    let mut batch = wal.batch();
    let recs: Vec<Vec<u8>> = (0..100).map(|i| record(i, 64)).collect();
    let rel: Vec<u64> = recs.iter().map(|r| batch.append(r).unwrap()).collect();
    assert_eq!(batch.len(), 100);
    assert_eq!(batch.bytes(), 6400);
    // Packed back to back: no per-record padding.
    assert!(rel.iter().enumerate().all(|(i, &o)| o == i as u64 * 64));
    let (w0, f0) = counts(&p);
    let (pos, receipt) = batch.commit().unwrap();
    let (w1, f1) = counts(&p);
    assert_eq!((w1 - w0, f1 - f0), (1, 1));
    assert!(batch.is_empty());
    assert_eq!(receipt.durable_through(), pos.offset() + 8192);
    drop((batch, wal, s));
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    let wal = reopen(&p).append_region("wal").unwrap();
    let mut out = vec![0u8; 8192];
    wal.read(pos.offset(), &mut out).unwrap();
    for (r, &o) in recs.iter().zip(&rel) {
        assert_eq!(&out[o as usize..o as usize + 64], r.as_slice());
    }
    assert!(out[6400..].iter().all(|&b| b == 0), "padding is zero");
}

#[test]
fn test_batch_staging_matches_a_byte_model_across_buffers() {
    // Copied and in-place records of every size, crossing buffer boundaries
    // (in-place records carry a partial block into a new buffer): the bytes
    // on the device are exactly the concatenation, positions included.
    let p = SimPlatform::new(SimConfig::volatile(14));
    let s = create_with(&p, opts());
    let wal = s.provision_append_region("wal", 4 << 20).unwrap();
    let mut batch = wal.batch();
    let mut model: Vec<u8> = Vec::new();
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut i = 0u32;
    while model.len() < 6 * CHUNK {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let len = 1 + (seed % 40_000) as usize;
        let rec = record(i, len);
        let at = if seed % 3 == 0 {
            batch
                .append_with(len, |buf| {
                    assert!(buf.iter().all(|&b| b == 0), "lent bytes are zeroed");
                    buf.copy_from_slice(&rec);
                })
                .unwrap()
        } else {
            batch.append(&rec).unwrap()
        };
        assert_eq!(at, model.len() as u64);
        model.extend_from_slice(&rec);
        i += 1;
    }
    assert_eq!(batch.bytes(), model.len() as u64);
    let (pos, _receipt) = batch.commit().unwrap();
    let mut out = vec![0u8; model.len()];
    assert_eq!(wal.read(pos.offset(), &mut out).unwrap(), model.len());
    assert!(out == model, "staged bytes differ from the model");
}

#[test]
fn test_batch_pool_exhaustion_leaves_the_batch_unchanged() {
    let p = SimPlatform::new(SimConfig::volatile(15));
    let s = create_with(
        &p,
        StoreOptions {
            buffers_per_class: 4,
            ..opts()
        },
    );
    let wal = s.provision_append_region("wal", 4 << 20).unwrap();
    let mut batch = wal.batch();
    let mut model = Vec::new();
    let err = loop {
        let rec = record(model.len() as u32, 50_000);
        match batch.append(&rec) {
            Ok(_) => model.extend_from_slice(&rec),
            Err(e) => break e,
        }
    };
    assert!(matches!(
        err,
        Error::NotWritten {
            cause: NotWrittenCause::PoolExhausted,
            ..
        }
    ));
    assert_eq!(batch.bytes(), model.len() as u64);
    assert!(matches!(
        batch.append_with(batch.max_in_place(), |_| {}),
        Err(Error::NotWritten {
            cause: NotWrittenCause::PoolExhausted,
            ..
        })
    ));
    assert_eq!(batch.bytes(), model.len() as u64);
    let (pos, _r) = batch.commit().unwrap();
    let mut out = vec![0u8; model.len()];
    wal.read(pos.offset(), &mut out).unwrap();
    assert!(out == model);
    // The buffers came back: the batch is usable again.
    batch.append(b"again").unwrap();
    batch.commit().unwrap();
}

#[test]
fn test_append_with_rejects_empty_and_oversized_records() {
    let p = SimPlatform::new(SimConfig::volatile(16));
    let s = create_with(&p, opts());
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    let mut batch = wal.batch();
    let too_big = batch.max_in_place() + 1;
    assert!(matches!(
        batch.append_with(too_big, |_| {}),
        Err(Error::NotWritten {
            cause: NotWrittenCause::TooLarge,
            ..
        })
    ));
    assert!(matches!(
        batch.append_with(0, |_| {}),
        Err(Error::NotWritten {
            cause: NotWrittenCause::Empty,
            ..
        })
    ));
    assert!(matches!(
        batch.append(&[]),
        Err(Error::NotWritten {
            cause: NotWrittenCause::Empty,
            ..
        })
    ));
    assert!(matches!(
        batch.commit(),
        Err(Error::NotWritten {
            cause: NotWrittenCause::Empty,
            ..
        })
    ));
}

#[test]
fn test_page_batch_commits_unordered_writes_with_one_flush() {
    let p = SimPlatform::new(SimConfig::volatile(17));
    let s = create_with(&p, opts());
    let pages = s.provision_page_region("pages", 1 << 20).unwrap();
    let bs = pages.block_size() as usize;
    let mut batch = pages.batch();
    for &blk in &[5u64, 1, 3, 9] {
        batch
            .write(pages.pos(blk * bs as u64).unwrap(), &record(blk as u32, bs))
            .unwrap();
    }
    batch
        .write_with(pages.pos(20 * bs as u64).unwrap(), bs, |b| {
            b.copy_from_slice(&record(20, bs));
        })
        .unwrap();
    // A large page write is split into pooled pieces.
    let big = record(64, 3 * CHUNK);
    batch
        .write(pages.pos(64 * bs as u64).unwrap(), &big)
        .unwrap();
    let (w0, f0) = counts(&p);
    let receipt = batch.commit().unwrap();
    let (w1, f1) = counts(&p);
    assert_eq!((w1 - w0, f1 - f0), (5 + 3, 1));
    assert_eq!(receipt.durable_through(), 64 * bs as u64 + big.len() as u64);
    drop((batch, pages, s));
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    let pages = reopen(&p).page_region("pages").unwrap();
    for blk in [1u32, 3, 5, 9, 20] {
        let mut out = vec![0u8; bs];
        pages
            .read(pages.pos(u64::from(blk) * bs as u64).unwrap(), &mut out)
            .unwrap();
        assert_eq!(out, record(blk, bs), "page {blk}");
    }
    let mut out = vec![0u8; big.len()];
    pages
        .read(pages.pos(64 * bs as u64).unwrap(), &mut out)
        .unwrap();
    assert!(out == big);
}

#[test]
fn test_page_batch_refuses_overlap_and_keeps_the_batch() {
    let p = SimPlatform::new(SimConfig::volatile(18));
    let s = create_with(&p, opts());
    let pages = s.provision_page_region("pages", 1 << 20).unwrap();
    let bs = pages.block_size() as usize;
    let mut batch = pages.batch();
    batch
        .write(pages.pos(0).unwrap(), &vec![1; 2 * bs])
        .unwrap();
    batch
        .write(pages.pos(bs as u64).unwrap(), &vec![2; bs])
        .unwrap();
    let (w0, _) = counts(&p);
    assert!(matches!(
        batch.commit(),
        Err(Error::NotWritten {
            cause: NotWrittenCause::Overlap,
            ..
        })
    ));
    assert_eq!(counts(&p).0, w0, "nothing written");
    assert_eq!(batch.len(), 2);
    batch.clear();
    assert!(batch.is_empty());
    // Misaligned and out-of-range writes are refused as they are added.
    assert!(batch.write(pages.pos(0).unwrap(), &[0; 100]).is_err());
    assert!(
        batch
            .write(
                pages.pos(pages.len() - bs as u64).unwrap(),
                &vec![0; 2 * bs]
            )
            .is_err()
    );
    assert!(batch.is_empty());
}

#[test]
fn test_receipts_cover_exactly_the_writes_completed_before_their_flush() {
    let p = SimPlatform::new(SimConfig::volatile(19));
    let s = create_with(&p, opts());
    let pages = s.provision_page_region("pages", 1 << 20).unwrap();
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    let bs = pages.block_size() as usize;
    let before = pages.write(pages.pos(0).unwrap(), &vec![1; bs]).unwrap();
    let mut batch = pages.batch();
    batch
        .write(pages.pos(bs as u64).unwrap(), &vec![2; bs])
        .unwrap();
    let receipt = batch.commit().unwrap();
    let after = pages
        .write(pages.pos(2 * bs as u64).unwrap(), &vec![3; bs])
        .unwrap();
    assert!(receipt.covers(&before), "completed before the flush");
    assert!(!receipt.covers(&after), "completed after the flush");
    // Receipts are per region.
    let t = wal.append(b"x").unwrap();
    assert!(!receipt.covers(&t));
    let r2 = pages.sync_through(&after).unwrap();
    assert!(r2.covers(&after) && r2.covers(&before));
}

#[test]
fn test_a_reopened_store_never_vouches_for_an_earlier_open() {
    let p = SimPlatform::new(SimConfig::volatile(20));
    let s = create_with(&p, opts());
    let pages = s.provision_page_region("pages", 1 << 20).unwrap();
    let bs = pages.block_size() as usize;
    // A few barriers in the first open, so the old ticket's flush number is
    // not trivially small.
    for _ in 0..3 {
        pages
            .write_durable(pages.pos(0).unwrap(), &vec![1; bs])
            .unwrap();
    }
    let old = pages.write(pages.pos(0).unwrap(), &vec![1; bs]).unwrap();
    drop((pages, s));
    let s = reopen(&p);
    let pages = s.page_region("pages").unwrap();
    assert!(matches!(
        pages.sync_through(&old),
        Err(Error::NotWritten {
            cause: NotWrittenCause::ForeignPosition,
            ..
        })
    ));
    // Run this open's flush count well past the old ticket's.
    for _ in 0..8 {
        pages
            .write_durable(pages.pos(0).unwrap(), &vec![2; bs])
            .unwrap();
    }
    let t = pages.write(pages.pos(0).unwrap(), &vec![2; bs]).unwrap();
    let receipt = pages.sync_through(&t).unwrap();
    assert!(receipt.covers(&t));
    assert!(!receipt.covers(&old));
}

#[test]
fn test_a_failed_piece_of_a_large_append_poisons_and_wakes_waiters() {
    let p = SimPlatform::new(SimConfig::volatile(21));
    let s = create_with(&p, opts());
    let wal = Arc::new(s.provision_append_region("wal", 4 << 20).unwrap());
    let (kept, _r) = wal.append_durable(&record(1, 5000)).unwrap();
    p.with_world(|w| {
        let next = w.writes() + 3;
        w.faults_mut().fail_write = Some(next);
    });
    let err = wal.append(&record(2, 6 * CHUNK)).unwrap_err();
    assert!(matches!(err, Error::DurabilityUnknown { .. }), "{err:?}");
    assert!(matches!(wal.append(b"y"), Err(Error::Poisoned { .. })));
    assert!(matches!(
        wal.batch().commit(),
        Err(Error::NotWritten { .. })
    ));
    let mut b = wal.batch();
    b.append(b"z").unwrap();
    assert!(matches!(b.commit(), Err(Error::Poisoned { .. })));
    drop((b, wal, s));
    // Recovery: the receipted record is intact.
    p.with_world(|w| {
        w.faults_mut().fail_write = None;
        w.crash(&CrashMode::LoseAll);
    });
    let wal = reopen(&p).append_region("wal").unwrap();
    let mut out = vec![0u8; 5000];
    wal.read(kept.offset(), &mut out).unwrap();
    assert_eq!(out, record(1, 5000));
}

#[test]
fn test_power_safe_receipts_cover_every_completed_write_without_a_flush() {
    let p = SimPlatform::new(SimConfig::power_safe(22));
    let s = create_with(&p, opts());
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    let mut batch = wal.batch();
    for i in 0..10 {
        batch.append(&record(i, 300)).unwrap();
    }
    let (_, f0) = counts(&p);
    let (pos, receipt) = batch.commit().unwrap();
    assert_eq!(counts(&p).1, f0, "no flush on a power-safe device");
    drop((batch, wal, s));
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    let wal = reopen(&p).append_region("wal").unwrap();
    let mut out = vec![0u8; 300];
    wal.read(pos.offset(), &mut out).unwrap();
    assert_eq!(out, record(0, 300));
    assert!(receipt.durable_through() >= pos.offset() + 3000);
}

#[test]
fn test_a_short_transfer_is_never_success() {
    for page in [false, true] {
        let p = SimPlatform::new(SimConfig::volatile(24));
        let s = create_with(&p, opts());
        let wal = s.provision_append_region("wal", 4 << 20).unwrap();
        let pages = s.provision_page_region("pages", 4 << 20).unwrap();
        p.with_world(|w| {
            let next = w.writes() + 2;
            w.faults_mut().short_write = Some(next);
        });
        let err = if page {
            pages
                .write(pages.pos(0).unwrap(), &record(1, 3 * CHUNK))
                .unwrap_err()
        } else {
            wal.append(&record(1, 3 * CHUNK)).unwrap_err()
        };
        assert!(matches!(err, Error::DurabilityUnknown { .. }), "{err:?}");
        assert!(matches!(wal.append(b"x"), Err(Error::Poisoned { .. })));
    }
}
