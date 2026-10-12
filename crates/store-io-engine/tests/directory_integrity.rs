//! Directory mode refuses a change it could never make durable before
//! writing anything, and a metadata failure after the writes is fail-stop.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::path::{Path, PathBuf};

use store_io_core::decide::Trust;
use store_io_core::error::{Capability, Error};
use store_io_engine::Directory;
use store_io_sim::{FaultPlan, SimConfig, SimPlatform};

const DIR: &str = "/conf";

/// A file system that cannot flush directories: every change is refused as
/// unsupported, before any write, and nothing appears in the directory.
/// Before 0.4 the replace wrote and renamed first, then failed the flush.
#[test]
fn test_replace_is_refused_before_any_write_when_directories_cannot_be_flushed() {
    let mut cfg = SimConfig::volatile(80);
    cfg.faults = FaultPlan {
        dir_sync_unsupported: true,
        ..FaultPlan::default()
    };
    let p = SimPlatform::new(cfg);
    // The directory exists already (creating it needs a parent flush).
    p.with_world(|w| w.faults_mut().dir_sync_unsupported = false);
    drop(Directory::open(p.clone(), Path::new(DIR), true, Trust::default()).unwrap());
    p.with_world(|w| w.faults_mut().dir_sync_unsupported = true);

    let d = Directory::open(p.clone(), Path::new(DIR), false, Trust::default()).unwrap();
    let writes = p.with_world(|w| w.writes());
    assert!(matches!(
        d.replace("CURRENT", b"MANIFEST-000001"),
        Err(Error::Unsupported {
            what: Capability::DirectoryFlush
        })
    ));
    assert!(matches!(
        d.remove("CURRENT"),
        Err(Error::Unsupported {
            what: Capability::DirectoryFlush
        })
    ));
    assert_eq!(p.with_world(|w| w.writes()), writes, "a write was issued");
    p.with_world(|w| {
        assert!(w.file_id(&PathBuf::from(DIR), "CURRENT").is_none());
        assert!(w.file_id(&PathBuf::from(DIR), "CURRENT.sio-tmp").is_none());
    });
}

/// A `set_len` refused after the new bytes were written poisons the handle
/// (fail-stop), and the target keeps its old contents. Before 0.4 the
/// handle stayed usable.
#[test]
fn test_a_failed_set_len_after_the_writes_poisons_and_keeps_the_target() {
    let p = SimPlatform::new(SimConfig::volatile(81));
    let d = Directory::open(p.clone(), Path::new(DIR), true, Trust::default()).unwrap();
    d.replace("CURRENT", b"old").unwrap();
    // The next set_len is the one the replace below makes after its writes.
    p.with_world(|w| w.faults_mut().fail_set_len = Some(2));
    let r = d.replace("CURRENT", b"new contents");
    assert!(
        matches!(r, Err(Error::DurabilityUnknown { .. })),
        "got {r:?}"
    );
    assert!(matches!(
        d.replace("CURRENT", b"again"),
        Err(Error::Poisoned { .. })
    ));
    assert_eq!(d.read("CURRENT").unwrap(), b"old");
}
