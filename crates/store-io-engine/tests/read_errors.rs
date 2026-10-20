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
        append_window: 256 << 12,
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

/// A device that refuses direct reads smaller than 4 KiB (dm-crypt with 4 KiB
/// sectors over a 512e disk) while the probe sees 512-byte blocks. Without a
/// reported direct-I/O alignment the bisection stops at the store block; with
/// a wrong one, a refused sub-block read is reported unreadable instead of
/// aborting the scan. Either way the scan names the bad block and goes on
/// (found by review).
#[test]
fn test_a_device_refusing_small_reads_never_aborts_the_bisection() {
    use store_io_core::class::Missing;
    let model = {
        let p = SimPlatform::new(SimConfig::volatile(73));
        let s = Store::create(p, Path::new(DIR), opts()).unwrap();
        s.report().evidence.clone()
    };
    let mut misaligned = Vec::new();
    for reported in [false, true] {
        let mut ev = model.clone();
        ev.device.logical_block = 512;
        ev.device.physical_block = 512;
        if reported {
            ev.fs.dio_offset_align = 512;
        } else {
            ev.fs.dio_offset_align = 0;
            ev.missing.insert(Missing::DioAlignment);
        }
        // The simulated device itself takes only 4 KiB-aligned transfers.
        let mut cfg = SimConfig::volatile(74);
        cfg.evidence = Some(ev);
        let p = SimPlatform::new(cfg);
        let (s, file, base) = setup(&p);
        let pages = s.page_region("pages").unwrap();
        p.with_world(|w| {
            *w.faults_mut() = FaultPlan {
                bad_ranges: vec![(file, base + 5 * BLOCK + 1024, base + 5 * BLOCK + 1536)],
                ..FaultPlan::default()
            };
        });
        let mut out = vec![0u8; 16 * BLOCK as usize];
        let r = pages.read(pages.pos(0).unwrap(), &mut out);
        let Err(Error::Corruption { kind, ctx }) = r else {
            panic!("reported={reported}: expected Corruption, got {r:?}");
        };
        assert_eq!(kind, CorruptionKind::MediaError);
        let range = ctx.range.unwrap();
        assert_eq!(
            (range.start, range.end),
            (5 * BLOCK, 6 * BLOCK),
            "reported={reported}"
        );
        let mut bad = Vec::new();
        let summary = pages
            .scan(0, |item| {
                if let ScanItem::Unreadable { start, end } = item {
                    bad.push((start, end));
                }
                ControlFlow::Continue(())
            })
            .unwrap();
        assert_eq!(bad, vec![(5 * BLOCK, 6 * BLOCK)], "reported={reported}");
        assert_eq!(summary.end, pages.len(), "the scan stopped early");
        // An odd number of blocks (3) with the bad sector in the last: the
        // halves must stay block-aligned, or the device refuses them.
        let mut three = vec![0u8; 3 * BLOCK as usize];
        let r = pages.read(pages.pos(3 * BLOCK).unwrap(), &mut three);
        let Err(Error::Corruption { kind, ctx }) = r else {
            panic!("reported={reported}: 3 blocks: expected Corruption, got {r:?}");
        };
        assert_eq!(kind, CorruptionKind::MediaError);
        let range = ctx.range.unwrap();
        assert_eq!((range.start, range.end), (5 * BLOCK, 6 * BLOCK), "3 blocks");
        misaligned.push(p.with_world(|w| w.misaligned_reads()));
    }
    // Without a reported alignment the bisection never reads below the store
    // block; with a wrong one it tries, the device refuses, and the
    // backstop absorbs it.
    assert_eq!(
        misaligned[0], 0,
        "split below the block without a reported alignment"
    );
    assert!(misaligned[1] > 0, "the backstop was never exercised");
}

/// A read error that is not a media error (here EINVAL), at any read an open
/// makes, either fails the open with that code or leaves the store whole:
/// it never marks a healthy region unavailable or corrupt for the session.
/// Before 0.4 any failed header read counted as damage (found by review).
#[test]
fn test_a_non_media_error_during_open_never_marks_a_region_damaged() {
    let p = SimPlatform::new(SimConfig::volatile(75));
    drop(setup(&p));
    // Read-only opens first, on the store as created: they write nothing, so
    // the volume record's second copy stays blank and a failed read of the
    // first cannot be rescued by it. Then writable opens.
    for read_only in [true, false] {
        let open = || {
            if read_only {
                Store::open_readonly(p.clone(), Path::new(DIR), opts())
            } else {
                Store::open(p.clone(), Path::new(DIR), opts())
            }
        };
        let before = p.with_world(|w| w.reads());
        drop(open().unwrap());
        let reads = p.with_world(|w| w.reads()) - before;
        assert!(reads > 4, "the open made only {reads} reads");
        let mut failed = 0;
        for k in 1..=reads {
            let next = p.with_world(|w| w.reads()) + k;
            p.with_world(|w| {
                *w.faults_mut() = FaultPlan {
                    fail_read: Some((next, 22)),
                    ..FaultPlan::default()
                };
            });
            match open() {
                Ok(s) => {
                    assert!(
                        s.unavailable_regions().is_empty(),
                        "read_only={read_only}: read {k} marked a region damaged"
                    );
                    s.page_region("pages").unwrap();
                }
                Err(Error::Io { raw, .. }) => {
                    assert_eq!(raw.code, 22, "read_only={read_only}: read {k}");
                    failed += 1;
                }
                Err(e) => panic!("read_only={read_only}: read {k}: {e:?}"),
            }
            p.with_world(|w| *w.faults_mut() = FaultPlan::default());
        }
        assert!(
            failed > 0,
            "read_only={read_only}: no read failure surfaced"
        );
    }
}

/// A media error that does not repeat when the range is read again is still
/// reported (the whole read's range), never hidden as success.
#[test]
fn test_a_media_error_that_does_not_repeat_is_still_reported() {
    let p = SimPlatform::new(SimConfig::volatile(76));
    let (s, _file, _base) = setup(&p);
    let pages = s.page_region("pages").unwrap();
    let next = p.with_world(|w| w.reads()) + 1;
    p.with_world(|w| {
        *w.faults_mut() = FaultPlan {
            fail_read: Some((next, 5)),
            ..FaultPlan::default()
        };
    });
    let mut out = vec![0u8; 4 * BLOCK as usize];
    let r = pages.read(pages.pos(0).unwrap(), &mut out);
    let Err(Error::Corruption { kind, ctx }) = r else {
        panic!("expected Corruption, got {r:?}");
    };
    assert_eq!(kind, CorruptionKind::MediaError);
    let range = ctx.range.unwrap();
    assert_eq!((range.start, range.end), (0, 4 * BLOCK));
}

/// With the smallest buffer pool the store allows, a media error in a read
/// of one largest buffer is still named exactly (the failed read's buffer is
/// returned before the rescan takes its own).
#[test]
fn test_a_one_buffer_pool_still_names_the_exact_unreadable_block() {
    let p = SimPlatform::new(SimConfig::volatile(77));
    let s = Store::create(
        p.clone(),
        Path::new(DIR),
        StoreOptions {
            buffers_per_class: 1,
            ..opts()
        },
    )
    .unwrap();
    // One largest buffer (1 MiB): the size class the rescan takes from too.
    let pages = s.provision_page_region("pages", 2 << 20).unwrap();
    let mut data = vec![7u8; 1 << 20];
    data[..12].copy_from_slice(b"READ-ERR-TST");
    pages.write_durable(pages.pos(0).unwrap(), &data).unwrap();
    let (file, base) = p.with_world(|w| {
        let media = w.media_of(&PathBuf::from(DIR), CONTAINER).unwrap();
        let at = media
            .windows(12)
            .position(|x| x == b"READ-ERR-TST")
            .unwrap() as u64;
        (w.file_id(&PathBuf::from(DIR), CONTAINER).unwrap(), at)
    });
    p.with_world(|w| {
        *w.faults_mut() = FaultPlan {
            bad_ranges: vec![(file, base + 3 * BLOCK, base + 4 * BLOCK)],
            ..FaultPlan::default()
        };
    });
    let mut out = vec![0u8; 1 << 20];
    let r = pages.read(pages.pos(0).unwrap(), &mut out);
    let Err(Error::Corruption { kind, ctx }) = r else {
        panic!("expected Corruption, got {r:?}");
    };
    assert_eq!(kind, CorruptionKind::MediaError);
    let range = ctx.range.unwrap();
    assert_eq!((range.start, range.end), (3 * BLOCK, 4 * BLOCK));
}

/// When the rescan that locates a media error fails itself (here with a
/// non-media error on its first read), the media error is still reported,
/// over the whole read: it is never replaced by the rescan's failure
/// (found by review).
#[test]
fn test_a_failed_rescan_still_reports_the_media_error() {
    let p = SimPlatform::new(SimConfig::volatile(78));
    let (s, file, base) = setup(&p);
    let pages = s.page_region("pages").unwrap();
    let next = p.with_world(|w| w.reads()) + 1;
    p.with_world(|w| {
        *w.faults_mut() = FaultPlan {
            bad_ranges: vec![(file, base + 2 * BLOCK, base + 3 * BLOCK)],
            // The read hits the bad range (read `next`); the rescan's first
            // read fails with EINVAL.
            fail_read: Some((next + 1, 22)),
            ..FaultPlan::default()
        };
    });
    let mut out = vec![0u8; 4 * BLOCK as usize];
    let r = pages.read(pages.pos(0).unwrap(), &mut out);
    let Err(Error::Corruption { kind, ctx }) = r else {
        panic!("expected Corruption, got {r:?}");
    };
    assert_eq!(kind, CorruptionKind::MediaError);
    let range = ctx.range.unwrap();
    assert_eq!((range.start, range.end), (0, 4 * BLOCK));
}
