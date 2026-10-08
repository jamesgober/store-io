<h1 align="center">
    <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br>
    <b>store-io</b>
    <br>
    <sub><sup>DURABLE STORAGE I/O</sup></sub>
</h1>

<div align="center">
    <a href="https://crates.io/crates/store-io"><img alt="Crates.io" src="https://img.shields.io/crates/v/store-io"></a>
    <a href="https://crates.io/crates/store-io"><img alt="Downloads" src="https://img.shields.io/crates/d/store-io?color=%230099ff"></a>
    <a href="https://docs.rs/store-io"><img alt="docs.rs" src="https://img.shields.io/docsrs/store-io"></a>
    <a href="https://github.com/jamesgober/store-io/actions"><img alt="CI" src="https://github.com/jamesgober/store-io/actions/workflows/ci.yml/badge.svg"></a>
    <a href="https://github.com/rust-lang/rfcs/blob/master/text/2495-min-rust-version.md"><img alt="MSRV" src="https://img.shields.io/badge/MSRV-1.85%2B-blue"></a>
</div>

<br>

<div align="left">
    <p>
        store-io is the storage I/O layer for Rust databases and storage engines. It moves caller bytes between memory and media on Linux, Windows and macOS, and proves when they are durable. Each volume is classed from evidence the device itself reports; every durable write or barrier produces an unforgeable receipt; the first failed write or flush stops the device's barrier domain cold. It never frames, parses, truncates or repairs caller data, and it never hides latency behind timers or group-commit windows.
    </p>
    <br>
    <hr>
    <p>
        <strong>MSRV is 1.85+</strong> (Rust 2024 edition).
    </p>
    <blockquote>
        <strong>Status: 0.1 &mdash; scaffold.</strong> This release reserves the crate name. There is no public API yet: the research sweep and the architecture are completed and approved before any library code is written. See the <a href="./dev/ROADMAP.md"><code>ROADMAP</code></a>.
    </blockquote>
</div>

<hr>
<br>

## What it is

store-io replaces [fsys](https://github.com/jamesgober/fsys-rs) as the only code in a database stack that touches files and devices. It is built for:

- **Write-ahead logs and commit layers** &mdash; small, sector-aligned, positioned writes at low queue depth, durable on completion on power-safe devices and with the minimum barrier cost on volatile-cache devices; exact receipts with out-of-order completion; raw, uninterpreted recovery reads.
- **Columnar and page stores** &mdash; large vectored writes, dependent writes across files and devices with at most one flush per device, release on compaction, clone for backup.
- **Buffer pools** &mdash; batched direct reads into caller-owned aligned frames, explicit cancellable prefetch, per-range errors.
- **Backup and replication shipping** &mdash; streaming direct-read copies with bounded memory.
- **Embedded deployments** &mdash; one portable container file, unprivileged, with zero idle wakeups.

## What it is not

- Not a general-purpose file manager. It does not replace `std::fs` for ordinary files.
- Not a log or a database. It has no frames, sequence numbers, payload checksums, commit windows or recovery policy: those belong to the caller.
- Not an async runtime. The core has none; async adapters live in separate crates.

## Design rules

- **Durability classes from evidence.** Power-safe, flush-required, unverified or unsafe, decided per path and per device. Operating-system cache toggles and vendor tables never promote a device.
- **Unforgeable receipts.** Positions, write tickets and receipts are distinct opaque types. A sequence number never type-checks as a byte position, and a barrier is never a silent no-op.
- **Fail-stop.** A failed write or flush poisons the device's barrier domain. A flush is never retried, and success is never reported after a failed one.
- **Opens modify nothing.** No open, scan or lock acquisition ever changes caller bytes, sizes, timestamps or directory entries.
- **No hidden latency.** No timers, no polling threads, no group-commit windows, no idle wakeups.
- **Measured, not claimed.** Every performance number names the device, durability class, filesystem, OS and kernel, and fsync status, and is gated against fio on the same box.

<hr>
<br>

## Installation

```toml
[dependencies]
store-io = "0.1"
```

The `0.1` release contains no public API; depend on it only to follow the project.

<hr>
<br>

## Contributing

See <a href="./dev/DIRECTIVES.md"><code>dev/DIRECTIVES.md</code></a> for engineering standards and the definition of done, and <a href="./REPS.md"><code>REPS.md</code></a> for the Rust Efficiency &amp; Performance Standards. Before a PR: `cargo fmt --all`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and `cargo test --workspace --all-features` must be clean.

<br>

<div id="license">
    <h2>License</h2>
    <p>Licensed under either of</p>
    <ul>
        <li><b>Apache License, Version 2.0</b> &mdash; <a href="./LICENSE-APACHE">LICENSE-APACHE</a></li>
        <li><b>MIT License</b> &mdash; <a href="./LICENSE-MIT">LICENSE-MIT</a></li>
    </ul>
    <p>at your option.</p>
</div>

<div align="center">
  <h2></h2>
  <sup>COPYRIGHT <small>&copy;</small> 2026 <strong>James Gober <me@jamesgober.com>.</strong></sup>
</div>
