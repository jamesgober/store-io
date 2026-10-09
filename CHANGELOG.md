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
- `store-io-engine`: the batch layer. `AppendBatch` packs records back to
  back into pooled aligned buffers as they are added (one copy, or none with
  `append_with`, which lends a zeroed slice to encode in place) and commits
  with one reservation, the fewest device writes kept in flight together, and
  one barrier: 100 records of 64 bytes are one write and one flush.
  `PageBatch` checks writes as they are added, refuses overlaps, and submits
  them together before one barrier. Both report exact positions.
- `store-io-engine`: appends and page writes of any length; data larger
  than the largest pooled buffer is written in pieces kept in flight together,
  reusing at most eight buffers.
- `store-io-engine`: appends wait for the append ring to drain instead of
  failing with `TooManyInFlight` (`AppendFrontier::reserve_wait`, woken by
  completions, never by a timer).
- `store-io-engine`: a reservation guard poisons the device domain if an
  append's reserved range is abandoned unwritten, including by a panic.
- `store-io-engine`: raw ordered scans (`AppendRegion::scan`,
  `PageRegion::scan`, `ScanItem`, `ScanSummary`). Eight large direct reads in
  flight, delivered strictly in order to a visitor that decides where valid
  data ends. A scan never stops at bad data: a failing read is bisected to
  whole blocks on an idle queue, unreadable ranges are reported (adjacent ones
  merged) and skipped, and short reads report their missing bytes. Scans work
  on read-only and poisoned stores and change nothing.
- `store-io-engine`: recycle and release, run only when the caller asks.
  `recycle` waits for every operation in flight on the region, writes the
  next generation into the region header, makes it durable, and returns the
  new generation's handle; it does no other I/O. `release` marks the region
  released in the region table, then in its header, then deallocates its
  space, which later provisioning refills before reuse. The table goes first
  so an interrupted release never leaks space. Handles, tickets and positions
  of an old generation or a released region are refused
  (`StaleGeneration`, `NotReady`); `generation()` on region handles.
- `store-io` (facade): the simple API on the native backend, with no
  generics in user code. `Store::create`, `Store::open`, `Store::open_with`
  and `Store::open_readonly` take a path; the store derefs to the engine for
  regions, slots, batches, scans, recycle, release, reservations and the
  report. `AppendRegion`, `PageRegion`, `Slot`, `AppendBatch`, `PageBatch`
  and `Reservation` name the native types; `Directory::open` opens a
  directory-mode handle; `probe` reports the evidence and class of a
  directory's device. Every error and evidence type is re-exported.
  `NativePlatform` is the Windows backend on Windows and the Linux backend on
  Linux.
- `store-io-conformance` crate: the suite every platform backend must pass,
  on any file system. Fifteen scenarios: the platform contract (exclusive
  create and not-found mapping, sizes, many operations in flight completing
  once each with their buffers, durable writes and flushes, queue limits,
  read-only handles, rename, unlink and directory sync, the ownership lock,
  range state after a full write, release, the probe listing what it could
  not read) and store behaviour on the platform (round trip through a
  reopen, a read-only open that changes nothing, ownership, directory
  replace). It passes on the simulator, NTFS and ext4.
- CI: conformance and end-to-end tests on loop-mounted ext4 and XFS images
  with barriers on (`STORE_IO_TEST_DIR`).
- `store-io-posix`: `PosixPlatform` is `Clone` (clones share the retry
  counters).
- `store-io-engine`: compile-fail suite (rustdoc `compile_fail` tests, no
  third-party harness): an integer never type-checks as a position, a
  position or receipt cannot be built by hand, a ticket is not a position and
  a receipt is not a ticket, a ticket cannot be copied, and dropping a ticket
  unused is flagged. Each case has a compiling twin.
- CI: a loom job model-checks the buffer pool's free lists, the append
  frontier, the flush domain and the region gate.
- `docs/GUIDE.md`: a walk through store-io from the first durable write to
  recovery (receipts, regions, batches, many writers, slots and directory
  files, scans and `resume_at`, the device report, every error and what to
  do, reservations, recycle and release, tuning).
- `docs/FORMAT.md`: the on-disk format of a store, precise enough for an
  independent reader (byte layouts, CRC coverage, validation order, winner
  rules, fill pattern, crash consistency of every change).
- `docs/PLATFORMS.md`: what store-io does on Windows and Linux, the
  durability primitive behind each operation, and known limits.
- `docs/DURABILITY.md`: classes, evidence, every reason and missing item,
  trust and labels, the flush domain and join rule, `covers()`, fail-stop.
- `README.md`: status, quick start and the documentation index.
- `docs/API.md`: every public item, grouped by purpose (replaces the 0.1
  placeholder).
- `store-io`: re-exports `RegionKind`, `DomainStats`, `CertId`, `Reason`,
  `Missing` and their sets, and `DurableOpen`.
- `store-io` binary (feature `cli`): `store-io probe <dir>` and
  `store-io info <store>` (read-only).
- End-to-end tests of the simple API on the machine's real file system
  (NTFS, ext4); where a device is refused they use the labelled override and
  check that every receipt carries it.
- `store-io-engine`: directory mode. `Directory::replace` swaps a whole
  file atomically and durably (temporary file in the same directory, exact
  length, full flush, rename over the target, directory flush); after any
  crash the file holds exactly the old or the new bytes. A failure from the
  rename on, or a failed write or flush of the new bytes, is
  `DurabilityUnknown` and poisons the handle; an unsupported directory flush
  is never success. `read` (direct I/O), `remove` (durable) and `sync`. The
  device is probed with the first file and refused per the caller's `Trust`.
- `store-io-platform`: `Platform::set_len` (exact file size; `ftruncate` on
  Linux, end-of-file information on Windows).
- `store-io-core`: `errno::is_already_exists`, judged per numbering.
- `store-io-core`: `errno::is_not_found`, judged per numbering (errno,
  Win32, NTSTATUS); the engine no longer treats errno 3 as "not found".
- `store-io-sim`: `FaultPlan::fail_dir_sync` and `World::dir_syncs()`.
- `store-io-engine`: space reservations and accounting. `Store::reserve`
  checks the tag's hard cap (`Store::set_cap`) and grows the container so that
  regions provisioned from the `Reservation` can never run out of space; it
  is the only place a full file system or quota surfaces, as
  `NoSpace { tag }`, and nothing is poisoned. Provisioning without a
  reservation grows the container past every outstanding reservation, so it
  never takes space promised to another tag. `Store::space` reports the
  container, ready and released regions, reservations, and logical and
  physical usage per tag; `Store::extent_of` gives a region's footprint.
- `store-io-format`: region table entries record the caller's
  space-accounting tag (4 of the 16 reserved bytes), so per-tag usage is
  derived again on every open.
- `store-io-engine`: per-region admission gate. Every data operation holds a
  pass for its I/O; recycle and release retire the region and wait for the
  last pass, so no write of an old generation lands after its successor
  exists. Read-modify-write ordering, proven with loom.
- `store-io-engine`: `Debug` for `Store`, the region handles and batches.
- `store-io-core`: `errno::is_media_error` tells device read failures apart
  from bad requests (errno, Win32 and NTSTATUS codes).
- `store-io-sim`: `World::reads()`, `World::file_id()` and
  `FaultPlan::short_read`.
- `store-io-sim`: `World::writes()` for arming write faults; data transfers
  must meet direct-I/O alignment (offset and length in logical blocks, buffer
  4096-aligned) or complete with `EINVAL`, as on real devices.
- `store-io-core`: `NotWrittenCause::Overlap`.
- `store-io-core`: `NotWrittenCause::NotPositioned`.
- `store-io-core`: `OsError` implements `std::error::Error`, so raw
  platform results work with `?` in callers returning boxed errors.
- `store-io-sim`: `World::flushes()` for arming flush faults.
- `store-io-buf`: loom-aware spin hint in the free-list retry loops.
- Workspace-wide package metadata and the REPS lint set as `[workspace.lints]`.
- CI packages every crate in dependency order (`cargo package --workspace`).
- `dev/ROADMAP.md`: v0.2 and v0.3 exit criteria marked with what is verified and what remains.
- `dev/DIRECTIVES.md`: the simple-API rule (simple, batch and engine layers;
  `docs/GUIDE.md` tutorial) and the rule that a lone durable write, concurrent
  writers and caller batches are all fast and all gated.
- `dev/ROADMAP.md`: performance gates for the lone-writer, concurrent-writer
  and caller-batch patterns; the phase 1 API sketch is layered.
- `dev/TODO.md`: the later list (hardware, measurements, settings changes,
  in-house replacements for `thiserror` and `zeroize`, design gaps).

### Changed

- `store-io-engine`: receipts are exact. A ticket records the first device
  flush that can cover its write and a receipt the flush its barrier waited
  for; `DurableReceipt::covers` holds exactly when the write completed before
  that flush (or, for append regions, lies in the durable prefix). Tickets and
  receipts from different opens of a store never match.
- `store-io-core`: `Op`, `NotWrittenCause`, `CorruptionKind` and `Capability`
  are `#[non_exhaustive]`.
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
- `dev/ROADMAP.md`: Linux CI conformance runs on loop-mounted ext4 and XFS
  images with barriers on; the probe found that GitHub's Ubuntu runners mount
  their root filesystem `nobarrier`, which store-io classes as unsafe.

### Fixed

- `store-io-win`: `allocate` to a smaller size truncated the file, unlike the
  other backends; it now never shrinks, as the platform contract says (found
  by the conformance suite).
- `store-io-engine`: with keyed fill, the first chunk of every region (about
  1 MiB) was written as zeros instead of the pattern; the pattern now starts
  at the first byte of the data area.
- `store-io-engine`: provisioning counted only ready entries against the
  region table's capacity and sized the table encoding for the whole slot,
  so a table near full failed in the middle of provisioning and poisoned the
  store. Released entries now count, and a full table is refused with
  `NoSpace(Table)` before any I/O.
- `store-io-engine`: a metadata write that failed before reaching the
  device poisoned the store; only failures after submission poison now.
- `store-io-engine`: a region skipped at open (its header failed to verify)
  left its name free, and reusing the name made the table undecodable; names
  are now checked against every ready table entry.
- `store-io-engine`: a slot pair carrying a read-only-compatible feature
  this version does not know is refused (`Unsupported(FormatVersion)`)
  instead of overwritten.
- `store-io-engine`: release deallocated the region's header together with
  its data, undoing the Released record; only the data area is released now.
  On Windows release trims in place instead of making the container sparse,
  which had cost the store its power-safe class for good.
- `store-io-engine`: a crash during `create` left a container that would not
  open and blocked every later `create`; `create` now takes over a container
  that never finished (unlocked, no larger than the metadata area, no volume
  record) and refuses anything else.
- `store-io-engine`: a flush refused without an OS error was reported with
  an invented error code; it now carries none.
- `store-io-sim`: a read wholly past the end of a file panicked instead of
  transferring nothing.
- Doc comments that promised more than the code does (`UntrustedBus`,
  `UserPowerProtection`, the table's entry limit, fuzzing of the format
  crate).
- `store-io-engine`: concurrent durable writers no longer queue behind one
  another's flushes. A barrier's followers held an I/O queue for the whole
  flush wait, and a caller finding every queue busy blocked on one fixed
  queue; now only the flush leader borrows a queue, for the flush alone, and
  a caller waits for whichever queue is returned first (found by the
  benchmark harness with 64 writers).
- `store-io-engine`: an append or page write larger than the largest pooled
  buffer failed with `PoolExhausted`; a page read did not check the
  position's volume.
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
