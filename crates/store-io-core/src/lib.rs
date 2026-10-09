//! # store-io-core
//!
//! The vocabulary every store-io crate shares: identities ([`id`]), durability
//! classes and receipt labels ([`class`]), the error model ([`error`]) and the
//! classification of raw OS errors ([`errno`]), the evidence a platform reports
//! about a device ([`evidence`]), and the pure rules that turn evidence into a
//! durability class ([`decide`]) and an untorn-write unit ([`untorn`]).
//!
//! Nothing here performs I/O or allocates on a hot path. Every decision rule is
//! a pure function tested by a table, so the same decision can be reproduced
//! from a printed device report.

#![deny(warnings)]
#![forbid(unsafe_code)]

pub mod align;
pub mod class;
pub mod decide;
pub mod errno;
pub mod error;
pub mod evidence;
pub mod id;
pub mod set;
pub mod untorn;

pub use class::{DurabilityClass, ReceiptLabel};
pub use error::{Error, Result};
pub use id::{Generation, RegionId, VolumeId};
