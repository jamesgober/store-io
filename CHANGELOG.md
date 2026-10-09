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
