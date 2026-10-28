//! Container format 2 in the engine, the open (E2a,
//! `.dev/design/format2-draft.md`): it judges every metadata pair, heals
//! the one torn write, reports stale and lost pages, refuses a newer
//! format, retries a failed read, and a clean close is recorded. Regions in
//! format 2 arrive with E3. Create is in `format2_create.rs`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::path::Path;

use store_io_core::error::{Capability, Error};
use store_io_engine::{CONTAINER, Healed, Store, StoreOptions};
use store_io_format::crc32c::crc32c;
use store_io_format::slot::{FORMAT_2, SlotFields, SlotIndex, encode};
use store_io_platform::{FileMode, FileName, Platform};
use store_io_sim::{CrashMode, FaultPlan, SimConfig, SimPlatform};

mod format2_util;
use format2_util::*;

#[test]
fn test_create_then_reopen_bumps_the_owner_and_records_a_clean_close() {
    let p = SimPlatform::new(SimConfig::volatile(70));
    let s = create(&p);
    let r = s.report().clone();
    assert_eq!(r.format, FORMAT_2);
    assert_eq!(r.block_size, 4096);
    assert_eq!(r.owner_generation, 1);
    assert_eq!(r.table_capacity, 3 * 169);
    assert!(!r.read_only);
    drop(s);
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    let s = open(&p);
    let r = s.report().clone();
    assert_eq!(r.format, FORMAT_2);
    assert_eq!(r.owner_generation, 2);
    assert!(r.metadata.clean_close, "{:?}", r.metadata);
    assert!(!r.metadata.frozen);
    assert_eq!(r.metadata.healed, None);
    drop(s);
    let s = open(&p);
    assert_eq!(s.report().owner_generation, 3);
    assert!(s.report().metadata.clean_close);
}

#[test]
fn test_an_unclean_stop_is_reported() {
    let p = SimPlatform::new(SimConfig::volatile(72));
    crash_open(&p, create(&p));
    let s = open(&p);
    let r = s.report();
    assert!(!r.metadata.clean_close);
    assert!(!r.metadata.frozen);
    assert!(!r.read_only);
    assert_eq!(r.owner_generation, 2);
    // The open itself records the store as open: a crash now is unclean too.
    crash_open(&p, s);
    assert!(!open(&p).report().metadata.clean_close);
}

#[test]
fn test_a_torn_volume_record_write_is_healed() {
    let p = SimPlatform::new(SimConfig::volatile(73));
    drop(create(&p));
    // The clean close's write was torn.
    tear(&p, newest_volume_slot(&p));
    let s = open(&p);
    let r = s.report().clone();
    assert_eq!(
        r.metadata.healed,
        Some(Healed::VolumeRecord),
        "{:?}",
        r.metadata
    );
    assert!(!r.metadata.clean_close);
    assert!(!r.metadata.frozen);
    assert!(!r.read_only);
    drop(s);
    let s = open(&p);
    assert_eq!(s.report().metadata.healed, None);
    assert!(s.report().metadata.clean_close);
}

#[test]
fn test_a_torn_page_write_is_healed() {
    let p = SimPlatform::new(SimConfig::volatile(74));
    crash_open(&p, create(&p));
    let at = page_at(&p, 1, 1);
    put(&p, at, &page_copy_b(&p, 1, true));
    let torn_seq = max_meta_seq(&p);
    let s = open(&p);
    let r = s.report().clone();
    assert_eq!(r.metadata.healed, Some(Healed::Page(1)), "{:?}", r.metadata);
    assert!(r.metadata.stale_pages.is_empty());
    assert!(!r.metadata.frozen);
    assert!(!r.read_only);
    // The heal rewrote the torn copy whole, at its own meta_seq.
    let m = media(&p);
    assert_eq!(u64_at(&m, at as usize + 96), torn_seq);
    assert_eq!(u64_at(&m, at as usize + 56), 2);
    drop(s);
    let s = open(&p);
    assert_eq!(s.report().metadata.healed, None);
    assert!(!s.report().metadata.frozen);
}

#[test]
fn test_a_torn_page_after_a_clean_close_is_stale_and_freezes() {
    let p = SimPlatform::new(SimConfig::volatile(75));
    drop(create(&p));
    put(&p, page_at(&p, 1, 1), &page_copy_b(&p, 1, true));
    let s = open(&p);
    let r = s.report().clone();
    // Nothing was in flight at a clean close: the damaged copy is damage.
    assert_eq!(r.metadata.healed, None);
    assert_eq!(r.metadata.stale_pages, vec![1]);
    assert!(r.metadata.lost_pages.is_empty());
    assert!(r.metadata.frozen);
    assert!(!r.read_only);
}

#[test]
fn test_a_page_with_no_usable_copy_is_lost() {
    let p = SimPlatform::new(SimConfig::volatile(76));
    drop(create(&p));
    put(&p, page_at(&p, 0, 0), &vec![0u8; TABLE as usize]);
    let s = open(&p);
    let r = s.report();
    assert_eq!(r.metadata.lost_pages, vec![0]);
    assert!(r.metadata.frozen);
    assert_eq!(r.metadata.healed, None);
}

#[test]
fn test_a_damaged_volume_record_with_a_page_candidate_opens_read_only() {
    let p = SimPlatform::new(SimConfig::volatile(77));
    drop(create(&p));
    tear(&p, newest_volume_slot(&p));
    put(&p, page_at(&p, 2, 1), &page_copy_b(&p, 2, true));
    let before = media(&p);
    let s = open(&p);
    let r = s.report();
    assert!(r.read_only);
    assert!(r.metadata.read_only_reason.is_some());
    assert!(r.metadata.volume_stale);
    assert_eq!(r.metadata.healed, None);
    // The evidence stays as found: not even the close writes.
    drop(s);
    assert!(media(&p) == before);
}

#[test]
fn test_a_page_of_a_newer_format_is_refused() {
    let p = SimPlatform::new(SimConfig::volatile(78));
    drop(create(&p));
    let mut slot = page_copy_b(&p, 0, false);
    // An incompatible flag this version does not know, header CRC redone.
    slot[16] = 0x80;
    slot[8..12].fill(0);
    let crc = crc32c(&slot[..128]);
    slot[8..12].copy_from_slice(&crc.to_le_bytes());
    put(&p, page_at(&p, 0, 1), &slot);
    for read_only in [false, true] {
        let r = if read_only {
            Store::open_readonly(p.clone(), Path::new(DIR), opts())
        } else {
            Store::open(p.clone(), Path::new(DIR), opts())
        };
        assert!(
            matches!(
                r,
                Err(Error::Unsupported {
                    what: Capability::FormatVersion
                })
            ),
            "{:?}",
            r.map(|_| ())
        );
    }
}

/// Regions are provisioned in format 2, with and without a reservation,
/// and found again after a reopen (the full tests: `format2_provision.rs`).
#[test]
fn test_regions_are_provisioned_in_format_2() {
    let p = SimPlatform::new(SimConfig::volatile(82));
    let s = create(&p);
    assert!(s.provision_page_region("pages", 1 << 20).is_ok());
    let mut r = s.reserve(1 << 20, 1).unwrap();
    assert!(r.provision_append_region("log", 1 << 19).is_ok());
    drop(r);
    drop(s);
    let s = open(&p);
    assert!(
        s.report().unavailable.is_empty(),
        "{:?}",
        s.report().unavailable
    );
    assert!(s.page_region("pages").is_ok());
    assert!(s.append_region("log").is_ok());
}

#[test]
fn test_a_page_naming_another_page_is_lost() {
    let p = SimPlatform::new(SimConfig::volatile(83));
    drop(create(&p));
    // A copy that verifies, but whose payload is page 2's.
    put(
        &p,
        page_at(&p, 1, 1),
        &page_copy_b_of(&p, 1, &empty_page(2), false),
    );
    let s = open(&p);
    assert_eq!(s.report().metadata.lost_pages, vec![1]);
    assert!(s.report().metadata.frozen);
}

/// An entry whose kind and name do not decode (all zero but its sequence)
/// makes only itself unavailable: it is reported by its place, and the
/// store opens.
#[test]
fn test_an_entry_that_does_not_decode_is_reported_not_refused() {
    let p = SimPlatform::new(SimConfig::volatile(84));
    drop(create(&p));
    let mut payload = vec![0u8; store_io_format::meta2::PAGE_HEADER_LEN + 96];
    payload[4] = 1;
    payload[8] = 1;
    put(
        &p,
        page_at(&p, 0, 1),
        &page_copy_b_of(&p, 0, &payload, false),
    );
    let s = Store::open(p.clone(), Path::new(DIR), opts()).unwrap();
    assert_eq!(s.report().metadata.invalid_entries, vec![(0, 0)]);
    assert!(s.report().unavailable.is_empty());
}

#[test]
fn test_a_volume_record_of_a_newer_format_is_refused() {
    let p = SimPlatform::new(SimConfig::volatile(85));
    crash_open(&p, create(&p));
    let m = media(&p);
    let b = block(&p);
    let mut payload = m[128..128 + store_io_format::meta2::VOLUME_META2_LEN].to_vec();
    // A flag this version does not know.
    payload[56] = 1;
    let mut slot = vec![0u8; b as usize];
    encode(
        &mut slot,
        &SlotFields {
            volume_uuid: m[32..48].try_into().unwrap(),
            object_id: 0,
            generation: 2,
            owner_generation: 1,
            slot_offset: b,
            slot_index: SlotIndex::B,
            log2_slot_size: m[78],
            prev_header_crc32c: u32_at(&m, 8),
            format_major: FORMAT_2,
            meta_seq: max_meta_seq(&p) + 1,
            compat_flags: 0,
        },
        &payload,
    )
    .unwrap();
    put(&p, b, &slot);
    let before = media(&p);
    let writes = p.with_world(|w| w.writes());
    for r in [
        Store::open_readonly(p.clone(), Path::new(DIR), opts()),
        Store::open(p.clone(), Path::new(DIR), opts()),
    ] {
        assert!(
            matches!(
                r,
                Err(Error::Unsupported {
                    what: Capability::FormatVersion
                })
            ),
            "{:?}",
            r.map(|_| ())
        );
    }
    assert_eq!(p.with_world(|w| w.writes()), writes, "a refused open wrote");
    assert!(media(&p) == before);
}

#[test]
fn test_a_read_only_open_writes_nothing() {
    let p = SimPlatform::new(SimConfig::volatile(86));
    crash_open(&p, create(&p));
    let before = media(&p);
    let s = Store::open_readonly(p.clone(), Path::new(DIR), opts()).unwrap();
    assert!(s.report().read_only);
    drop(s);
    assert!(media(&p) == before);
}

#[test]
fn test_a_file_shorter_than_its_metadata_is_corrupt() {
    let p = SimPlatform::new(SimConfig::volatile(92));
    drop(create(&p));
    let dir = p.open_dir(Path::new(DIR), false).unwrap();
    let file = p
        .open_file(
            &dir,
            &FileName::new(CONTAINER).unwrap(),
            FileMode::ReadWrite,
        )
        .unwrap();
    let len = page_at(&p, PAGES - 1, 1);
    p.set_len(&file, len).unwrap();
    p.flush_all(&file).unwrap();
    let r = Store::open(p.clone(), Path::new(DIR), opts());
    assert!(
        matches!(r, Err(Error::Corruption { .. })),
        "{:?}",
        r.map(|_| ())
    );
}

#[test]
fn test_a_torn_copy_with_a_damaged_header_is_healed_at_the_next_number() {
    let p = SimPlatform::new(SimConfig::volatile(93));
    crash_open(&p, create(&p));
    let next = max_meta_seq(&p) + 1;
    let mut slot = page_copy_b(&p, 1, false);
    // The header itself is torn: its meta_seq is unknown.
    slot[100] ^= 0xff;
    let at = page_at(&p, 1, 1);
    put(&p, at, &slot);
    let s = open(&p);
    assert_eq!(
        s.report().metadata.healed,
        Some(Healed::Page(1)),
        "{:?}",
        s.report().metadata
    );
    let m = media(&p);
    assert_eq!(u64_at(&m, at as usize + 56), 2);
    assert_eq!(u64_at(&m, at as usize + 96), next);
}

#[test]
fn test_a_page_failing_its_checks_stops_a_page_heal() {
    let p = SimPlatform::new(SimConfig::volatile(94));
    crash_open(&p, create(&p));
    // Page 2's newest copy verifies but names page 0; then page 1's write
    // is torn.
    put(
        &p,
        page_at(&p, 2, 1),
        &page_copy_b_of(&p, 2, &empty_page(0), false),
    );
    put(&p, page_at(&p, 1, 1), &page_copy_b(&p, 1, true));
    let before = media(&p)[page_at(&p, 1, 1) as usize..][..CHUNK].to_vec();
    let s = open(&p);
    let r = s.report().clone();
    assert_eq!(r.metadata.healed, None, "{:?}", r.metadata);
    assert_eq!(r.metadata.stale_pages, vec![1]);
    assert_eq!(r.metadata.lost_pages, vec![2]);
    assert!(r.metadata.frozen);
    // The torn copy stays as evidence.
    assert!(media(&p)[page_at(&p, 1, 1) as usize..][..CHUNK] == before[..]);
}

#[test]
fn test_an_unknown_read_only_feature_opens_read_only() {
    let p = SimPlatform::new(SimConfig::volatile(95));
    drop(create(&p));
    let mut slot = page_copy_b(&p, 0, false);
    slot[20] = 0x80;
    rehash(&mut slot);
    put(&p, page_at(&p, 0, 1), &slot);
    let before = media(&p);
    let s = open(&p);
    assert!(s.report().read_only);
    assert!(s.report().metadata.read_only_reason.is_some());
    drop(s);
    assert!(media(&p) == before);
}

#[test]
fn test_a_failed_metadata_read_is_retried() {
    let mut retried = 0;
    for k in 1..=24u64 {
        let p = SimPlatform::new(SimConfig::volatile(96));
        drop(create(&p));
        p.with_world(|w| {
            let n = w.reads() + k;
            w.faults_mut().fail_read = Some((n, 5));
        });
        let s = open(&p);
        let r = s.report();
        assert!(!r.read_only, "read {k}: {:?}", r.metadata);
        assert!(!r.metadata.frozen, "read {k}: {:?}", r.metadata);
        assert!(r.metadata.clean_close, "read {k}: {:?}", r.metadata);
        assert!(!r.metadata.volume_stale, "read {k}: {:?}", r.metadata);
        retried += r.metadata.retried_reads;
    }
    assert!(retried > PAGES, "{retried}");
}

#[test]
fn test_a_close_during_a_panic_is_not_clean() {
    let p = SimPlatform::new(SimConfig::volatile(97));
    let s = create(&p);
    let t = std::thread::spawn(move || {
        let _s = s;
        panic!("a caller's bug");
    });
    assert!(t.join().is_err());
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    assert!(!open(&p).report().metadata.clean_close);
}

#[test]
fn test_an_interrupted_open_voids_an_earlier_clean_close() {
    let p = SimPlatform::new(SimConfig::volatile(98));
    drop(create(&p));
    // The next open's volume-record write (clean_close 0) is torn: the
    // intact copy still says clean, but a write was in flight.
    crash_open(&p, open(&p));
    tear(&p, newest_volume_slot(&p));
    let s = open(&p);
    assert!(
        !s.report().metadata.clean_close,
        "{:?}",
        s.report().metadata
    );
    assert_eq!(s.report().metadata.healed, Some(Healed::VolumeRecord));
}

#[test]
fn test_a_bit_flip_in_the_format_field_does_not_refuse_the_store() {
    let p = SimPlatform::new(SimConfig::volatile(99));
    drop(create(&p));
    // Slot A (the older copy) now claims format 1; its header no longer
    // verifies, so it decides nothing.
    let mut b = media(&p)[..CHUNK].to_vec();
    b[14] = 1;
    put(&p, 0, &b);
    let s = open(&p);
    assert!(!s.report().read_only);
    assert!(!s.report().metadata.frozen);
    assert_eq!(s.report().format, FORMAT_2);
}

#[test]
fn test_a_damaged_volume_copy_without_page_candidates_is_replaced() {
    let p = SimPlatform::new(SimConfig::volatile(110));
    crash_open(&p, create(&p));
    // A newer copy that is not the next generation: damage, not a tear.
    put(&p, block(&p), &volume_copy_b(&p, 3, true));
    let s = open(&p);
    let r = s.report().clone();
    assert!(r.metadata.volume_stale);
    assert!(!r.read_only);
    assert!(!r.metadata.frozen);
    assert_eq!(r.metadata.healed, None);
    // The open's own write went over the damaged copy.
    let m = media(&p);
    let b = block(&p) as usize;
    assert_eq!(u64_at(&m, b + 56), 2);
    drop(s);
    assert!(open(&p).report().metadata.clean_close);
}

#[test]
fn test_a_retry_reads_only_the_slot_that_failed() {
    // A slot that read is never read again: a second read of it could fail
    // and lose a good copy.
    let reads = |p: &SimPlatform| p.with_world(|w| w.reads());
    let p = SimPlatform::new(SimConfig::volatile(111));
    drop(create(&p));
    let r0 = reads(&p);
    drop(open(&p));
    let base = reads(&p) - r0;
    let mut extra = Vec::new();
    for k in 1..=12u64 {
        let p = SimPlatform::new(SimConfig::volatile(111));
        drop(create(&p));
        let r0 = reads(&p);
        p.with_world(|w| w.faults_mut().fail_read = Some((r0 + k, 5)));
        drop(open(&p));
        extra.push(reads(&p) - r0 - base);
    }
    assert!(extra.iter().all(|&e| e <= 1), "{extra:?}");
    assert!(
        extra.iter().filter(|&&e| e == 1).count() > PAGES as usize,
        "{extra:?}"
    );
}

#[test]
fn test_a_longer_volume_record_is_refused() {
    let p = SimPlatform::new(SimConfig::volatile(112));
    crash_open(&p, create(&p));
    put(&p, block(&p), &volume_copy_b_longer(&p, 2, 8, false));
    for read_only in [false, true] {
        let r = if read_only {
            Store::open_readonly(p.clone(), Path::new(DIR), opts())
        } else {
            Store::open(p.clone(), Path::new(DIR), opts())
        };
        assert!(
            matches!(
                r,
                Err(Error::Unsupported {
                    what: Capability::FormatVersion
                })
            ),
            "{:?}",
            r.map(|_| ())
        );
    }
}

#[test]
fn test_a_newer_volume_copy_is_refused_even_after_a_failed_read() {
    for k in 1..=6u64 {
        let p = SimPlatform::new(SimConfig::volatile(114));
        crash_open(&p, create(&p));
        let mut slot = volume_copy_b(&p, 2, false);
        // An incompatible flag this version does not know.
        slot[16] = 0x80;
        rehash(&mut slot);
        put(&p, block(&p), &slot);
        p.with_world(|w| {
            let n = w.reads() + k;
            w.faults_mut().fail_read = Some((n, 5));
        });
        let r = Store::open(p.clone(), Path::new(DIR), opts());
        assert!(
            matches!(
                r,
                Err(Error::Unsupported {
                    what: Capability::FormatVersion
                })
            ),
            "read {k}: {:?}",
            r.map(|_| ())
        );
    }
}

#[test]
fn test_a_store_of_one_table_page_round_trips() {
    let p = SimPlatform::new(SimConfig::volatile(115));
    let o = StoreOptions {
        region_capacity: 100,
        ..opts()
    };
    let s = Store::create(p.clone(), Path::new(DIR), o.clone()).unwrap();
    assert_eq!(s.report().table_capacity, 169);
    drop(s);
    let s = Store::open(p.clone(), Path::new(DIR), o).unwrap();
    assert!(s.report().metadata.clean_close);
    assert!(!s.report().metadata.frozen);
}

/// A store opened for writing and then found read-only (here, a feature
/// this version may read but not write) runs one flush, with no extra
/// read-write handles, however many flushes the options asked for.
#[test]
fn test_a_store_found_read_only_runs_one_flush() {
    let p = SimPlatform::new(SimConfig::volatile(117));
    drop(create(&p));
    let mut slot = page_copy_b(&p, 0, false);
    slot[20] = 0x80;
    rehash(&mut slot);
    put(&p, page_at(&p, 0, 1), &slot);
    let o = StoreOptions {
        flushes_in_flight: 4,
        ..opts()
    };
    let reopens = p.with_world(|w| w.reopens());
    let s = Store::open(p.clone(), Path::new(DIR), o).unwrap();
    assert!(s.report().read_only);
    assert_eq!(s.report().flushes_in_flight, 1);
    assert_eq!(
        p.with_world(|w| w.reopens()),
        reopens,
        "opened a flush handle"
    );
}

/// A store poisoned by a failed flush records no clean close when it is
/// dropped: a write it gave up on may still land, so the next open must
/// judge the store as not closed cleanly.
#[test]
fn test_a_poisoned_store_records_no_clean_close() {
    let p = SimPlatform::new(SimConfig::volatile(118));
    let s = create(&p);
    let log = s.provision_append_region("log", 1 << 20).unwrap();
    p.with_world(|w| w.faults_mut().fail_flush = Some(w.flushes() + 1));
    assert!(log.append_durable(&[1u8; 4096]).is_err());
    assert!(s.poisoned().is_some());
    p.with_world(|w| *w.faults_mut() = FaultPlan::default());
    drop((log, s));
    assert!(!open(&p).report().metadata.clean_close);
}

/// The heal of the newest write (torn) and the open's own volume-record
/// write leave the numbers dense: the highest `meta_seq` afterwards is one
/// above the torn copy's (spec §7), no number skipped.
#[test]
fn test_a_heal_leaves_the_numbers_dense() {
    let p = SimPlatform::new(SimConfig::volatile(119));
    drop(create(&p));
    let s = open(&p);
    let log = s.provision_append_region("log", 1 << 20).unwrap();
    drop((log, s));
    // The newest write, the clean close's volume record, torn: its header
    // (and its number) intact.
    tear(&p, newest_volume_slot(&p));
    let torn = max_meta_seq(&p);
    let s = open(&p);
    assert_eq!(s.report().metadata.healed, Some(Healed::VolumeRecord));
    assert_eq!(max_meta_seq(&p), torn + 1);
}

/// A torn copy's number counts (spec §7): a stale page's torn newer copy
/// keeps its `meta_seq`, and the open's volume-record write is numbered
/// above it, so no two slots ever share a number.
#[test]
fn test_a_torn_copys_number_is_never_reused() {
    let p = SimPlatform::new(SimConfig::volatile(120));
    drop(create(&p));
    format2_util::craft::tear_newer_copy_of(&p, 0);
    let torn = max_meta_seq(&p);
    let s = open(&p);
    let r = s.report().clone();
    assert!(r.metadata.healed.is_none(), "{:?}", r.metadata);
    let volume = newest_volume_slot(&p);
    let numbered = u64_at(&media(&p), volume as usize + 96);
    assert!(
        numbered > torn,
        "the volume record took {numbered}, the torn copy has {torn}"
    );
    drop(s);
}

/// Slot B's newer volume-record copy naming one more table page than slot
/// A's (its CRCs redone): a record describing another table.
fn volume_copy_b_other_table(pf: &SimPlatform) -> Vec<u8> {
    let mut slot = volume_copy_b(pf, 2, false);
    let len = u32_at(&slot, 72) as usize;
    let pages = u32_at(&slot, 128 + 48) + 1;
    slot[128 + 48..128 + 52].copy_from_slice(&pages.to_le_bytes());
    let crc = crc32c(&slot[128..128 + len]);
    slot[28..32].copy_from_slice(&crc.to_le_bytes());
    rehash(&mut slot);
    slot
}

/// A volume record read again after a failed read may be the newer copy:
/// it must describe the same table as the copy the open began with, or the
/// store is corrupt; it is never opened on a table the record does not
/// describe.
#[test]
fn test_a_reread_volume_record_naming_another_table_is_corrupt() {
    for k in 0..=6 {
        let p = SimPlatform::new(SimConfig::volatile(121));
        crash_open(&p, create(&p));
        put(&p, block(&p), &volume_copy_b_other_table(&p));
        if k > 0 {
            p.with_world(|w| {
                let n = w.reads() + k;
                w.faults_mut().fail_read = Some((n, 5));
            });
        }
        let r = Store::open(p.clone(), Path::new(DIR), opts());
        assert!(
            matches!(r, Err(Error::Corruption { .. })),
            "read {k} failed: {:?}",
            r.map(|s| s.report().metadata.clone())
        );
    }
}

/// A slot's `compat_flags` (may-ignore bits, none defined yet) survive every
/// rewrite: an earlier release never erases a later one's bits (spec §5).
#[test]
fn test_compat_flags_survive_a_rewrite() {
    const BIT: u32 = 0x8000_0000;
    let p = SimPlatform::new(SimConfig::volatile(122));
    drop(create(&p));
    let at = newest_volume_slot(&p) as usize;
    let mut slot = media(&p)[at..at + block(&p) as usize].to_vec();
    slot[24..28].copy_from_slice(&BIT.to_le_bytes());
    rehash(&mut slot);
    put(&p, at as u64, &slot);
    // The open's volume-record write, from that winner.
    let s = open(&p);
    assert!(!s.report().read_only, "{:?}", s.report());
    let now = newest_volume_slot(&p) as usize;
    assert_ne!(now, at, "the open wrote no volume record");
    assert_eq!(u32_at(&media(&p), now + 24), BIT, "the open's write");
    // The clean close's, from the open's.
    drop(s);
    let last = newest_volume_slot(&p) as usize;
    assert_ne!(last, now, "the close wrote no volume record");
    assert_eq!(
        u32_at(&media(&p), last + 24),
        BIT,
        "the clean close's write"
    );
    // The carried bit breaks no chain and no judgement.
    let s = open(&p);
    let r = s.report();
    assert!(!r.read_only && r.metadata.clean_close, "{r:?}");
    assert_eq!(r.metadata.healed, None, "{r:?}");
}
