# store-io performance harness: linux / msft-virtual-disk (2026-10-14)

> **OVERRIDE IN EFFECT.** `Store::create` with default trust was **refused** on this device:
> `device durability is unverified: running under a hypervisor without a certificate for this volume type; missing: device identify data (needs a readable device node); SMART / health log (needs privilege on Linux); volatile write cache presence; volatile write cache enabled state; device atomic-write fields; [Unverified { missing: {Identify, Smart, CachePresence, CacheEnabled, AtomicFields}, reasons: {Hypervisor} }]`
>
> Every store in this run was created with the documented labelled override (`Trust { override_refusal: true, .. }`): the class stays as decided, every receipt is labelled `overridden`, and the most conservative primitive (write + device flush) is used. Nothing else was weakened. The numbers below are therefore **not** gate numbers for a certified durability class; they measure software cost on this stack.

## Run

| item | value |
|---|---|
| date (UTC) | 2026-10-14T08:14:09Z |
| command | caller-batch |
| store-io commit | e4f90ecec8fe39c18eccbe385c6a10fe51cfce2c (crates/ modified) |
| OS | Ubuntu 24.04.4 LTS |
| kernel / build | Linux version 6.6.87.2-microsoft-standard-WSL2 (root@439a258ad544) (gcc (GCC) 11.2.0, GNU ld (GNU Binutils) 2.37) #1 SMP PREEMPT_DYNAMIC Thu Jun  5 18:30:46 UTC 2025 |
| CPU | AMD Ryzen 9 9950X3D 16-Core Processor |
| logical CPUs | 32 |
| filesystem | ext4 on / (device /dev/sdd 8:48, mount options rw,relatime, super options rw,discard,errors=remount-ro,data=ordered); store-io classified it Ext4 |
| directory under test | /home/james/store-io-harness-59380 |
| backend | store-io-posix (PosixPlatform, O_DIRECT; tier in the report's io line) |
| durability class | unverified (receipt label overridden(running under a hypervisor without a certificate for this volume type), durable open Overridden (Trust::override_refusal), block size 4096 B) |
| time per point | 1.0 s measured after 0.2 s warm-up (time-boxed; a point may end early at region capacity, noted where it does) |
| fsys | fsys 1.1.3 (crates.io), Handle method Sync, durability primitive fsync |
| raw primitives | pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync; preadv2 on an O_DIRECT descriptor (one call per request) |
| harness build | release, lto=fat, codegen-units=1 (same as store-io's release profile) |
| total wall time | 101.1 s |

## Device report (`Store::report()`)

```text
volume           dab54467-4ad7-cd95-8b23-636865b6ead1
io               io_uring (chosen)
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
    linux_tier: Uring,
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
| caller-batch | batch of 1 x 64 B, one barrier | records/s (higher is better) | 1503.7 | 392.6 | 185.4 | 3.830 | +283.0% | 8.112 | +711.2% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(64 B) + one sync_through | records/s (higher is better) | 1557.1 | 1226.3 | 182.7 | 1.270 | +27.0% | 8.521 | +752.1% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1 x 512 B, one barrier | records/s (higher is better) | 385.5 | 387.2 | 178.9 | 0.996 | -0.4% | 2.155 | +115.5% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(512 B) + one sync_through | records/s (higher is better) | 394.0 | 394.5 | 178.1 | 0.999 | -0.1% | 2.212 | +121.2% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1 x 4096 B, one barrier | records/s (higher is better) | 366.0 | 371.4 | 184.8 | 0.985 | -1.5% | 1.980 | +98.0% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(4096 B) + one sync_through | records/s (higher is better) | 367.7 | 368.1 | 174.0 | 0.999 | -0.1% | 2.114 | +111.4% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 64 B, one barrier | records/s (higher is better) | 4015.2 | 3948.4 | 1893.7 | 1.017 | +1.7% | 2.120 | +112.0% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(64 B) + one sync_through | records/s (higher is better) | 2624.9 | 2794.7 | 1848.0 | 0.939 | -6.1% | 1.420 | +42.0% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 512 B, one barrier | records/s (higher is better) | 3868.8 | 3968.3 | 1830.4 | 0.975 | -2.5% | 2.114 | +111.4% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(512 B) + one sync_through | records/s (higher is better) | 2750.7 | 2751.9 | 1816.9 | 1.000 | -0.0% | 1.514 | +51.4% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 4096 B, one barrier | records/s (higher is better) | 3755.6 | 2914.0 | 698.9 | 1.289 | +28.9% | 5.373 | +437.3% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(4096 B) + one sync_through | records/s (higher is better) | 2518.3 | 2706.4 | 838.9 | 0.930 | -7.0% | 3.002 | +200.2% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 64 B, one barrier | records/s (higher is better) | 37135.1 | 37981.1 | 16295.2 | 0.978 | -2.2% | 2.279 | +127.9% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(64 B) + one sync_through | records/s (higher is better) | 4533.3 | 6300.0 | 17128.1 | 0.720 | -28.0% | 0.265 | -73.5% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 512 B, one barrier | records/s (higher is better) | 40388.8 | 41610.2 | 18962.7 | 0.971 | -2.9% | 2.130 | +113.0% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(512 B) + one sync_through | records/s (higher is better) | 5792.1 | 6499.5 | 17948.4 | 0.891 | -10.9% | 0.323 | -67.7% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 4096 B, one barrier | records/s (higher is better) | 34776.4 | 25303.8 | 14765.4 | 1.374 | +37.4% | 2.355 | +135.5% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(4096 B) + one sync_through | records/s (higher is better) | 5732.0 | 6624.4 | 13971.1 | 0.865 | -13.5% | 0.410 | -59.0% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 64 B, one barrier | records/s (higher is better) | 402335.8 | 406725.4 | 172001.9 | 0.989 | -1.1% | 2.339 | +133.9% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(64 B) + one sync_through | records/s (higher is better) | 6991.7 | 7616.5 | 161314.9 | 0.918 | -8.2% | 0.043 | -95.7% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 512 B, one barrier | records/s (higher is better) | 257819.4 | 315347.1 | 168399.9 | 0.818 | -18.2% | 1.531 | +53.1% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(512 B) + one sync_through | records/s (higher is better) | 5443.5 | 7428.6 | 130548.8 | 0.733 | -26.7% | 0.042 | -95.8% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 4096 B, one barrier | records/s (higher is better) | 140022.5 | 159314.0 | 95629.4 | 0.879 | -12.1% | 1.464 | +46.4% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(4096 B) + one sync_through | records/s (higher is better) | 7399.7 | 9353.9 | 67518.2 | 0.791 | -20.9% | 0.110 | -89.0% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |

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
| 1 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 1557 | 1557 | 0.10 | 599.5 | 885.7 | 0.00 | 0 | 1.00 |
| 1 | 64 | store-io | (b) AppendBatch: N x append + commit | 1504 | 1504 | 0.10 | 596.4 | 2607.1 | 0.00 | 0 | 1.00 |
| 1 | 64 | raw | (a') N writes of one padded block + one flush | 1226 | 1226 | 0.08 | 583.5 | 4805.8 | 1.00 | 4096 | n/a |
| 1 | 64 | raw | (b') packed: one write of N records + one flush | 393 | 393 | 0.03 | 2451.9 | 3435.1 | 1.00 | 4096 | n/a |
| 1 | 64 | fsys | append x N + sync_through (window off) | 183 | 183 | 0.01 | 5231.2 | 21425.7 | 1.00 | 76 | n/a |
| 1 | 64 | fsys | append_batch(N) + sync_through (window off) | 185 | 185 | 0.01 | 5186.6 | 21018.8 | 1.00 | 76 | n/a |
| 1 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 394 | 394 | 0.20 | 2428.6 | 4344.9 | 0.00 | 0 | 1.00 |
| 1 | 512 | store-io | (b) AppendBatch: N x append + commit | 386 | 386 | 0.20 | 2442.3 | 4877.6 | 0.00 | 0 | 1.00 |
| 1 | 512 | raw | (a') N writes of one padded block + one flush | 394 | 394 | 0.20 | 2405.9 | 6271.0 | 1.00 | 4096 | n/a |
| 1 | 512 | raw | (b') packed: one write of N records + one flush | 387 | 387 | 0.20 | 2403.1 | 6650.3 | 1.00 | 4096 | n/a |
| 1 | 512 | fsys | append x N + sync_through (window off) | 178 | 178 | 0.09 | 5332.5 | 21079.4 | 1.00 | 524 | n/a |
| 1 | 512 | fsys | append_batch(N) + sync_through (window off) | 179 | 179 | 0.09 | 5374.2 | 20994.1 | 1.00 | 524 | n/a |
| 1 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 368 | 368 | 1.51 | 2554.0 | 6055.6 | 0.00 | 0 | 1.00 |
| 1 | 4096 | store-io | (b) AppendBatch: N x append + commit | 366 | 366 | 1.50 | 2559.6 | 6765.8 | 0.00 | 0 | 1.00 |
| 1 | 4096 | raw | (a') N writes of one padded block + one flush | 368 | 368 | 1.51 | 2523.0 | 7274.2 | 1.00 | 4096 | n/a |
| 1 | 4096 | raw | (b') packed: one write of N records + one flush | 371 | 371 | 1.52 | 2487.1 | 6856.7 | 1.00 | 4096 | n/a |
| 1 | 4096 | fsys | append x N + sync_through (window off) | 174 | 174 | 0.71 | 5372.2 | 21669.2 | 1.00 | 4108 | n/a |
| 1 | 4096 | fsys | append_batch(N) + sync_through (window off) | 185 | 185 | 0.76 | 5140.8 | 20729.3 | 1.00 | 4108 | n/a |
| 10 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 262 | 2625 | 0.17 | 3622.1 | 6552.7 | 0.00 | 0 | 1.00 |
| 10 | 64 | store-io | (b) AppendBatch: N x append + commit | 402 | 4015 | 0.26 | 2363.3 | 4688.2 | 0.00 | 0 | 1.00 |
| 10 | 64 | raw | (a') N writes of one padded block + one flush | 279 | 2795 | 0.18 | 3435.9 | 6216.2 | 10.00 | 40960 | n/a |
| 10 | 64 | raw | (b') packed: one write of N records + one flush | 395 | 3948 | 0.25 | 2369.9 | 7214.7 | 1.00 | 4096 | n/a |
| 10 | 64 | fsys | append x N + sync_through (window off) | 185 | 1848 | 0.12 | 5058.7 | 20738.9 | 10.00 | 760 | n/a |
| 10 | 64 | fsys | append_batch(N) + sync_through (window off) | 189 | 1894 | 0.12 | 5072.6 | 20875.4 | 1.00 | 760 | n/a |
| 10 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 275 | 2751 | 1.41 | 3561.3 | 4327.9 | 0.00 | 0 | 1.00 |
| 10 | 512 | store-io | (b) AppendBatch: N x append + commit | 387 | 3869 | 1.98 | 2375.9 | 6742.2 | 0.00 | 0 | 1.00 |
| 10 | 512 | raw | (a') N writes of one padded block + one flush | 275 | 2752 | 1.41 | 3545.4 | 5389.4 | 10.00 | 40960 | n/a |
| 10 | 512 | raw | (b') packed: one write of N records + one flush | 397 | 3968 | 2.03 | 2366.5 | 6218.0 | 1.00 | 8192 | n/a |
| 10 | 512 | fsys | append x N + sync_through (window off) | 182 | 1817 | 0.93 | 5169.4 | 21030.0 | 10.00 | 5240 | n/a |
| 10 | 512 | fsys | append_batch(N) + sync_through (window off) | 183 | 1830 | 0.94 | 5259.6 | 20838.2 | 1.00 | 5240 | n/a |
| 10 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 252 | 2518 | 10.31 | 3748.8 | 8337.1 | 0.00 | 0 | 1.00 |
| 10 | 4096 | store-io | (b) AppendBatch: N x append + commit | 376 | 3756 | 15.38 | 2520.8 | 5055.0 | 0.00 | 0 | 1.00 |
| 10 | 4096 | raw | (a') N writes of one padded block + one flush | 271 | 2706 | 11.09 | 3657.2 | 4360.9 | 10.00 | 40960 | n/a |
| 10 | 4096 | raw | (b') packed: one write of N records + one flush | 291 | 2914 | 11.94 | 2723.2 | 11316.2 | 1.00 | 40960 | n/a |
| 10 | 4096 | fsys | append x N + sync_through (window off) | 84 | 839 | 3.44 | 10871.6 | n/a | 10.00 | 41080 | n/a |
| 10 | 4096 | fsys | append_batch(N) + sync_through (window off) | 70 | 699 | 2.86 | 12788.8 | n/a | 1.00 | 41080 | n/a |
| 100 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 45 | 4533 | 0.29 | 21653.1 | n/a | 0.00 | 0 | 1.00 |
| 100 | 64 | store-io | (b) AppendBatch: N x append + commit | 371 | 37135 | 2.38 | 2485.5 | 7121.8 | 0.00 | 0 | 1.00 |
| 100 | 64 | raw | (a') N writes of one padded block + one flush | 63 | 6300 | 0.40 | 14958.0 | n/a | 100.00 | 409600 | n/a |
| 100 | 64 | raw | (b') packed: one write of N records + one flush | 380 | 37981 | 2.43 | 2523.6 | 3984.7 | 1.00 | 8192 | n/a |
| 100 | 64 | fsys | append x N + sync_through (window off) | 171 | 17128 | 1.10 | 5476.4 | 21407.5 | 100.00 | 7600 | n/a |
| 100 | 64 | fsys | append_batch(N) + sync_through (window off) | 163 | 16295 | 1.04 | 5458.2 | 21504.5 | 1.00 | 7600 | n/a |
| 100 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 58 | 5792 | 2.97 | 17294.2 | n/a | 0.00 | 0 | 1.00 |
| 100 | 512 | store-io | (b) AppendBatch: N x append + commit | 404 | 40389 | 20.68 | 2398.2 | 4830.1 | 0.00 | 132 | 1.00 |
| 100 | 512 | raw | (a') N writes of one padded block + one flush | 65 | 6499 | 3.33 | 15101.7 | n/a | 100.00 | 409600 | n/a |
| 100 | 512 | raw | (b') packed: one write of N records + one flush | 416 | 41610 | 21.30 | 2360.9 | 3098.0 | 1.00 | 53248 | n/a |
| 100 | 512 | fsys | append x N + sync_through (window off) | 179 | 17948 | 9.19 | 5222.1 | 9492.6 | 100.00 | 52400 | n/a |
| 100 | 512 | fsys | append_batch(N) + sync_through (window off) | 190 | 18963 | 9.71 | 5106.3 | 8776.6 | 1.00 | 52400 | n/a |
| 100 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 57 | 5732 | 23.48 | 16370.5 | n/a | 0.00 | 0 | 1.00 |
| 100 | 4096 | store-io | (b) AppendBatch: N x append + commit | 348 | 34776 | 142.44 | 2738.9 | 4770.2 | 0.00 | 1194 | 1.00 |
| 100 | 4096 | raw | (a') N writes of one padded block + one flush | 66 | 6624 | 27.13 | 14932.5 | n/a | 100.00 | 409600 | n/a |
| 100 | 4096 | raw | (b') packed: one write of N records + one flush | 253 | 25304 | 103.64 | 2768.4 | 10817.5 | 1.00 | 409600 | n/a |
| 100 | 4096 | fsys | append x N + sync_through (window off) | 140 | 13971 | 57.23 | 6187.1 | 16138.8 | 100.00 | 410800 | n/a |
| 100 | 4096 | fsys | append_batch(N) + sync_through (window off) | 148 | 14765 | 60.48 | 5959.1 | 10752.2 | 1.00 | 410800 | n/a |
| 1000 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 7 | 6992 | 0.45 | 143817.1 | n/a | 0.00 | 0 | 1.00 |
| 1000 | 64 | store-io | (b) AppendBatch: N x append + commit | 402 | 402336 | 25.75 | 2435.4 | 4720.7 | 0.00 | 0 | 1.00 |
| 1000 | 64 | raw | (a') N writes of one padded block + one flush | 8 | 7616 | 0.49 | 130991.1 | n/a | 1000.00 | 4096000 | n/a |
| 1000 | 64 | raw | (b') packed: one write of N records + one flush | 407 | 406725 | 26.03 | 2410.0 | 4396.2 | 1.00 | 65536 | n/a |
| 1000 | 64 | fsys | append x N + sync_through (window off) | 161 | 161315 | 10.32 | 5899.1 | 8724.5 | 1000.00 | 76000 | n/a |
| 1000 | 64 | fsys | append_batch(N) + sync_through (window off) | 172 | 172002 | 11.01 | 5545.8 | 9937.1 | 1.00 | 76000 | n/a |
| 1000 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 5 | 5444 | 2.79 | 167417.0 | n/a | 0.00 | 0 | 1.00 |
| 1000 | 512 | store-io | (b) AppendBatch: N x append + commit | 258 | 257819 | 132.00 | 2960.4 | 9707.9 | 0.00 | 0 | 1.00 |
| 1000 | 512 | raw | (a') N writes of one padded block + one flush | 7 | 7429 | 3.80 | 131335.9 | n/a | 1000.00 | 4096000 | n/a |
| 1000 | 512 | raw | (b') packed: one write of N records + one flush | 315 | 315347 | 161.46 | 2534.9 | 7159.8 | 1.00 | 512000 | n/a |
| 1000 | 512 | fsys | append x N + sync_through (window off) | 131 | 130549 | 66.84 | 8257.6 | 13016.9 | 1000.00 | 524000 | n/a |
| 1000 | 512 | fsys | append_batch(N) + sync_through (window off) | 168 | 168400 | 86.22 | 5490.1 | 9803.3 | 1.00 | 524000 | n/a |
| 1000 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 7 | 7400 | 30.31 | 135457.8 | n/a | 0.00 | 0 | 1.00 |
| 1000 | 4096 | store-io | (b) AppendBatch: N x append + commit | 140 | 140023 | 573.53 | 7296.1 | 22827.4 | 0.01 | 7092 | 1.00 |
| 1000 | 4096 | raw | (a') N writes of one padded block + one flush | 9 | 9354 | 38.31 | 105270.0 | n/a | 1000.00 | 4096000 | n/a |
| 1000 | 4096 | raw | (b') packed: one write of N records + one flush | 159 | 159314 | 652.55 | 6876.0 | 23225.1 | 1.00 | 4096000 | n/a |
| 1000 | 4096 | fsys | append x N + sync_through (window off) | 68 | 67518 | 276.55 | 14445.3 | n/a | 1000.00 | 4108000 | n/a |
| 1000 | 4096 | fsys | append_batch(N) + sync_through (window off) | 96 | 95629 | 391.70 | 10351.4 | n/a | 1.00 | 4108000 | n/a |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 1867 records read back and compared byte for byte; all 1867 checked for overlap and alignment | provisioned a 64 MiB append region in 0.06 s (1139 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 1814 records read back and compared byte for byte; all 1814 checked for overlap and alignment | provisioned a 64 MiB append region in 0.03 s (2673 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 220 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 224 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 471 of 471 records read back and compared byte for byte; all 471 checked for overlap and alignment | provisioned a 64 MiB append region in 0.06 s (1131 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 462 of 462 records read back and compared byte for byte; all 462 checked for overlap and alignment | provisioned a 64 MiB append region in 0.04 s (1787 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 216 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 214 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 438 of 438 records read back and compared byte for byte; all 438 checked for overlap and alignment | provisioned a 64 MiB append region in 0.05 s (1377 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 435 of 435 records read back and compared byte for byte; all 435 checked for overlap and alignment | provisioned a 64 MiB append region in 0.03 s (2091 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 211 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 221 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 3160 records read back and compared byte for byte; all 3160 checked for overlap and alignment | provisioned a 235 MiB append region in 0.09 s (2634 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 4780 records read back and compared byte for byte; all 4780 checked for overlap and alignment | provisioned a 64 MiB append region in 0.04 s (1653 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 2250 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 2300 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 3300 records read back and compared byte for byte; all 3300 checked for overlap and alignment | provisioned a 235 MiB append region in 0.13 s (1948 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 4660 records read back and compared byte for byte; all 4660 checked for overlap and alignment | provisioned a 64 MiB append region in 0.03 s (2137 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 2210 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 2220 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 3040 records read back and compared byte for byte; all 3040 checked for overlap and alignment | provisioned a 235 MiB append region in 0.10 s (2576 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 4480 records read back and compared byte for byte; all 4480 checked for overlap and alignment | provisioned a 235 MiB append region in 0.08 s (3139 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 1040 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 930 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 5600 records read back and compared byte for byte; all 5600 checked for overlap and alignment | provisioned a 2048 MiB append region in 1.03 s (2087 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 44200 records read back and compared byte for byte; all 44200 checked for overlap and alignment | provisioned a 64 MiB append region in 0.03 s (2218 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 20400 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 19900 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 7000 records read back and compared byte for byte; all 7000 checked for overlap and alignment | provisioned a 2048 MiB append region in 1.09 s (1964 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 48300 records read back and compared byte for byte; all 48300 checked for overlap and alignment | provisioned a 305 MiB append region in 0.16 s (1992 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 21600 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 22900 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 7100 records read back and compared byte for byte; all 7100 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.85 s (2525 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 40900 records read back and compared byte for byte; all 40900 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.73 s (2959 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 16800 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 17600 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 9000 records read back and compared byte for byte; all 9000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.61 s (3542 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 468000 records read back and compared byte for byte; all 468000 checked for overlap and alignment | provisioned a 375 MiB append region in 0.12 s (3195 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 192000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 206000 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 8000 records read back and compared byte for byte; all 8000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.78 s (2766 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 304000 records read back and compared byte for byte; all 304000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.61 s (3525 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 155000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 197000 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 10000 records read back and compared byte for byte; all 10000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.80 s (2676 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 157000 records read back and compared byte for byte; all 157000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.61 s (3508 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 82000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 110000 records replayed with JournalReader and compared byte for byte |  |

