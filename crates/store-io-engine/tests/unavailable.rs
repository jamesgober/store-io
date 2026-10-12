//! Regions the region table names that open cannot use are reported, with
//! the reason, and a lookup returns a corruption error naming the region,
//! never `NotFound` (D-090). Before 0.4 such regions were dropped silently.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::path::{Path, PathBuf};

use store_io_buf::{BufPool, PoolConfig};
use store_io_core::error::{CorruptionKind, Error};
use store_io_engine::{CONTAINER, Store, StoreOptions, Unavailable};
use store_io_platform::{CompletionBuf, FileMode, FileName, IoOp, Platform, Queue, QueueConfig};
use store_io_sim::{SimConfig, SimPlatform};

const DIR: &str = "/db";
const BLOCK: u64 = 4096;

fn opts() -> StoreOptions {
    StoreOptions {
        queues: 2,
        buffers_per_class: 8,
        append_ring: 256,
        ..StoreOptions::default()
    }
}

fn media(p: &SimPlatform) -> Vec<u8> {
    p.with_world(|w| w.media_of(&PathBuf::from(DIR), CONTAINER).unwrap().to_vec())
}

/// Overwrites `[at, at + 4096)` of the container with `byte`, durably.
fn scribble(p: &SimPlatform, at: u64, byte: u8) {
    let dir = p.open_dir(Path::new(DIR), false).unwrap();
    let file = p
        .open_file(
            &dir,
            &FileName::new(CONTAINER).unwrap(),
            FileMode::ReadWrite,
        )
        .unwrap();
    let pool = BufPool::new(&PoolConfig::uniform(4096, 4096, 4096, 2)).unwrap();
    let mut q = p.queue(QueueConfig::default()).unwrap();
    let mut buf = pool.take(4096).unwrap();
    buf.as_mut_slice().fill(byte);
    q.submit(
        IoOp::Write {
            file: &file,
            offset: at,
            buf,
            dsync: false,
        },
        1,
    )
    .unwrap();
    let mut out = CompletionBuf::with_capacity(2);
    q.wait(1, &mut out).unwrap();
    assert_eq!(out.drain().next().unwrap().result, Ok(4096));
    p.flush_all(&file).unwrap();
}

fn record(tag: &str) -> Vec<u8> {
    let mut r = vec![0x33u8; BLOCK as usize];
    r[..tag.len()].copy_from_slice(tag.as_bytes());
    r
}

#[test]
fn test_a_healthy_store_reports_no_unavailable_region() {
    let p = SimPlatform::new(SimConfig::volatile(60));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    s.provision_append_region("wal", 1 << 20).unwrap();
    s.provision_slot("manifest").unwrap();
    drop(s);
    let s = Store::open(p.clone(), Path::new(DIR), opts()).unwrap();
    assert!(s.unavailable_regions().is_empty());
    assert!(s.report().unavailable.is_empty());
    assert!(!s.report().to_string().contains("unavailable"));
}

#[test]
fn test_a_region_whose_header_is_destroyed_is_reported_and_never_not_found() {
    let p = SimPlatform::new(SimConfig::volatile(61));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    let other = s.provision_append_region("other", 1 << 20).unwrap();
    wal.append_durable(&record("WAL-RECORD-0001")).unwrap();
    other.append_durable(&record("OTHER-RECORD-01")).unwrap();
    drop((wal, other, s));
    // The region's data area starts at the record; its header pair sits in
    // the two blocks before it. Destroy both copies.
    let m = media(&p);
    let at = m.windows(15).position(|w| w == b"WAL-RECORD-0001").unwrap() as u64;
    scribble(&p, at - 2 * BLOCK, 0xEE);
    scribble(&p, at - BLOCK, 0xEE);

    let s = Store::open(p.clone(), Path::new(DIR), opts()).unwrap();
    let r = s.append_region("wal");
    let Err(Error::Corruption { kind, ctx }) = r else {
        panic!("expected Corruption, got {:?}", r.map(|_| ()));
    };
    assert_eq!(kind, CorruptionKind::HeaderCrc);
    let u = s.unavailable_regions();
    assert_eq!(u.len(), 1);
    assert_eq!(u[0].name, "wal");
    assert_eq!(u[0].reason, Unavailable::HeaderUnreadable);
    assert_eq!(ctx.region, Some(u[0].id));
    assert!(
        s.report()
            .to_string()
            .contains("unavailable      region \"wal\"")
    );
    // The other region is untouched and usable; the name stays reserved.
    let other = s.append_region("other").unwrap();
    let mut b = vec![0u8; BLOCK as usize];
    other.read(0, &mut b).unwrap();
    assert_eq!(&b[..15], b"OTHER-RECORD-01");
    assert!(matches!(
        s.provision_append_region("wal", 1 << 20),
        Err(Error::AlreadyExists { .. })
    ));
    // A name no table entry holds is still NotFound.
    assert!(matches!(
        s.append_region("never"),
        Err(Error::NotFound { .. })
    ));
    // The same holds for a read-only open.
    drop((other, s));
    let ro = Store::open_readonly(p.clone(), Path::new(DIR), opts()).unwrap();
    assert!(matches!(
        ro.append_region("wal"),
        Err(Error::Corruption {
            kind: CorruptionKind::HeaderCrc,
            ..
        })
    ));
}

#[test]
fn test_a_region_cut_off_by_a_truncated_container_is_reported_as_truncated() {
    let p = SimPlatform::new(SimConfig::volatile(62));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let a = s.provision_append_region("a", 64 << 10).unwrap();
    let b = s.provision_append_region("b", 1 << 20).unwrap();
    a.append_durable(&record("A-RECORD-000001")).unwrap();
    b.append_durable(&record("B-RECORD-000001")).unwrap();
    drop((a, b, s));
    let len = media(&p).len() as u64;
    {
        let dir = p.open_dir(Path::new(DIR), false).unwrap();
        let file = p
            .open_file(
                &dir,
                &FileName::new(CONTAINER).unwrap(),
                FileMode::ReadWrite,
            )
            .unwrap();
        p.set_len(&file, len - 16 * BLOCK).unwrap();
        p.flush_all(&file).unwrap();
    }
    let s = Store::open(p.clone(), Path::new(DIR), opts()).unwrap();
    assert!(matches!(
        s.append_region("b"),
        Err(Error::Corruption {
            kind: CorruptionKind::Truncated,
            ..
        })
    ));
    let u = s.unavailable_regions();
    assert_eq!(u.len(), 1);
    assert_eq!(u[0].name, "b");
    assert!(matches!(
        u[0].reason,
        Unavailable::ContainerShort { actual, needed } if actual == len - 16 * BLOCK && needed > actual
    ));
    // Region "a" lies before the cut and is intact.
    let a = s.append_region("a").unwrap();
    let mut buf = vec![0u8; BLOCK as usize];
    a.read(0, &mut buf).unwrap();
    assert_eq!(&buf[..15], b"A-RECORD-000001");
}

/// A truncated region is never brought back by growing the container over
/// it: growth would zero-fill its missing tail, and the next open would find
/// it whole, its lost records read as zeros (found by review). Provisioning
/// and reserving are refused while it exists, and it stays reported.
#[test]
fn test_growth_never_resurrects_a_truncated_region() {
    let p = SimPlatform::new(SimConfig::volatile(63));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let b = s.provision_append_region("b", 1 << 20).unwrap();
    b.append_durable(&record("B-RECORD-000001")).unwrap();
    drop((b, s));
    let len = media(&p).len() as u64;
    {
        let dir = p.open_dir(Path::new(DIR), false).unwrap();
        let file = p
            .open_file(
                &dir,
                &FileName::new(CONTAINER).unwrap(),
                FileMode::ReadWrite,
            )
            .unwrap();
        p.set_len(&file, len - 16 * BLOCK).unwrap();
        p.flush_all(&file).unwrap();
    }
    let s = Store::open(p.clone(), Path::new(DIR), opts()).unwrap();
    let truncated = |r: Result<(), Error>| {
        matches!(
            r,
            Err(Error::Corruption {
                kind: CorruptionKind::Truncated,
                ..
            })
        )
    };
    assert!(truncated(s.provision_append_region("c", 1 << 20).map(drop)));
    assert!(truncated(s.provision_page_region("d", 64 << 10).map(drop)));
    assert!(truncated(s.reserve(1 << 20, 1).map(drop)));
    assert_eq!(
        media(&p).len() as u64,
        len - 16 * BLOCK,
        "the container grew"
    );
    drop(s);
    let s = Store::open(p.clone(), Path::new(DIR), opts()).unwrap();
    assert!(matches!(
        s.append_region("b"),
        Err(Error::Corruption {
            kind: CorruptionKind::Truncated,
            ..
        })
    ));
    assert_eq!(s.unavailable_regions().len(), 1);
}

#[test]
fn test_every_reason_maps_to_a_corruption_kind() {
    assert_eq!(
        Unavailable::ContainerShort {
            needed: 2,
            actual: 1
        }
        .corruption_kind(),
        CorruptionKind::Truncated
    );
    assert_eq!(
        Unavailable::HeaderUnreadable.corruption_kind(),
        CorruptionKind::HeaderCrc
    );
    for r in [
        Unavailable::HeaderInvalid,
        Unavailable::HeaderMismatch,
        Unavailable::HeaderReleased,
    ] {
        assert_eq!(r.corruption_kind(), CorruptionKind::Metadata);
        assert!(!r.to_string().is_empty());
    }
}
