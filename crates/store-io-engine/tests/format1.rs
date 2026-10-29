//! Format 1 is read-only from 0.5 (v0.5 E5 C3, D-112, D-116): a writable
//! open of a format-1 store is refused before anything is written, its
//! message naming the way forward; a read-only open reads it; its copy
//! (format 2) opens writable.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::path::{Path, PathBuf};

mod format2_util;

use store_io_core::error::{Capability, Error};
use store_io_engine::{CONTAINER, CopyOptions, Store, StoreOptions, Unavailable, copy_store};
use store_io_format::crc32c::crc32c;
use store_io_format::meta::{RegionMeta, RegionState};
use store_io_format::slot::FORMAT_1;
use store_io_sim::{CrashMode, SimConfig, SimPlatform};

const DIR: &str = "/db";
const NEW: &str = "/db2";
const B: u64 = 4096;

fn opts() -> StoreOptions {
    StoreOptions {
        queues: 2,
        buffers_per_class: 8,
        append_window: 256 << 12,
        region_capacity: 400,
        ..StoreOptions::default()
    }
}

fn media(p: &SimPlatform) -> Vec<u8> {
    p.with_world(|w| w.media_of(&PathBuf::from(DIR), CONTAINER).unwrap().to_vec())
}

#[test]
fn test_format_2_is_the_default() {
    let p = SimPlatform::new(SimConfig::volatile(1101));
    let s = Store::create(p, Path::new(DIR), opts()).unwrap();
    assert_eq!(s.report().format, 2);
}

#[test]
fn test_a_format_1_store_opens_read_only_and_copies_to_a_writable_one() {
    let p = SimPlatform::new(SimConfig::volatile(1102));
    let s = Store::create(
        p.clone(),
        Path::new(DIR),
        StoreOptions {
            container_format: FORMAT_1,
            ..opts()
        },
    )
    .unwrap();
    assert_eq!(s.report().format, 1);
    let pages = s.provision_page_region("pages", 16 * B).unwrap();
    pages
        .write_durable(pages.pos(B).unwrap(), &[9; B as usize])
        .unwrap();
    drop((pages, s));
    // Not closed cleanly either: the refusal still comes before any write
    // (a writable open of format 1 would have healed or recorded the
    // takeover).
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    let before = media(&p);
    let writes = p.with_world(|w| w.writes());
    let e = Store::open(p.clone(), Path::new(DIR), opts()).unwrap_err();
    assert!(
        matches!(
            e,
            Error::Unsupported {
                what: Capability::FormatVersion
            }
        ),
        "{e:?}"
    );
    let message = e.to_string();
    assert!(
        message.contains("format-1") && message.contains("copy_store"),
        "{message}"
    );
    assert!(media(&p) == before, "the refused open wrote");
    assert_eq!(
        p.with_world(|w| w.writes()),
        writes,
        "the refused open wrote"
    );
    // A second writable open is refused alike.
    assert!(Store::open(p.clone(), Path::new(DIR), opts()).is_err());
    // Read-only: readable.
    let r = Store::open_readonly(p.clone(), Path::new(DIR), opts()).unwrap();
    assert_eq!(r.report().format, 1);
    let pages = r.page_region("pages").unwrap();
    let mut back = vec![0u8; B as usize];
    pages.read(pages.pos(B).unwrap(), &mut back).unwrap();
    assert!(back == [9; B as usize]);
    drop((pages, r));
    assert!(media(&p) == before, "the read-only open wrote");
    assert_eq!(
        p.with_world(|w| w.writes()),
        writes,
        "the read-only open wrote"
    );
    // Copied: format 2, writable.
    copy_store(
        p.clone(),
        Path::new(DIR),
        Path::new(NEW),
        CopyOptions {
            store: opts(),
            ..CopyOptions::default()
        },
    )
    .unwrap();
    let s = Store::open(p.clone(), Path::new(NEW), opts()).unwrap();
    assert_eq!(s.report().format, 2);
    let pages = s.page_region("pages").unwrap();
    pages
        .write_durable(pages.pos(0).unwrap(), &[1; B as usize])
        .unwrap();
    pages.read(pages.pos(B).unwrap(), &mut back).unwrap();
    assert!(back == [9; B as usize]);
}

/// The offset of the newest slot of region `region_id`'s header pair (its
/// object `2^32 + id`), found by its magic and object id on 4 KiB blocks.
fn header_slot(m: &[u8], region_id: u32) -> Option<usize> {
    let object = (1u64 << 32) | u64::from(region_id);
    (0..m.len() / B as usize)
        .map(|i| i * B as usize)
        .filter(|&at| &m[at..at + 8] == b"SIOSLOT\0")
        .filter(|&at| u64::from_le_bytes(m[at + 48..at + 56].try_into().unwrap()) == object)
        .max_by_key(|&at| u64::from_le_bytes(m[at + 56..at + 64].try_into().unwrap()))
}

/// A format-1 region whose header slot is valid but says the wrong thing
/// (released while its entry is ready, another region's id, a payload that
/// does not decode) is unavailable with that reason, on a real damaged
/// store; the other region is served.
#[test]
fn test_format_1_header_damage_has_its_reason() {
    type Edit = fn(&mut RegionMeta, &mut [u8]);
    let cases: [(Edit, Unavailable); 3] = [
        (
            |h, _| h.state = RegionState::Released,
            Unavailable::HeaderReleased,
        ),
        (|h, _| h.region_id += 7, Unavailable::HeaderMismatch),
        (|_, raw| raw[7] = 1, Unavailable::HeaderInvalid),
    ];
    for (k, (edit, reason)) in cases.into_iter().enumerate() {
        let p = SimPlatform::new(SimConfig::volatile(1300 + k as u64));
        let s = Store::create(
            p.clone(),
            Path::new(DIR),
            StoreOptions {
                container_format: FORMAT_1,
                ..opts()
            },
        )
        .unwrap();
        s.provision_append_region("keep", 64 * B).unwrap();
        let hurt = s.provision_append_region("hurt", 64 * B).unwrap();
        hurt.append_durable(&[3u8; 100]).unwrap();
        drop((hurt, s));
        let mut m = media(&p);
        // The region whose header names "hurt" has the higher id of the two.
        let at = (0u32..8)
            .filter_map(|id| header_slot(&m, id))
            .next_back()
            .unwrap();
        let len = u32::from_le_bytes(m[at + 72..at + 76].try_into().unwrap()) as usize;
        let payload = at + 128..at + 128 + len;
        let mut h = RegionMeta::decode(&m[payload.clone()]).unwrap();
        let mut raw = m[payload.clone()].to_vec();
        edit(&mut h, &mut raw);
        if raw == m[payload.clone()] {
            h.encode(&mut raw).unwrap();
        }
        m[payload.clone()].copy_from_slice(&raw);
        let crc = crc32c(&m[payload]);
        m[at + 28..at + 32].copy_from_slice(&crc.to_le_bytes());
        m[at + 8..at + 12].fill(0);
        let crc = crc32c(&m[at..at + 128]);
        m[at + 8..at + 12].copy_from_slice(&crc.to_le_bytes());
        format2_util::put(&p, at as u64, &m[at..at + B as usize]);
        let s = Store::open_readonly(p.clone(), Path::new(DIR), opts()).unwrap();
        let u = &s.report().unavailable;
        assert_eq!(u.len(), 1, "{reason:?}: {u:?}");
        assert_eq!((u[0].name.as_str(), u[0].reason), ("hurt", reason));
        assert!(s.append_region("keep").is_ok());
        assert!(s.append_region("hurt").is_err());
    }
}
