//! Fail-stop under real device errors (Linux CI, device-mapper).
//!
//! Driven by the CI job "Fail-stop under device errors": the file system sits
//! on a device-mapper target that the job switches, mid-run, from a working
//! mapping to one that fails every I/O.
//!
//! 1. `dm_phase_write` appends durable records, logging each acknowledged
//!    one to `$STORE_IO_DM_ACKS`, until the device starts failing. The first
//!    failure must be `DurabilityUnknown` (or `Poisoned`), and no write or
//!    barrier may succeed after it.
//! 2. The job heals the device and remounts the file system.
//! 3. `dm_phase_verify` reopens the store and checks every acknowledged
//!    record byte for byte.
//!
//! Both phases are `#[ignore]`d: they make sense only inside that job.

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    unused_results
)]

use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use store_io::{Error, Store, StoreOptions, Trust};

const RECORD: usize = 4096;

fn env_path(var: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(var).unwrap_or_else(|| panic!("{var} is not set")))
}

/// The CI device is a loop device on a virtual machine: refused, so the
/// labelled override is used (the test is about fail-stop, not the class).
fn opts() -> StoreOptions {
    StoreOptions {
        trust: Trust {
            override_refusal: true,
            ..Trust::default()
        },
        ..StoreOptions::default()
    }
}

fn record(i: u64) -> Vec<u8> {
    let mut r = vec![0u8; RECORD];
    for (j, b) in r.iter_mut().enumerate() {
        *b = (i as usize * 131 + j * 7) as u8;
    }
    r[..8].copy_from_slice(&i.to_le_bytes());
    r
}

#[test]
#[ignore = "run by the dm-error CI job"]
fn dm_phase_write() {
    let dir = env_path("STORE_IO_DM_DIR").join("store");
    let mut acks = std::fs::File::create(env_path("STORE_IO_DM_ACKS")).unwrap();
    let store = Store::create(&dir, opts()).unwrap();
    let wal = store.provision_append_region("wal", 256 << 20).unwrap();
    let start = Instant::now();
    let mut i = 0u64;
    let first = loop {
        assert!(
            start.elapsed() < Duration::from_secs(300),
            "the device never failed"
        );
        match wal.append_durable(&record(i)) {
            Ok((pos, _receipt)) => {
                writeln!(acks, "{i} {}", pos.offset()).unwrap();
                acks.sync_all().unwrap();
                i += 1;
            }
            Err(e) => break e,
        }
    };
    eprintln!("first failure after {i} acknowledged records: {first}");
    assert!(
        matches!(
            first,
            Error::DurabilityUnknown { .. } | Error::Poisoned { .. }
        ),
        "the first failure must be durability-unknown: {first:?}"
    );
    // Fail-stop: nothing succeeds after a failure, whatever the device does.
    for k in 0..50 {
        let r = wal.append_durable(&record(i + k));
        assert!(r.is_err(), "a write succeeded after the domain failed");
    }
    let t = wal.append(b"x");
    assert!(t.is_err(), "an append was accepted after the domain failed");
    assert!(matches!(
        store.provision_page_region("after", 1 << 20),
        Err(Error::Poisoned { .. })
    ));
}

#[test]
#[ignore = "run by the dm-error CI job"]
fn dm_phase_verify() {
    let dir = env_path("STORE_IO_DM_DIR").join("store");
    let acks = std::fs::read_to_string(env_path("STORE_IO_DM_ACKS")).unwrap();
    let store = Store::open_readonly(&dir).unwrap();
    let wal = store.append_region("wal").unwrap();
    let mut n = 0;
    for line in acks.lines() {
        let mut parts = line.split(' ');
        let i: u64 = parts.next().unwrap().parse().unwrap();
        let offset: u64 = parts.next().unwrap().parse().unwrap();
        let mut back = vec![0u8; RECORD];
        wal.read(offset, &mut back).unwrap();
        assert!(
            back == record(i),
            "acknowledged record {i} at {offset} lost or altered"
        );
        n += 1;
    }
    assert!(n > 0, "no record was acknowledged before the failure");
    eprintln!("all {n} acknowledged records intact");
}
