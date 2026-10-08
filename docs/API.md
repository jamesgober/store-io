# store-io &mdash; API Reference

> **Status: 0.1 &mdash; no public API.** This document becomes the offline mirror of the rustdoc on [docs.rs](https://docs.rs/store-io) as the public surface lands.

The public surface is designed in the architecture phase and lands from the next phase onward (see [`dev/ROADMAP.md`](../dev/ROADMAP.md)). Its shape is fixed by these rules, which every item added here will follow:

- **One submitter per thread.** Submission and completion are polled (`submit`, `reap`), with an optional blocking `wait` and an optional driver thread per device. Completions carry a caller `u64` tag. The core depends on no async runtime.
- **Owned buffers.** An aligned I/O buffer moves into a write or read and comes back in that operation's completion. The caller cannot touch it while it is in flight, and dropping a submitter never frees memory the kernel still uses.
- **Typed positions and receipts.** Volume and region identities, generations, positions, write tickets and durability receipts are distinct opaque types with no public integer constructors. A region converts a byte offset into a position only after checking alignment and bounds.
- **Barriers return receipts.** A barrier completes with a receipt that names the durability class, the evidence label, the generation and the durable frontier. A barrier on an already-durable target returns the existing receipt and is counted, never silently skipped.
- **Structured errors.** Every error carries the raw OS error, the operation, the volume, the region and the byte range, and distinguishes "not written" (media provably untouched) from "durability unknown" (the device's barrier domain is poisoned).

## Stability

`0.x` releases make no compatibility promise. The SemVer promise starts at `1.0`.
