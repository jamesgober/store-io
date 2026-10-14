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
