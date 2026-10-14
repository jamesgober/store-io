//! store-io's own io_uring binding (D-093): the kernel ABI (`abi`), the
//! system calls and shared-memory protocol (`sys`, the only `unsafe`) and a
//! safe ring (`ring`), and the `Queue` on it (`queue`, D-103 3b).

// The fixed-buffer and vectored opcodes and the registration wrappers are
// used from step 3d (D-078).
#![allow(dead_code)]

pub(crate) mod abi;
pub(crate) mod queue;
pub(crate) mod ring;
pub(crate) mod sys;

/// For tests that need real rings: whether to skip, because this kernel
/// refuses store-io's rings (io_uring disabled, or older than 5.18, say).
/// With `STORE_IO_EXPECT_URING` set a refusal fails the test instead, so
/// coverage cannot vanish where io_uring is meant to work.
#[cfg(test)]
pub(crate) fn skip() -> bool {
    for flavour in [ring::Flavour::Shared, ring::Flavour::Owned] {
        if let Err(e) = ring::Ring::new(flavour, 1) {
            assert!(
                std::env::var_os("STORE_IO_EXPECT_URING").is_none(),
                "io_uring refused ({e}) where STORE_IO_EXPECT_URING says it works"
            );
            return true;
        }
    }
    false
}
