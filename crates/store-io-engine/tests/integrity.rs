//! Metadata integrity: the table's capacity, names that a damaged region
//! still owns, keyed fill, release keeping its header, and creates that never
//! finished.

// Test setup unwraps (a failure is the test failing) and ignores values a
// given test does not inspect.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use store_io_buf::{BufPool, PoolConfig};
use store_io_core::error::{Error, Named, ReserveTag};
use store_io_engine::{CONTAINER, ScanItem, Store, StoreOptions};
use store_io_platform::{CompletionBuf, FileMode, FileName, IoOp, Platform, Queue, QueueConfig};
use store_io_sim::{CrashMode, SimConfig, SimPlatform};

const DIR: &str = "/db";
const BLOCK: u64 = 4096;

fn opts() -> StoreOptions {
    StoreOptions {
        queues: 2,
        buffers_per_class: 8,
        max_io: 1 << 17,
        ..StoreOptions::default()
    }
}

fn media(p: &SimPlatform) -> Vec<u8> {
    p.with_world(|w| w.media_of(&PathBuf::from(DIR), CONTAINER).unwrap().to_vec())
}

/// Writes `bytes` (whole blocks) at a container offset through the platform,
/// bypassing the store, and makes them durable.
fn overwrite(p: &SimPlatform, offset: u64, bytes: &[u8]) {
    let dir = p.open_dir(Path::new(DIR), false).unwrap();
    let file = p
        .open_file(
            &dir,
            &FileName::new(CONTAINER).unwrap(),
            FileMode::ReadWrite,
        )
        .unwrap();
    let pool = BufPool::new(&PoolConfig::uniform(4096, 4096, 1 << 16, 4)).unwrap();
    let mut q = p.queue(QueueConfig { depth: 4 }).unwrap();
    let mut buf = pool.take(bytes.len()).unwrap();
    buf.as_mut_slice().copy_from_slice(bytes);
    q.submit(
        IoOp::Write {
            file: &file,
            offset,
            buf,
            dsync: false,
        },
        1,
    )
    .map_err(|_| ())
    .unwrap();
    let mut out = CompletionBuf::with_capacity(4);
    q.wait(1, &mut out).unwrap();
    assert!(out.drain().all(|c| c.result.is_ok()));
    p.flush_all(&file).unwrap();
}

#[test]
fn test_a_full_table_refuses_before_any_io_and_poisons_nothing() {
    let p = SimPlatform::new(SimConfig::volatile(71));
    // The smallest table: 4 KiB less its headers holds 61 entries.
    let s = Store::create(
        p.clone(),
        Path::new(DIR),
        StoreOptions {
            log2_table_slot: 12,
            ..opts()
        },
    )
    .unwrap();
    let capacity = s.report().table_capacity;
    for i in 0..capacity {
        s.provision_page_region(&format!("p{i}"), BLOCK).unwrap();
    }
    let full = |r: Result<_, Error>| {
        matches!(
            r,
            Err(Error::NoSpace {
                tag: ReserveTag::Table,
                ..
            })
        )
    };
    assert!(full(s.provision_page_region("one-more", BLOCK).map(|_| ())));
    // A released entry stays in the table: another size still does not fit,
    // and the refusal comes before any write, so nothing is poisoned.
    s.page_region("p0").unwrap().release().unwrap();
    let writes = p.with_world(|w| w.writes());
    assert!(full(
        s.provision_page_region("bigger", 2 * BLOCK).map(|_| ())
    ));
    assert_eq!(p.with_world(|w| w.writes()), writes, "refused after I/O");
    let pages = s.page_region("p1").unwrap();
    pages
        .write_durable(pages.pos(0).unwrap(), &[1; BLOCK as usize])
        .unwrap();
    // The same size reuses the released entry in place.
    s.provision_page_region("same-size", BLOCK).unwrap();
}

#[test]
fn test_a_region_skipped_at_open_keeps_its_name() {
    let p = SimPlatform::new(SimConfig::volatile(72));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    let marker = [0xA5u8; 4096];
    wal.append_durable(&marker).unwrap();
    s.provision_page_region("other", 1 << 20).unwrap();
    drop((wal, s));
    // Destroy both header blocks of "wal" (they sit just before its data).
    let data = media(&p).windows(4096).position(|w| w == marker).unwrap() as u64;
    overwrite(&p, data - 2 * BLOCK, &[0u8; 2 * BLOCK as usize]);
    let s = Store::open(p.clone(), Path::new(DIR), opts()).unwrap();
    assert!(matches!(
        s.append_region("wal"),
        Err(Error::NotFound { .. })
    ));
    // The table still names it: the name cannot be reused, so the table
    // stays decodable and the store keeps opening.
    assert!(matches!(
        s.provision_append_region("wal", 1 << 20),
        Err(Error::AlreadyExists {
            what: Named::Region
        })
    ));
    drop(s);
    Store::open(p.clone(), Path::new(DIR), opts()).unwrap();
}

#[test]
fn test_keyed_fill_covers_the_whole_data_area() {
    let p = SimPlatform::new(SimConfig::volatile(73));
    let s = Store::create(
        p,
        Path::new(DIR),
        StoreOptions {
            keyed_fill: true,
            ..opts()
        },
    )
    .unwrap();
    let pages = s.provision_page_region("pages", 3 << 20).unwrap();
    let mut zero_blocks = 0;
    pages
        .scan(0, |item| {
            if let ScanItem::Data { bytes, .. } = item {
                zero_blocks += bytes
                    .chunks(BLOCK as usize)
                    .filter(|b| b.iter().all(|&x| x == 0))
                    .count();
            }
            ControlFlow::Continue(())
        })
        .unwrap();
    assert_eq!(zero_blocks, 0, "unfilled blocks in a keyed region");
}

#[test]
fn test_release_keeps_the_header_that_says_released() {
    let p = SimPlatform::new(SimConfig::volatile(74));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    let marker = [0x5Au8; 4096];
    wal.append_durable(&marker).unwrap();
    let data = media(&p).windows(4096).position(|w| w == marker).unwrap();
    wal.release().unwrap();
    // What a reader sees now (the released range is not flushed yet).
    let m = p.with_world(|w| {
        let id = w.file_id(&PathBuf::from(DIR), CONTAINER).unwrap();
        w.visible(id).unwrap().to_vec()
    });
    let header = &m[data - 2 * BLOCK as usize..data];
    let slots_with_magic = header
        .chunks(BLOCK as usize)
        .filter(|b| b[..8] == store_io_format::slot::MAGIC)
        .count();
    assert!(
        slots_with_magic >= 1,
        "release deallocated the region header"
    );
}

#[test]
fn test_a_create_that_never_finished_is_taken_over() {
    let p = SimPlatform::new(SimConfig::volatile(75));
    // What a crash in the middle of create leaves: the container exists,
    // durable, with no metadata.
    let dir = p.open_dir(Path::new(DIR), true).unwrap();
    let file = p
        .create_file(&dir, &FileName::new(CONTAINER).unwrap())
        .unwrap();
    p.allocate(&file, 3 * BLOCK).unwrap();
    p.flush_all(&file).unwrap();
    p.sync_dir(&dir).unwrap();
    let _closed = file;
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    assert!(Store::open(p.clone(), Path::new(DIR), opts()).is_err());
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    s.provision_slot("manifest").unwrap().commit(b"ok").unwrap();
    drop(s);
    // A real store is never taken over.
    assert!(matches!(
        Store::create(p.clone(), Path::new(DIR), opts()),
        Err(Error::AlreadyExists { .. })
    ));
    let s = Store::open(p, Path::new(DIR), opts()).unwrap();
    assert_eq!(s.slot("manifest").unwrap().read().unwrap(), b"ok");
}
