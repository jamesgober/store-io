//! The Windows device probe's parsers on hostile input: storage property
//! descriptors, protocol-specific (NVMe) answers and volume structures. They
//! are pure, so this target runs on any platform. The backend's
//! `fuzzing::run` picks a parser from the first byte and checks its
//! properties (`crates/store-io-win/src/fuzzing.rs`).

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The hidden entry point exists only in fuzz builds (`--cfg fuzzing`).
    #[cfg(fuzzing)]
    store_io_win::fuzzing::run(data);
    #[cfg(not(fuzzing))]
    let _ = data;
});
