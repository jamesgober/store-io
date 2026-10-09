<h1 align="center">
    <img width="90px" height="auto" src="https://raw.githubusercontent.com/jamesgober/jamesgober/main/media/icons/hexagon-3.svg" alt="Triple Hexagon">
    <br><b>CHANGELOG</b>
</h1>
<p>
  All notable changes to <code>store-io</code> will be documented in this file. The format is based on <a href="https://keepachangelog.com/en/1.1.0/">Keep a Changelog</a>,
  and this project adheres to <a href="https://semver.org/spec/v2.0.0.html/">Semantic Versioning</a>.
</p>

---

## [Unreleased]

### Added

- `store-io-format` crate (workspace member): CRC-32C with SSE4.2 and
  AArch64 hardware paths on three interleaved streams, recombined with
  compile-time shift tables, plus a slicing-by-8 fallback; verified against
  the standard check value, the RFC 3720 vectors and a bit-at-a-time
  reference at every tail length and alignment.
- `dev/ROADMAP.md`: architecture phase closed (file map, scenario review,
  requirement traceability, independent critique resolved).
- `dev/TODO.md`: Windows ownership-lock kill test.
- `store-io-format`: bounds-checked little-endian codec; the A/B slot
  header (encode and validate in a fixed order: blank, magic, header CRC,
  version, flags, identity, location, payload CRC); slot-pair winner logic
  (confirmed, unconfirmed, fork, refused, lost; generation-gap and chain-break
  anomalies); volume, region-table and region-header payload codecs with
  overlap, duplicate, alignment and id checks; the keyed fill pattern v1.
  Tests include a slot overwrite torn at every byte boundary (never accepted
  as a mixed version) and every single-bit flip (never accepted).
- `store-io-core` crate: identities (`VolumeId`, `RegionId`, `Generation`);
  durability classes, receipt labels and the reason / missing-evidence
  vocabularies; the error model (`NotWritten` and `DurabilityUnknown` never
  conflated, raw OS codes kept, no payload bytes, at most 64 bytes); errno
  classification by stage; the device evidence model; the pure class
  decision (unsafe and unverified rules, power-safe only on device evidence
  or an exact-model certificate, attestation and labelled override); the
  corrected untorn-unit rule; checked alignment helpers.
- `store-io-buf` crate: page-aligned arena (anonymous `mmap` with
  `MADV_DONTFORK` on Linux, `VirtualAlloc` on Windows, the global allocator
  under Miri); power-of-two size classes with lock-free bounded MPMC free
  lists; owned `IoBuf` that returns to its class on drop and never prints its
  contents; sensitive pools that wipe on return and lock their arena in RAM
  and out of core dumps where the OS allows; `wipe` with volatile writes;
  `CachePadded`.
- Workspace-wide package metadata and the REPS lint set as `[workspace.lints]`.
- `dev/DIRECTIVES.md`: the simple-API rule (simple, batch and engine layers;
  `docs/GUIDE.md` tutorial) and the rule that a lone durable write, concurrent
  writers and caller batches are all fast and all gated.
- `dev/ROADMAP.md`: performance gates for the lone-writer, concurrent-writer
  and caller-batch patterns; the phase 1 API sketch is layered.
- `dev/TODO.md`: the later list (hardware, measurements, settings changes,
  in-house replacements for `thiserror` and `zeroize`, design gaps).

### Changed

- `dev/ROADMAP.md`: research phase progress. Thirteen research tracks, a
  critic pass and the synthesis are complete; requirement amendments await
  approval; measurements on bare-metal Linux, a power-loss-protected NVMe and
  macOS are outstanding.
- Hardware-bound measurements move to `dev/TODO.md` (recorded in the roadmap
  per the anti-deferral rule); they gate the certified 1.0, not usable 0.x
  releases.
- `dev/ROADMAP.md`: research phase closed; build order changed so the first
  usable release (v0.3) covers Windows and the Linux synchronous tier, with
  io_uring, macOS, multi-device and passthrough after it and certification at
  1.0.

### Fixed

### Security

---

## [0.1.0] - 2026-10-08

The project scaffold. Reserves the crate name; no public API.

### Added

- Crate manifest (edition 2024, MSRV 1.85, `Apache-2.0 OR MIT`), with the
  facade crate as the workspace root so backend, simulator, conformance and
  harness crates can join as members.
- `src/lib.rs` with the REPS crate-root lint set and the crate overview.
- `README.md`, `CHANGELOG.md`, `REPS.md`, `LICENSE-APACHE`, `LICENSE-MIT`.
- `dev/DIRECTIVES.md` (engineering standards, definition of done, release
  ceremony) and `dev/ROADMAP.md` (research, architecture and build phases
  with exit gates and per-device-class performance gates).
- CI: format, clippy, test and doc on Linux, macOS and Windows, on stable and
  1.85; a package dry run; `cargo audit` and `cargo deny`.
- `rust-toolchain.toml`, `clippy.toml`, `rustfmt.toml`, `deny.toml`,
  `.gitattributes`, `.github/FUNDING.yml`.
- `docs/API.md` and `docs/release/v0.1.0.md`.

[Unreleased]: https://github.com/jamesgober/store-io/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/jamesgober/store-io/releases/tag/v0.1.0
