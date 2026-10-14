//! store-io's own io_uring binding (D-093): the kernel ABI (`abi`), the
//! system calls and shared-memory protocol (`sys`, the only `unsafe`) and a
//! safe ring (`ring`). The `Queue` on it and the tier selection follow
//! (D-103, 3b and 3c).

// Used by the uring queue (D-103 3b); until then only by tests.
#![allow(dead_code)]

pub(crate) mod abi;
pub(crate) mod ring;
pub(crate) mod sys;
