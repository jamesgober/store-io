//! Reproductions of the v0.3.0 hazards found while recording the retroactive
//! decisions (`.dev/DECISIONS-0.3.0.md`, D-057 and D-059). Each test states
//! the behaviour store-io must have; on v0.3.0 it fails.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::path::{Path, PathBuf};

use store_io_buf::{BufPool, PoolConfig};
use store_io_core::error::Error;
use store_io_engine::{CONTAINER, Store, StoreOptions};
use store_io_platform::{CompletionBuf, FileName, IoOp, Platform, Queue, QueueConfig};
use store_io_sim::{CrashMode, FaultPlan, SimConfig, SimPlatform};

const DIR: &str = "/db";

fn opts() -> StoreOptions {
    StoreOptions {
        queues: 2,
        buffers_per_class: 8,
        append_window: 256 << 12,
        // A small table: the store stays inside a crashed create's
        // takeover bound (format 2's is its table area).
        region_capacity: 400,
        ..StoreOptions::default()
    }
}

/// `create` on an existing small store whose volume record cannot be read
/// (here: a latent sector error over the first blocks) must refuse, never
/// truncate it. v0.3.0 takes it over as "abandoned" and truncates it to zero.
#[test]
fn test_create_never_truncates_a_store_whose_volume_record_is_unreadable() {
    let p = SimPlatform::new(SimConfig::volatile(41));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let manifest = s.provision_slot("manifest").unwrap();
    manifest.commit(b"acknowledged manifest").unwrap();
    drop(manifest);
    drop(s);
    let (file, size) = p.with_world(|w| {
        let id = w.file_id(&PathBuf::from(DIR), CONTAINER).unwrap();
        let len = w.media_of(&PathBuf::from(DIR), CONTAINER).unwrap().len() as u64;
        (id, len)
    });
    assert!(
        size <= 12 << 20,
        "store is {size} bytes; the second create's takeover bound must cover it"
    );
    // Both copies of the volume record become unreadable for a while.
    p.with_world(|w| {
        *w.faults_mut() = FaultPlan {
            bad_ranges: vec![(file, 0, 128 << 10)],
            ..FaultPlan::default()
        };
    });
    // A create whose own bound covers the store (a table of 65,536
    // regions, about 12.1 MiB), so the refusal comes from its scan of the
    // unreadable blocks, not from the file's size.
    let large = StoreOptions {
        region_capacity: 65_536,
        ..opts()
    };
    let created = Store::create(p.clone(), Path::new(DIR), large);
    assert!(
        matches!(created, Err(Error::AlreadyExists { .. })),
        "create must refuse an existing store, got {:?}",
        created.as_ref().map(|_| "a new store")
    );
    drop(created);
    // The read errors clear; the store and its acknowledged slot are intact.
    p.with_world(|w| *w.faults_mut() = FaultPlan::default());
    let s = Store::open(p.clone(), Path::new(DIR), opts()).unwrap();
    assert_eq!(
        s.slot("manifest").unwrap().read().as_deref(),
        Some(&b"acknowledged manifest"[..])
    );
}

/// The D-068 rule still lets `create` take over what a crash in the middle of
/// `create` leaves: here a container whose first header was torn (slot magic
/// present, header CRC wrong) and nothing else.
#[test]
fn test_a_create_torn_mid_header_is_still_taken_over() {
    let p = SimPlatform::new(SimConfig::volatile(46));
    let dir = p.open_dir(Path::new(DIR), true).unwrap();
    let file = p
        .create_file(&dir, &FileName::new(CONTAINER).unwrap())
        .unwrap();
    p.allocate(&file, 3 * 4096).unwrap();
    let pool = BufPool::new(&PoolConfig::uniform(4096, 4096, 4096, 2)).unwrap();
    let mut q = p.queue(QueueConfig::default()).unwrap();
    let mut buf = pool.take(4096).unwrap();
    buf.as_mut_slice().fill(0xEE);
    buf.as_mut_slice()[..8].copy_from_slice(&store_io_format::slot::MAGIC);
    q.submit(
        IoOp::Write {
            file: &file,
            offset: 0,
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
    p.sync_dir(&dir).unwrap();
    drop(q);
    let _closed = file;
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    assert!(Store::open(p.clone(), Path::new(DIR), opts()).is_err());
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    s.provision_slot("manifest").unwrap().commit(b"ok").unwrap();
}

/// Offset of the 4 KiB block holding the first copy of `needle` on media.
fn block_of(p: &SimPlatform, needle: &[u8]) -> u64 {
    p.with_world(|w| {
        let media = w.media_of(&PathBuf::from(DIR), CONTAINER).unwrap();
        let at = media
            .windows(needle.len())
            .position(|win| win == needle)
            .expect("needle on media");
        (at as u64) & !4095
    })
}

/// After a commit returns a receipt, the slot reads back the committed
/// version, even when the slot copy just written cannot be read back at that
/// moment. v0.3.0 re-read the pair after the durable write and, seeing the
/// new copy unreadable, kept the previous version as current.
#[test]
fn test_slot_reads_its_own_acknowledged_commit_when_the_new_copy_is_unreadable() {
    let p = SimPlatform::new(SimConfig::volatile(42));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let slot = s.provision_slot("manifest").unwrap();
    slot.commit(b"manifest-version-1").unwrap();
    slot.commit(b"manifest-version-2").unwrap();
    // The third commit overwrites the copy that holds version 1.
    let file = p.with_world(|w| w.file_id(&PathBuf::from(DIR), CONTAINER).unwrap());
    let at = block_of(&p, b"manifest-version-1");
    p.with_world(|w| {
        *w.faults_mut() = FaultPlan {
            bad_ranges: vec![(file, at, at + 4096)],
            ..FaultPlan::default()
        };
    });
    slot.commit(b"manifest-version-3").unwrap();
    assert_eq!(slot.read().as_deref(), Some(&b"manifest-version-3"[..]));
    p.with_world(|w| *w.faults_mut() = FaultPlan::default());
    drop(slot);
    drop(s);
    let s = Store::open(p.clone(), Path::new(DIR), opts()).unwrap();
    assert_eq!(
        s.slot("manifest").unwrap().read().as_deref(),
        Some(&b"manifest-version-3"[..])
    );
}

/// Small-object commits and append barriers running together on one queue
/// never deadlock. v0.3.0 could: a commit held the queue while following a
/// flush whose leader was waiting for that queue.
#[test]
fn test_slot_commits_and_barriers_on_one_queue_never_deadlock() {
    let p = SimPlatform::new(SimConfig::volatile(43));
    let o = StoreOptions {
        queues: 1,
        ..opts()
    };
    let s = Store::create(p.clone(), Path::new(DIR), o).unwrap();
    let slot = s.provision_slot("manifest").unwrap();
    let wal = s.provision_append_region("wal", 4 << 20).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let tx2 = tx.clone();
    let a = std::thread::spawn(move || {
        for i in 0..400u32 {
            if slot.commit(&i.to_le_bytes()).is_err() {
                break;
            }
        }
        tx.send(()).unwrap();
    });
    let b = std::thread::spawn(move || {
        for i in 0..400u32 {
            let Ok(t) = wal.append(&i.to_le_bytes()) else {
                break;
            };
            if wal.sync_through(&t).is_err() {
                break;
            }
        }
        tx2.send(()).unwrap();
    });
    let finished = (0..2).all(|_| rx.recv_timeout(std::time::Duration::from_secs(60)).is_ok());
    if !finished {
        // Unblock the threads so the test process can exit, then fail.
        s.poison();
        let _ = a.join();
        let _ = b.join();
        panic!("slot commits and barriers deadlocked on one queue");
    }
    a.join().unwrap();
    b.join().unwrap();
    assert!(s.domain_stats().flushes > 0);
}

/// A provisioning fill write that fails after submission poisons the store
/// (fail-stop). v0.3.0 returned the error and kept accepting writes.
#[test]
fn test_failed_fill_write_poisons_the_store() {
    for short in [false, true] {
        let p = SimPlatform::new(SimConfig::volatile(44));
        let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
        let next = p.with_world(|w| w.writes()) + 1;
        p.with_world(|w| {
            let f = w.faults_mut();
            if short {
                f.short_write = Some(next);
            } else {
                f.fail_write = Some(next);
            }
        });
        let r = s.provision_append_region("wal", 1 << 20);
        assert!(
            matches!(r, Err(Error::DurabilityUnknown { .. })),
            "short={short}: fill failure must be DurabilityUnknown, got {:?}",
            r.as_ref().err()
        );
        p.with_world(|w| *w.faults_mut() = FaultPlan::default());
        assert!(
            matches!(s.provision_slot("other"), Err(Error::Poisoned { .. })),
            "short={short}: the store must be poisoned after a failed fill write"
        );
    }
}

/// `resume_at` is refused on a read-only store: it would move
/// `durable_through()` without the open-time device flush a writable open
/// performs. v0.3.0 accepted it.
#[test]
fn test_resume_at_is_refused_on_a_read_only_store() {
    let p = SimPlatform::new(SimConfig::volatile(45));
    let s = Store::create(p.clone(), Path::new(DIR), opts()).unwrap();
    let wal = s.provision_append_region("wal", 1 << 20).unwrap();
    wal.append_durable(b"record").unwrap();
    drop(wal);
    drop(s);
    let s = Store::open_readonly(p.clone(), Path::new(DIR), opts()).unwrap();
    let wal = s.append_region("wal").unwrap();
    assert!(matches!(
        wal.resume_at(4096),
        Err(Error::NotWritten {
            cause: store_io_core::error::NotWrittenCause::ReadOnly,
            ..
        })
    ));
    assert_eq!(wal.durable_through(), 0);
}
