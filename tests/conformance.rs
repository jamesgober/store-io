//! The conformance suite on this machine's real file system, through the
//! native backend.
//!
//! The scratch root is `$STORE_IO_TEST_DIR` when set (CI points it at
//! loop-mounted ext4 and XFS images with barriers on), else a fresh
//! directory under `$HOME` on Linux or `%TEMP%` on Windows. Where the device
//! is refused (a virtual disk, a `nobarrier` mount), the store scenarios use
//! the labelled override and say so.

#![cfg(any(windows, target_os = "linux", target_os = "macos"))]
#![allow(clippy::unwrap_used, clippy::print_stderr)]

use std::path::PathBuf;

use store_io::{DurableOpen, NativePlatform, Trust};
use store_io_conformance::Suite;

#[test]
fn test_the_native_backend_conforms() {
    conforms(&NativePlatform::new(), "native");
}

/// The io_uring tier (D-103 3b): the stores as on that tier (inline
/// lone-operation lanes, ring parallel lanes and submitters, D-104); the
/// suite's own queues inline.
#[cfg(target_os = "linux")]
#[test]
fn test_the_io_uring_tier_conforms() {
    let Some(p) = uring() else { return };
    conforms(&p, "uring");
}

/// The io_uring platform, or `None` (and a note) where the kernel refuses
/// io_uring: the synchronous tier is the native backend there.
#[cfg(target_os = "linux")]
fn uring() -> Option<store_io_posix::PosixPlatform> {
    match store_io_posix::PosixPlatform::with_tier(store_io_posix::LinuxTier::Uring) {
        Ok(p) => Some(p),
        Err(e) => {
            assert!(
                std::env::var_os("STORE_IO_EXPECT_URING").is_none(),
                "io_uring refused ({e}) where STORE_IO_EXPECT_URING says it works"
            );
            eprintln!("io_uring refused here ({e}): its conformance run is skipped");
            None
        }
    }
}

/// The io_uring tier with the suite's own queues thread-owned (the queue
/// scenarios on thread-owned rings; the stores as always on that tier).
#[cfg(target_os = "linux")]
#[test]
fn test_the_io_uring_tier_conforms_on_owned_rings() {
    let Some(p) = uring() else { return };
    conforms_as(&p, "uring-owned", store_io_platform::Owner::Thread);
}

/// The io_uring tier with the suite's own queues shared and asked for
/// concurrency: the queue scenarios on shared rings, as a store's parallel
/// lanes use them (a lone-operation queue is synchronous on either tier,
/// D-104, and is covered by the default runs).
#[cfg(target_os = "linux")]
#[test]
fn test_the_io_uring_tier_conforms_on_shared_rings() {
    let Some(p) = uring() else { return };
    conforms_with(&p, "uring-shared", store_io_platform::Owner::Shared, true);
}

fn conforms(platform: &NativePlatform, label: &str) {
    conforms_as(platform, label, store_io_platform::Owner::Shared);
}

fn conforms_as(platform: &NativePlatform, label: &str, owner: store_io_platform::Owner) {
    conforms_with(platform, label, owner, false);
}

fn conforms_with(
    platform: &NativePlatform,
    label: &str,
    owner: store_io_platform::Owner,
    parallel: bool,
) {
    let base = std::env::var_os("STORE_IO_TEST_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|_| cfg!(target_os = "linux"))
                .map(PathBuf::from)
        })
        .unwrap_or_else(std::env::temp_dir);
    let root = base.join(format!(
        ".store-io-conformance-{label}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let probe = store_io::probe(&root).unwrap();
    let refused = !matches!(probe.decision.durable_open, DurableOpen::Allowed);
    if refused {
        eprintln!(
            "conformance on a refused device ({:?}: {:?}); store scenarios use the labelled override",
            probe.decision.class, probe.decision.reasons
        );
    }
    let trust = Trust {
        override_refusal: refused,
        ..Trust::default()
    };
    let report = Suite::new(platform, &root, trust)
        .owner(owner)
        .parallel(parallel)
        .run();
    let _cleanup = std::fs::remove_dir_all(&root);
    assert!(report.passed(), "{label}:\n{report}");
}
