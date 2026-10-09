//! # store-io-format
//!
//! The on-disk formats of store-io: the CRC-32C checksum, the A/B slot header
//! that every piece of store-io metadata is written with, and the codecs for
//! the payloads those slots carry.
//!
//! Everything here is pure: no I/O, no allocation, no global state. Every
//! decoder treats its input as hostile, checks every bound before reading,
//! and is fuzzed. The byte-level specification is `docs/FORMAT.md` in the
//! repository.

#![deny(warnings)]
#![cfg_attr(
    not(any(target_arch = "x86_64", target_arch = "aarch64")),
    forbid(unsafe_code)
)]

pub mod codec;
pub mod crc32c;
pub mod fill;
pub mod meta;
pub mod pair;
pub mod slot;
