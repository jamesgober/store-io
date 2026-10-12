//! The Linux device probe's parsers on hostile input: sysfs and procfs text,
//! `mountinfo`, `uevent`, and NVMe Identify and SMART pages. The backend's
//! `fuzzing::run` picks a parser from the first byte and checks its
//! properties (`crates/store-io-posix/src/fuzzing.rs`).

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The hidden entry point exists only in fuzz builds (`--cfg fuzzing`).
    #[cfg(all(fuzzing, target_os = "linux"))]
    store_io_posix::fuzzing::run(data);
    #[cfg(not(all(fuzzing, target_os = "linux")))]
    let _ = data;
});
