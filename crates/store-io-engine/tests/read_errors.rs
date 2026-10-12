//! A failed region read is classified accurately: a media error names the
//! exact unreadable range, and any other error keeps its raw code. Before 0.4
//! every failed read was reported as a media error and the code was lost.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use store_io_core::error::{CorruptionKind, Error, Op};
use store_io_engine::{CONTAINER, ScanItem, Store, StoreOptions};
use store_io_sim::{FaultPlan, SimConfig, SimPlatform};

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

/// A store with one page region holding 16 written blocks; returns the
/// file id and the region's data offset in the container.
fn setup(p: &SimPlatform) -> (Store<SimPlatform>, u32, u64) {
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let pages = s.provision_page_region("pages", 1 << 20).unwrap();
    let mut data = vec![0u8; 16 * BLOCK as usize];
    for (i, b) in data.chunks_mut(BLOCK as usize).enumerate() {
        b.fill(i as u8 + 1);
        b[..12].copy_from_slice(b"READ-ERR-TST");
    }
    pages.write_durable(pages.pos(0).unwrap(), &data).unwrap();
    let (file, base) = p.with_world(|w| {
        let media = w.media_of(&PathBuf::from(DIR), CONTAINER).unwrap();
        let at = media
            .windows(12)
            .position(|x| x == b"READ-ERR-TST")
            .unwrap() as u64;
        (w.file_id(&PathBuf::from(DIR), CONTAINER).unwrap(), at)
    });
    (s, file, base)
}

#[test]
fn test_a_non_media_read_error_keeps_its_code_and_is_not_corruption() {
    let p = SimPlatform::new(SimConfig::volatile(70));
    let (s, _file, _base) = setup(&p);
    let pages = s.page_region("pages").unwrap();
    let next = p.with_world(|w| w.reads()) + 1;
    p.with_world(|w| {
        *w.faults_mut() = FaultPlan {
            fail_read: Some((next, 22)),
            ..FaultPlan::default()
        };
    });
    let mut out = vec![0u8; 4 * BLOCK as usize];
    let r = pages.read(pages.pos(0).unwrap(), &mut out);
    let Err(Error::Io { op, raw, ctx }) = r else {
        panic!("expected Io, got {r:?}");
    };
    assert_eq!(op, Op::Read);
    assert_eq!(raw.code, 22);
    assert!(ctx.range.is_some());
}

#[test]
fn test_a_media_error_names_the_exact_unreadable_block() {
    let p = SimPlatform::new(SimConfig::volatile(71));
    let (s, file, base) = setup(&p);
    let pages = s.page_region("pages").unwrap();
    // Block 5 of the region cannot be read.
    p.with_world(|w| {
        *w.faults_mut() = FaultPlan {
            bad_ranges: vec![(file, base + 5 * BLOCK, base + 6 * BLOCK)],
            ..FaultPlan::default()
        };
    });
    let mut out = vec![0u8; 16 * BLOCK as usize];
    let r = pages.read(pages.pos(0).unwrap(), &mut out);
    let Err(Error::Corruption { kind, ctx }) = r else {
        panic!("expected Corruption, got {r:?}");
    };
    assert_eq!(kind, CorruptionKind::MediaError);
    let range = ctx.range.unwrap();
    assert_eq!((range.start, range.end), (5 * BLOCK, 6 * BLOCK));
    // The rest of the region still reads.
    p.with_world(|w| *w.faults_mut() = FaultPlan::default());
    pages.read(pages.pos(0).unwrap(), &mut out).unwrap();
    assert_eq!(out[6 * BLOCK as usize + 12], 7);
}

/// On a 512-byte-sector device, the bisection goes below the 4 KiB store
/// block to the one failing sector: a single read, a read larger than one
/// buffer and a scan all name exactly that sector, and a scan delivers every
/// other byte. Before 0.4 the whole 4 KiB block was reported.
#[test]
fn test_a_media_error_names_the_exact_sector_below_the_store_block() {
    let mut cfg = SimConfig::volatile(72);
    cfg.logical_block = 512;
    let p = SimPlatform::new(cfg);
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let pages = s.provision_page_region("pages", 4 << 20).unwrap();
    let mut data = vec![0u8; 4 << 20];
    data[..12].copy_from_slice(b"READ-ERR-TST");
    for (i, b) in data.iter_mut().enumerate().skip(12) {
        *b = (i % 249) as u8;
    }
    pages.write_durable(pages.pos(0).unwrap(), &data).unwrap();
    let (file, base) = p.with_world(|w| {
        let media = w.media_of(&PathBuf::from(DIR), CONTAINER).unwrap();
        let at = media
            .windows(12)
            .position(|x| x == b"READ-ERR-TST")
            .unwrap() as u64;
        (w.file_id(&PathBuf::from(DIR), CONTAINER).unwrap(), at)
    });
    // The third sector of block 5, and a sector deep in the region.
    let bad = 5 * BLOCK + 1024;
    let deep = (3 << 20) + 512;
    p.with_world(|w| {
        *w.faults_mut() = FaultPlan {
            bad_ranges: vec![
                (file, base + bad, base + bad + 512),
                (file, base + deep, base + deep + 512),
            ],
            ..FaultPlan::default()
        };
    });
    let named = |r: Result<usize, Error>| {
        let Err(Error::Corruption { kind, ctx }) = r else {
            panic!("expected Corruption, got {r:?}");
        };
        assert_eq!(kind, CorruptionKind::MediaError);
        let range = ctx.range.unwrap();
        (range.start, range.end)
    };
    let mut small = vec![0u8; 16 * BLOCK as usize];
    assert_eq!(
        named(pages.read(pages.pos(0).unwrap(), &mut small)),
        (bad, bad + 512)
    );
    // Larger than one pooled buffer: the pipelined path.
    let mut big = vec![0u8; 2 << 20];
    assert_eq!(
        named(pages.read(pages.pos(2 << 20).unwrap(), &mut big)),
        (deep, deep + 512)
    );
    let mut unreadable = Vec::new();
    let mut delivered = 0u64;
    let summary = pages
        .scan(0, |item| {
            match item {
                ScanItem::Data { offset, bytes } => {
                    let at = offset as usize;
                    assert!(bytes == &data[at..at + bytes.len()], "wrong bytes at {at}");
                    delivered += bytes.len() as u64;
                }
                ScanItem::Unreadable { start, end } => unreadable.push((start, end)),
            }
            ControlFlow::Continue(())
        })
        .unwrap();
    assert_eq!(unreadable, vec![(bad, bad + 512), (deep, deep + 512)]);
    assert_eq!(summary.unreadable, 1024);
    assert_eq!(delivered, (4 << 20) - 1024);
}
