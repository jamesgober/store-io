//! # store-io
//!
//! Durable storage I/O for Rust databases.
//!
//! store-io is the layer between a storage engine and the files and devices it
//! writes to. It moves caller bytes to media and proves when they are durable.
//! It does not frame, parse or validate the bytes it moves, and it never decides
//! what a caller keeps after a crash.
//!
//! The design rests on four rules:
//!
//! - **Durability is decided per device, with evidence.** Each volume is probed
//!   at open and classed as power-safe, flush-required, unverified or unsafe
//!   from what the device and operating system report. Operating-system cache
//!   toggles and vendor tables never promote a device.
//! - **A durable write produces an unforgeable receipt.** Positions, write
//!   tickets and receipts are distinct opaque types, so a sequence number can
//!   never be passed where a byte position is expected, and a barrier can never
//!   be a silent no-op.
//! - **Failure is fail-stop.** The first failed write or flush poisons the
//!   device's barrier domain. A flush is never retried and success is never
//!   reported after a failed one.
//! - **Nothing hides latency.** There are no timers, no group-commit windows and
//!   no background threads that wake an idle process.
//!
//! ## Status
//!
//! This release reserves the crate name and publishes the project scaffold. It
//! has no public API yet. The research sweep and the architecture are being
//! completed first, and the public surface is added only after the architecture
//! is approved. The plan and its exit gates are in the repository's
//! `dev/ROADMAP.md`.

#![deny(warnings)]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(unused_must_use)]
#![deny(unused_results)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::todo)]
#![deny(clippy::unimplemented)]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
#![deny(clippy::unreachable)]
#![deny(clippy::undocumented_unsafe_blocks)]
#![cfg_attr(docsrs, feature(doc_cfg))]
