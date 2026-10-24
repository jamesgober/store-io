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

use store_io_core::error::{Capability, Error};
use store_io_engine::{CONTAINER, CopyOptions, Store, StoreOptions, copy_store};
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
        CopyOptions { store: opts() },
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
