# store-io performance harness: linux / msft-virtual-disk (2026-10-14)

> **OVERRIDE IN EFFECT.** `Store::create` with default trust was **refused** on this device:
> `device durability is unverified: running under a hypervisor without a certificate for this volume type; missing: device identify data (needs a readable device node); SMART / health log (needs privilege on Linux); volatile write cache presence; volatile write cache enabled state; device atomic-write fields; [Unverified { missing: {Identify, Smart, CachePresence, CacheEnabled, AtomicFields}, reasons: {Hypervisor} }]`
>
> Every store in this run was created with the documented labelled override (`Trust { override_refusal: true, .. }`): the class stays as decided, every receipt is labelled `overridden`, and the most conservative primitive (write + device flush) is used. Nothing else was weakened. The numbers below are therefore **not** gate numbers for a certified durability class; they measure software cost on this stack.

## Run

| item | value |
|---|---|
| date (UTC) | 2026-10-14T08:18:14Z |
| command | caller-batch |
| store-io commit | e4f90ecec8fe39c18eccbe385c6a10fe51cfce2c (crates/ modified) |
| OS | Ubuntu 24.04.4 LTS |
| kernel / build | Linux version 6.6.87.2-microsoft-standard-WSL2 (root@439a258ad544) (gcc (GCC) 11.2.0, GNU ld (GNU Binutils) 2.37) #1 SMP PREEMPT_DYNAMIC Thu Jun  5 18:30:46 UTC 2025 |
| CPU | AMD Ryzen 9 9950X3D 16-Core Processor |
| logical CPUs | 32 |
| filesystem | ext4 on / (device /dev/sdd 8:48, mount options rw,relatime, super options rw,discard,errors=remount-ro,data=ordered); store-io classified it Ext4 |
| directory under test | /home/james/store-io-harness-59549 |
| backend | store-io-posix (PosixPlatform, O_DIRECT; tier in the report's io line) |
| durability class | unverified (receipt label overridden(running under a hypervisor without a certificate for this volume type), durable open Overridden (Trust::override_refusal), block size 4096 B) |
| time per point | 1.0 s measured after 0.2 s warm-up (time-boxed; a point may end early at region capacity, noted where it does) |
| fsys | fsys 1.1.3 (crates.io), Handle method Sync, durability primitive fsync |
| raw primitives | pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync; preadv2 on an O_DIRECT descriptor (one call per request) |
| harness build | release, lto=fat, codegen-units=1 (same as store-io's release profile) |
| total wall time | 103.4 s |

## Device report (`Store::report()`)

```text
volume           c18681f4-64cf-9bce-1f89-679387342581
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
| caller-batch | batch of 1 x 64 B, one barrier | records/s (higher is better) | 369.4 | 232.2 | 80.3 | 1.591 | +59.1% | 4.599 | +359.9% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(64 B) + one sync_through | records/s (higher is better) | 389.4 | 367.4 | 101.5 | 1.060 | +6.0% | 3.836 | +283.6% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1 x 512 B, one barrier | records/s (higher is better) | 153.7 | 270.7 | 178.7 | 0.568 | -43.2% | 0.860 | -14.0% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(512 B) + one sync_through | records/s (higher is better) | 181.8 | 227.4 | 177.4 | 0.799 | -20.1% | 1.024 | +2.4% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1 x 4096 B, one barrier | records/s (higher is better) | 370.0 | 370.2 | 173.7 | 0.999 | -0.1% | 2.130 | +113.0% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(4096 B) + one sync_through | records/s (higher is better) | 383.8 | 369.3 | 168.4 | 1.039 | +3.9% | 2.279 | +127.9% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 64 B, one barrier | records/s (higher is better) | 3880.0 | 3821.9 | 1346.9 | 1.015 | +1.5% | 2.881 | +188.1% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(64 B) + one sync_through | records/s (higher is better) | 2650.8 | 2627.1 | 1665.6 | 1.009 | +0.9% | 1.591 | +59.1% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 512 B, one barrier | records/s (higher is better) | 3813.8 | 4003.0 | 1813.8 | 0.953 | -4.7% | 2.103 | +110.3% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(512 B) + one sync_through | records/s (higher is better) | 2457.1 | 2765.5 | 1850.2 | 0.889 | -11.1% | 1.328 | +32.8% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 4096 B, one barrier | records/s (higher is better) | 3865.6 | 3914.1 | 1886.9 | 0.988 | -1.2% | 2.049 | +104.9% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(4096 B) + one sync_through | records/s (higher is better) | 2439.3 | 2661.7 | 1499.6 | 0.916 | -8.4% | 1.627 | +62.7% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 64 B, one barrier | records/s (higher is better) | 35909.6 | 18876.0 | 18411.1 | 1.902 | +90.2% | 1.950 | +95.0% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(64 B) + one sync_through | records/s (higher is better) | 4943.5 | 5009.2 | 15783.9 | 0.987 | -1.3% | 0.313 | -68.7% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 512 B, one barrier | records/s (higher is better) | 38643.0 | 40461.0 | 18171.6 | 0.955 | -4.5% | 2.127 | +112.7% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(512 B) + one sync_through | records/s (higher is better) | 5704.0 | 6170.8 | 18126.0 | 0.924 | -7.6% | 0.315 | -68.5% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 4096 B, one barrier | records/s (higher is better) | 18974.7 | 26461.4 | 15325.4 | 0.717 | -28.3% | 1.238 | +23.8% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(4096 B) + one sync_through | records/s (higher is better) | 5504.6 | 6298.5 | 14347.9 | 0.874 | -12.6% | 0.384 | -61.6% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 64 B, one barrier | records/s (higher is better) | 380763.2 | 402545.7 | 116798.0 | 0.946 | -5.4% | 3.260 | +226.0% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(64 B) + one sync_through | records/s (higher is better) | 6884.0 | 7577.2 | 152957.3 | 0.909 | -9.1% | 0.045 | -95.5% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 512 B, one barrier | records/s (higher is better) | 136861.9 | 325388.6 | 164887.4 | 0.421 | -57.9% | 0.830 | -17.0% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(512 B) + one sync_through | records/s (higher is better) | 6524.1 | 7160.9 | 126106.8 | 0.911 | -8.9% | 0.052 | -94.8% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 4096 B, one barrier | records/s (higher is better) | 117304.0 | 70734.5 | 74578.0 | 1.658 | +65.8% | 1.573 | +57.3% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(4096 B) + one sync_through | records/s (higher is better) | 6244.0 | 5387.7 | 55534.6 | 1.159 | +15.9% | 0.112 | -88.8% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |

## Anomalies, errors and integrity failures

- None: every measurement completed and every integrity check passed.

## W3: caller batch (N records, one barrier) (`caller-batch`)

Wall time: 102.9 s.

**Method.**

- N in [1, 10, 100, 1000], record sizes [64, 512, 4096] bytes; one commit = N records made durable by one barrier; commits run back to back on one thread. Each point ends at the time box or after 3000 measured commits, whichever comes first.
- store-io (a) `N x append + sync_through`: each append is its own write, padded to a block (documented behaviour). (b) `AppendBatch`: records packed back to back into pooled buffers, `commit()` = one reservation, the fewest writes (largest pooled buffer 1 MiB), one barrier. A fresh store per (N, size).
- raw (a') N writes of one zero-padded block each, then one flush; (b') the N records packed into one aligned buffer padded to a block, one write, one flush. Primitive: pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync. A ready 1 GiB file, offsets cycling.
- fsys: `append` x N + `sync_through(last lsn)`, and `append_batch(&records)` + `sync_through`, on a fresh journal per point, window off.
- records/s = commits/s x N; payload MB/s counts caller bytes only (10^6 bytes); `write calls/commit` and `bytes written/commit` come from the OS counters and show the device writes each design issues per commit.

Results:

| N | record B | system | variant | commits/s | records/s | payload MB/s | commit p50 µs | commit p99 µs | write calls/commit | bytes written/commit | flushes/commit |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 389 | 389 | 0.02 | 2445.3 | 5135.0 | 0.00 | 0 | 1.00 |
| 1 | 64 | store-io | (b) AppendBatch: N x append + commit | 369 | 369 | 0.02 | 2533.1 | 6863.3 | 0.00 | 0 | 1.00 |
| 1 | 64 | raw | (a') N writes of one padded block + one flush | 367 | 367 | 0.02 | 2551.4 | 5553.0 | 1.00 | 4096 | n/a |
| 1 | 64 | raw | (b') packed: one write of N records + one flush | 232 | 232 | 0.01 | 3798.9 | 11692.7 | 1.00 | 4096 | n/a |
| 1 | 64 | fsys | append x N + sync_through (window off) | 102 | 102 | 0.01 | 7784.1 | 35548.7 | 1.00 | 76 | n/a |
| 1 | 64 | fsys | append_batch(N) + sync_through (window off) | 80 | 80 | 0.01 | 7633.8 | n/a | 1.00 | 76 | n/a |
| 1 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 182 | 182 | 0.09 | 4047.9 | 16371.9 | 0.00 | 0 | 1.00 |
| 1 | 512 | store-io | (b) AppendBatch: N x append + commit | 154 | 154 | 0.08 | 6332.6 | 10989.6 | 0.00 | 0 | 1.00 |
| 1 | 512 | raw | (a') N writes of one padded block + one flush | 227 | 227 | 0.12 | 3106.7 | 13571.8 | 1.00 | 4096 | n/a |
| 1 | 512 | raw | (b') packed: one write of N records + one flush | 271 | 271 | 0.14 | 2829.3 | 10224.7 | 1.00 | 4096 | n/a |
| 1 | 512 | fsys | append x N + sync_through (window off) | 177 | 177 | 0.09 | 5224.0 | 22120.6 | 1.00 | 524 | n/a |
| 1 | 512 | fsys | append_batch(N) + sync_through (window off) | 179 | 179 | 0.09 | 5186.1 | 21339.0 | 1.00 | 524 | n/a |
| 1 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 384 | 384 | 1.57 | 2461.0 | 6625.5 | 0.00 | 0 | 1.00 |
| 1 | 4096 | store-io | (b) AppendBatch: N x append + commit | 370 | 370 | 1.52 | 2543.5 | 6683.6 | 0.00 | 0 | 1.00 |
| 1 | 4096 | raw | (a') N writes of one padded block + one flush | 369 | 369 | 1.51 | 2539.0 | 5369.4 | 1.00 | 4096 | n/a |
| 1 | 4096 | raw | (b') packed: one write of N records + one flush | 370 | 370 | 1.52 | 2543.6 | 5855.2 | 1.00 | 4096 | n/a |
| 1 | 4096 | fsys | append x N + sync_through (window off) | 168 | 168 | 0.69 | 5494.9 | 21228.0 | 1.00 | 4108 | n/a |
| 1 | 4096 | fsys | append_batch(N) + sync_through (window off) | 174 | 174 | 0.71 | 5391.2 | 21538.6 | 1.00 | 4108 | n/a |
| 10 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 265 | 2651 | 0.17 | 3674.5 | 5988.0 | 0.00 | 0 | 1.00 |
| 10 | 64 | store-io | (b) AppendBatch: N x append + commit | 388 | 3880 | 0.25 | 2396.6 | 4672.3 | 0.00 | 0 | 1.00 |
| 10 | 64 | raw | (a') N writes of one padded block + one flush | 263 | 2627 | 0.17 | 3490.6 | 7633.1 | 10.00 | 40960 | n/a |
| 10 | 64 | raw | (b') packed: one write of N records + one flush | 382 | 3822 | 0.24 | 2418.1 | 6329.5 | 1.00 | 4096 | n/a |
| 10 | 64 | fsys | append x N + sync_through (window off) | 167 | 1666 | 0.11 | 5468.8 | 11182.5 | 10.00 | 760 | n/a |
| 10 | 64 | fsys | append_batch(N) + sync_through (window off) | 135 | 1347 | 0.09 | 6462.4 | 17338.1 | 1.00 | 760 | n/a |
| 10 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 246 | 2457 | 1.26 | 3775.5 | 7616.5 | 0.00 | 0 | 1.00 |
| 10 | 512 | store-io | (b) AppendBatch: N x append + commit | 381 | 3814 | 1.95 | 2411.4 | 6463.1 | 0.00 | 0 | 1.00 |
| 10 | 512 | raw | (a') N writes of one padded block + one flush | 277 | 2765 | 1.42 | 3499.8 | 6047.4 | 10.00 | 40960 | n/a |
| 10 | 512 | raw | (b') packed: one write of N records + one flush | 400 | 4003 | 2.05 | 2357.1 | 4640.6 | 1.00 | 8192 | n/a |
| 10 | 512 | fsys | append x N + sync_through (window off) | 185 | 1850 | 0.95 | 5114.4 | 20926.2 | 10.00 | 5240 | n/a |
| 10 | 512 | fsys | append_batch(N) + sync_through (window off) | 181 | 1814 | 0.93 | 5189.2 | 21148.1 | 1.00 | 5240 | n/a |
| 10 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 244 | 2439 | 9.99 | 3771.5 | 8808.2 | 0.00 | 0 | 1.00 |
| 10 | 4096 | store-io | (b) AppendBatch: N x append + commit | 387 | 3866 | 15.83 | 2525.9 | 4471.0 | 0.00 | 0 | 1.00 |
| 10 | 4096 | raw | (a') N writes of one padded block + one flush | 266 | 2662 | 10.90 | 3585.3 | 7696.3 | 10.00 | 40960 | n/a |
| 10 | 4096 | raw | (b') packed: one write of N records + one flush | 391 | 3914 | 16.03 | 2465.0 | 5312.5 | 1.00 | 40960 | n/a |
| 10 | 4096 | fsys | append x N + sync_through (window off) | 150 | 1500 | 6.14 | 5518.9 | 21090.0 | 10.00 | 41080 | n/a |
| 10 | 4096 | fsys | append_batch(N) + sync_through (window off) | 189 | 1887 | 7.73 | 5143.8 | 7305.4 | 1.00 | 41080 | n/a |
| 100 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 49 | 4944 | 0.32 | 19503.4 | n/a | 0.00 | 0 | 1.00 |
| 100 | 64 | store-io | (b) AppendBatch: N x append + commit | 359 | 35910 | 2.30 | 2602.1 | 5268.1 | 0.00 | 0 | 1.00 |
| 100 | 64 | raw | (a') N writes of one padded block + one flush | 50 | 5009 | 0.32 | 19775.0 | n/a | 100.00 | 409600 | n/a |
| 100 | 64 | raw | (b') packed: one write of N records + one flush | 189 | 18876 | 1.21 | 3176.6 | 31550.8 | 1.00 | 8192 | n/a |
| 100 | 64 | fsys | append x N + sync_through (window off) | 158 | 15784 | 1.01 | 5960.3 | 13972.5 | 100.00 | 7600 | n/a |
| 100 | 64 | fsys | append_batch(N) + sync_through (window off) | 184 | 18411 | 1.18 | 5162.0 | 20934.6 | 1.00 | 7600 | n/a |
| 100 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 57 | 5704 | 2.92 | 16661.9 | n/a | 0.00 | 0 | 1.00 |
| 100 | 512 | store-io | (b) AppendBatch: N x append + commit | 386 | 38643 | 19.79 | 2366.1 | 6172.2 | 0.00 | 0 | 1.00 |
| 100 | 512 | raw | (a') N writes of one padded block + one flush | 62 | 6171 | 3.16 | 15376.0 | n/a | 100.00 | 409600 | n/a |
| 100 | 512 | raw | (b') packed: one write of N records + one flush | 405 | 40461 | 20.72 | 2373.9 | 4355.9 | 1.00 | 53248 | n/a |
| 100 | 512 | fsys | append x N + sync_through (window off) | 181 | 18126 | 9.28 | 5251.0 | 9871.8 | 100.00 | 52400 | n/a |
| 100 | 512 | fsys | append_batch(N) + sync_through (window off) | 182 | 18172 | 9.30 | 5246.6 | 10026.8 | 1.00 | 52400 | n/a |
| 100 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 55 | 5505 | 22.55 | 17244.9 | n/a | 0.00 | 0 | 1.00 |
| 100 | 4096 | store-io | (b) AppendBatch: N x append + commit | 190 | 18975 | 77.72 | 4934.5 | 26209.4 | 0.00 | 0 | 1.00 |
| 100 | 4096 | raw | (a') N writes of one padded block + one flush | 63 | 6299 | 25.80 | 15478.2 | n/a | 100.00 | 409600 | n/a |
| 100 | 4096 | raw | (b') packed: one write of N records + one flush | 265 | 26461 | 108.39 | 2814.1 | 9243.0 | 1.00 | 409600 | n/a |
| 100 | 4096 | fsys | append x N + sync_through (window off) | 143 | 14348 | 58.77 | 6090.2 | 11885.9 | 100.00 | 410800 | n/a |
| 100 | 4096 | fsys | append_batch(N) + sync_through (window off) | 153 | 15325 | 62.77 | 5598.0 | 21533.8 | 1.00 | 410800 | n/a |
| 1000 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 7 | 6884 | 0.44 | 143884.6 | n/a | 0.00 | 0 | 1.00 |
| 1000 | 64 | store-io | (b) AppendBatch: N x append + commit | 381 | 380763 | 24.37 | 2402.4 | 5165.1 | 0.00 | 0 | 1.00 |
| 1000 | 64 | raw | (a') N writes of one padded block + one flush | 8 | 7577 | 0.48 | 131521.9 | n/a | 1000.00 | 4096000 | n/a |
| 1000 | 64 | raw | (b') packed: one write of N records + one flush | 403 | 402546 | 25.76 | 2429.7 | 3300.0 | 1.00 | 65536 | n/a |
| 1000 | 64 | fsys | append x N + sync_through (window off) | 153 | 152957 | 9.79 | 5891.1 | 21638.9 | 1000.00 | 76000 | n/a |
| 1000 | 64 | fsys | append_batch(N) + sync_through (window off) | 117 | 116798 | 7.48 | 6844.1 | 21639.9 | 1.00 | 76000 | n/a |
| 1000 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 7 | 6524 | 3.34 | 153010.2 | n/a | 0.00 | 0 | 1.00 |
| 1000 | 512 | store-io | (b) AppendBatch: N x append + commit | 137 | 136862 | 70.07 | 6594.2 | 25674.2 | 0.00 | 0 | 1.00 |
| 1000 | 512 | raw | (a') N writes of one padded block + one flush | 7 | 7161 | 3.67 | 130972.4 | n/a | 1000.00 | 4096000 | n/a |
| 1000 | 512 | raw | (b') packed: one write of N records + one flush | 325 | 325389 | 166.60 | 2642.7 | 6820.5 | 1.00 | 512000 | n/a |
| 1000 | 512 | fsys | append x N + sync_through (window off) | 126 | 126107 | 64.57 | 7200.3 | 10068.5 | 1000.00 | 524000 | n/a |
| 1000 | 512 | fsys | append_batch(N) + sync_through (window off) | 165 | 164887 | 84.42 | 5838.2 | 10418.7 | 1.00 | 524000 | n/a |
| 1000 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 6 | 6244 | 25.58 | 157865.3 | n/a | 0.00 | 0 | 1.00 |
| 1000 | 4096 | store-io | (b) AppendBatch: N x append + commit | 117 | 117304 | 480.48 | 9637.0 | 15006.6 | 0.00 | 0 | 1.00 |
| 1000 | 4096 | raw | (a') N writes of one padded block + one flush | 5 | 5388 | 22.07 | 188105.4 | n/a | 1000.00 | 4096000 | n/a |
| 1000 | 4096 | raw | (b') packed: one write of N records + one flush | 71 | 70735 | 289.73 | 12132.5 | n/a | 1.00 | 4096000 | n/a |
| 1000 | 4096 | fsys | append x N + sync_through (window off) | 56 | 55535 | 227.47 | 17057.2 | n/a | 1000.00 | 4108000 | n/a |
| 1000 | 4096 | fsys | append_batch(N) + sync_through (window off) | 75 | 74578 | 305.47 | 13262.6 | n/a | 1.00 | 4108000 | n/a |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | (a) N x AppendRegion::append + sync_through | passed: 462 of 462 records read back and compared byte for byte; all 462 checked for overlap and alignment | provisioned a 64 MiB append region in 0.04 s (1501 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 449 of 449 records read back and compared byte for byte; all 449 checked for overlap and alignment | provisioned a 64 MiB append region in 0.03 s (2106 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 126 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 106 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 220 of 220 records read back and compared byte for byte; all 220 checked for overlap and alignment | provisioned a 64 MiB append region in 0.08 s (881 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 180 of 180 records read back and compared byte for byte; all 180 checked for overlap and alignment | provisioned a 64 MiB append region in 0.12 s (556 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 214 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 218 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 466 of 466 records read back and compared byte for byte; all 466 checked for overlap and alignment | provisioned a 64 MiB append region in 0.10 s (672 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 450 of 450 records read back and compared byte for byte; all 450 checked for overlap and alignment | provisioned a 64 MiB append region in 0.06 s (1107 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 203 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 208 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 3170 records read back and compared byte for byte; all 3170 checked for overlap and alignment | provisioned a 235 MiB append region in 0.23 s (1054 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 4590 records read back and compared byte for byte; all 4590 checked for overlap and alignment | provisioned a 64 MiB append region in 0.03 s (2287 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 2050 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 1610 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 2880 records read back and compared byte for byte; all 2880 checked for overlap and alignment | provisioned a 235 MiB append region in 0.14 s (1777 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 4620 records read back and compared byte for byte; all 4620 checked for overlap and alignment | provisioned a 64 MiB append region in 0.04 s (1530 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 2180 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 2180 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 2970 records read back and compared byte for byte; all 2970 checked for overlap and alignment | provisioned a 235 MiB append region in 0.15 s (1687 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 4530 records read back and compared byte for byte; all 4530 checked for overlap and alignment | provisioned a 235 MiB append region in 0.21 s (1195 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 1870 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 2240 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 6100 records read back and compared byte for byte; all 6100 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.76 s (2827 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 42800 records read back and compared byte for byte; all 42800 checked for overlap and alignment | provisioned a 64 MiB append region in 0.06 s (1212 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 17000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 22100 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 6900 records read back and compared byte for byte; all 6900 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.72 s (2987 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 46300 records read back and compared byte for byte; all 46300 checked for overlap and alignment | provisioned a 305 MiB append region in 0.09 s (3594 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 21800 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 21700 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 6700 records read back and compared byte for byte; all 6700 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.83 s (2597 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 23100 records read back and compared byte for byte; all 23100 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.96 s (2240 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 17200 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 18200 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 9000 records read back and compared byte for byte; all 9000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.74 s (2898 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 450000 records read back and compared byte for byte; all 450000 checked for overlap and alignment | provisioned a 375 MiB append region in 0.11 s (3513 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 185000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 130000 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 8000 records read back and compared byte for byte; all 8000 checked for overlap and alignment | provisioned a 2048 MiB append region in 1.02 s (2112 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 167000 records read back and compared byte for byte; all 167000 checked for overlap and alignment | provisioned a 2048 MiB append region in 1.07 s (2002 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 148000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 188000 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 9000 records read back and compared byte for byte; all 9000 checked for overlap and alignment | provisioned a 2048 MiB append region in 1.20 s (1783 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 132000 records read back and compared byte for byte; all 132000 checked for overlap and alignment | provisioned a 2048 MiB append region in 1.14 s (1883 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 64000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 88000 records replayed with JournalReader and compared byte for byte |  |

