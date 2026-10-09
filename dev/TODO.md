# store-io &mdash; Later List

> Work that is parked, not dropped. Nothing here blocks a usable store-io release; everything here blocks the *certified* 1.0. Each item says what it unlocks. Tick it when done and note the date.

---

## Hardware we do not have yet

- [ ] **Power-loss-protected enterprise NVMe** (for example a used Samsung PM9A3, ~$100-200). Unlocks: the power-safe performance gates (durable write in ~6-12 us) and every "power-safe" measurement.
- [ ] **Bare-metal Linux box** (any machine that boots Linux directly, not WSL). Unlocks: real Linux latency numbers, kernel-version matrix (5.15, 6.1, 6.6, 6.12, mainline), `blktrace`/`nvme` traces, null_blk CPU ceilings.
- [ ] **Power-cut rig** (relay that cuts the drive's own power + a second controller logging acknowledgements; ~$300-600 in parts, design in the research notes). Unlocks: proof that acknowledged writes survive real power loss, 500 / 3,000-cycle certification, and certifying (or ruling out) the fast write-through path on Windows.
- [ ] **A Mac** (one Apple-silicon machine, plus an external Thunderbolt NVMe if possible). Unlocks: macOS flush costs and macOS gates.

## Measurements waiting on that hardware

- [ ] Linux: FUA write vs write + flush, per kernel and filesystem (needs bare metal + PLP drive).
- [ ] Linux: raw block device flush counts and latency (bare metal).
- [ ] Linux: flush sharing between concurrent writers (bare metal).
- [ ] Linux: lock release vs a crashed owner's in-flight writes, across kernels (bare metal or VMs).
- [ ] Linux: io_uring wake-up and reap costs; null_blk per-core ceilings (bare metal).
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

## Design gaps carried into the architecture phase

- [ ] Streaming copy for backup and replication (sink protocol, bounded memory).
- [x] Finding the exact bad sector when a read fails: scans bisect a failing read down to one store block (LBA-exact where the block is larger than the logical block is still open).
- [ ] Keeping commit writes fast while background writes run (store-io's own pacing).

## Found while documenting (2026-10-09), not yet fixed

- [ ] Windows probe: a Storage Spaces disk reports a virtual bus and is classed `Unverified` with the reason `Hypervisor`; it should be a storage-stack layer with its own reason.
- [ ] Linux probe: NVMe over fabrics transports are reported as a local NVMe bus.
- [ ] Windows probe: Identify Namespace always asks for namespace 1.
- [ ] Defined but never produced: `Reason::KnownBug`, `Error::Fenced`, the `Filter` and `VirtualDisk` stack layers, and `DurableReceipt::untorn()` (always false until untorn writes are certified).
- [ ] Open is lenient where it could be strict: a non-zero volume `features` value reports `Corruption` instead of `Unsupported`; the volume record's reserved bytes are not checked; a region header's data offset and size are not compared with its table entry; unconfirmed winners and the generation-gap and chain-break flags are accepted silently.
- [ ] Every lock failure is reported as `Locked`, even when the cause is another error.
- [ ] Released table entries are reused only by a region of exactly the same size: a store that releases many differently sized regions can fill its region table with released entries (`NoSpace(Table)`). Needs merging or compaction of released entries.
- [ ] A test that a pair carrying an unknown read-only-compatible flag is never overwritten (needs a slot writer that can set flags).

## Performance follow-ups from the harness (2026-10-09)

- [ ] Unbatched small appends: N × `append` + one `sync_through` writes one block-padded write per append (2-9% of fsys on 64-byte records, where fsys buffers appends in memory). `AppendBatch` is the fast path (1.4-4x fsys); the engine layer (0.4) should also let appends be submitted without waiting for each completion.
- [ ] Linux synchronous tier runs batches and large reads one operation at a time (page batches 0.13x fsys at 256 pages on WSL, 8 MiB reads 0.37x raw). Needs io_uring (0.4) or a bounded submit pool / vectored transfers.
- [ ] Region reads copy from the pooled buffer into the caller's (the remaining 1 MiB read gap, about 20%). Lending pooled buffers (engine layer) removes it.
- [ ] Harness: store-io variants always run first in a workload, so they pay for a consumer SSD's recovery after the previous workload's writes (one run measured `append_durable` at 0.12x raw, the same calls split in two at 0.8x moments later). Interleave the systems or idle between workloads.
