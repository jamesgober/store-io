//! Large single operations (D-101, G4): a submitter writes up to
//! `StoreOptions::max_write` from one pooled buffer as one OS write (one
//! device write in the simulator); the blocking layers keep their `max_io`
//! pieces.

#![allow(clippy::unwrap_used, unused_results, unused_must_use)]

use std::path::Path;

use store_io_core::error::{Error, NotWrittenCause};
use store_io_engine::completion::Done;
use store_io_engine::{Store, StoreOptions, SubmitterConfig, WriteMode};
use store_io_sim::{CrashMode, SimConfig, SimPlatform};

const MIB: usize = 1 << 20;
const DIR: &str = "/db";

fn opts() -> StoreOptions {
    StoreOptions {
        queues: 1,
        ..StoreOptions::default()
    }
}

fn data(len: usize) -> Vec<u8> {
    (0..len)
        .map(|k| (k as u8).wrapping_mul(17) ^ (k >> 12) as u8)
        .collect()
}

fn writes(p: &SimPlatform) -> u64 {
    p.with_world(|w| w.writes())
}

/// A 4 MiB durable append from one buffer is one device write, and it
/// survives a crash.
#[test]
fn test_a_4_mib_append_is_one_device_write() {
    let p = SimPlatform::new(SimConfig::volatile(1300));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let wal = s.provision_append_region("wal", 16 * MIB as u64).unwrap();
    let mut sub = s.submitter(SubmitterConfig::default()).unwrap();
    assert_eq!(sub.max_buf(), 4 * MIB);
    let bytes = data(4 * MIB);
    let mut buf = sub.take_buf(bytes.len()).unwrap();
    buf.as_mut_slice().copy_from_slice(&bytes);
    let before = writes(&p);
    let at = sub
        .append(&wal, buf, WriteMode::Durable, 1)
        .unwrap()
        .offset();
    let c = sub.wait(1).unwrap().next().unwrap();
    let Ok(Done::Written(w)) = c.outcome else {
        panic!("{:?}", c.outcome)
    };
    assert!(w.receipt.unwrap().covers(&w.ticket));
    assert_eq!(
        writes(&p) - before,
        1,
        "a 4 MiB append took several device writes"
    );
    drop((sub, wal, s));
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    let s = Store::open(p.clone(), Path::new(DIR), opts()).unwrap();
    let mut out = vec![0; bytes.len()];
    s.append_region("wal").unwrap().read(at, &mut out).unwrap();
    assert!(out == bytes, "the 4 MiB append was lost");
}

/// The blocking layers still write in `max_io` pieces (their behaviour is
/// unchanged until measured otherwise).
#[test]
fn test_blocking_writes_keep_max_io_pieces() {
    let p = SimPlatform::new(SimConfig::volatile(1301));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let wal = s.provision_append_region("wal", 16 * MIB as u64).unwrap();
    let before = writes(&p);
    wal.append(&data(3 * MIB)).unwrap();
    assert_eq!(writes(&p) - before, 3);
}

/// Without large buffers a submitter's largest operation is `max_io`.
#[test]
fn test_no_large_buffers_means_max_io() {
    let p = SimPlatform::new(SimConfig::volatile(1302));
    let o = StoreOptions {
        large_buffers: 0,
        ..opts()
    };
    let s = Store::create(p, Path::new(DIR), o).unwrap();
    let sub = s.submitter(SubmitterConfig::default()).unwrap();
    assert_eq!(sub.max_buf(), MIB);
    assert!(matches!(
        sub.take_buf(2 * MIB),
        Err(Error::NotWritten {
            cause: NotWrittenCause::TooLarge,
            ..
        })
    ));
}

/// The blocking layers never use the large buffers: a batch that runs out
/// of ordinary buffers is refused, and a submitter's largest buffer is
/// still there.
#[test]
fn test_blocking_calls_leave_the_large_buffers_alone() {
    let p = SimPlatform::new(SimConfig::volatile(1303));
    let o = StoreOptions {
        buffers_per_class: 4,
        large_buffers: 1,
        ..opts()
    };
    let s = Store::create(p, Path::new(DIR), o).unwrap();
    let wal = s.provision_append_region("wal", 32 * MIB as u64).unwrap();
    let mut batch = wal.batch();
    let mut refused = false;
    for _ in 0..8 {
        if batch.append(&data(MIB)).is_err() {
            refused = true;
            break;
        }
    }
    assert!(refused, "the batch borrowed beyond the ordinary buffers");
    let sub = s.submitter(SubmitterConfig::default()).unwrap();
    assert!(
        sub.take_buf(sub.max_buf()).is_ok(),
        "the large buffer was used up"
    );
}

/// Provisioning with a small `max_io` fills in `max_io` pieces and works
/// while every large buffer is held.
#[test]
fn test_provisioning_never_needs_a_large_buffer() {
    let p = SimPlatform::new(SimConfig::volatile(1304));
    let o = StoreOptions {
        max_io: 1 << 17,
        large_buffers: 1,
        ..opts()
    };
    let s = Store::create(p, Path::new(DIR), o).unwrap();
    let sub = s.submitter(SubmitterConfig::default()).unwrap();
    let held = sub.take_buf(sub.max_buf()).unwrap();
    s.provision_page_region("pages", 4 * MIB as u64).unwrap();
    drop(held);
}

/// `max_write` is rounded up to a power of two; at or below `max_io` there
/// is no large class.
#[test]
fn test_max_write_is_clamped() {
    for (max_write, want) in [(3 * MIB, 4 * MIB), (MIB / 2, MIB), (MIB, MIB)] {
        let p = SimPlatform::new(SimConfig::volatile(1305));
        let o = StoreOptions {
            max_write,
            ..opts()
        };
        let s = Store::create(p, Path::new(DIR), o).unwrap();
        let sub = s.submitter(SubmitterConfig::default()).unwrap();
        assert_eq!(sub.max_buf(), want, "max_write {max_write}");
    }
}

/// Oversized options are clamped: `max_io` to at least 128 KiB, `max_write`
/// to at most 1 GiB (no large class built when that is all there is).
#[test]
fn test_extreme_options_are_clamped() {
    let p = SimPlatform::new(SimConfig::volatile(1306));
    let o = StoreOptions {
        max_io: 1,
        large_buffers: 0,
        max_write: usize::MAX,
        ..opts()
    };
    let s = Store::create(p, Path::new(DIR), o).unwrap();
    let sub = s.submitter(SubmitterConfig::default()).unwrap();
    assert_eq!(sub.max_buf(), 128 << 10);
}
