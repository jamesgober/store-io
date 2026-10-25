# store-io &mdash; Roadmap

> Path from scaffold to 1.0 (the stable API and container format), then power-cut certification in 1.x. Hard parts are front-loaded; each phase has hard exit criteria, and every exit gate needs the maintainer's approval.
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
- [x] Baseline measurement plan written, and run where hardware exists (Windows dev box and WSL2). **Moved:** measurements that need bare-metal Linux, a power-loss-protected NVMe, a Mac or the power-cut rig move to [`dev/TODO.md`](./TODO.md). Reason: the maintainer wants a usable release before that hardware exists; those measurements gate the 1.x certification, not usable 0.x releases or the 1.0 API and format freeze. Durability claims stay honest in the meantime because every receipt names its device class and evidence.
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
- [x] Compile-fail suite green (no integer, sequence number or forged ticket or receipt type-checks): rustdoc `compile_fail` tests in `store-io-engine`, each with a compiling twin.
- [x] Same seed, same trace (simulator replay test with reordering and random crashes).
- [x] Every fault class detected, where an I/O layer can detect it: failed and short writes, failed flushes (kept or dropped cache), out of space, unreadable ranges and page-cache pollution each have a test. A lying flush, a lost write and a misdirected write are by nature invisible to a layer that does not frame data: they are detected by the caller's checksums, and certified against with the power-cut rig.
- [x] Mutation gate: every engine and simulator mutation in the private mutation list (37 so far, each a plausible bug) is killed by the tests. Not yet automated in CI.
- [x] loom and Miri clean: loom in CI (buffer pool, frontier, flush domain, region gate); Miri in CI on the buffer crate (all `unsafe` outside the platform FFI), clean locally on the format crate's CRC paths.

---

## v0.3 &mdash; Usable: probe, Windows, Linux synchronous tier

The first release a database can build on. Probe with evidence and classes; Windows backend (IOCP, bounded flush pool, rename and directory protocol); Linux synchronous backend (`pwritev2`, O_DIRECT, `RWF_DSYNC`, fdatasync); provisioning, recycle and release; regions; durable writes and barriers with flush sharing; A/B slots; atomic replace; ownership locks; the simple and batch API layers and `docs/GUIDE.md`.

Exit criteria:
- [x] Conformance green on NTFS (dev box) and ext4 (WSL2), plus ext4 and XFS loop images in Linux CI (`store-io-conformance`).
  GitHub's Ubuntu runners mount their ext4 root `nobarrier` (found by the probe, 2026-10-09), which store-io classes as Unsafe; Linux CI conformance therefore runs on a loop-mounted ext4 (and XFS) image with barriers on, never on the runner's root filesystem.
- [x] dm-error run in Linux CI (first run: 582 records acknowledged, the first failure `DurabilityUnknown` with errno 5 and its exact byte range, no success after it, all 582 intact after healing): a file system on a device-mapper target switched mid-run to fail every I/O; no success after the first failure, every acknowledged record intact after healing (`tests/dm.rs`, CI job "Fail-stop under device errors").
  **Moved to v0.4:** the dm-log-writes replay (crash states at every flush mark on a real kernel). Reason: it needs the xfstests `replay-log` tooling built in CI, while the same crash enumeration already runs on the simulator; the maintainer asked for a usable release first. Recorded 2026-10-09.
- [x] Barrier within 3% of the raw primitive on the dev box; no timer quantum on any path.
  - **Windows: met.** QD1 durable 4 KiB append at 1.033x raw ops/s and page overwrite at 1.001x (`harness/results/2026-10-09-windows-t-force-tm8fpz004t-after-fixes-lone.md`).
  - **Linux: moved to v0.4.** Page overwrite p50 789.8 us against 747.2 us raw on WSL2 (`harness/results/2026-10-09-linux-msft-virtual-disk.md`), against a + 2 us Linux gate. Reason: the synchronous tier issues one operation at a time; the io_uring tier and the bounded submit pool are the planned fix. Recorded 2026-10-12.
- [x] Lone-writer, concurrent-writer and caller-batch gates on the dev box; at least 20% better than fsys per comparable operation or proven at the device floor. **Moved to v0.4** for the rows that miss them; reason as above, recorded 2026-10-12. The rows are:
  - N x `append` + one `sync_through` on Windows (0.087x to 0.84x fsys);
  - Linux page batches (0.125x to 0.98x fsys);
  - Windows concurrent writers at 1, 2 and 4 (under +20%).
- [x] Certification items (power cuts, power-safe reference box, golden captures from real drives) tracked in [`dev/TODO.md`](./TODO.md).

---

## v0.4 &mdash; Linux io_uring and the engine layer

**The engine layer.** A per-thread `Submitter` with:
- owned buffers;
- `append`, `write`, `read`, `barrier`, `barrier_all`;
- `submit`, `reap` and `wait` with no timeout;
- `oldest_outstanding`;
- completions carrying the caller's tag.

It runs on every backend, and the simple and batch layers are rebuilt on top of it.

**Linux.** The io_uring tier, built into the Linux backend with store-io's own private io_uring binding (no third-party io_uring crate; Linux depends on `libc` only):
- one ring per thread;
- direct descriptors and registered buffers (registered buffers dropped, D-107: a read into one never completes on at least Linux 6.5.0 and 6.6.0, which no probe detects; recorded 2026-10-19);
- no kernel worker threads, ever: every ring read and write is non-blocking, and anything that would block runs on store-io's own path.

Linux also gets a bounded worker pool for the synchronous tier, and the tier (io_uring or synchronous) is chosen at run time and reported.

**Windows.** The `Submitter` on IOCP. IoRing was evaluated and not adopted: no gain over IOCP, and its flush blocks the submitter.

**Engine-layer flushes** run on one parked flush thread per store.

**Also carried from v0.3:**
- `cargo vet`;
- per-stage instrumentation;
- fuzz targets;
- the mutation list in CI.

Exit criteria (dispositions recorded 2026-10-19, at the v0.4.0 release):
- [x] `cargo vet` passes (CI "Security": audit, deny, vet). The observer is compiled out when off and costs at most 20 ns per operation when on (D-100: 15 ns with all 16 cores busy on Windows; WSL2 14.4 to 15.8). Fuzz targets run a CI smoke (60 s per target). The mutation list runs in CI with no survivor, invalid or missing entry (CI "Mutation").
- [x] No io-wq worker thread exists after any CI workload, on the runner's kernel, on ext4 and XFS (`dev/tools/iowq-gate.sh` in CI, with a fio control that must create them; the VM kernels run the suites, not this gate). Non-blocking fallbacks are counted (`store_io::io_uring_fallbacks()`) and reported. Punts are not allowed at all: every ring read and write carries `RWF_NOWAIT`, and the allowlist admits nothing that always punts.
- [x] dm-log-writes replay in Linux CI on ext4 and XFS: every logged prefix of a store workload, and subsets of the writes cached before each flush (all of them for runs of up to six writes, sampled beyond), every receipt checked (`tests/crash.rs`, CI "Crash replay").
- [x] 60 s idle: zero syscalls, wakeups, timers and I/O on both Linux tiers, from the moment the store is idle (`tests/idle.rs` in CI).
- [x] Conformance green on the io_uring tier (thread-owned and shared rings) and the synchronous tier on ext4 and XFS images, and on IOCP (Windows CI and the dev box), including the `Submitter` scenarios.
- [x] CI kernel matrix: conformance, the end-to-end tests and the Linux backend's tests on 5.15.150 (5.15.3 has no published build; the point release stands for the line), 6.1 and 6.6 in VMs, beside the runner's kernel. The matrix found that a read into an io_uring registered buffer never completes on 6.6.0 (and 6.5.0): registered buffers were dropped (D-107); the direct descriptor stays.
- [x] Performance gates on the hardware available, labelled Unverified (D-066): `harness gates` in CI on a memory-backed device (the gate of record), on WSL2 and on the Windows dev box; relative to the raw primitive and fio, same box and flags. On CI every gate passes or is reported, except G1, which is NOT RESOLVABLE: the VM's rounds drift by +-5 us against a +2 us gate, while store-io's own cost is 0.1-0.35 us per write (`harness/src/workloads/gates_g1.rs`; bare metal in `dev/TODO.md`). Power-safe and bare-metal gates are tracked in `dev/TODO.md`.
- [x] The v0.3 gates moved here (recorded 2026-10-12):
  - Linux barrier within + 2 us of the raw primitive: met (G2 in CI: p50 34.7 against 33.5 us, p99 46.7 against 45.0 us; measured in two consecutive windows, to be re-measured alternated as G1 is, `dev/TODO.md`);
  - N x `append` + one `sync_through` on Windows at least 20% better than fsys, or at the device floor: met (G13b, the dev box: 12 of 12 points at the floor; a later noisy run's one exception measured at the floor when rerun);
  - Linux page batches at least 20% better than fsys, or at the device floor: met (G13a in CI: 7 of 8 at least 20% better, 1 at the floor);
  - Windows concurrent writers at 1, 2 and 4 at least 20% better than fsys, or at the device floor: met (G13c, the dev box: 1 better, 5 at the floor).

---

## v0.5 &mdash; macOS and the single-file container

macOS backend (`F_FULLFSYNC` for every flush; never classed power-safe); container format 2 (a paged region table, 65,536 regions by default; regions that grow in place, fixed size by default, or reserved and capped with `max_size`; any freed space reused; a damaged region made unavailable without closing the store); format-1 stores read-only, with a copy tool; one portable format on every system; unprivileged mode as a tested guarantee.

Exit criteria:
- [x] Conformance on APFS (CI runner), NTFS and ext4. (APFS: the macOS backend, M2-M4; every CI run on macOS stable and 1.85.)
- [x] Containers made by the same operations on Linux, Windows and macOS identical in structure and data (field by field and byte for byte, except the fields that differ between any two creates: ids, times, fill keys), and each opens on every system. (`tests/portable.rs`, on every CI system: the masked container hashes the same, and a container made on Linux opens, reads and writes on all three.)
- [ ] Zero idle wakeups.
- [ ] Every new integrity guard has a mutation entry; the simulator enumerates every crash point of every format-2 change.

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

## 1.0 &mdash; Stable API and format

The SemVer promise starts here: the API and the container format are stable. Proven with the hardware available (CI's Linux kernel matrix, Windows and macOS runners, WSL, the dev box): fuzz campaigns; SemVer and MSRV policy; supply-chain checks. The 1.0 documentation states plainly that no receipt has yet been checked against a real power cut.

Exit criteria:
- [ ] Every MUST requirement green, except those that need the certification hardware, which are listed in 1.x.
- [ ] The durability documentation names what is proven and what waits for 1.x.

**Moved to 1.x (2026-10-20, D-120):** the power-cut rig, the power-safe reference measurements, the bare-metal Linux kernel matrix and the per-model Mac measurements. Reason: the maintainer wants a stable 1.0 on the hardware available; the certification hardware (a local test box) is approved but not yet bought, and certification adds evidence without changing the API or the format, so it fits minor releases. A 2.0 follows only if that testing forces a format or API break.

---

## 1.x &mdash; Certification

Everything in [`dev/TODO.md`](./TODO.md) that needs hardware: power-cut rig (the local test box, a smart plug cutting its power and the maintainer's workstation as controller; at least 500 cycles per device class, 3,000 per certified model and firmware), power-safe reference measurements, bare-metal Linux kernel matrix, macOS measurements per model.

Exit criteria:
- [ ] Rig logs published with the release.
- [ ] The power-safe gates measured on a power-loss-protected drive.

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
