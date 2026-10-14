# store-io performance harness: linux / msft-virtual-disk (2026-10-14)

> **OVERRIDE IN EFFECT.** `Store::create` with default trust was **refused** on this device:
> `device durability is unverified: running under a hypervisor without a certificate for this volume type; missing: device identify data (needs a readable device node); SMART / health log (needs privilege on Linux); volatile write cache presence; volatile write cache enabled state; device atomic-write fields; [Unverified { missing: {Identify, Smart, CachePresence, CacheEnabled, AtomicFields}, reasons: {Hypervisor} }]`
>
> Every store in this run was created with the documented labelled override (`Trust { override_refusal: true, .. }`): the class stays as decided, every receipt is labelled `overridden`, and the most conservative primitive (write + device flush) is used. Nothing else was weakened. The numbers below are therefore **not** gate numbers for a certified durability class; they measure software cost on this stack.

## Run

| item | value |
|---|---|
| date (UTC) | 2026-10-14T08:20:18Z |
| command | caller-batch |
| store-io commit | e4f90ecec8fe39c18eccbe385c6a10fe51cfce2c (crates/ modified) |
| OS | Ubuntu 24.04.4 LTS |
| kernel / build | Linux version 6.6.87.2-microsoft-standard-WSL2 (root@439a258ad544) (gcc (GCC) 11.2.0, GNU ld (GNU Binutils) 2.37) #1 SMP PREEMPT_DYNAMIC Thu Jun  5 18:30:46 UTC 2025 |
| CPU | AMD Ryzen 9 9950X3D 16-Core Processor |
| logical CPUs | 32 |
| filesystem | ext4 on / (device /dev/sdd 8:48, mount options rw,relatime, super options rw,discard,errors=remount-ro,data=ordered); store-io classified it Ext4 |
| directory under test | /home/james/store-io-harness-59563 |
| backend | store-io-posix (PosixPlatform, O_DIRECT; tier in the report's io line) |
| durability class | unverified (receipt label overridden(running under a hypervisor without a certificate for this volume type), durable open Overridden (Trust::override_refusal), block size 4096 B) |
| time per point | 1.0 s measured after 0.2 s warm-up (time-boxed; a point may end early at region capacity, noted where it does) |
| fsys | fsys 1.1.3 (crates.io), Handle method Sync, durability primitive fsync |
| raw primitives | pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync; preadv2 on an O_DIRECT descriptor (one call per request) |
| harness build | release, lto=fat, codegen-units=1 (same as store-io's release profile) |
| total wall time | 103.8 s |

## Device report (`Store::report()`)

```text
volume           2a8614e1-0bc7-3d5b-0430-19a0eca85052
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
| caller-batch | batch of 1 x 64 B, one barrier | records/s (higher is better) | 401.7 | 395.1 | 182.7 | 1.016 | +1.6% | 2.198 | +119.8% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(64 B) + one sync_through | records/s (higher is better) | 268.6 | 403.3 | 188.7 | 0.666 | -33.4% | 1.423 | +42.3% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1 x 512 B, one barrier | records/s (higher is better) | 151.6 | 179.3 | 181.5 | 0.846 | -15.4% | 0.835 | -16.5% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(512 B) + one sync_through | records/s (higher is better) | 175.8 | 131.3 | 70.6 | 1.338 | +33.8% | 2.490 | +149.0% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1 x 4096 B, one barrier | records/s (higher is better) | 388.7 | 375.9 | 176.1 | 1.034 | +3.4% | 2.208 | +120.8% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(4096 B) + one sync_through | records/s (higher is better) | 388.6 | 381.9 | 175.4 | 1.018 | +1.8% | 2.215 | +121.5% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 64 B, one barrier | records/s (higher is better) | 3671.3 | 3009.3 | 1845.5 | 1.220 | +22.0% | 1.989 | +98.9% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(64 B) + one sync_through | records/s (higher is better) | 2427.4 | 1696.8 | 1779.6 | 1.431 | +43.1% | 1.364 | +36.4% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 512 B, one barrier | records/s (higher is better) | 3817.7 | 3996.2 | 1864.1 | 0.955 | -4.5% | 2.048 | +104.8% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(512 B) + one sync_through | records/s (higher is better) | 2568.4 | 2623.2 | 1840.1 | 0.979 | -2.1% | 1.396 | +39.6% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 4096 B, one barrier | records/s (higher is better) | 3965.0 | 3729.4 | 1810.3 | 1.063 | +6.3% | 2.190 | +119.0% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(4096 B) + one sync_through | records/s (higher is better) | 2481.5 | 2611.1 | 1709.2 | 0.950 | -5.0% | 1.452 | +45.2% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 64 B, one barrier | records/s (higher is better) | 37460.2 | 39478.6 | 18520.3 | 0.949 | -5.1% | 2.023 | +102.3% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(64 B) + one sync_through | records/s (higher is better) | 4087.1 | 6236.5 | 18106.4 | 0.655 | -34.5% | 0.226 | -77.4% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 512 B, one barrier | records/s (higher is better) | 37380.6 | 39013.7 | 18866.6 | 0.958 | -4.2% | 1.981 | +98.1% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(512 B) + one sync_through | records/s (higher is better) | 6185.7 | 6302.6 | 17823.9 | 0.981 | -1.9% | 0.347 | -65.3% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 4096 B, one barrier | records/s (higher is better) | 9812.6 | 24306.0 | 13458.4 | 0.404 | -59.6% | 0.729 | -27.1% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(4096 B) + one sync_through | records/s (higher is better) | 3300.9 | 6387.9 | 12484.1 | 0.517 | -48.3% | 0.264 | -73.6% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 64 B, one barrier | records/s (higher is better) | 256327.1 | 146514.0 | 157885.6 | 1.750 | +75.0% | 1.623 | +62.3% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(64 B) + one sync_through | records/s (higher is better) | 6810.5 | 4671.9 | 45023.2 | 1.458 | +45.8% | 0.151 | -84.9% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 512 B, one barrier | records/s (higher is better) | 205903.5 | 288136.1 | 140156.6 | 0.715 | -28.5% | 1.469 | +46.9% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(512 B) + one sync_through | records/s (higher is better) | 7266.0 | 6835.6 | 115231.1 | 1.063 | +6.3% | 0.063 | -93.7% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 4096 B, one barrier | records/s (higher is better) | 64142.3 | 64930.1 | 56800.5 | 0.988 | -1.2% | 1.129 | +12.9% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(4096 B) + one sync_through | records/s (higher is better) | 7270.8 | 5887.8 | 30968.4 | 1.235 | +23.5% | 0.235 | -76.5% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |

## Anomalies, errors and integrity failures

- None: every measurement completed and every integrity check passed.

## W3: caller batch (N records, one barrier) (`caller-batch`)

Wall time: 103.3 s.

**Method.**

- N in [1, 10, 100, 1000], record sizes [64, 512, 4096] bytes; one commit = N records made durable by one barrier; commits run back to back on one thread. Each point ends at the time box or after 3000 measured commits, whichever comes first.
- store-io (a) `N x append + sync_through`: each append is its own write, padded to a block (documented behaviour). (b) `AppendBatch`: records packed back to back into pooled buffers, `commit()` = one reservation, the fewest writes (largest pooled buffer 1 MiB), one barrier. A fresh store per (N, size).
- raw (a') N writes of one zero-padded block each, then one flush; (b') the N records packed into one aligned buffer padded to a block, one write, one flush. Primitive: pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync. A ready 1 GiB file, offsets cycling.
- fsys: `append` x N + `sync_through(last lsn)`, and `append_batch(&records)` + `sync_through`, on a fresh journal per point, window off.
- records/s = commits/s x N; payload MB/s counts caller bytes only (10^6 bytes); `write calls/commit` and `bytes written/commit` come from the OS counters and show the device writes each design issues per commit.

Results:

| N | record B | system | variant | commits/s | records/s | payload MB/s | commit p50 µs | commit p99 µs | write calls/commit | bytes written/commit | flushes/commit |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 269 | 269 | 0.02 | 3113.8 | 10771.1 | 1.00 | 4096 | 1.00 |
| 1 | 64 | store-io | (b) AppendBatch: N x append + commit | 402 | 402 | 0.03 | 2360.3 | 4290.5 | 1.00 | 4096 | 1.00 |
| 1 | 64 | raw | (a') N writes of one padded block + one flush | 403 | 403 | 0.03 | 2366.0 | 3101.9 | 1.00 | 4096 | n/a |
| 1 | 64 | raw | (b') packed: one write of N records + one flush | 395 | 395 | 0.03 | 2390.7 | 3239.6 | 1.00 | 4096 | n/a |
| 1 | 64 | fsys | append x N + sync_through (window off) | 189 | 189 | 0.01 | 5091.0 | 21005.8 | 1.00 | 76 | n/a |
| 1 | 64 | fsys | append_batch(N) + sync_through (window off) | 183 | 183 | 0.01 | 5240.3 | 20872.6 | 1.00 | 76 | n/a |
| 1 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 176 | 176 | 0.09 | 4107.2 | 36021.3 | 1.00 | 4096 | 1.00 |
| 1 | 512 | store-io | (b) AppendBatch: N x append + commit | 152 | 152 | 0.08 | 4599.1 | 30115.3 | 1.00 | 4096 | 1.00 |
| 1 | 512 | raw | (a') N writes of one padded block + one flush | 131 | 131 | 0.07 | 6801.9 | 24927.3 | 1.00 | 4096 | n/a |
| 1 | 512 | raw | (b') packed: one write of N records + one flush | 179 | 179 | 0.09 | 4015.3 | 31164.3 | 1.00 | 4096 | n/a |
| 1 | 512 | fsys | append x N + sync_through (window off) | 71 | 71 | 0.04 | 13809.0 | n/a | 1.00 | 524 | n/a |
| 1 | 512 | fsys | append_batch(N) + sync_through (window off) | 182 | 182 | 0.09 | 5092.3 | 20989.2 | 1.00 | 524 | n/a |
| 1 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 389 | 389 | 1.59 | 2394.6 | 6459.7 | 1.00 | 4096 | 1.00 |
| 1 | 4096 | store-io | (b) AppendBatch: N x append + commit | 389 | 389 | 1.59 | 2393.0 | 6056.1 | 1.00 | 4096 | 1.00 |
| 1 | 4096 | raw | (a') N writes of one padded block + one flush | 382 | 382 | 1.56 | 2429.0 | 6290.9 | 1.00 | 4096 | n/a |
| 1 | 4096 | raw | (b') packed: one write of N records + one flush | 376 | 376 | 1.54 | 2509.4 | 4612.9 | 1.00 | 4096 | n/a |
| 1 | 4096 | fsys | append x N + sync_through (window off) | 175 | 175 | 0.72 | 5462.1 | 21126.7 | 1.00 | 4108 | n/a |
| 1 | 4096 | fsys | append_batch(N) + sync_through (window off) | 176 | 176 | 0.72 | 5432.8 | 21206.4 | 1.00 | 4108 | n/a |
| 10 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 243 | 2427 | 0.16 | 3853.8 | 10308.5 | 10.00 | 40960 | 1.00 |
| 10 | 64 | store-io | (b) AppendBatch: N x append + commit | 367 | 3671 | 0.23 | 2566.9 | 5090.9 | 1.00 | 4096 | 1.00 |
| 10 | 64 | raw | (a') N writes of one padded block + one flush | 170 | 1697 | 0.11 | 4742.6 | 28469.0 | 10.00 | 40960 | n/a |
| 10 | 64 | raw | (b') packed: one write of N records + one flush | 301 | 3009 | 0.19 | 2861.3 | 7567.3 | 1.00 | 4096 | n/a |
| 10 | 64 | fsys | append x N + sync_through (window off) | 178 | 1780 | 0.11 | 5152.3 | 21157.5 | 10.00 | 760 | n/a |
| 10 | 64 | fsys | append_batch(N) + sync_through (window off) | 185 | 1845 | 0.12 | 5099.6 | 21102.3 | 1.00 | 760 | n/a |
| 10 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 257 | 2568 | 1.32 | 3621.4 | 9377.7 | 10.00 | 40960 | 1.00 |
| 10 | 512 | store-io | (b) AppendBatch: N x append + commit | 382 | 3818 | 1.95 | 2413.0 | 5555.7 | 1.00 | 8192 | 1.00 |
| 10 | 512 | raw | (a') N writes of one padded block + one flush | 262 | 2623 | 1.34 | 3569.2 | 7119.3 | 10.00 | 40960 | n/a |
| 10 | 512 | raw | (b') packed: one write of N records + one flush | 400 | 3996 | 2.05 | 2343.6 | 4746.8 | 1.00 | 8192 | n/a |
| 10 | 512 | fsys | append x N + sync_through (window off) | 184 | 1840 | 0.94 | 5172.0 | 21061.1 | 10.00 | 5240 | n/a |
| 10 | 512 | fsys | append_batch(N) + sync_through (window off) | 186 | 1864 | 0.95 | 5103.4 | 20883.4 | 1.00 | 5240 | n/a |
| 10 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 248 | 2481 | 10.16 | 3762.6 | 8204.7 | 10.00 | 40960 | 1.00 |
| 10 | 4096 | store-io | (b) AppendBatch: N x append + commit | 397 | 3965 | 16.24 | 2457.1 | 4691.3 | 1.00 | 40960 | 1.00 |
| 10 | 4096 | raw | (a') N writes of one padded block + one flush | 261 | 2611 | 10.70 | 3625.2 | 11117.6 | 10.00 | 40960 | n/a |
| 10 | 4096 | raw | (b') packed: one write of N records + one flush | 373 | 3729 | 15.28 | 2505.7 | 5394.1 | 1.00 | 40960 | n/a |
| 10 | 4096 | fsys | append x N + sync_through (window off) | 171 | 1709 | 7.00 | 5377.1 | 10590.4 | 10.00 | 41080 | n/a |
| 10 | 4096 | fsys | append_batch(N) + sync_through (window off) | 181 | 1810 | 7.42 | 5333.7 | 10073.6 | 1.00 | 41080 | n/a |
| 100 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 41 | 4087 | 0.26 | 20934.2 | n/a | 100.00 | 409600 | 1.00 |
| 100 | 64 | store-io | (b) AppendBatch: N x append + commit | 375 | 37460 | 2.40 | 2519.5 | 4683.3 | 1.00 | 8192 | 1.00 |
| 100 | 64 | raw | (a') N writes of one padded block + one flush | 62 | 6236 | 0.40 | 15234.6 | n/a | 100.00 | 409600 | n/a |
| 100 | 64 | raw | (b') packed: one write of N records + one flush | 395 | 39479 | 2.53 | 2386.0 | 5088.1 | 1.00 | 8192 | n/a |
| 100 | 64 | fsys | append x N + sync_through (window off) | 181 | 18106 | 1.16 | 5175.2 | 20840.4 | 100.00 | 7600 | n/a |
| 100 | 64 | fsys | append_batch(N) + sync_through (window off) | 185 | 18520 | 1.19 | 5112.6 | 9230.5 | 1.00 | 7600 | n/a |
| 100 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 62 | 6186 | 3.17 | 15846.9 | n/a | 100.00 | 409600 | 1.00 |
| 100 | 512 | store-io | (b) AppendBatch: N x append + commit | 374 | 37381 | 19.14 | 2405.4 | 5326.7 | 1.00 | 53248 | 1.00 |
| 100 | 512 | raw | (a') N writes of one padded block + one flush | 63 | 6303 | 3.23 | 15477.9 | n/a | 100.00 | 409600 | n/a |
| 100 | 512 | raw | (b') packed: one write of N records + one flush | 390 | 39014 | 19.98 | 2361.1 | 6241.4 | 1.00 | 53248 | n/a |
| 100 | 512 | fsys | append x N + sync_through (window off) | 178 | 17824 | 9.13 | 5202.5 | 12687.2 | 100.00 | 52400 | n/a |
| 100 | 512 | fsys | append_batch(N) + sync_through (window off) | 189 | 18867 | 9.66 | 5171.7 | 7934.6 | 1.00 | 52400 | n/a |
| 100 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 33 | 3301 | 13.52 | 26806.7 | n/a | 100.00 | 409600 | 1.00 |
| 100 | 4096 | store-io | (b) AppendBatch: N x append + commit | 98 | 9813 | 40.19 | 8507.2 | n/a | 1.00 | 409600 | 1.00 |
| 100 | 4096 | raw | (a') N writes of one padded block + one flush | 64 | 6388 | 26.16 | 15265.4 | n/a | 100.00 | 409600 | n/a |
| 100 | 4096 | raw | (b') packed: one write of N records + one flush | 243 | 24306 | 99.56 | 2949.6 | 8567.3 | 1.00 | 409600 | n/a |
| 100 | 4096 | fsys | append x N + sync_through (window off) | 125 | 12484 | 51.14 | 6803.8 | 26427.3 | 100.00 | 410800 | n/a |
| 100 | 4096 | fsys | append_batch(N) + sync_through (window off) | 135 | 13458 | 55.13 | 6329.5 | 15173.4 | 1.00 | 410800 | n/a |
| 1000 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 7 | 6810 | 0.44 | 145177.6 | n/a | 1000.00 | 4096000 | 1.00 |
| 1000 | 64 | store-io | (b) AppendBatch: N x append + commit | 256 | 256327 | 16.40 | 2704.3 | 67318.8 | 1.00 | 65536 | 1.00 |
| 1000 | 64 | raw | (a') N writes of one padded block + one flush | 5 | 4672 | 0.30 | 198636.9 | n/a | 1000.00 | 4096000 | n/a |
| 1000 | 64 | raw | (b') packed: one write of N records + one flush | 147 | 146514 | 9.38 | 2601.0 | 140038.6 | 1.00 | 65536 | n/a |
| 1000 | 64 | fsys | append x N + sync_through (window off) | 45 | 45023 | 2.88 | 6680.9 | n/a | 1000.00 | 76000 | n/a |
| 1000 | 64 | fsys | append_batch(N) + sync_through (window off) | 158 | 157886 | 10.10 | 5603.0 | 12163.9 | 1.00 | 76000 | n/a |
| 1000 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 7 | 7266 | 3.72 | 133981.6 | n/a | 1000.00 | 4096000 | 1.00 |
| 1000 | 512 | store-io | (b) AppendBatch: N x append + commit | 206 | 205903 | 105.42 | 3738.2 | 19774.0 | 1.00 | 512000 | 1.00 |
| 1000 | 512 | raw | (a') N writes of one padded block + one flush | 7 | 6836 | 3.50 | 133082.6 | n/a | 1000.00 | 4096000 | n/a |
| 1000 | 512 | raw | (b') packed: one write of N records + one flush | 288 | 288136 | 147.53 | 2929.7 | 6002.9 | 1.00 | 512000 | n/a |
| 1000 | 512 | fsys | append x N + sync_through (window off) | 115 | 115231 | 59.00 | 9113.1 | 25659.4 | 1000.00 | 524000 | n/a |
| 1000 | 512 | fsys | append_batch(N) + sync_through (window off) | 140 | 140157 | 71.76 | 6183.8 | 13409.7 | 1.00 | 524000 | n/a |
| 1000 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 7 | 7271 | 29.78 | 135518.8 | n/a | 1000.00 | 4096000 | 1.00 |
| 1000 | 4096 | store-io | (b) AppendBatch: N x append + commit | 64 | 64142 | 262.73 | 12715.3 | n/a | 4.00 | 4096000 | 1.00 |
| 1000 | 4096 | raw | (a') N writes of one padded block + one flush | 6 | 5888 | 24.12 | 166410.1 | n/a | 1000.00 | 4096000 | n/a |
| 1000 | 4096 | raw | (b') packed: one write of N records + one flush | 65 | 64930 | 265.95 | 11850.2 | n/a | 1.00 | 4096000 | n/a |
| 1000 | 4096 | fsys | append x N + sync_through (window off) | 31 | 30968 | 126.85 | 29787.6 | n/a | 1000.00 | 4108000 | n/a |
| 1000 | 4096 | fsys | append_batch(N) + sync_through (window off) | 57 | 56800 | 232.65 | 17014.7 | n/a | 1.00 | 4108000 | n/a |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | (a) N x AppendRegion::append + sync_through | passed: 337 of 337 records read back and compared byte for byte; all 337 checked for overlap and alignment | provisioned a 64 MiB append region in 0.05 s (1372 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 482 of 482 records read back and compared byte for byte; all 482 checked for overlap and alignment | provisioned a 64 MiB append region in 0.05 s (1227 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 227 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 222 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 223 of 223 records read back and compared byte for byte; all 223 checked for overlap and alignment | provisioned a 64 MiB append region in 0.13 s (511 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 193 of 193 records read back and compared byte for byte; all 193 checked for overlap and alignment | provisioned a 64 MiB append region in 0.03 s (2021 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 99 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 197 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 466 of 466 records read back and compared byte for byte; all 466 checked for overlap and alignment | provisioned a 64 MiB append region in 0.04 s (1520 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 463 of 463 records read back and compared byte for byte; all 463 checked for overlap and alignment | provisioned a 64 MiB append region in 0.03 s (2491 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 213 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 209 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 2950 records read back and compared byte for byte; all 2950 checked for overlap and alignment | provisioned a 235 MiB append region in 0.09 s (2633 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 4450 records read back and compared byte for byte; all 4450 checked for overlap and alignment | provisioned a 64 MiB append region in 0.06 s (1216 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 2140 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 2230 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 3120 records read back and compared byte for byte; all 3120 checked for overlap and alignment | provisioned a 235 MiB append region in 0.19 s (1274 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 4660 records read back and compared byte for byte; all 4660 checked for overlap and alignment | provisioned a 64 MiB append region in 0.05 s (1292 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 2170 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 2180 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 2960 records read back and compared byte for byte; all 2960 checked for overlap and alignment | provisioned a 235 MiB append region in 0.19 s (1322 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 4730 records read back and compared byte for byte; all 4730 checked for overlap and alignment | provisioned a 235 MiB append region in 0.19 s (1286 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 2080 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 2200 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 5200 records read back and compared byte for byte; all 5200 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.93 s (2301 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 44700 records read back and compared byte for byte; all 44700 checked for overlap and alignment | provisioned a 64 MiB append region in 0.04 s (1753 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 22000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 22500 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 7400 records read back and compared byte for byte; all 7400 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.50 s (4272 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 45100 records read back and compared byte for byte; all 45100 checked for overlap and alignment | provisioned a 305 MiB append region in 0.08 s (3972 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 21500 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 22700 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 4300 records read back and compared byte for byte; all 4300 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.76 s (2832 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 12000 records read back and compared byte for byte; all 12000 checked for overlap and alignment | provisioned a 2048 MiB append region in 1.70 s (1263 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 15000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 16100 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 9000 records read back and compared byte for byte; all 9000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.55 s (3906 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 321000 records read back and compared byte for byte; all 321000 checked for overlap and alignment | provisioned a 375 MiB append region in 0.23 s (1719 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 58000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 166000 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 10000 records read back and compared byte for byte; all 10000 checked for overlap and alignment | provisioned a 2048 MiB append region in 1.14 s (1876 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 231000 records read back and compared byte for byte; all 231000 checked for overlap and alignment | provisioned a 2048 MiB append region in 1.35 s (1588 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 139000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 165000 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 10000 records read back and compared byte for byte; all 10000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.65 s (3287 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 101000 records read back and compared byte for byte; all 101000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.68 s (3175 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 40000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 64000 records replayed with JournalReader and compared byte for byte |  |

