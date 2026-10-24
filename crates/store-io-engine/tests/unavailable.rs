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
use store_io_core::error::{CorruptionKind, Error, NotWrittenCause};
use store_io_engine::{CONTAINER, Store, StoreOptions, Unavailable};
use store_io_platform::{CompletionBuf, FileMode, FileName, IoOp, Platform, Queue, QueueConfig};
use store_io_sim::{SimConfig, SimPlatform};

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

/// [`opts`] creating format 1: the tests of format 1's reader (format 1
/// opens read-only from 0.5; its own media-error and newer-record rules
/// still hold).
fn opts1() -> StoreOptions {
    StoreOptions {
        container_format: store_io_format::slot::FORMAT_1,
        ..opts()
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
/// it whole, its lost records read as zeros (found by review). Format 2:
/// anything that would grow the container is refused while it exists (a
/// provisioning that fits inside, writing nothing past the end, proceeds),
/// and it stays reported.
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
    assert!(truncated(s.reserve(1 << 20, 1).map(drop)));
    // Inside the container: placed in its free space, the truncated span
    // untouched.
    let d = s.provision_page_region("d", 16 << 10).unwrap();
    d.write_durable(d.pos(0).unwrap(), &[7; BLOCK as usize])
        .unwrap();
    drop(d);
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
    let d = s.page_region("d").unwrap();
    let mut back = vec![0u8; BLOCK as usize];
    d.read(d.pos(0).unwrap(), &mut back).unwrap();
    assert!(back == [7; BLOCK as usize]);
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

/// Writes one 4 KiB block of `data` at `at` of the container, durably.
fn put(p: &SimPlatform, at: u64, data: &[u8]) {
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
    buf.as_mut_slice().copy_from_slice(data);
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

/// A media error where the volume record lives, on a store just created
/// (its second copy still blank), is reported as a media error: never as
/// `HeaderCrc` corruption, which would invite a caller to recreate the store
/// over its data (found by review).
#[test]
fn test_a_media_error_at_the_volume_record_is_reported_as_one() {
    let p = SimPlatform::new(SimConfig::volatile(64));
    drop(Store::create(p.clone(), Path::new(DIR), opts1()).unwrap());
    let file = p.with_world(|w| w.file_id(&PathBuf::from(DIR), CONTAINER).unwrap());
    p.with_world(|w| w.faults_mut().bad_ranges = vec![(file, 0, BLOCK)]);
    for read_only in [true, false] {
        let r = if read_only {
            Store::open_readonly(p.clone(), Path::new(DIR), opts1())
        } else {
            Store::open(p.clone(), Path::new(DIR), opts1())
        };
        assert!(
            matches!(
                r,
                Err(Error::Corruption {
                    kind: CorruptionKind::MediaError,
                    ..
                })
            ),
            "read_only={read_only}: {:?}",
            r.err()
        );
    }
}

/// A volume record from a newer format (a longer record, or a feature bit
/// this version does not know) is refused as unsupported, never read as
/// corruption and never rewritten (found by review).
#[test]
fn test_a_newer_volume_record_is_refused_as_unsupported() {
    use store_io_core::error::Capability;
    use store_io_format::meta::VolumeMeta;
    use store_io_format::slot::{Expect, SlotFields, SlotIndex, SlotStatus, encode, validate};
    for newer in ["features", "longer", "features-and-reserved"] {
        let p = SimPlatform::new(SimConfig::volatile(65));
        drop(Store::create(p.clone(), Path::new(DIR), opts1()).unwrap());
        // Slot A holds the volume record as created; write a newer
        // generation into the blank slot B.
        let a = media(&p)[..BLOCK as usize].to_vec();
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&a[32..48]);
        let SlotStatus::Valid(info) = validate(
            &a,
            &Expect {
                volume_uuid: uuid,
                object_id: 0,
                slot_offset: 0,
                slot_index: SlotIndex::A,
                log2_slot_size: 12,
                format_major: store_io_format::slot::FORMAT_1,
                numbered: false,
            },
        ) else {
            panic!("slot A is not a valid volume record");
        };
        let mut vm = VolumeMeta::decode(info.payload(&a)).unwrap();
        let mut payload = vec![0u8; 48];
        if newer.starts_with("features") {
            vm.features = 1;
        }
        let _n = vm.encode(&mut payload).unwrap();
        if newer == "features-and-reserved" {
            // A newer format may also reuse a reserved field: still newer.
            // Byte 4 is the reserved word the decoder checked first before.
            payload[4] = 1;
            payload[40] = 1;
        }
        if newer == "longer" {
            payload.extend_from_slice(&[0u8; 8]);
        }
        let mut b = vec![0u8; BLOCK as usize];
        let _crc = encode(
            &mut b,
            &SlotFields {
                volume_uuid: uuid,
                object_id: 0,
                generation: info.generation + 1,
                owner_generation: info.owner_generation,
                slot_offset: BLOCK,
                slot_index: SlotIndex::B,
                log2_slot_size: 12,
                prev_header_crc32c: info.header_crc32c,
                format_major: store_io_format::slot::FORMAT_1,
                meta_seq: 0,
            },
            &payload,
        )
        .unwrap();
        put(&p, BLOCK, &b);
        let before = media(&p);
        for read_only in [true, false] {
            let r = if read_only {
                Store::open_readonly(p.clone(), Path::new(DIR), opts1())
            } else {
                Store::open(p.clone(), Path::new(DIR), opts1())
            };
            assert!(
                matches!(
                    r,
                    Err(Error::Unsupported {
                        what: Capability::FormatVersion
                    })
                ),
                "{newer}, read_only={read_only}: {:?}",
                r.err()
            );
        }
        assert!(
            media(&p) == before,
            "{newer}: the refused store was modified"
        );
    }
}

/// Format 2: both copies of the volume record unreadable is a media error,
/// never `HeaderCrc` corruption (which would invite recreating the store
/// over its data), for both kinds of open, and nothing is written. (One
/// unreadable copy opens from the other, the record taken as stale: the
/// store is judged not closed cleanly.)
#[test]
fn test_a_format_2_volume_record_unreadable_is_a_media_error() {
    let p = SimPlatform::new(SimConfig::volatile(68));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    s.provision_page_region("r", 16 << 10).unwrap();
    drop(s);
    let file = p.with_world(|w| w.file_id(&PathBuf::from(DIR), CONTAINER).unwrap());
    let before = media(&p);
    let writes = p.with_world(|w| w.writes());
    p.with_world(|w| w.faults_mut().bad_ranges = vec![(file, 0, 2 * BLOCK)]);
    for read_only in [true, false] {
        let r = if read_only {
            Store::open_readonly(p.clone(), Path::new(DIR), opts())
        } else {
            Store::open(p.clone(), Path::new(DIR), opts())
        };
        assert!(
            matches!(
                r,
                Err(Error::Corruption {
                    kind: CorruptionKind::MediaError,
                    ..
                })
            ),
            "read_only={read_only}: {:?}",
            r.err()
        );
    }
    assert_eq!(p.with_world(|w| w.writes()), writes);
    assert!(media(&p) == before);
    p.with_world(|w| w.faults_mut().bad_ranges = vec![(file, 0, BLOCK)]);
    let s = Store::open_readonly(p.clone(), Path::new(DIR), opts()).unwrap();
    assert!(
        !s.report().metadata.clean_close,
        "one copy unreadable: stale"
    );
    assert_eq!(s.regions().len(), 1);
}

/// Format 2: a table page whose two copies are unreadable is lost: the
/// store opens with allocation frozen and the page reported, its regions
/// unknown (never read as absent and their space reused).
#[test]
fn test_a_format_2_table_page_unreadable_is_lost_and_freezes() {
    let p = SimPlatform::new(SimConfig::volatile(69));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    s.provision_page_region("r", 16 << 10).unwrap();
    drop(s);
    let file = p.with_world(|w| w.file_id(&PathBuf::from(DIR), CONTAINER).unwrap());
    // Page 0's pair, after the volume pair: two 16 KiB table slots.
    p.with_world(|w| {
        w.faults_mut().bad_ranges = vec![(file, 2 * BLOCK, 2 * BLOCK + 2 * (16 << 10))];
    });
    for read_only in [true, false] {
        let s = if read_only {
            Store::open_readonly(p.clone(), Path::new(DIR), opts())
        } else {
            Store::open(p.clone(), Path::new(DIR), opts())
        }
        .unwrap();
        let m = &s.report().metadata;
        assert_eq!(m.lost_pages, [0], "read_only={read_only}");
        assert!(m.frozen, "read_only={read_only}");
        assert!(s.page_region("r").is_err(), "read_only={read_only}");
        let e = s
            .provision_page_region("new", 16 << 10)
            .map(drop)
            .unwrap_err();
        // Read-only refuses first; a writable store because it is frozen.
        let want = if read_only {
            NotWrittenCause::ReadOnly
        } else {
            NotWrittenCause::Frozen
        };
        assert!(
            matches!(e, Error::NotWritten { cause, .. } if cause == want),
            "read_only={read_only}: {e:?}"
        );
    }
}

/// With 16 KiB blocks, a media error inside the volume record's first slot
/// where none of the search's 4 KiB probes reads (bytes 12-16 KiB) is also
/// reported as a media error rather than `HeaderCrc`: the pair read counts
/// it (found by review: only the probes were counted).
#[test]
fn test_a_media_error_inside_the_volume_slot_is_reported_as_one() {
    let p = SimPlatform::new(SimConfig::volatile(66));
    let big = StoreOptions {
        block_size: Some(16384),
        ..opts1()
    };
    drop(Store::create(p.clone(), Path::new(DIR), big.clone()).unwrap());
    let file = p.with_world(|w| w.file_id(&PathBuf::from(DIR), CONTAINER).unwrap());
    p.with_world(|w| w.faults_mut().bad_ranges = vec![(file, 12288, 16384)]);
    let r = Store::open_readonly(p.clone(), Path::new(DIR), big);
    assert!(
        matches!(
            r,
            Err(Error::Corruption {
                kind: CorruptionKind::MediaError,
                ..
            })
        ),
        "{:?}",
        r.err()
    );
}

/// A media error in the region table's first slot, on a store just created
/// (its second slot still blank), is reported as a media error, not
/// `HeaderCrc` corruption (found by review).
#[test]
fn test_a_media_error_in_the_region_table_is_reported_as_one() {
    let p = SimPlatform::new(SimConfig::volatile(67));
    drop(Store::create(p.clone(), Path::new(DIR), opts1()).unwrap());
    // Default layout: the volume pair at 0 and B, the table pair from 2B.
    let file = p.with_world(|w| w.file_id(&PathBuf::from(DIR), CONTAINER).unwrap());
    p.with_world(|w| w.faults_mut().bad_ranges = vec![(file, 2 * BLOCK, 3 * BLOCK)]);
    let r = Store::open_readonly(p.clone(), Path::new(DIR), opts1());
    assert!(
        matches!(
            r,
            Err(Error::Corruption {
                kind: CorruptionKind::MediaError,
                ..
            })
        ),
        "{:?}",
        r.err()
    );
}
