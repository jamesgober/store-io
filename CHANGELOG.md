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
- `store-io-platform` crate: the completion-shaped I/O boundary. `Platform`
  (metadata operations and queue factory) and `Queue` (`submit`, `reap`,
  `wait`, never with a timeout); `IoOp` owns its buffer and the buffer returns
  in the `Completion` or `Rejected`; validated single-component `FileName`;
  a fixed-capacity `CompletionBuf` that never grows on the hot path.
- `store-io-sim` crate: deterministic simulated platform. One device with a
  volatile write cache; a flush persists exactly the writes that completed
  before it was submitted; FUA and power-safe writes are durable at
  completion; directory entries need a directory sync; operations take
  effect at completion and can complete out of order; crashes keep any
  subset (or a seeded random tearing) of cached writes, invalidate handles
  and release locks; injectable faults (failed writes and flushes, flush
  failure that drops the cache, lying flushes, short, lost and misdirected
  writes, latent sector errors, out of space, page-cache pollution); trace
  hash for same-seed replay checks.
- `store-io-engine` crate: the engine over any platform.
  - Flush domain with the join rule and leader-inline flushing: a lone
    durable write costs one flush; concurrent barriers share flushes with no
    timer or window; a flush is issued only after the barrier's ticket, so it
    covers every write that completed before it; a failed flush poisons the
    device forever and is never retried.
  - Lock-free append frontier: atomic tail reservation, out-of-order
    completion, contiguous completed and durable prefixes; ring-slot
    accesses are acquire-release read-modify-writes so no completion is
    stranded under any memory model (found by loom).
  - `Store`: `create` (exclusive, probed, refused on unsafe or unverified
    devices unless attested or overridden, parent directory synced), `open`
    (ownership lock, drain a dead owner's writes, metadata from A/B slots,
    every ready region verified against its header, owner generation bumped),
    `open_readonly` (no write rights, modifies nothing), provisioning with
    direct-I/O fill, full flush, verification and table flip, never-reused
    region ids, the device report, explicit `poison`.
  - `AppendRegion` (store-chosen positions, blocks never rewritten,
    `NotPositioned` after a reopen until `resume_at`), `PageRegion` (checked
    positions, foreign and stale positions rejected), `Slot` (old or new after
    any crash), unforgeable `RegionPos`, `WriteTicket` and `DurableReceipt`.
  - End-to-end simulator tests: receipts survive every crash subset, fsyncgate
    stays poisoned, slots never mix, power-safe barriers never flush, read-only
    opens change nothing.
- `store-io-win` crate: the Windows backend. Unbuffered overlapped I/O
  reaped from a per-queue I/O completion port (`GetQueuedCompletionStatusEx`,
  waits are infinite or zero, never timed); every queue reopens its own handle
  to a file, so any number of queues share a file and each reaps only its own
  completions; synchronous completions skip the port. A durable write is the
  write followed by the data flush (`NtFlushBuffersFileEx` data-sync on NTFS,
  `FlushFileBuffers` elsewhere), never write-through alone. Rename and delete
  by handle with POSIX semantics; an ownership lock on a sentinel byte;
  allocation with valid-data checks; the probe reads the volume, file system,
  storage stack and, on NVMe, the identify data, write-cache feature and
  health log, all parsed as untrusted bytes. Dropping a queue with I/O in
  flight cancels it and waits for every completion before any buffer is
  released.
- `store-io-posix` crate: the Linux synchronous backend (tier T3).
  `O_DIRECT` handles opened relative to their directory with `openat2`
  (no symlinks, no escaping the directory), or `openat` with `O_NOFOLLOW` on
  older kernels; `pwritev2` / `preadv2`, with `RWF_DSYNC` for a durable write on a
  file and write then `fdatasync` on a raw block device; open-file-description
  locks; `fallocate` with size checks; unwritten and shared extents from
  FIEMAP and resident pages from `cachestat`; the probe reads `statx`
  (direct-I/O and atomic-write limits), the file system and its mount
  options, the block stack from sysfs (device-mapper, MD, loop, virtual
  disks, hypervisor detection) and, where permitted, NVMe identify data.
- `store-io-core`: `NotWrittenCause::NotPositioned`.
- `store-io-sim`: `World::flushes()` for arming flush faults.
- `store-io-buf`: loom-aware spin hint in the free-list retry loops.
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

- Lints that only fire on some toolchains and platforms: the free-list
  sequence comparison is a `match` (clippy 1.85), Linux `madvise` calls
  carry their safety comments in dedicated helpers, and the simulator's
  crash path takes the cache with `mem::take`.

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
