# store-io &mdash; Roadmap

> Path from scaffold to a certified 1.0. Hard parts are front-loaded; each phase has hard exit criteria, and every exit gate needs the maintainer's approval.
>
> **Anti-deferral rule:** no listed task moves to a later phase unless this file records the move and the reason.

---

## v0.1.0 &mdash; Scaffold (DONE)

- [x] Repository, crate name reserved, dual license, REPS, directives, CI (Linux, macOS, Windows on stable and MSRV), audit and deny.
- [x] No public API: the surface is designed in phase 1 and lands from phase 2.

---

## Phase 0 &mdash; Research (DONE)

Catalogue every file-management and I/O method on every target platform, source or measure its real cost and guarantee, then choose the best mechanism for every (platform x device class x operation) cell.

Research tracks, one note each:

| Track | Scope |
|---|---|
| R1 | Linux interfaces: pread/pwrite families and every `RWF_*` flag, O_DIRECT/O_DSYNC, fsync/fdatasync/sync_file_range/syncfs, io_uring setup flags, registered buffers and files, linked SQEs, MSG_RING, `uring_cmd`, io-wq, every fallocate mode, statx, FIEMAP, discard and zero-out, copy and clone, openat2, O_TMPFILE, renameat2, OFD locks, cachestat |
| R2 | Windows interfaces: CreateFile flags, overlapped I/O and IOCP, IoRing versions and per-op support, FlushFileBuffers and every NtFlushBuffersFileEx mode, file-information classes, sparse/zero/trim FSCTLs, ReFS block clone, storage IOCTLs, LockFileEx |
| R3 | macOS interfaces: F_FULLFSYNC, F_BARRIERFSYNC, F_NOCACHE, F_PREALLOCATE, F_PUNCHHOLE, clonefile, renamex_np, APFS semantics |
| R4 | Device features: NVMe VWC, FUA, Flush scope, atomic-write fields, LBA formats, Write Zeroes, Deallocate, FDP, ZNS, SMART; SATA/SAS; power-loss protection and drives that lie; cloud block volumes; persistent memory and CXL |
| R5 | Submission engines: thread pools, libaio, io_uring per kernel tier, NVMe passthrough, SPDK, IOCP, IoRing, macOS; QD1 latency, IOPS per core, CPU per I/O, privilege, availability |
| R6 | Durability semantics per filesystem and stacked layer, fsync error semantics, directory and rename durability, virtualisation |
| R7 | Buffer management: alignment, registration limits, huge pages, NUMA, owned-buffer completion models, DMA safety under cancellation, sensitive buffers |
| R8 | Space management: preallocation, pre-zeroing, write-zeroes, recycling, hole punching, discard, thin provisioning, ENOSPC behaviour |
| R9 | Small-object commits and copy-on-write: rename protocols, A/B headers, double-write, RWF_ATOMIC, reflink |
| R10 | Crash-consistency research and tools: ALICE, CrashMonkey/B3, Chipmunk, LazyFS, dm-log-writes, dm-flakey/error/dust, power-cut rigs |
| R11 | Deterministic simulation: FoundationDB, TigerBeetle VOPR, madsim, shuttle, loom; modelling device caches and crash states |
| R12 | Existing Rust crates: io-uring, rustix, libc, nix, windows-sys, compio, tokio-uring, glommio, monoio, memmap2, fs4, fsys |
| R13 | Prior art: PostgreSQL, SQLite, RocksDB, InnoDB, SQL Server, Seastar, TigerBeetle, LeanStore, DuckDB, ClickHouse, Kafka, etcd/bbolt |

Then a critic pass over every track, and a synthesis with seven matrices: durability primitive, submission engine, provisioning and space, small-object commit, device feature, crash-testing tool, crate.

Progress: all thirteen tracks written; an independent critic pass (a second model) adjudicated every cross-track conflict against kernel and driver source; the synthesis maps every requirement and proposes 37 amendments to the requirements, each awaiting maintainer approval. First measurements on the development box (Windows, consumer NVMe; Linux behaviour under WSL2) are recorded.

Exit criteria:
- [x] Every number labelled.
- [x] Every requirement mapped to a chosen mechanism per platform and device class, with the rejected alternatives and the labelled fallback for each cell (64 fully, 28 pending a measurement, 3 design gaps carried into phase 1).
- [x] Critic findings resolved (dispositions recorded; two overridden by measurement).
- [x] Requirement amendments approved (37, 2026-10-09).
- [x] Baseline measurement plan written, and run where hardware exists (Windows dev box and WSL2). **Moved:** measurements that need bare-metal Linux, a power-loss-protected NVMe, a Mac or the power-cut rig move to [`dev/TODO.md`](./TODO.md). Reason: the maintainer wants a usable release before that hardware exists; those measurements gate the certified 1.0, not usable 0.x releases. Durability claims stay honest in the meantime because every receipt names its device class and evidence.
- Measurement items (all bound to the hardware above): a fio matrix (4 and 16 KiB; write+fdatasync, write+fsync, DSYNC, FUA; O_DIRECT; 1/4/16 jobs) per device class, a null_blk per-core ceiling, idle versus loaded flush cost, and the fsys baseline for every comparable operation.

---

## Phase 1 &mdash; Architecture (DONE)

`PLANNING`, `FILEMAP` (every file, every field), public API sketch in three layers (simple, batch, engine) with the `docs/GUIDE.md` outline, on-disk header formats with versioning, error model, crate layout, instrumentation stage list, harness design.

Exit criteria:
- [x] Scenario review passes: every scenario has an owning file.
- [x] Requirement traceability table (requirement, file, test).
- [x] Independent architecture critique (a second model) resolved: 4 critical and 11 major findings adopted before any engine code.
- [x] **No library code before this gate.**

---

## v0.2 &mdash; Core, simulator, conformance skeleton

Core types, errors and traits; the deterministic simulated backend with the full fault and crash model; the conformance crate skeleton; the harness skeleton with per-stage metrics.

Exit criteria:
- [ ] Compile-fail suite green (no integer, sequence number or forged ticket or receipt type-checks).
- [ ] Same seed, same trace.
- [ ] Every fault class detected.
- [ ] Mutation gate on the reference model.
- [ ] loom and Miri clean.

---

## v0.3 &mdash; Usable: probe, Windows, Linux synchronous tier

The first release a database can build on. Probe with evidence and classes; Windows backend (IOCP, bounded flush pool, rename and directory protocol); Linux synchronous backend (`pwritev2`, O_DIRECT, `RWF_DSYNC`, fdatasync); provisioning, recycle and release; regions; durable writes and barriers with flush sharing; A/B slots; atomic replace; ownership locks; the simple and batch API layers and `docs/GUIDE.md`.

Exit criteria:
- [ ] Conformance green on NTFS (dev box) and ext4 (Linux CI), plus XFS where the runner allows.
  GitHub's Ubuntu runners mount their ext4 root `nobarrier` (found by the probe, 2026-10-09), which store-io classes as Unsafe; Linux CI conformance therefore runs on a loop-mounted ext4 (and XFS) image with barriers on, never on the runner's root filesystem.
- [ ] dm-log-writes and dm-error runs in Linux CI.
- [ ] Barrier within 3% of the raw primitive on the dev box; no timer quantum on any path.
- [ ] Lone-writer, concurrent-writer and caller-batch gates on the dev box; at least 20% better than fsys per comparable operation or proven at the device floor.
- [ ] Certification items (power cuts, power-safe reference box, golden captures from real drives) tracked in [`dev/TODO.md`](./TODO.md).

---

## v0.4 &mdash; Linux io_uring

Ring per thread, registered buffers, direct descriptors, FUA versus flush selection, io-wq tuning.

Exit criteria:
- [ ] io-wq punt gate per (filesystem, kernel) table.
- [ ] 60 s idle: zero syscalls (from 5 s after the last punted operation).
- [ ] Performance gates on the hardware available; power-safe and bare-metal gates tracked in `dev/TODO.md`.

---

## v0.5 &mdash; macOS and the single-file container

F_FULLFSYNC backend; portable container format; unprivileged mode.

Exit criteria:
- [ ] Conformance on APFS (CI runner), NTFS and ext4.
- [ ] Container byte-identical across operating systems.
- [ ] Zero idle wakeups.

---

## v0.6 &mdash; Multi-device, NUMA, QoS

Mirror writes, dependent writes, multi-device barriers, flush-sharing certification, I/O classes, auto-tuned background budget, clone and unshare, many-file mode, streaming copy, prefetch.

Exit criteria:
- [ ] At least 90% of summed per-device fio on the devices available.
- [ ] Commit-class p99 at most 2x idle p99 under background load.
- [ ] Flush-sharing conformance per filesystem and kernel.

---

## v0.7 &mdash; Raw namespace and NVMe passthrough

`uring_cmd`, IOPOLL with a second ring, privilege reporting.

Exit criteria:
- [ ] Same conformance suite.
- [ ] Passthrough performance published.

---

## 1.0 &mdash; Certification

Everything in [`dev/TODO.md`](./TODO.md): power-cut rig (at least 500 cycles per device class, 3,000 per certified model and firmware), power-safe reference box, bare-metal Linux kernel matrix, macOS measurements; fuzz campaigns; SemVer and MSRV policy; supply-chain checks.

Exit criteria:
- [ ] Every MUST requirement green.
- [ ] Rig logs published with the release.

---

## Performance gates (per device class)

Every gate is relative to fio on the same box, kernel, filesystem and flags. Every result names the device, durability class, filesystem, OS and kernel, and fsync status.

| Metric | Power-safe device | Flush-required device |
|---|---|---|
| QD1 4 KiB durable write | p50 within fio + 2 us, p99 within fio + 5 us | within fio with the same primitive + 2 us (Linux) or + 3% (Windows, macOS) |
| Barrier, one writer | completion tracking, 0 syscalls, at most 1 us over completion | raw primitive + 2 us p50 / + 5 us p99 (Linux); + 3% (Windows, macOS); never a timer quantum |
| QD1 durable writes/s | at least 90% of fio | at least 90% of fio write+fdatasync |
| 4 KiB durable random write, QD 32+ | at least 90% of fio per device | syncs/s reported at QD 1, 4 and 16 against fio |
| Per-core submit efficiency | at least 90% of `t/io_uring` IOPS per core on null_blk | same |
| Sequential write, 128 KiB+ | at least 90% of fio | same |
| Scan | at least 85% of fio sequential read | same |
| Hot path | 0 allocations after init, no lock or futex in submit/reap, no timed waits | same |
| Idle, 60 s | 0 syscalls, 0 I/Os, 0 timer wakeups | same |
| QoS | commit-class p99 at most 2x idle p99 under background load | same |
| Observer | compiled out when off; at most 20 ns/op and 5% IOPS when on | same |
| Multi-device | at least 90% of summed per-device fio, up to 4 devices | same |
| Against fsys | at least 20% faster per comparable operation, or proven at the device floor | same |
| Lone writer | one durable write costs one primitive, never waits for company | same |
| Concurrent writers | durable ops/s rises with writer count through flush sharing (no window, no timer) | same |
| Caller batch | N writes + one barrier costs N writes + one primitive | same |
