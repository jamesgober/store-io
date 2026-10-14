# store-io performance harness: linux / msft-virtual-disk (2026-10-14)

> **OVERRIDE IN EFFECT.** `Store::create` with default trust was **refused** on this device:
> `device durability is unverified: running under a hypervisor without a certificate for this volume type; missing: device identify data (needs a readable device node); SMART / health log (needs privilege on Linux); volatile write cache presence; volatile write cache enabled state; device atomic-write fields; [Unverified { missing: {Identify, Smart, CachePresence, CacheEnabled, AtomicFields}, reasons: {Hypervisor} }]`
>
> Every store in this run was created with the documented labelled override (`Trust { override_refusal: true, .. }`): the class stays as decided, every receipt is labelled `overridden`, and the most conservative primitive (write + device flush) is used. Nothing else was weakened. The numbers below are therefore **not** gate numbers for a certified durability class; they measure software cost on this stack.

## Run

| item | value |
|---|---|
| date (UTC) | 2026-10-14T08:16:11Z |
| command | caller-batch |
| store-io commit | e4f90ecec8fe39c18eccbe385c6a10fe51cfce2c (crates/ modified) |
| OS | Ubuntu 24.04.4 LTS |
| kernel / build | Linux version 6.6.87.2-microsoft-standard-WSL2 (root@439a258ad544) (gcc (GCC) 11.2.0, GNU ld (GNU Binutils) 2.37) #1 SMP PREEMPT_DYNAMIC Thu Jun  5 18:30:46 UTC 2025 |
| CPU | AMD Ryzen 9 9950X3D 16-Core Processor |
| logical CPUs | 32 |
| filesystem | ext4 on / (device /dev/sdd 8:48, mount options rw,relatime, super options rw,discard,errors=remount-ro,data=ordered); store-io classified it Ext4 |
| directory under test | /home/james/store-io-harness-59526 |
| backend | store-io-posix (PosixPlatform, O_DIRECT; tier in the report's io line) |
| durability class | unverified (receipt label overridden(running under a hypervisor without a certificate for this volume type), durable open Overridden (Trust::override_refusal), block size 4096 B) |
| time per point | 1.0 s measured after 0.2 s warm-up (time-boxed; a point may end early at region capacity, noted where it does) |
| fsys | fsys 1.1.3 (crates.io), Handle method Sync, durability primitive fsync |
| raw primitives | pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync; preadv2 on an O_DIRECT descriptor (one call per request) |
| harness build | release, lto=fat, codegen-units=1 (same as store-io's release profile) |
| total wall time | 101.1 s |

## Device report (`Store::report()`)

```text
volume           a4f76e80-757e-6614-2a91-b9cf3f9cae73
io               synchronous (chosen)
class            unverified
label            overridden(running under a hypervisor without a certificate for this volume type)
durable open     Overridden
reason           running under a hypervisor without a certificate for this volume type
missing          device identify data (needs a readable device node)
missing          SMART / health log (needs privilege on Linux)
missing          volatile write cache presence
missing          volatile write cache enabled state
missing          device atomic-write fields
flush execution  inline (leader)
block size       4096
table capacity   253 regions
filesystem       Ext4
bus              Virtual
model            Msft Virtual Disk
mode             read-write
```

## Store options (every store in this run)

```text
StoreOptions {
    trust: Trust {
        certificate: None,
        attestation: None,
        override_refusal: true,
    },
    block_size: None,
    log2_table_slot: 14,
    max_io: 1048576,
    buffers_per_class: 64,
    max_write: 4194304,
    large_buffers: 8,
    queues: 0,
    queue_depth: 64,
    t3_pool_threads: 0,
    append_ring: 4096,
    keyed_fill: false,
    linux_tier: Sync,
}
```

<details><summary>Raw evidence from the probe</summary>

```text
Evidence {
    platform: Linux,
    device: DeviceFacts {
        model: Some(
            "Msft Virtual Disk",
        ),
        firmware: Some(
            "1.0",
        ),
        bus: Virtual,
        cache_present: Unknown,
        cache_enabled: Unknown,
        os_cache_mode: WriteBack,
        os_flush_supported: Yes,
        user_power_protection: Unknown,
        backup_failed: Unknown,
        fua_supported: No,
        logical_block: 512,
        physical_block: 4096,
        optimal_write: 4096,
        max_transfer: 1310720,
        atomic: AtomicFields {
            awupf: None,
            nsabp: false,
            nawupf: 0,
            nabspf: 0,
            nabo: 0,
        },
        numa_node: Some(
            0,
        ),
        multi_controller: false,
    },
    fs: FsFacts {
        kind: Ext4,
        data_journal: false,
        no_barrier: false,
        sync_disabled: false,
        weak_error_mode: false,
        transformed: false,
        cow: No,
        direct_io: Unknown,
        full_flush: Yes,
        dir_flush: Yes,
        os_atomic_unit: 0,
        dio_mem_align: 4,
        dio_offset_align: 512,
    },
    stack: [],
    hypervisor: Some(
        HyperV,
    ),
    privilege: Unprivileged,
    kernel: Some(
        (
            6,
            6,
            87,
        ),
    ),
    timings: Timings {
        flush_idle_ns: None,
        flush_loaded_ns: None,
        fua_extra_ns: None,
        flush_blocks_writes_pct: None,
    },
    missing: {
        Identify,
        Smart,
        CachePresence,
        CacheEnabled,
        AtomicFields,
    },
}
```
</details>

## How to read the numbers

- Every value in the workload tables is **[measured]** by this harness in this run; ratios and percentages are **[derived]** from those measurements.
- Latencies are per operation (per commit for batches), in microseconds, nearest-rank percentiles over every measured operation; p99 needs at least 100 samples and p99.9 at least 1000, otherwise `n/a`.
- Throughput is operations completed inside the measured window divided by the window (wall clock).
- `write calls` / `other calls` are the OS per-process counters (/proc/self/io (syscw: write-family system calls; fdatasync is not counted)), read before and after the measured window; they count system calls, not device commands.
- **store-io advantage**: positive means store-io is better (more throughput, or less latency) by that percentage; negative means worse.

## Summary: store-io vs raw vs fsys

| workload | case | metric | store-io | raw | fsys | store-io ÷ raw | store-io advantage vs raw | store-io ÷ fsys | store-io advantage vs fsys | compared |
|---|---|---|---|---|---|---|---|---|---|---|
| caller-batch | batch of 1 x 64 B, one barrier | records/s (higher is better) | 391.8 | 400.4 | 191.4 | 0.978 | -2.2% | 2.047 | +104.7% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(64 B) + one sync_through | records/s (higher is better) | 400.4 | 396.5 | 187.2 | 1.010 | +1.0% | 2.139 | +113.9% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1 x 512 B, one barrier | records/s (higher is better) | 394.9 | 379.4 | 181.0 | 1.041 | +4.1% | 2.182 | +118.2% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(512 B) + one sync_through | records/s (higher is better) | 403.6 | 381.2 | 161.0 | 1.059 | +5.9% | 2.508 | +150.8% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1 x 4096 B, one barrier | records/s (higher is better) | 391.1 | 397.8 | 183.1 | 0.983 | -1.7% | 2.136 | +113.6% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(4096 B) + one sync_through | records/s (higher is better) | 385.7 | 402.3 | 188.3 | 0.959 | -4.1% | 2.048 | +104.8% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 64 B, one barrier | records/s (higher is better) | 3777.2 | 3764.4 | 1819.8 | 1.003 | +0.3% | 2.076 | +107.6% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(64 B) + one sync_through | records/s (higher is better) | 2512.6 | 2606.5 | 1737.2 | 0.964 | -3.6% | 1.446 | +44.6% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 512 B, one barrier | records/s (higher is better) | 3974.6 | 4067.4 | 1852.9 | 0.977 | -2.3% | 2.145 | +114.5% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(512 B) + one sync_through | records/s (higher is better) | 2742.2 | 2742.1 | 1860.4 | 1.000 | +0.0% | 1.474 | +47.4% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 4096 B, one barrier | records/s (higher is better) | 3942.1 | 4130.2 | 1899.6 | 0.954 | -4.6% | 2.075 | +107.5% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(4096 B) + one sync_through | records/s (higher is better) | 2658.6 | 2769.9 | 1821.2 | 0.960 | -4.0% | 1.460 | +46.0% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 64 B, one barrier | records/s (higher is better) | 37519.1 | 38758.0 | 18411.8 | 0.968 | -3.2% | 2.038 | +103.8% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(64 B) + one sync_through | records/s (higher is better) | 6265.8 | 6347.0 | 18490.1 | 0.987 | -1.3% | 0.339 | -66.1% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 512 B, one barrier | records/s (higher is better) | 38421.0 | 39298.3 | 17556.2 | 0.978 | -2.2% | 2.188 | +118.8% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(512 B) + one sync_through | records/s (higher is better) | 6421.1 | 6845.4 | 17812.0 | 0.938 | -6.2% | 0.360 | -64.0% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 4096 B, one barrier | records/s (higher is better) | 28805.6 | 22989.8 | 9931.9 | 1.253 | +25.3% | 2.900 | +190.0% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(4096 B) + one sync_through | records/s (higher is better) | 6466.2 | 6745.3 | 9942.1 | 0.959 | -4.1% | 0.650 | -35.0% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 64 B, one barrier | records/s (higher is better) | 118972.2 | 410702.1 | 179663.9 | 0.290 | -71.0% | 0.662 | -33.8% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(64 B) + one sync_through | records/s (higher is better) | 7267.2 | 7921.7 | 166630.6 | 0.917 | -8.3% | 0.044 | -95.6% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 512 B, one barrier | records/s (higher is better) | 377345.0 | 302216.2 | 135473.6 | 1.249 | +24.9% | 2.785 | +178.5% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(512 B) + one sync_through | records/s (higher is better) | 7694.1 | 7409.9 | 117072.0 | 1.038 | +3.8% | 0.066 | -93.4% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 4096 B, one barrier | records/s (higher is better) | 107285.2 | 196541.8 | 75598.1 | 0.546 | -45.4% | 1.419 | +41.9% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(4096 B) + one sync_through | records/s (higher is better) | 7573.8 | 7485.7 | 64032.6 | 1.012 | +1.2% | 0.118 | -88.2% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |

## Anomalies, errors and integrity failures

- None: every measurement completed and every integrity check passed.

## W3: caller batch (N records, one barrier) (`caller-batch`)

Wall time: 100.6 s.

**Method.**

- N in [1, 10, 100, 1000], record sizes [64, 512, 4096] bytes; one commit = N records made durable by one barrier; commits run back to back on one thread. Each point ends at the time box or after 3000 measured commits, whichever comes first.
- store-io (a) `N x append + sync_through`: each append is its own write, padded to a block (documented behaviour). (b) `AppendBatch`: records packed back to back into pooled buffers, `commit()` = one reservation, the fewest writes (largest pooled buffer 1 MiB), one barrier. A fresh store per (N, size).
- raw (a') N writes of one zero-padded block each, then one flush; (b') the N records packed into one aligned buffer padded to a block, one write, one flush. Primitive: pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync. A ready 1 GiB file, offsets cycling.
- fsys: `append` x N + `sync_through(last lsn)`, and `append_batch(&records)` + `sync_through`, on a fresh journal per point, window off.
- records/s = commits/s x N; payload MB/s counts caller bytes only (10^6 bytes); `write calls/commit` and `bytes written/commit` come from the OS counters and show the device writes each design issues per commit.

Results:

| N | record B | system | variant | commits/s | records/s | payload MB/s | commit p50 µs | commit p99 µs | write calls/commit | bytes written/commit | flushes/commit |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 400 | 400 | 0.03 | 2364.7 | 4435.7 | 1.00 | 4096 | 1.00 |
| 1 | 64 | store-io | (b) AppendBatch: N x append + commit | 392 | 392 | 0.03 | 2386.6 | 6424.9 | 1.00 | 4096 | 1.00 |
| 1 | 64 | raw | (a') N writes of one padded block + one flush | 396 | 396 | 0.03 | 2396.5 | 4383.8 | 1.00 | 4096 | n/a |
| 1 | 64 | raw | (b') packed: one write of N records + one flush | 400 | 400 | 0.03 | 2392.2 | 4290.4 | 1.00 | 4096 | n/a |
| 1 | 64 | fsys | append x N + sync_through (window off) | 187 | 187 | 0.01 | 4997.4 | 20656.4 | 1.00 | 76 | n/a |
| 1 | 64 | fsys | append_batch(N) + sync_through (window off) | 191 | 191 | 0.01 | 4978.0 | 20996.8 | 1.00 | 76 | n/a |
| 1 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 404 | 404 | 0.21 | 2350.9 | 4229.4 | 1.00 | 4096 | 1.00 |
| 1 | 512 | store-io | (b) AppendBatch: N x append + commit | 395 | 395 | 0.20 | 2391.8 | 6494.1 | 1.00 | 4096 | 1.00 |
| 1 | 512 | raw | (a') N writes of one padded block + one flush | 381 | 381 | 0.20 | 2457.5 | 6661.6 | 1.00 | 4096 | n/a |
| 1 | 512 | raw | (b') packed: one write of N records + one flush | 379 | 379 | 0.19 | 2487.0 | 5511.8 | 1.00 | 4096 | n/a |
| 1 | 512 | fsys | append x N + sync_through (window off) | 161 | 161 | 0.08 | 5555.6 | 22908.2 | 1.00 | 524 | n/a |
| 1 | 512 | fsys | append_batch(N) + sync_through (window off) | 181 | 181 | 0.09 | 5120.0 | 21098.2 | 1.00 | 524 | n/a |
| 1 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 386 | 386 | 1.58 | 2423.9 | 8007.9 | 1.00 | 4096 | 1.00 |
| 1 | 4096 | store-io | (b) AppendBatch: N x append + commit | 391 | 391 | 1.60 | 2383.7 | 6324.6 | 1.00 | 4096 | 1.00 |
| 1 | 4096 | raw | (a') N writes of one padded block + one flush | 402 | 402 | 1.65 | 2369.7 | 4293.3 | 1.00 | 4096 | n/a |
| 1 | 4096 | raw | (b') packed: one write of N records + one flush | 398 | 398 | 1.63 | 2379.6 | 4508.8 | 1.00 | 4096 | n/a |
| 1 | 4096 | fsys | append x N + sync_through (window off) | 188 | 188 | 0.77 | 5121.9 | 20857.0 | 1.00 | 4108 | n/a |
| 1 | 4096 | fsys | append_batch(N) + sync_through (window off) | 183 | 183 | 0.75 | 5235.7 | 20879.4 | 1.00 | 4108 | n/a |
| 10 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 251 | 2513 | 0.16 | 3621.6 | 7968.7 | 10.00 | 40960 | 1.00 |
| 10 | 64 | store-io | (b) AppendBatch: N x append + commit | 378 | 3777 | 0.24 | 2541.2 | 4476.3 | 1.00 | 4096 | 1.00 |
| 10 | 64 | raw | (a') N writes of one padded block + one flush | 261 | 2607 | 0.17 | 3618.7 | 7797.9 | 10.00 | 40960 | n/a |
| 10 | 64 | raw | (b') packed: one write of N records + one flush | 376 | 3764 | 0.24 | 2532.5 | 4931.6 | 1.00 | 4096 | n/a |
| 10 | 64 | fsys | append x N + sync_through (window off) | 174 | 1737 | 0.11 | 5328.6 | 21390.4 | 10.00 | 760 | n/a |
| 10 | 64 | fsys | append_batch(N) + sync_through (window off) | 182 | 1820 | 0.12 | 5257.5 | 21197.8 | 1.00 | 760 | n/a |
| 10 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 274 | 2742 | 1.40 | 3482.1 | 7259.7 | 10.00 | 40960 | 1.00 |
| 10 | 512 | store-io | (b) AppendBatch: N x append + commit | 397 | 3975 | 2.04 | 2374.3 | 5037.6 | 1.00 | 8192 | 1.00 |
| 10 | 512 | raw | (a') N writes of one padded block + one flush | 274 | 2742 | 1.40 | 3470.1 | 7440.2 | 10.00 | 40960 | n/a |
| 10 | 512 | raw | (b') packed: one write of N records + one flush | 407 | 4067 | 2.08 | 2351.8 | 4120.2 | 1.00 | 8192 | n/a |
| 10 | 512 | fsys | append x N + sync_through (window off) | 186 | 1860 | 0.95 | 5109.4 | 20818.3 | 10.00 | 5240 | n/a |
| 10 | 512 | fsys | append_batch(N) + sync_through (window off) | 185 | 1853 | 0.95 | 5139.5 | 20963.1 | 1.00 | 5240 | n/a |
| 10 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 266 | 2659 | 10.89 | 3531.6 | 9361.0 | 10.00 | 40960 | 1.00 |
| 10 | 4096 | store-io | (b) AppendBatch: N x append + commit | 394 | 3942 | 16.15 | 2416.8 | 4967.9 | 1.00 | 40960 | 1.00 |
| 10 | 4096 | raw | (a') N writes of one padded block + one flush | 277 | 2770 | 11.35 | 3526.0 | 4716.0 | 10.00 | 40960 | n/a |
| 10 | 4096 | raw | (b') packed: one write of N records + one flush | 413 | 4130 | 16.92 | 2350.6 | 4686.9 | 1.00 | 40960 | n/a |
| 10 | 4096 | fsys | append x N + sync_through (window off) | 182 | 1821 | 7.46 | 5064.6 | 20778.9 | 10.00 | 41080 | n/a |
| 10 | 4096 | fsys | append_batch(N) + sync_through (window off) | 190 | 1900 | 7.78 | 5058.0 | 12541.4 | 1.00 | 41080 | n/a |
| 100 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 63 | 6266 | 0.40 | 15089.3 | n/a | 100.00 | 409600 | 1.00 |
| 100 | 64 | store-io | (b) AppendBatch: N x append + commit | 375 | 37519 | 2.40 | 2477.5 | 7508.1 | 1.00 | 8192 | 1.00 |
| 100 | 64 | raw | (a') N writes of one padded block + one flush | 63 | 6347 | 0.41 | 15015.5 | n/a | 100.00 | 409600 | n/a |
| 100 | 64 | raw | (b') packed: one write of N records + one flush | 388 | 38758 | 2.48 | 2431.0 | 5547.8 | 1.00 | 8192 | n/a |
| 100 | 64 | fsys | append x N + sync_through (window off) | 185 | 18490 | 1.18 | 5161.0 | 20990.9 | 100.00 | 7600 | n/a |
| 100 | 64 | fsys | append_batch(N) + sync_through (window off) | 184 | 18412 | 1.18 | 5145.5 | 21122.9 | 1.00 | 7600 | n/a |
| 100 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 64 | 6421 | 3.29 | 14867.1 | n/a | 100.00 | 409600 | 1.00 |
| 100 | 512 | store-io | (b) AppendBatch: N x append + commit | 384 | 38421 | 19.67 | 2509.2 | 6359.9 | 1.00 | 53248 | 1.00 |
| 100 | 512 | raw | (a') N writes of one padded block + one flush | 68 | 6845 | 3.50 | 14415.4 | n/a | 100.00 | 409600 | n/a |
| 100 | 512 | raw | (b') packed: one write of N records + one flush | 393 | 39298 | 20.12 | 2477.5 | 4543.8 | 1.00 | 53248 | n/a |
| 100 | 512 | fsys | append x N + sync_through (window off) | 178 | 17812 | 9.12 | 5406.4 | 8154.2 | 100.00 | 52400 | n/a |
| 100 | 512 | fsys | append_batch(N) + sync_through (window off) | 176 | 17556 | 8.99 | 5604.0 | 12929.0 | 1.00 | 52400 | n/a |
| 100 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 65 | 6466 | 26.49 | 14861.2 | n/a | 100.00 | 409600 | 1.00 |
| 100 | 4096 | store-io | (b) AppendBatch: N x append + commit | 288 | 28806 | 117.99 | 2586.3 | 7814.4 | 1.00 | 409600 | 1.00 |
| 100 | 4096 | raw | (a') N writes of one padded block + one flush | 67 | 6745 | 27.63 | 14538.6 | n/a | 100.00 | 409600 | n/a |
| 100 | 4096 | raw | (b') packed: one write of N records + one flush | 230 | 22990 | 94.17 | 2842.8 | 19211.8 | 1.00 | 409600 | n/a |
| 100 | 4096 | fsys | append x N + sync_through (window off) | 99 | 9942 | 40.72 | 9179.9 | 16890.5 | 100.00 | 410800 | n/a |
| 100 | 4096 | fsys | append_batch(N) + sync_through (window off) | 99 | 9932 | 40.68 | 7807.7 | 48509.1 | 1.00 | 410800 | n/a |
| 1000 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 7 | 7267 | 0.47 | 137059.7 | n/a | 1000.00 | 4096000 | 1.00 |
| 1000 | 64 | store-io | (b) AppendBatch: N x append + commit | 119 | 118972 | 7.61 | 7956.1 | 16976.9 | 1.00 | 65536 | 1.00 |
| 1000 | 64 | raw | (a') N writes of one padded block + one flush | 8 | 7922 | 0.51 | 124837.1 | n/a | 1000.00 | 4096000 | n/a |
| 1000 | 64 | raw | (b') packed: one write of N records + one flush | 411 | 410702 | 26.28 | 2393.7 | 4082.6 | 1.00 | 65536 | n/a |
| 1000 | 64 | fsys | append x N + sync_through (window off) | 167 | 166631 | 10.66 | 5792.1 | 11505.0 | 1000.00 | 76000 | n/a |
| 1000 | 64 | fsys | append_batch(N) + sync_through (window off) | 180 | 179664 | 11.50 | 5458.8 | 8971.7 | 1.00 | 76000 | n/a |
| 1000 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 8 | 7694 | 3.94 | 127755.3 | n/a | 1000.00 | 4096000 | 1.00 |
| 1000 | 512 | store-io | (b) AppendBatch: N x append + commit | 377 | 377345 | 193.20 | 2578.2 | 4636.9 | 1.00 | 512000 | 1.00 |
| 1000 | 512 | raw | (a') N writes of one padded block + one flush | 7 | 7410 | 3.79 | 127140.9 | n/a | 1000.00 | 4096000 | n/a |
| 1000 | 512 | raw | (b') packed: one write of N records + one flush | 302 | 302216 | 154.73 | 2592.2 | 9234.5 | 1.00 | 512000 | n/a |
| 1000 | 512 | fsys | append x N + sync_through (window off) | 117 | 117072 | 59.94 | 8989.2 | 25489.8 | 1000.00 | 524000 | n/a |
| 1000 | 512 | fsys | append_batch(N) + sync_through (window off) | 135 | 135474 | 69.36 | 6176.4 | 22282.6 | 1.00 | 524000 | n/a |
| 1000 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 8 | 7574 | 31.02 | 127288.4 | n/a | 1000.00 | 4096000 | 1.00 |
| 1000 | 4096 | store-io | (b) AppendBatch: N x append + commit | 107 | 107285 | 439.44 | 9281.9 | 17142.7 | 4.00 | 4096000 | 1.00 |
| 1000 | 4096 | raw | (a') N writes of one padded block + one flush | 7 | 7486 | 30.66 | 124193.5 | n/a | 1000.00 | 4096000 | n/a |
| 1000 | 4096 | raw | (b') packed: one write of N records + one flush | 197 | 196542 | 805.04 | 4761.4 | 9258.5 | 1.00 | 4096000 | n/a |
| 1000 | 4096 | fsys | append x N + sync_through (window off) | 64 | 64033 | 262.28 | 15671.1 | n/a | 1000.00 | 4108000 | n/a |
| 1000 | 4096 | fsys | append_batch(N) + sync_through (window off) | 76 | 75598 | 309.65 | 13078.2 | n/a | 1.00 | 4108000 | n/a |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | (a) N x AppendRegion::append + sync_through | passed: 482 of 482 records read back and compared byte for byte; all 482 checked for overlap and alignment | provisioned a 64 MiB append region in 0.05 s (1320 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 475 of 475 records read back and compared byte for byte; all 475 checked for overlap and alignment | provisioned a 64 MiB append region in 0.03 s (2331 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 225 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 229 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 482 of 482 records read back and compared byte for byte; all 482 checked for overlap and alignment | provisioned a 64 MiB append region in 0.05 s (1328 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 469 of 469 records read back and compared byte for byte; all 469 checked for overlap and alignment | provisioned a 64 MiB append region in 0.03 s (2014 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 195 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 220 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 466 of 466 records read back and compared byte for byte; all 466 checked for overlap and alignment | provisioned a 64 MiB append region in 0.05 s (1345 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 470 of 470 records read back and compared byte for byte; all 470 checked for overlap and alignment | provisioned a 64 MiB append region in 0.03 s (2052 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 224 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 223 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 3040 records read back and compared byte for byte; all 3040 checked for overlap and alignment | provisioned a 235 MiB append region in 0.11 s (2304 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 4570 records read back and compared byte for byte; all 4570 checked for overlap and alignment | provisioned a 64 MiB append region in 0.04 s (1771 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 2110 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 2190 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 3290 records read back and compared byte for byte; all 3290 checked for overlap and alignment | provisioned a 235 MiB append region in 0.14 s (1732 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 4730 records read back and compared byte for byte; all 4730 checked for overlap and alignment | provisioned a 64 MiB append region in 0.03 s (2200 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 2270 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 2220 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 3190 records read back and compared byte for byte; all 3190 checked for overlap and alignment | provisioned a 235 MiB append region in 0.09 s (2698 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 4710 records read back and compared byte for byte; all 4710 checked for overlap and alignment | provisioned a 235 MiB append region in 0.08 s (3252 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 2210 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 2310 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 7700 records read back and compared byte for byte; all 7700 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.66 s (3252 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 44400 records read back and compared byte for byte; all 44400 checked for overlap and alignment | provisioned a 64 MiB append region in 0.03 s (2308 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 22300 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 22400 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 7800 records read back and compared byte for byte; all 7800 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.71 s (3005 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 45300 records read back and compared byte for byte; all 45300 checked for overlap and alignment | provisioned a 305 MiB append region in 0.11 s (2882 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 21200 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 21300 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 7900 records read back and compared byte for byte; all 7900 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.63 s (3394 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 35900 records read back and compared byte for byte; all 35900 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.67 s (3202 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 11800 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 12000 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 10000 records read back and compared byte for byte; all 10000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.73 s (2948 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 157000 records read back and compared byte for byte; all 157000 checked for overlap and alignment | provisioned a 375 MiB append region in 0.15 s (2671 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 201000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 214000 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 10000 records read back and compared byte for byte; all 10000 checked for overlap and alignment | provisioned a 2048 MiB append region in 1.05 s (2047 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 442000 records read back and compared byte for byte; all 442000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.98 s (2198 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 141000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 159000 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 10000 records read back and compared byte for byte; all 10000 checked for overlap and alignment | provisioned a 2048 MiB append region in 1.00 s (2157 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 124000 records read back and compared byte for byte; all 124000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.91 s (2353 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 79000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 89000 records replayed with JournalReader and compared byte for byte |  |

