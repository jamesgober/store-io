//! Directory mode on the deterministic simulator: atomic, durable replace,
//! remove, and what a crash or a failed directory flush leaves behind.

// Test setup unwraps (a failure is the test failing) and ignores values a
// given test does not inspect.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::path::Path;

use store_io_core::decide::Trust;
use store_io_core::error::{Error, Op};
use store_io_engine::Directory;
use store_io_sim::{CrashMode, SimConfig, SimPlatform};

const DIR: &str = "/conf";

fn open(p: &SimPlatform) -> Directory<SimPlatform> {
    Directory::open(p.clone(), Path::new(DIR), true, Trust::default()).unwrap()
}

fn bytes(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| seed.wrapping_add((i * 7 + (i >> 8)) as u8))
        .collect()
}

#[test]
fn test_replace_and_read_back_exact_bytes_of_any_length() {
    let p = SimPlatform::new(SimConfig::volatile(61));
    let d = open(&p);
    for (i, len) in [0usize, 1, 100, 4095, 4096, 4097, 70_000, (1 << 20) + 3]
        .into_iter()
        .enumerate()
    {
        let data = bytes(i as u8, len);
        d.replace("manifest", &data).unwrap();
        assert_eq!(d.read("manifest").unwrap(), data, "length {len}");
    }
    assert!(d.class().is_some(), "probed with the first file");
    assert!(matches!(d.read("missing"), Err(Error::NotFound { .. })));
}

#[test]
fn test_a_returned_replace_survives_a_crash() {
    let p = SimPlatform::new(SimConfig::volatile(62));
    let d = open(&p);
    d.replace("CURRENT", b"MANIFEST-000001").unwrap();
    d.replace("CURRENT", b"MANIFEST-000002").unwrap();
    drop(d);
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    let d = open(&p);
    assert_eq!(d.read("CURRENT").unwrap(), b"MANIFEST-000002");
}

#[test]
fn test_every_crash_point_leaves_the_old_or_the_new_file() {
    // Crash after each step of a replace: before the rename is durable the
    // old bytes survive; after it, the new ones. Never a mix, a short file
    // or a missing file.
    let old = bytes(1, 6000);
    let new = bytes(2, 9000);
    let p = SimPlatform::new(SimConfig::volatile(63));
    let d = open(&p);
    d.replace("m", &old).unwrap();
    // Fail the directory flush: the rename happened but is not durable.
    p.with_world(|w| {
        let next = w.dir_syncs() + 1;
        w.faults_mut().fail_dir_sync = Some(next);
    });
    let err = d.replace("m", &new).unwrap_err();
    assert!(
        matches!(
            err,
            Error::DurabilityUnknown {
                op: Op::SyncDir,
                ..
            }
        ),
        "{err:?}"
    );
    // The handle is poisoned: no further change is accepted.
    assert!(matches!(d.replace("m", &old), Err(Error::Poisoned { .. })));
    assert!(matches!(d.remove("m"), Err(Error::Poisoned { .. })));
    // Reads still work, and show the new version before any crash.
    assert_eq!(d.read("m").unwrap(), new);
    drop(d);
    let base = p.with_world(|w| w.clone());
    for mode in [CrashMode::LoseAll, CrashMode::KeepAll] {
        let p2 = SimPlatform::new(SimConfig::volatile(63));
        p2.with_world(|w| {
            *w = base.clone();
            w.faults_mut().fail_dir_sync = None;
            w.crash(&mode);
        });
        let got = open(&p2).read("m").unwrap();
        assert!(
            got == old || got == new,
            "crash {mode:?}: {} bytes",
            got.len()
        );
    }
}

#[test]
fn test_a_failed_flush_of_the_new_bytes_leaves_the_target_untouched() {
    let p = SimPlatform::new(SimConfig::volatile(64));
    let d = open(&p);
    d.replace("m", b"version one").unwrap();
    p.with_world(|w| {
        let next = w.flushes() + 1;
        w.faults_mut().fail_flush = Some(next);
    });
    let err = d.replace("m", b"version two").unwrap_err();
    assert!(
        matches!(
            err,
            Error::DurabilityUnknown {
                op: Op::FlushAll,
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(d.read("m").unwrap(), b"version one");
    drop(d);
    p.with_world(|w| {
        w.faults_mut().fail_flush = None;
        w.crash(&CrashMode::LoseAll);
    });
    let d = open(&p);
    assert_eq!(d.read("m").unwrap(), b"version one");
    // A leftover temporary file never gets in the way of the next replace.
    d.replace("m", b"version three").unwrap();
    assert_eq!(d.read("m").unwrap(), b"version three");
}

#[test]
fn test_remove_is_durable_and_reports_missing_files() {
    let p = SimPlatform::new(SimConfig::volatile(65));
    let d = open(&p);
    d.replace("a", b"x").unwrap();
    d.replace("b", b"y").unwrap();
    d.remove("a").unwrap();
    assert!(matches!(d.remove("a"), Err(Error::NotFound { .. })));
    drop(d);
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    let d = open(&p);
    assert!(matches!(d.read("a"), Err(Error::NotFound { .. })));
    assert_eq!(d.read("b").unwrap(), b"y");
}

#[test]
fn test_names_are_single_components() {
    let p = SimPlatform::new(SimConfig::volatile(66));
    let d = open(&p);
    for bad in ["", "a/b", "..", "a\\b"] {
        assert!(
            matches!(d.replace(bad, b"x"), Err(Error::NotWritten { .. })),
            "{bad:?}"
        );
    }
}

#[test]
fn test_a_durable_leftover_temporary_file_does_not_block_a_replace() {
    // A crash after the temporary file's entry became durable (another
    // directory flush ran) but before the rename leaves it behind.
    use store_io_platform::{FileName, Platform};
    let p = SimPlatform::new(SimConfig::volatile(67));
    let d = open(&p);
    d.replace("m", b"one").unwrap();
    let dir = p.open_dir(Path::new(DIR), false).unwrap();
    let left = p
        .create_file(&dir, &FileName::new("m.sio-tmp").unwrap())
        .unwrap();
    p.sync_dir(&dir).unwrap();
    let _closed = left;
    d.replace("m", b"two").unwrap();
    assert_eq!(d.read("m").unwrap(), b"two");
}
