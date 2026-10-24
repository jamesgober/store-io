//! Container format 2 in the engine, create (E2a,
//! `.dev/design/format2-draft.md`): the pages are durable before the volume
//! record, and a crashed format-2 create, and only that, is taken over by
//! the next create, after a power failure at any step.

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
use store_io_engine::{CONTAINER, Store, StoreOptions};
use store_io_platform::{FileMode, FileName, Platform};
use store_io_sim::{CrashMode, SimConfig, SimPlatform};

mod format2_util;
use format2_util::*;

#[test]
fn test_create_writes_every_page_before_the_volume_record() {
    let p = SimPlatform::new(SimConfig::volatile(71));
    drop(create(&p));
    let m = media(&p);
    // Pages carry meta_seq 1..P in order; the volume record's first copy P + 1.
    for pg in 0..PAGES {
        let a = page_at(&p, pg, 0) as usize;
        assert_eq!(u64_at(&m, a + 96), u64::from(pg) + 1);
        assert_eq!(u64_at(&m, a + 48), (1 << 61) | u64::from(pg));
    }
    assert_eq!(u64_at(&m, 96), u64::from(PAGES) + 1);
    // The clean close wrote the volume record's next copy, numbered past it.
    let b = block(&p) as usize;
    assert_eq!(u64_at(&m, b + 96), u64::from(PAGES) + 2);
}

#[test]
fn test_a_crashed_create_is_taken_over() {
    for torn in [false, true] {
        let p = SimPlatform::new(SimConfig::volatile(79));
        crash_open(&p, create(&p));
        // The crash came before the volume record was durable: none, or torn.
        if torn {
            tear(&p, 0);
        } else {
            put(&p, 0, &vec![0u8; CHUNK]);
        }
        let s = create(&p);
        assert_eq!(s.report().owner_generation, 1);
        drop(s);
        assert!(open(&p).report().metadata.clean_close);
    }
}

#[test]
fn test_format_1_create_does_not_take_over_format_2_pages() {
    let p = SimPlatform::new(SimConfig::volatile(80));
    crash_open(&p, create(&p));
    put(&p, 0, &vec![0u8; CHUNK]);
    let v1 = StoreOptions {
        container_format: store_io_format::slot::FORMAT_1,
        ..opts()
    };
    let r = Store::create(p.clone(), Path::new(DIR), v1);
    assert!(
        matches!(r, Err(Error::AlreadyExists { .. })),
        "{:?}",
        r.map(|_| ())
    );
}

/// A file past format 1's bound whose only verifying block is a crashed
/// create's leftover (an empty page, a torn volume record) but not where
/// any create writes one (in a damaged store's data, say), is never taken
/// over; the same image where a create writes it is. The images say they
/// are where they are (their slot offsets).
#[test]
fn test_a_leftover_image_out_of_place_proves_no_crashed_create() {
    // (offset, object, torn, generation, the slot offset it states, slot
    // B, taken): page 0 at 512 KiB, at 64 KiB (page 0's place at 32 KiB
    // blocks: 2B = 64 KiB) and at its 4 KiB-block place; at that place but
    // of generation 2 (a store's), stating another offset, or written as
    // slot B (no create writes those); a torn volume record at 512 KiB and
    // at 0.
    let p0 = page_at_of(0, 0);
    for (at, object, torn, generation, stated, slot_b, taken) in [
        (512 << 10, 1 << 61, false, 1, 512 << 10, false, false),
        (64 << 10, 1 << 61, false, 1, 64 << 10, false, true),
        (p0, 1 << 61, false, 1, p0, false, true),
        (p0, 1 << 61, false, 2, p0, false, false),
        (p0, 1 << 61, false, 1, 512 << 10, false, false),
        (p0, 1 << 61, false, 1, p0, true, false),
        (512 << 10, 0, true, 1, 512 << 10, false, false),
        (0, 0, true, 1, 0, false, true),
    ] {
        let p = SimPlatform::new(SimConfig::volatile(88));
        let dir = p.open_dir(Path::new(DIR), true).unwrap();
        let f = p
            .create_file(&dir, &FileName::new(CONTAINER).unwrap())
            .unwrap();
        p.set_len(&f, 1 << 20).unwrap();
        p.flush_all(&f).unwrap();
        p.sync_dir(&dir).unwrap();
        let mut slot = vec![0u8; TABLE as usize];
        store_io_format::slot::encode(
            &mut slot,
            &store_io_format::slot::SlotFields {
                volume_uuid: [7; 16],
                object_id: object,
                generation,
                owner_generation: 1,
                slot_offset: stated,
                slot_index: if slot_b {
                    store_io_format::slot::SlotIndex::B
                } else {
                    store_io_format::slot::SlotIndex::A
                },
                log2_slot_size: LOG2_TABLE,
                prev_header_crc32c: 0,
                format_major: store_io_format::slot::FORMAT_2,
                meta_seq: 1,
            },
            &if object == 0 {
                vec![1u8; store_io_format::meta2::VOLUME_META2_LEN]
            } else {
                empty_page(0)
            },
        )
        .unwrap();
        if torn {
            slot[128] ^= 0xff;
        }
        put(&p, at, &slot);
        let before = media(&p);
        let r = Store::create(p.clone(), Path::new(DIR), opts_large());
        if taken {
            assert!(r.is_ok(), "at {at} gen {generation}: {:?}", r.map(|_| ()));
        } else {
            assert!(
                matches!(r, Err(Error::AlreadyExists { .. })),
                "at {at} gen {generation} stated {stated} B {slot_b}: {:?}",
                r.map(|_| ())
            );
            assert!(media(&p) == before, "at {at}: the file changed");
        }
    }
}

/// Page `p`'s slot `s` at 4 KiB blocks.
fn page_at_of(p: u64, s: u64) -> u64 {
    2 * 4096 + 2 * p * TABLE + s * TABLE
}

#[test]
fn test_create_over_a_format_2_store_fails() {
    let p = SimPlatform::new(SimConfig::volatile(81));
    drop(create(&p));
    let r = Store::create(p.clone(), Path::new(DIR), opts());
    assert!(
        matches!(r, Err(Error::AlreadyExists { .. })),
        "{:?}",
        r.map(|_| ())
    );
}

#[test]
fn test_create_makes_the_pages_durable_before_writing_the_volume_record() {
    let p = SimPlatform::new(SimConfig::volatile(87));
    // At each submit: writes not yet durable.
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (q, s2) = (p.clone(), seen.clone());
    p.with_world(|w| {
        w.set_submit_probe(Some(std::sync::Arc::new(move || {
            let pending = q.with_world(|w| w.cached_writes());
            s2.lock().unwrap().push(pending);
        })));
    });
    let s = create(&p);
    p.with_world(|w| w.set_submit_probe(None));
    // The last write of create is the volume record's.
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), PAGES as usize + 1, "{seen:?}");
    // Page 0 was durable before the file grew to the whole table (between
    // its write and page 1's), and every page before the volume record's.
    assert_eq!(seen[1], 0, "{seen:?}");
    assert_eq!(seen[PAGES as usize], 0, "{seen:?}");
    drop(s);
}

#[test]
fn test_a_store_left_open_is_not_taken_over() {
    let p = SimPlatform::new(SimConfig::volatile(88));
    crash_open(&p, create(&p));
    // Its volume record (generation 1) is valid: this is a store.
    let r = Store::create(p.clone(), Path::new(DIR), opts());
    assert!(
        matches!(r, Err(Error::AlreadyExists { .. })),
        "{:?}",
        r.map(|_| ())
    );
}

#[test]
fn test_a_crashed_create_with_a_torn_page_is_taken_over() {
    let p = SimPlatform::new(SimConfig::volatile(89));
    crash_open(&p, create(&p));
    put(&p, 0, &vec![0u8; CHUNK]);
    tear(&p, page_at(&p, 2, 0));
    drop(create(&p));
    assert!(open(&p).report().metadata.clean_close);
}

#[test]
fn test_rewritten_pages_are_not_a_crashed_create() {
    let p = SimPlatform::new(SimConfig::volatile(90));
    crash_open(&p, create(&p));
    put(&p, page_at(&p, 1, 1), &page_copy_b(&p, 1, false));
    put(&p, 0, &vec![0u8; CHUNK]);
    let r = Store::create(p.clone(), Path::new(DIR), opts());
    assert!(
        matches!(r, Err(Error::AlreadyExists { .. })),
        "{:?}",
        r.map(|_| ())
    );
}

#[test]
fn test_a_file_longer_than_the_metadata_is_not_a_crashed_create() {
    let p = SimPlatform::new(SimConfig::volatile(91));
    crash_open(&p, create(&p));
    put(&p, 0, &vec![0u8; CHUNK]);
    let end = page_at(&p, PAGES, 0);
    put(&p, end, &vec![0u8; CHUNK]);
    let r = Store::create(p.clone(), Path::new(DIR), opts());
    assert!(
        matches!(r, Err(Error::AlreadyExists { .. })),
        "{:?}",
        r.map(|_| ())
    );
}

#[test]
fn test_a_create_failing_at_any_step_leaves_a_store_or_room_for_one() {
    // Each step of a create fails in turn (every write, every flush, the
    // directory flush), then power fails, keeping, dropping or tearing what
    // was cached; the next create succeeds, or what is there is a store
    // that opens. (A failed create removes its file: the takeover itself is
    // the next test's.)
    let steps = (1..=u64::from(PAGES) + 1)
        .map(|k| (Some(k), None, None))
        .chain((1..=3).map(|k| (None, Some(k), None)))
        .chain([(None, None, Some(1))]);
    for (write, flush, dir) in steps {
        for seed in 0..6 {
            let p = SimPlatform::new(SimConfig::volatile(100 + seed));
            p.with_world(|w| {
                let (wr, fl) = (w.writes(), w.flushes());
                let f = w.faults_mut();
                f.fail_write = write.map(|k| wr + k);
                f.fail_flush = flush.map(|k| fl + k);
                f.fail_dir_sync = dir;
            });
            let first = Store::create(p.clone(), Path::new(DIR), opts());
            match first {
                Ok(s) => crash_open(&p, s),
                Err(_) => p.with_world(|w| w.crash(&CrashMode::Random)),
            }
            p.with_world(|w| *w.faults_mut() = Default::default());
            let step = format!("{write:?} {flush:?} {dir:?} seed {seed}");
            match Store::create(p.clone(), Path::new(DIR), opts()) {
                Ok(s) => drop(s),
                Err(Error::AlreadyExists { .. }) => {}
                Err(e) => panic!("{step}: {e:?}"),
            }
            let s = Store::open(p.clone(), Path::new(DIR), opts());
            assert!(s.is_ok(), "{step}: {:?}", s.map(|_| ()));
        }
    }
}

/// Power fails inside a create, at its `k`-th write submission (its file's
/// name already durable), keeping, dropping or tearing what was cached.
/// With `settled`, everything issued so far (the file's size included)
/// reached the media first, as a file system's journal may make it.
fn power_fails_in_create(p: &SimPlatform, opts: &StoreOptions, k: u64, settled: bool) {
    let n = Arc::new(AtomicU64::new(0));
    let q = p.clone();
    p.with_world(|w| {
        w.set_submit_probe(Some(Arc::new(move || {
            if n.fetch_add(1, Ordering::SeqCst) + 1 == k {
                let dir = q.open_dir(Path::new(DIR), false).unwrap();
                q.sync_dir(&dir).unwrap();
                if settled {
                    let name = FileName::new(store_io_engine::CONTAINER).unwrap();
                    let file = q.open_file(&dir, &name, FileMode::ReadWrite).unwrap();
                    q.flush_all(&file).unwrap();
                }
                panic!("power fails");
            }
        })));
    });
    let o = opts.clone();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Store::create(p.clone(), Path::new(DIR), o)
    }));
    assert!(r.is_err(), "the create finished before write {k}");
    p.with_world(|w| {
        w.set_submit_probe(None);
        w.crash(&CrashMode::Random);
    });
}

#[test]
fn test_a_power_failure_inside_a_create_is_taken_over() {
    // Up to and including the volume record's write: no store exists yet,
    // so the next create must take over whatever is left.
    // 64 KiB blocks: the table's slots (and so a leftover's place) follow
    // the block size.
    let big = || StoreOptions {
        block_size: Some(65536),
        ..opts()
    };
    let cases = (1..=u64::from(PAGES) + 1)
        .flat_map(|k| (0..6).map(move |seed| (opts(), k, seed)))
        .chain((1..=3).flat_map(|k| (0..3).map(move |seed| (opts_large(), k, seed))))
        .chain((1..=2).flat_map(move |k| (0..3).map(move |seed| (big(), k, seed))));
    for ((o, k, seed), settled) in cases.flat_map(|c| [(c.clone(), false), (c, true)]) {
        let p = SimPlatform::new(SimConfig::volatile(300 + seed));
        power_fails_in_create(&p, &o, k, settled);
        let s = Store::create(p.clone(), Path::new(DIR), o.clone());
        assert!(
            s.is_ok(),
            "write {k} seed {seed} {settled}: {:?}",
            s.map(|_| ())
        );
        drop(s);
        let s = Store::open(p.clone(), Path::new(DIR), o.clone()).unwrap();
        assert!(s.report().metadata.clean_close);
    }
}

#[test]
fn test_a_large_file_with_nothing_recognisable_is_not_taken_over() {
    // Past format 1's size bound, a file whose headers all fail may be a
    // damaged store of either format: only a format-2 create's own remains
    // justify a takeover.
    let p = SimPlatform::new(SimConfig::volatile(113));
    let o = opts_large();
    crash_open(
        &p,
        Store::create(p.clone(), Path::new(DIR), o.clone()).unwrap(),
    );
    let dir = p.open_dir(Path::new(DIR), false).unwrap();
    let file = p
        .open_file(
            &dir,
            &FileName::new(store_io_engine::CONTAINER).unwrap(),
            FileMode::ReadWrite,
        )
        .unwrap();
    let len = p.size(&file).unwrap();
    assert!(len > 4 << 16);
    p.set_len(&file, 0).unwrap();
    p.set_len(&file, len).unwrap();
    p.flush_all(&file).unwrap();
    let r = Store::create(p.clone(), Path::new(DIR), o);
    assert!(
        matches!(r, Err(Error::AlreadyExists { .. })),
        "{:?}",
        r.map(|_| ())
    );
}

#[test]
fn test_a_crashed_create_with_small_table_pages_is_taken_over() {
    // At 4 KiB table pages a small block size gives the largest table area
    // (more pages, each with its header): the size bound must cover it.
    let o = StoreOptions {
        log2_table_slot: 12,
        ..opts_large()
    };
    let p = SimPlatform::new(SimConfig::volatile(116));
    power_fails_in_create(&p, &o, 2, true);
    // Larger than the table area at 64 KiB blocks: only the bound over every
    // block size admits it.
    let len = p.with_world(|w| {
        w.media_of(&std::path::PathBuf::from(DIR), store_io_engine::CONTAINER)
            .unwrap()
            .len()
    });
    assert!(len > 12_845_056, "{len}");
    let s = Store::create(p.clone(), Path::new(DIR), o.clone());
    assert!(s.is_ok(), "{:?}", s.map(|_| ()));
    drop(s);
    assert!(
        Store::open(p.clone(), Path::new(DIR), o)
            .unwrap()
            .report()
            .metadata
            .clean_close
    );
}
