//! # store-io-sim
//!
//! A deterministic simulated platform for store-io. See [`world`] for the
//! device, cache, metadata and crash model, [`fault`] for injectable faults,
//! and [`platform`] for the `Platform` / `Queue` implementation.
//!
//! Same seed and same calls give the same run, byte for byte
//! ([`world::World::trace_hash`]).

#![deny(warnings)]
#![forbid(unsafe_code)]

pub mod fault;
pub mod platform;
pub mod rng;
pub mod world;

pub use fault::{FaultPlan, FlushFailure};
pub use platform::{SimDir, SimFile, SimLock, SimPlatform, SimQueue};
pub use world::{CrashMode, DeviceKind, SimConfig, World};
