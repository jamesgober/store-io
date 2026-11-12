//! Damage to one copy of a slot's header pair (independent review F-2,
//! D-162): after a clean close no write was in flight, so a written copy
//! that no longer verifies is damage, and if it is not provably the served
//! version's predecessor the open reports the pair in
//! `MetadataReport::lost_header_writes` (the older value is served, as
//! after a lost write). After a crash it may be a torn, unacknowledged
//! commit: served as before, unreported.

// Test setup unwraps (a failure is the test failing).
#![allow(clippy::unwrap_used, clippy::expect_used, unused_results)]

mod format2_util;

use format2_util::*;
use store_io_sim::{SimConfig, SimPlatform};

const B: usize = 4096;

/// A slot committed `v1` then `v2`, the store closed cleanly or not.
fn two_commits(seed: u64, clean: bool) -> SimPlatform {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    let s = create(&p);
    let slot = s.provision_slot("manifest").unwrap();
    let _r1 = slot.commit(V1).unwrap();
    let _r2 = slot.commit(V2).unwrap();
    drop(slot);
    if clean {
        drop(s);
    } else {
        crash_open(&p, s);
    }
    p
}

const V1: &[u8] = b"manifest version one: wal at 4096, checkpoint 1";
const V2: &[u8] = b"manifest version two: wal at 8192, checkpoint 2";

/// The block (one header copy) holding `value`.
fn copy_holding(p: &SimPlatform, value: &[u8]) -> usize {
    let m = media(p);
    let at = m
        .windows(value.len())
        .position(|w| w == value)
        .expect("the value on the media");
    at / B * B
}

/// Flips a byte of a copy's header (not its stored CRC, bytes 8 to 12).
fn damage_header(p: &SimPlatform, copy: usize) {
    let m = media(p);
    let mut b = m[copy..copy + B].to_vec();
    b[100] ^= 0x5a;
    put(p, copy as u64, &b);
}

fn reported(p: &SimPlatform) -> (Option<Vec<u8>>, Vec<u64>, bool) {
    let s = open(p);
    let read = s.slot("manifest").unwrap().read();
    let m = &s.report().metadata;
    let lost = m.lost_header_writes.iter().map(|r| r.get()).collect();
    (read, lost, m.clean_close)
}

/// The newest copy's header damaged after a clean close: the older value
/// is served and the pair reported, so a caller keeping a manifest knows
/// its last commit may be lost.
#[test]
fn test_a_damaged_newest_slot_copy_after_a_clean_close_is_reported() {
    let p = two_commits(901, true);
    damage_header(&p, copy_holding(&p, V2));
    let (read, lost, clean) = reported(&p);
    assert!(clean);
    assert_eq!(read.as_deref(), Some(V1));
    assert_eq!(lost, vec![0], "the slot's region reported");
}

/// The older copy's header damaged after a clean close, its stored CRC
/// intact: provably the served version's predecessor, so nothing is lost
/// and nothing is reported.
#[test]
fn test_a_damaged_older_slot_copy_after_a_clean_close_loses_nothing() {
    let p = two_commits(902, true);
    damage_header(&p, copy_holding(&p, V1));
    let (read, lost, clean) = reported(&p);
    assert!(clean);
    assert_eq!(read.as_deref(), Some(V2));
    assert!(lost.is_empty(), "{lost:?}");
}

/// The same damage after a crash: a torn, unacknowledged commit explains
/// it, so the older value is served unreported, as before.
#[test]
fn test_a_damaged_newest_slot_copy_after_a_crash_is_a_torn_write() {
    let p = two_commits(903, false);
    damage_header(&p, copy_holding(&p, V2));
    let (read, lost, clean) = reported(&p);
    assert!(!clean);
    assert_eq!(read.as_deref(), Some(V1));
    assert!(lost.is_empty(), "{lost:?}");
}

/// After a crash, the newest copy's header verifying with the newer
/// generation and its payload torn: a commit possibly lost, reported as
/// before (spec §8), the older value served.
#[test]
fn test_a_torn_newest_slot_payload_after_a_crash_is_reported() {
    let p = two_commits(904, false);
    // A byte of the value itself: inside the payload its CRC covers.
    let m = media(&p);
    let at = m.windows(V2.len()).position(|w| w == V2).unwrap();
    let copy = at / B * B;
    let mut b = m[copy..copy + B].to_vec();
    b[at - copy] ^= 0x5a;
    put(&p, copy as u64, &b);
    let (read, lost, clean) = reported(&p);
    assert!(!clean);
    assert_eq!(read.as_deref(), Some(V1));
    assert_eq!(lost, vec![0]);
}
