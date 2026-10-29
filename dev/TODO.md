# store-io &mdash; Later List

> Work that is parked, not dropped. Nothing here blocks a usable store-io release or the 1.0 API and format freeze; the hardware items are the 1.x certification (ROADMAP, D-120). Each item says what it unlocks. Tick it when done and note the date.

---

## Hardware we do not have yet

A local test box is approved (2026-10-20), not yet bought: a bare-metal Linux machine (8+ cores, 64 GB ECC, PCIe 4.0) with a consumer NVMe boot drive and a separate power-loss-protected NVMe, "power on after AC loss" in its firmware, a smart plug that cuts its power, and the maintainer's workstation as the power-cut controller. When it arrives it covers the first three items below.

- [ ] **Power-loss-protected enterprise NVMe** (for example a used Samsung PM9A3). Unlocks: the power-safe performance gates (durable write in ~6-12 us) and every "power-safe" measurement.
- [ ] **Bare-metal Linux box** (the test box). Unlocks: real Linux latency numbers, kernel-version matrix (5.15, 6.1, 6.6, 6.12, mainline), `blktrace`/`nvme` traces, null_blk CPU ceilings.
- [ ] **Power-cut rig** (the test box, a smart plug cutting its power, the workstation logging acknowledgements over the network and checking them after each reboot). Unlocks: proof that acknowledged writes survive real power loss, 500 / 3,000-cycle certification, and certifying (or ruling out) the fast write-through path on Windows (Windows on a second partition of the test box's boot drive).
- [ ] **Mac measurements** (the maintainer's MacBook, when offered; an external Thunderbolt NVMe if possible). Unlocks: macOS flush costs per model, beyond the CI runners.

## Later opt-ins (need approval when designed)

- [ ] **Evidence attestation for unprivileged stores** (1.x, D-120): a one-time privileged `store-io probe --attest` records the device evidence (SMART capacitor health, identify data) in a root-owned file; an unprivileged store verifies it at open (device identity, age limit) and may then be classed power-safe. Unlocks: the power-safe fast path without the database holding disk privileges.

## Measurements waiting on that hardware

- [ ] Linux: FUA write vs write + flush, per kernel and filesystem (needs bare metal + PLP drive).
- [ ] Linux: raw block device flush counts and latency (bare metal).
- [ ] Linux: flush sharing between concurrent writers (bare metal).
- [ ] Linux: lock release vs a crashed owner's in-flight writes, across kernels (bare metal or VMs).
- [ ] Linux: io_uring wake-up and reap costs; null_blk per-core ceilings (bare metal).
- [ ] Linux: G1 (QD1 4 KiB durable write p50 within the raw primitive + 2 us) on bare metal. On CI's VM it is NOT RESOLVABLE: 20 alternated rounds drift +-5 us against each other. store-io's own cost is 0.1-0.35 us per write (simulator; CI A/B against the raw call on the store's own container, 2026-10-19).
- [ ] Probe golden captures from real drives (bare metal + PLP drive).
- [ ] macOS: flush costs per model, directory flush support (Mac).
- [ ] Windows: does a flush stall writes in the driver or the drive? (ETW timeline; can run on the dev box later).
- [ ] Windows: kill a process with writes in flight and prove the ownership lock is not released until they have landed (can run on the dev box). Until it passes, fencing on Windows rests on the lock alone and is not certified.

## Settings changes James runs and reverts

- [ ] Turn this drive's write cache **off** for one test, then back on (tells us whether the 55 us write-through result is real or the drive ignoring it).
- [ ] Turn the NVMe idle power timeout **off** for one test, then back on (removes wake-up delay from flush measurements).

## Our own replacements for third-party crates

- [ ] **Error-derive crate** (our own `thiserror`): a small macro that writes `Display` and `Error` impls for error enums. store-io hand-writes these impls for now.
- [ ] **Secure-wipe crate** (our own `zeroize`): wipe secret bytes so the compiler cannot skip the wipe (volatile writes + a compiler fence). store-io carries a small internal version for now.

## Found in review (2026-10-12), not yet fixed

- [x] (2026-10-20, v0.5) `errno::is_media_error` matches Linux errno numbers only (61, 74 and 117 mean other things on macOS). Harmless while the posix backend is Linux-only; make it per-OS like `is_unsupported` before the macOS backend (v0.5).
- [ ] A region found unavailable (for example cut off by a short container) has no handle, so it can never be released, and while a truncated one exists the store refuses all growth. The store stays safe but cannot provision again. An explicit, logged repair operation needs a design and James's approval.
- [x] (2026-10-28, v0.5: format 2 in tests/format2_regions.rs and format2_release.rs; format 1 in tests/format1.rs) The `HeaderInvalid`, `HeaderMismatch` and `HeaderReleased` reasons are tested only through their error mapping, never by damaging a real store.

## Supply chain

- [ ] Own `cargo vet` audit of `libc` (a delta from the newest version an imported audit set covers). Until then it is exempted in `supply-chain/config.toml`; `windows-sys` is trusted through its publisher, and only `loom`'s test-only tree is exempted besides.

## Design gaps carried into the architecture phase

- [ ] Streaming copy for backup and replication (sink protocol, bounded memory).
- [x] Finding the exact bad sector when a read fails: scans and reads bisect a failing read down to one logical block (sector) where the probe reports it, else one store block.
- [ ] Keeping commit writes fast while background writes run (store-io's own pacing).

## Found while documenting (2026-10-09), not yet fixed

- [x] (2026-10-28, v0.5) Windows probe: a Storage Spaces disk reports a virtual bus and is classed `Unverified` with the reason `Hypervisor`; it should be a storage-stack layer with its own reason. (Now `StackLayer::Pool`, reason `StoragePool`.)
- [x] (2026-10-28, v0.5) Linux probe: NVMe over fabrics transports are reported as a local NVMe bus. (Now `Bus::Remote`, reason `RemoteTarget`, also for iSCSI and Fibre Channel on Linux and Windows; every controller of a multipath subsystem counted.)
- [x] (2026-10-28, v0.5) Windows probe: Identify Namespace always asks for namespace 1. (Its answer is now used only when the namespace is the disk's size; two namespaces of one size still cannot be told apart.)
- [ ] Defined but never produced: `Error::Fenced` (D-049 makes fencing the OS lock and the acquire barrier, so nothing can raise it; documented as never returned since 2026-10-28): keep or remove it at the 1.0 API freeze. Reserved and documented as such: `Reason::KnownBug` (no rule yet), `StackLayer::Filter` (no backend detects one), `DurableReceipt::untorn()` (false until untorn writes are certified). (`StackLayer::VirtualDisk` is produced, for a macOS disk image.)
- [x] (2026-10-28, v0.5: format 2 checks every region header against its table entry, `check_region` and `Entry2::check_header`; the gap and chain-break flags never change a winner and format 2 does not act on them by its spec, FORMAT.md Part I; format 1 opens read-only, its map from the entry) Open is lenient where it could be strict: a region header's data offset and size are not compared with its table entry; unconfirmed winners and the generation-gap and chain-break flags are accepted silently. (Fixed 2026-10-12: a non-zero volume `features` value or a longer record is refused as `Unsupported`, and the volume record's reserved bytes must be zero.)
- [x] (2026-10-28, v0.5: fixed by the 0.5 audit, `errno::is_lock_held` at every lock; tested with the simulator's `fail_lock` at create, open, read-only open, copy and directory changes) Every lock failure is reported as `Locked`, even when the cause is another error.
- [ ] Released table entries are reused only by a region of exactly the same size: a store that releases many differently sized regions can fill its region table with released entries (`NoSpace(Table)`). Needs merging or compaction of released entries.
- [x] (2026-10-28, v0.5: tests/format2_regions.rs, a region header pair through every call, and a table page or volume record opening read-only) A test that a pair carrying an unknown read-only-compatible flag is never overwritten (needs a slot writer that can set flags).

## Found in v0.4 (2026-10-19), not yet done

- [x] (2026-10-28, v0.5: both formats report success unless the store is poisoned) A release whose table update is durable but whose header rewrite then fails (pool exhausted, a lane that cannot be cleared) returns an error for a release that is committed (since 0.3.0). Report it as done with the header write's failure noted, or retry the header write.
- [x] (2026-10-28, v0.5: the visitor never runs with a shared lane held; an append waiting for room also no longer holds one) A scan visitor that provisions holds a shared lane while it waits for the metadata lock; with every shared lane so held (one is enough with `queues: 1`), provisioning and release, which need a lane under the lock, wait forever (since the visitor-provisioning case existed). Give metadata changes a lane of their own, as the flush leader has.

- [x] (2026-10-20, v0.5) Harness: G2 (barrier p50 + 2 us, p99 + 5 us) compares two consecutive W1 windows, the drift G1 was moved off (gates_g1.rs); give it the same alternation.
- [x] (2026-10-20, v0.5: `append_window`, in bytes) `StoreOptions::append_ring` is counted in blocks, so its default (4096) holds only 2 MiB ahead at 512-byte blocks: a `Submitter` appending 1 MiB eight deep there gets `TooManyInFlight` (G6 raises it). A ring sized in bytes, or a default scaled by the block size.
- [ ] Registered io_uring buffers were dropped (D-107: `READ_FIXED` never completes on Linux 6.5.0 and 6.6.0, and no probe tells kernels apart). Bringing them back needs a safe way to tell a fixed kernel from a broken one and a measured gain; a new roadmap item for the maintainer.
- [ ] Windows: the gates on the dev box are noise-bound (a consumer NVMe whose flush swings 1-4 ms); G2, G6 and one G13b point failed in one run (the G13b point measured at the device floor, three times, when rerun alone; the earlier full run passed G13b 12 of 12). A quieter Windows box (or ETW-timed runs) for a Windows gate of record.

## Performance follow-ups from the harness (2026-10-09)

- [ ] Unbatched small appends: N × `append` + one `sync_through` writes one block-padded write per append (2-9% of fsys on 64-byte records, where fsys buffers appends in memory). `AppendBatch` is the fast path (1.4-4x fsys); the engine layer (0.4) should also let appends be submitted without waiting for each completion.
- [ ] Linux synchronous tier runs batches and large reads one operation at a time (page batches 0.13x fsys at 256 pages on WSL, 8 MiB reads 0.37x raw). Needs io_uring (0.4) or a bounded submit pool / vectored transfers.
- [ ] Region reads copy from the pooled buffer into the caller's (the remaining 1 MiB read gap, about 20%). Lending pooled buffers (engine layer) removes it.
- [ ] Harness: store-io variants always run first in a workload, so they pay for a consumer SSD's recovery after the previous workload's writes (one run measured `append_durable` at 0.12x raw, the same calls split in two at 0.8x moments later). Interleave the systems or idle between workloads.
