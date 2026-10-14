# store-io performance harness: linux / msft-virtual-disk (2026-10-14)

> **OVERRIDE IN EFFECT.** `Store::create` with default trust was **refused** on this device:
> `device durability is unverified: running under a hypervisor without a certificate for this volume type; missing: device identify data (needs a readable device node); SMART / health log (needs privilege on Linux); volatile write cache presence; volatile write cache enabled state; device atomic-write fields; [Unverified { missing: {Identify, Smart, CachePresence, CacheEnabled, AtomicFields}, reasons: {Hypervisor} }]`
>
> Every store in this run was created with the documented labelled override (`Trust { override_refusal: true, .. }`): the class stays as decided, every receipt is labelled `overridden`, and the most conservative primitive (write + device flush) is used. Nothing else was weakened. The numbers below are therefore **not** gate numbers for a certified durability class; they measure software cost on this stack.

## Run

| item | value |
|---|---|
| date (UTC) | 2026-10-14T07:51:15Z |
| command | all |
| store-io commit | e4f90ecec8fe39c18eccbe385c6a10fe51cfce2c |
| OS | Ubuntu 24.04.4 LTS |
| kernel / build | Linux version 6.6.87.2-microsoft-standard-WSL2 (root@439a258ad544) (gcc (GCC) 11.2.0, GNU ld (GNU Binutils) 2.37) #1 SMP PREEMPT_DYNAMIC Thu Jun  5 18:30:46 UTC 2025 |
| CPU | AMD Ryzen 9 9950X3D 16-Core Processor |
| logical CPUs | 32 |
| filesystem | ext4 on / (device /dev/sdd 8:48, mount options rw,relatime, super options rw,discard,errors=remount-ro,data=ordered); store-io classified it Ext4 |
| directory under test | /home/james/store-io-harness-130084 |
| backend | store-io-posix (PosixPlatform, O_DIRECT; tier in the report's io line) |
| durability class | unverified (receipt label overridden(running under a hypervisor without a certificate for this volume type), durable open Overridden (Trust::override_refusal), block size 4096 B) |
| time per point | 3.0 s measured after 0.3 s warm-up (time-boxed; a point may end early at region capacity, noted where it does) |
| fsys | fsys 1.1.3 (crates.io), Handle method Sync, durability primitive fsync |
| raw primitives | pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync; preadv2 on an O_DIRECT descriptor (one call per request) |
| harness build | release, lto=fat, codegen-units=1 (same as store-io's release profile) |
| total wall time | 521.7 s |

## Device report (`Store::report()`)

```text
volume           ce298fe7-c277-f085-f8be-f37ed7432265
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
| lone | QD1 durable 4 KiB append | durable ops/s (higher is better) | 395.4 | 375.5 | 186.2 | 1.053 | +5.3% | 2.123 | +112.3% | append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config) |
| lone | QD1 durable 4 KiB append | p50 µs (lower is better) | 2378.6 | 2479.7 | 5050.8 | 0.959 | +4.1% | 0.471 | +52.9% | append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config) |
| lone | QD1 durable 4 KiB append | p99 µs (lower is better) | 4714.6 | 6648.6 | 20654.3 | 0.709 | +29.1% | 0.228 | +77.2% | append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config) |
| lone | QD1 durable 4 KiB append, fsys default config | durable ops/s (higher is better) | 395.4 | 375.5 | 166.0 | 1.053 | +5.3% | 2.381 | +138.1% | fsys JournalOptions::new() (500 µs group-commit window) |
| lone | QD1 durable 4 KiB page overwrite | durable ops/s (higher is better) | 373.2 | 375.5 | 194.8 | 0.994 | -0.6% | 1.916 | +91.6% | write_durable vs raw write+flush vs fsys write_at+sync |
| lone | QD1 durable 4 KiB page overwrite | p50 µs (lower is better) | 2514.2 | 2479.7 | 4901.5 | 1.014 | -1.4% | 0.513 | +48.7% | write_durable vs raw write+flush vs fsys write_at+sync |
| lone | QD1 durable 4 KiB page overwrite | p99 µs (lower is better) | 6521.7 | 6648.6 | 20530.5 | 0.981 | +1.9% | 0.318 | +68.2% | write_durable vs raw write+flush vs fsys write_at+sync |
| concurrent | 1 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 393.9 | 277.3 | 150.9 | 1.421 | +42.1% | 2.610 | +161.0% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 1 concurrent durable 4 KiB writers | p99 µs (lower is better) | 4645.1 | 11419.7 | 21673.1 | 0.407 | +59.3% | 0.214 | +78.6% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 2 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 420.4 | 423.9 | 335.3 | 0.992 | -0.8% | 1.254 | +25.4% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 2 concurrent durable 4 KiB writers | p99 µs (lower is better) | 20445.8 | 20319.0 | 10972.5 | 1.006 | -0.6% | 1.863 | -86.3% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 4 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 842.5 | 439.2 | 657.0 | 1.918 | +91.8% | 1.282 | +28.2% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 4 concurrent durable 4 KiB writers | p99 µs (lower is better) | 8451.8 | 24510.6 | 10834.4 | 0.345 | +65.5% | 0.780 | +22.0% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 8 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 1377.2 | 431.2 | 1303.4 | 3.194 | +219.4% | 1.057 | +5.7% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 8 concurrent durable 4 KiB writers | p99 µs (lower is better) | 21026.7 | 33578.0 | 10423.8 | 0.626 | +37.4% | 2.017 | -101.7% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 16 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 2361.8 | 429.3 | 1657.8 | 5.502 | +450.2% | 1.425 | +42.5% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 16 concurrent durable 4 KiB writers | p99 µs (lower is better) | 20964.1 | 74695.3 | 14171.9 | 0.281 | +71.9% | 1.479 | -47.9% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 32 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 4502.6 | 499.0 | 2777.5 | 9.024 | +802.4% | 1.621 | +62.1% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 32 concurrent durable 4 KiB writers | p99 µs (lower is better) | 12921.2 | 103219.5 | 21662.1 | 0.125 | +87.5% | 0.596 | +40.4% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 64 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 5053.5 | 775.3 | 5230.9 | 6.519 | +551.9% | 0.966 | -3.4% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 64 concurrent durable 4 KiB writers | p99 µs (lower is better) | 31075.1 | 116772.7 | 27342.9 | 0.266 | +73.4% | 1.136 | -13.6% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| caller-batch | batch of 1 x 64 B, one barrier | records/s (higher is better) | 400.8 | 391.6 | 173.7 | 1.023 | +2.3% | 2.307 | +130.7% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(64 B) + one sync_through | records/s (higher is better) | 379.9 | 387.8 | 179.6 | 0.980 | -2.0% | 2.115 | +111.5% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1 x 512 B, one barrier | records/s (higher is better) | 388.8 | 380.9 | 190.5 | 1.021 | +2.1% | 2.041 | +104.1% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(512 B) + one sync_through | records/s (higher is better) | 382.0 | 372.7 | 188.2 | 1.025 | +2.5% | 2.030 | +103.0% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1 x 4096 B, one barrier | records/s (higher is better) | 396.8 | 374.9 | 179.9 | 1.059 | +5.9% | 2.206 | +120.6% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(4096 B) + one sync_through | records/s (higher is better) | 396.6 | 385.8 | 184.3 | 1.028 | +2.8% | 2.152 | +115.2% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 64 B, one barrier | records/s (higher is better) | 3699.7 | 3892.8 | 1846.4 | 0.950 | -5.0% | 2.004 | +100.4% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(64 B) + one sync_through | records/s (higher is better) | 2555.3 | 2607.1 | 1886.7 | 0.980 | -2.0% | 1.354 | +35.4% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 512 B, one barrier | records/s (higher is better) | 3852.4 | 1924.1 | 1819.7 | 2.002 | +100.2% | 2.117 | +111.7% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(512 B) + one sync_through | records/s (higher is better) | 2694.5 | 2657.9 | 1046.8 | 1.014 | +1.4% | 2.574 | +157.4% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 4096 B, one barrier | records/s (higher is better) | 3670.5 | 3787.4 | 1841.3 | 0.969 | -3.1% | 1.993 | +99.3% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(4096 B) + one sync_through | records/s (higher is better) | 2627.2 | 2581.1 | 1820.0 | 1.018 | +1.8% | 1.443 | +44.3% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 64 B, one barrier | records/s (higher is better) | 39555.0 | 37606.2 | 18419.6 | 1.052 | +5.2% | 2.147 | +114.7% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(64 B) + one sync_through | records/s (higher is better) | 6015.8 | 6328.1 | 18080.6 | 0.951 | -4.9% | 0.333 | -66.7% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 512 B, one barrier | records/s (higher is better) | 36762.0 | 42114.8 | 17676.6 | 0.873 | -12.7% | 2.080 | +108.0% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(512 B) + one sync_through | records/s (higher is better) | 5569.9 | 5864.7 | 18884.5 | 0.950 | -5.0% | 0.295 | -70.5% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 4096 B, one barrier | records/s (higher is better) | 26930.4 | 25677.7 | 15263.0 | 1.049 | +4.9% | 1.764 | +76.4% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(4096 B) + one sync_through | records/s (higher is better) | 5148.9 | 5561.9 | 13280.6 | 0.926 | -7.4% | 0.388 | -61.2% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 64 B, one barrier | records/s (higher is better) | 190826.6 | 400520.1 | 186991.6 | 0.476 | -52.4% | 1.021 | +2.1% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(64 B) + one sync_through | records/s (higher is better) | 6429.9 | 7048.9 | 172050.1 | 0.912 | -8.8% | 0.037 | -96.3% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 512 B, one barrier | records/s (higher is better) | 238542.5 | 306907.4 | 118368.2 | 0.777 | -22.3% | 2.015 | +101.5% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(512 B) + one sync_through | records/s (higher is better) | 7208.4 | 7067.5 | 117493.8 | 1.020 | +2.0% | 0.061 | -93.9% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 4096 B, one barrier | records/s (higher is better) | 178604.9 | 216352.0 | 36307.4 | 0.826 | -17.4% | 4.919 | +391.9% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(4096 B) + one sync_through | records/s (higher is better) | 6645.5 | 7348.0 | 47666.7 | 0.904 | -9.6% | 0.139 | -86.1% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| page-batch | 1 random 4 KiB pages + one barrier | pages/s (higher is better) | 281.1 | 311.0 | 155.9 | 0.904 | -9.6% | 1.803 | +80.3% | PageBatch::commit vs raw QD1 writes + flush vs fsys write_at x N + sync |
| page-batch | 1 random 4 KiB pages + one barrier | commit p99 µs (lower is better) | 18294.3 | 11030.6 | 12560.2 | 1.659 | -65.9% | 1.457 | -45.7% | PageBatch::commit vs raw QD1 writes + flush vs fsys write_at x N + sync |
| page-batch | 16 random 4 KiB pages + one barrier | pages/s (higher is better) | 4091.0 | 3502.0 | 2215.1 | 1.168 | +16.8% | 1.847 | +84.7% | PageBatch::commit vs raw QD1 writes + flush vs fsys write_at x N + sync |
| page-batch | 16 random 4 KiB pages + one barrier | commit p99 µs (lower is better) | 11315.0 | 8540.9 | 42454.7 | 1.325 | -32.5% | 0.267 | +73.3% | PageBatch::commit vs raw QD1 writes + flush vs fsys write_at x N + sync |
| page-batch | 64 random 4 KiB pages + one barrier | pages/s (higher is better) | 17632.7 | 6362.4 | 9908.7 | 2.771 | +177.1% | 1.780 | +78.0% | PageBatch::commit vs raw QD1 writes + flush vs fsys write_at x N + sync |
| page-batch | 64 random 4 KiB pages + one barrier | commit p99 µs (lower is better) | 5473.3 | 16453.6 | 12619.4 | 0.333 | +66.7% | 0.434 | +56.6% | PageBatch::commit vs raw QD1 writes + flush vs fsys write_at x N + sync |
| page-batch | 256 random 4 KiB pages + one barrier | pages/s (higher is better) | 37646.9 | 7597.3 | 31050.8 | 4.955 | +395.5% | 1.212 | +21.2% | PageBatch::commit vs raw QD1 writes + flush vs fsys write_at x N + sync |
| page-batch | 256 random 4 KiB pages + one barrier | commit p99 µs (lower is better) | 9296.0 | n/a | 10642.9 | n/a | n/a | 0.873 | +12.7% | PageBatch::commit vs raw QD1 writes + flush vs fsys write_at x N + sync |
| sequential | sequential 1 MiB writes, one barrier at the end | MB/s (higher is better) | 1630.9 | 1188.7 | 527.3 | 1.372 | +37.2% | 3.093 | +209.3% | fsys: the better of its buffered and direct journals |
| sequential | sequential 2 MiB writes, one barrier at the end | MB/s (higher is better) | 1121.4 | 918.1 | n/a | 1.221 | +22.1% | n/a | n/a | raw: one 2 MiB write per call |
| sequential | sequential 8 MiB writes, one barrier at the end | MB/s (higher is better) | 1076.6 | 895.3 | n/a | 1.203 | +20.3% | n/a | n/a | raw: one 8 MiB write per call |
| sequential | sequential read, 1 MiB requests | MB/s (higher is better) | 907.0 | 1464.7 | n/a | 0.619 | -38.1% | n/a | n/a | fsys: no comparable unbuffered read (see method) |
| sequential | sequential read, 8 MiB requests | MB/s (higher is better) | 2712.1 | 4236.1 | n/a | 0.640 | -36.0% | n/a | n/a | fsys: no comparable unbuffered read (see method) |
| submitter | randwrite 4 KiB, 1 in flight | ops/s (higher is better) | 6432.7 | 6533.8 | n/a | 0.985 | -1.5% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randwrite 4 KiB, 1 in flight | p99 µs (lower is better) | 246.0 | 220.2 | n/a | 1.117 | -11.7% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randwrite 4 KiB, 8 in flight | ops/s (higher is better) | 39737.7 | 67225.6 | n/a | 0.591 | -40.9% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randwrite 4 KiB, 8 in flight | p99 µs (lower is better) | 296.0 | 268.3 | n/a | 1.103 | -10.3% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randwrite 4 KiB, 32 in flight | ops/s (higher is better) | 60182.0 | 87716.8 | n/a | 0.686 | -31.4% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randwrite 4 KiB, 32 in flight | p99 µs (lower is better) | 619.3 | 2113.5 | n/a | 0.293 | +70.7% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randwrite 4 KiB, 128 in flight | ops/s (higher is better) | 159646.6 | 175036.0 | n/a | 0.912 | -8.8% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randwrite 4 KiB, 128 in flight | p99 µs (lower is better) | 1180.7 | 1253.4 | n/a | 0.942 | +5.8% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randread 4 KiB, 1 in flight | ops/s (higher is better) | 4632.6 | 5380.2 | n/a | 0.861 | -13.9% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randread 4 KiB, 1 in flight | p99 µs (lower is better) | 345.7 | 264.2 | n/a | 1.309 | -30.9% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randread 4 KiB, 8 in flight | ops/s (higher is better) | 54696.9 | 40435.9 | n/a | 1.353 | +35.3% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randread 4 KiB, 8 in flight | p99 µs (lower is better) | 236.8 | 309.2 | n/a | 0.766 | +23.4% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randread 4 KiB, 32 in flight | ops/s (higher is better) | 62646.9 | 143543.8 | n/a | 0.436 | -56.4% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randread 4 KiB, 32 in flight | p99 µs (lower is better) | 583.0 | 428.0 | n/a | 1.362 | -36.2% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randread 4 KiB, 128 in flight | ops/s (higher is better) | 56291.7 | 145997.3 | n/a | 0.386 | -61.4% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randread 4 KiB, 128 in flight | p99 µs (lower is better) | 3121.7 | 4554.8 | n/a | 0.685 | +31.5% | n/a | n/a | Submitter vs fio io_uring |

## Anomalies, errors and integrity failures

- None: every measurement completed and every integrity check passed.

## W1: lone writer, QD1 durable 4 KiB (`lone`)

Wall time: 27.6 s.

**Method.**

- One thread, one operation at a time; each operation is a 4 KiB write made durable before the next starts.
- store-io `append_durable` appends to a freshly provisioned append region; `write_durable` overwrites a 256 MiB page region at sequentially cycling 4 KiB offsets.
- raw: copy the payload into an aligned buffer (the copy store-io also makes), then pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync, at sequentially cycling offsets of a ready 256 MiB file.
- fsys: `JournalHandle::append` + `sync_through(lsn)` (default, window off, direct+window off) on a fresh journal file; `Handle::write_at` + `Handle::sync` on a ready 256 MiB file (fsys opens the file on every call; that is its API).
- Latency covers the write and the durability call only; payload generation happens before the timer starts.

Results:

| system | variant | durable ops/s | p50 µs | p90 µs | p99 µs | p99.9 µs | max µs | mean µs | flushes/op | write calls/op | other calls/op |
|---|---|---|---|---|---|---|---|---|---|---|---|
| store-io | AppendRegion::append_durable | 395 | 2378.6 | 2528.5 | 4714.6 | 18175.3 | 18222.7 | 2529.2 | 1.00 | 1.00 | n/a |
| store-io | AppendRegion::append + sync_through (halves timed separately) | 391 | 2398.5 | 2571.4 | 5284.4 | 18138.5 | 18162.8 | 2560.1 | 1.00 | 1.00 | n/a |
| store-io | PageRegion::write_durable | 373 | 2514.2 | 2674.8 | 6521.7 | 18157.6 | 18319.5 | 2679.8 | 1.00 | 1.00 | n/a |
| raw | write + data flush | 375 | 2479.7 | 2642.4 | 6648.6 | 18240.6 | 18419.8 | 2663.3 | n/a | 1.00 | n/a |
| fsys | journal, default (buffered, 500 µs group-commit window): append + sync_through | 166 | 5662.7 | 6111.3 | 21394.0 | n/a | 21868.2 | 6022.9 | n/a | 1.00 | n/a |
| fsys | journal, buffered, window off: append + sync_through | 186 | 5050.8 | 5333.3 | 20654.3 | n/a | 24382.0 | 5369.9 | n/a | 1.00 | n/a |
| fsys | journal, direct, window off: append + sync_through | 186 | 5189.5 | 5459.9 | 8910.2 | n/a | 20878.0 | 5376.0 | n/a | 0.00 | n/a |
| fsys | Handle::write_at + Handle::sync | 195 | 4901.5 | 5171.7 | 20530.5 | n/a | 20844.7 | 5133.8 | n/a | 1.00 | n/a |

Where the time goes (write call vs durability call, timed separately on the same thread):

| system | variant | write p50 µs | write p99 µs | write mean µs | durability p50 µs | durability p99 µs | durability mean µs |
|---|---|---|---|---|---|---|---|
| store-io | AppendRegion::append + sync_through (halves timed separately) | 142.6 | 2016.6 | 180.1 | 2247.5 | 4343.0 | 2366.5 |
| raw | write + data flush | 142.5 | 2185.0 | 184.1 | 2331.9 | 4571.6 | 2483.7 |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append_durable | passed: 1306 of 1306 records read back and compared byte for byte; all 1306 checked for overlap and alignment | provisioned a 323 MiB append region in 0.12 s (2937 MB/s: fill, flush, verify) |
| store-io | AppendRegion::append + sync_through (halves timed separately) | passed: 1296 of 1296 records read back and compared byte for byte; all 1296 checked for overlap and alignment | provisioned a 323 MiB append region in 0.11 s (3036 MB/s: fill, flush, verify) |
| store-io | PageRegion::write_durable | passed: 1231 of 1231 written pages read back (latest write each) and compared byte for byte |  |
| raw | write + data flush | passed: 1239 of 1239 written slots read back unbuffered and compared byte for byte | file made ready (zero-filled unbuffered + flushed) in 0.05 s (5925 MB/s) |
| fsys | journal, default (buffered, 500 µs group-commit window): append + sync_through | passed: all 546 records replayed with JournalReader and compared byte for byte | journal backend KernelBuffered, direct active false |
| fsys | journal, buffered, window off: append + sync_through | passed: all 619 records replayed with JournalReader and compared byte for byte | journal backend KernelBuffered, direct active false |
| fsys | journal, direct, window off: append + sync_through | passed: all 617 records replayed with JournalReader and compared byte for byte | journal backend KernelDirect, direct active true |
| fsys | Handle::write_at + Handle::sync | passed: 647 of 647 written slots read back unbuffered and compared byte for byte |  |

## W2: concurrent durable writers, 4 KiB (`concurrent`)

Wall time: 97.7 s.

**Method.**

- Writer counts [1, 2, 4, 8, 16, 32, 64]; every thread loops a durable 4 KiB write at queue depth 1 for the whole point.
- store-io: all threads call `AppendRegion::append_durable` on one append region of one store (a fresh store per point). Flush sharing is read from `Store::domain_stats()` deltas over the window: `flushes` issued, barriers that `led` a flush, barriers that `joined` one.
- raw: each thread writes its own contiguous range of one ready 256 MiB file and makes it durable itself with pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync (no sharing).
- fsys: all threads call `append` + `sync_through` on one shared journal (fresh per point), window off and default (500 µs group-commit window).
- An operation counts when it started and finished inside the measured window; rate = counted operations / window.

Results:

| threads | system | variant | durable ops/s | p50 µs | p99 µs | p99.9 µs | flushes issued | barriers led | barriers joined | writes per flush |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | store-io | AppendRegion::append_durable (shared region) | 394 | 2391.8 | 4645.1 | 18017.4 | 1199 | 1199 | 0 | 1.00 |
| 1 | raw | own write + own data flush (per thread range) | 277 | 2711.0 | 11419.7 | n/a | n/a | n/a | n/a | n/a |
| 1 | fsys | journal, buffered, window off: shared journal append + sync_through | 80 | 9638.0 | 35825.8 | n/a | n/a | n/a | n/a | n/a |
| 1 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 151 | 5742.3 | 21673.1 | n/a | n/a | n/a | n/a | n/a |
| 2 | store-io | AppendRegion::append_durable (shared region) | 420 | 4599.5 | 20445.8 | 22482.2 | 1335 | 1335 | 0 | 1.00 |
| 2 | raw | own write + own data flush (per thread range) | 424 | 4634.2 | 20319.0 | 22349.5 | n/a | n/a | n/a | n/a |
| 2 | fsys | journal, buffered, window off: shared journal append + sync_through | 324 | 5406.2 | 20944.9 | n/a | n/a | n/a | n/a | n/a |
| 2 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 335 | 5710.9 | 10972.5 | 22109.3 | n/a | n/a | n/a | n/a |
| 4 | store-io | AppendRegion::append_durable (shared region) | 842 | 4550.8 | 8451.8 | 20554.2 | 1277 | 1277 | 1261 | 1.98 |
| 4 | raw | own write + own data flush (per thread range) | 439 | 8496.1 | 24510.6 | 29959.8 | n/a | n/a | n/a | n/a |
| 4 | fsys | journal, buffered, window off: shared journal append + sync_through | 537 | 5326.9 | 16243.1 | 26458.3 | n/a | n/a | n/a | n/a |
| 4 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 657 | 5888.6 | 10834.4 | 21687.1 | n/a | n/a | n/a | n/a |
| 8 | store-io | AppendRegion::append_durable (shared region) | 1377 | 5158.4 | 21026.7 | 28774.3 | 1133 | 1133 | 3007 | 3.65 |
| 8 | raw | own write + own data flush (per thread range) | 431 | 17398.6 | 33578.0 | 34891.6 | n/a | n/a | n/a | n/a |
| 8 | fsys | journal, buffered, window off: shared journal append + sync_through | 874 | 10216.1 | 26060.1 | 26761.2 | n/a | n/a | n/a | n/a |
| 8 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 1303 | 5986.2 | 10423.8 | 22000.9 | n/a | n/a | n/a | n/a |
| 16 | store-io | AppendRegion::append_durable (shared region) | 2362 | 7135.5 | 20964.1 | 24065.7 | 1126 | 1126 | 5976 | 6.29 |
| 16 | raw | own write + own data flush (per thread range) | 429 | 33753.2 | 74695.3 | 82398.0 | n/a | n/a | n/a | n/a |
| 16 | fsys | journal, buffered, window off: shared journal append + sync_through | 1615 | 10196.5 | 25929.8 | 28565.4 | n/a | n/a | n/a | n/a |
| 16 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 1658 | 10213.0 | 14171.9 | 29782.4 | n/a | n/a | n/a | n/a |
| 32 | store-io | AppendRegion::append_durable (shared region) | 4503 | 7123.7 | 12921.2 | 23883.3 | 1174 | 1174 | 12367 | 11.51 |
| 32 | raw | own write + own data flush (per thread range) | 499 | 58506.3 | 103219.5 | 105688.6 | n/a | n/a | n/a | n/a |
| 32 | fsys | journal, buffered, window off: shared journal append + sync_through | 2777 | 11203.3 | 21662.1 | 29338.3 | n/a | n/a | n/a | n/a |
| 32 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 1574 | 16664.8 | 54308.3 | 66574.4 | n/a | n/a | n/a | n/a |
| 64 | store-io | AppendRegion::append_durable (shared region) | 5054 | 9385.3 | 31075.1 | 34576.7 | 708 | 708 | 14518 | 21.42 |
| 64 | raw | own write + own data flush (per thread range) | 775 | 83440.3 | 116772.7 | 119262.7 | n/a | n/a | n/a | n/a |
| 64 | fsys | journal, buffered, window off: shared journal append + sync_through | 5094 | 11497.3 | 29755.6 | 49293.3 | n/a | n/a | n/a | n/a |
| 64 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 5231 | 11559.4 | 27342.9 | 28461.6 | n/a | n/a | n/a | n/a |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 1326 records read back and compared byte for byte; all 1326 checked for overlap and alignment | provisioned a 516 MiB append region in 0.15 s (3545 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 964 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 282 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.004 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 500 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.004 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 1469 records read back and compared byte for byte; all 1469 checked for overlap and alignment | provisioned a 516 MiB append region in 0.20 s (2650 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1026 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 1097 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 1106 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 2789 records read back and compared byte for byte; all 2789 checked for overlap and alignment | provisioned a 516 MiB append region in 0.23 s (2382 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1028 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.004 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 1793 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 2260 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 4558 records read back and compared byte for byte; all 4558 checked for overlap and alignment | provisioned a 516 MiB append region in 0.22 s (2425 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1032 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.006 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 2920 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 4343 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 7851 records read back and compared byte for byte; all 7851 checked for overlap and alignment | provisioned a 516 MiB append region in 0.22 s (2497 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1040 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.012 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 5345 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 5498 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 14972 records read back and compared byte for byte; all 14972 checked for overlap and alignment | provisioned a 516 MiB append region in 0.18 s (3059 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.004 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1056 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.020 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 9245 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.004 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 5305 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.007 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 16352 records read back and compared byte for byte; all 16352 checked for overlap and alignment | provisioned a 516 MiB append region in 0.28 s (1904 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.004 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1088 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.027 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 17129 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.004 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 17338 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.005 (straddling ops make this slightly above 1) |

**Findings.**

- store-io scaling: 394 durable ops/s at 1 writer, 5054 at 64 writers (21.42 writes per flush) [derived from the table].

## W3: caller batch (N records, one barrier) (`caller-batch`)

Wall time: 255.2 s.

**Method.**

- N in [1, 10, 100, 1000], record sizes [64, 512, 4096] bytes; one commit = N records made durable by one barrier; commits run back to back on one thread. Each point ends at the time box or after 3000 measured commits, whichever comes first.
- store-io (a) `N x append + sync_through`: each append is its own write, padded to a block (documented behaviour). (b) `AppendBatch`: records packed back to back into pooled buffers, `commit()` = one reservation, the fewest writes (largest pooled buffer 1 MiB), one barrier. A fresh store per (N, size).
- raw (a') N writes of one zero-padded block each, then one flush; (b') the N records packed into one aligned buffer padded to a block, one write, one flush. Primitive: pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync. A ready 1 GiB file, offsets cycling.
- fsys: `append` x N + `sync_through(last lsn)`, and `append_batch(&records)` + `sync_through`, on a fresh journal per point, window off.
- records/s = commits/s x N; payload MB/s counts caller bytes only (10^6 bytes); `write calls/commit` and `bytes written/commit` come from the OS counters and show the device writes each design issues per commit.

Results:

| N | record B | system | variant | commits/s | records/s | payload MB/s | commit p50 µs | commit p99 µs | write calls/commit | bytes written/commit | flushes/commit |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 380 | 380 | 0.02 | 2410.7 | 6541.2 | 1.00 | 4096 | 1.00 |
| 1 | 64 | store-io | (b) AppendBatch: N x append + commit | 401 | 401 | 0.03 | 2378.8 | 4417.7 | 1.00 | 4096 | 1.00 |
| 1 | 64 | raw | (a') N writes of one padded block + one flush | 388 | 388 | 0.02 | 2407.0 | 5227.0 | 1.00 | 4096 | n/a |
| 1 | 64 | raw | (b') packed: one write of N records + one flush | 392 | 392 | 0.03 | 2374.7 | 6225.6 | 1.00 | 4096 | n/a |
| 1 | 64 | fsys | append x N + sync_through (window off) | 180 | 180 | 0.01 | 5183.0 | 21147.7 | 1.00 | 76 | n/a |
| 1 | 64 | fsys | append_batch(N) + sync_through (window off) | 174 | 174 | 0.01 | 5394.5 | 21196.7 | 1.00 | 76 | n/a |
| 1 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 382 | 382 | 0.20 | 2463.5 | 4540.8 | 1.00 | 4096 | 1.00 |
| 1 | 512 | store-io | (b) AppendBatch: N x append + commit | 389 | 389 | 0.20 | 2431.5 | 4831.8 | 1.00 | 4096 | 1.00 |
| 1 | 512 | raw | (a') N writes of one padded block + one flush | 373 | 373 | 0.19 | 2522.1 | 6654.0 | 1.00 | 4096 | n/a |
| 1 | 512 | raw | (b') packed: one write of N records + one flush | 381 | 381 | 0.20 | 2486.2 | 4555.8 | 1.00 | 4096 | n/a |
| 1 | 512 | fsys | append x N + sync_through (window off) | 188 | 188 | 0.10 | 5036.6 | 20559.1 | 1.00 | 524 | n/a |
| 1 | 512 | fsys | append_batch(N) + sync_through (window off) | 190 | 190 | 0.10 | 4999.4 | 20628.7 | 1.00 | 524 | n/a |
| 1 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 397 | 397 | 1.62 | 2379.5 | 4517.8 | 1.00 | 4096 | 1.00 |
| 1 | 4096 | store-io | (b) AppendBatch: N x append + commit | 397 | 397 | 1.63 | 2346.8 | 6231.4 | 1.00 | 4096 | 1.00 |
| 1 | 4096 | raw | (a') N writes of one padded block + one flush | 386 | 386 | 1.58 | 2451.1 | 4517.7 | 1.00 | 4096 | n/a |
| 1 | 4096 | raw | (b') packed: one write of N records + one flush | 375 | 375 | 1.54 | 2450.4 | 6686.7 | 1.00 | 4096 | n/a |
| 1 | 4096 | fsys | append x N + sync_through (window off) | 184 | 184 | 0.75 | 5143.1 | 20998.1 | 1.00 | 4108 | n/a |
| 1 | 4096 | fsys | append_batch(N) + sync_through (window off) | 180 | 180 | 0.74 | 5232.4 | 21027.6 | 1.00 | 4108 | n/a |
| 10 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 256 | 2555 | 0.16 | 3737.9 | 6556.5 | 10.00 | 40960 | 1.00 |
| 10 | 64 | store-io | (b) AppendBatch: N x append + commit | 370 | 3700 | 0.24 | 2544.6 | 5224.7 | 1.00 | 4096 | 1.00 |
| 10 | 64 | raw | (a') N writes of one padded block + one flush | 261 | 2607 | 0.17 | 3662.2 | 6327.0 | 10.00 | 40960 | n/a |
| 10 | 64 | raw | (b') packed: one write of N records + one flush | 389 | 3893 | 0.25 | 2389.8 | 4510.4 | 1.00 | 4096 | n/a |
| 10 | 64 | fsys | append x N + sync_through (window off) | 189 | 1887 | 0.12 | 5035.9 | 20663.2 | 10.00 | 760 | n/a |
| 10 | 64 | fsys | append_batch(N) + sync_through (window off) | 185 | 1846 | 0.12 | 5100.6 | 20709.3 | 1.00 | 760 | n/a |
| 10 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 269 | 2694 | 1.38 | 3555.3 | 6184.3 | 10.00 | 40960 | 1.00 |
| 10 | 512 | store-io | (b) AppendBatch: N x append + commit | 385 | 3852 | 1.97 | 2412.7 | 6305.9 | 1.00 | 8192 | 1.00 |
| 10 | 512 | raw | (a') N writes of one padded block + one flush | 266 | 2658 | 1.36 | 3663.1 | 5970.7 | 10.00 | 40960 | n/a |
| 10 | 512 | raw | (b') packed: one write of N records + one flush | 192 | 1924 | 0.99 | 4301.5 | 16636.2 | 1.00 | 8192 | n/a |
| 10 | 512 | fsys | append x N + sync_through (window off) | 105 | 1047 | 0.54 | 6593.8 | 24976.3 | 10.00 | 5240 | n/a |
| 10 | 512 | fsys | append_batch(N) + sync_through (window off) | 182 | 1820 | 0.93 | 5229.3 | 20846.0 | 1.00 | 5240 | n/a |
| 10 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 263 | 2627 | 10.76 | 3717.1 | 5863.0 | 10.00 | 40960 | 1.00 |
| 10 | 4096 | store-io | (b) AppendBatch: N x append + commit | 367 | 3671 | 15.03 | 2580.0 | 5499.7 | 1.00 | 40960 | 1.00 |
| 10 | 4096 | raw | (a') N writes of one padded block + one flush | 258 | 2581 | 10.57 | 3755.4 | 7888.4 | 10.00 | 40960 | n/a |
| 10 | 4096 | raw | (b') packed: one write of N records + one flush | 379 | 3787 | 15.51 | 2551.8 | 5436.8 | 1.00 | 40960 | n/a |
| 10 | 4096 | fsys | append x N + sync_through (window off) | 182 | 1820 | 7.45 | 5197.2 | 10794.3 | 10.00 | 41080 | n/a |
| 10 | 4096 | fsys | append_batch(N) + sync_through (window off) | 184 | 1841 | 7.54 | 5126.3 | 11018.5 | 1.00 | 41080 | n/a |
| 100 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 60 | 6016 | 0.39 | 16088.8 | 26717.2 | 100.00 | 409600 | 1.00 |
| 100 | 64 | store-io | (b) AppendBatch: N x append + commit | 396 | 39555 | 2.53 | 2347.3 | 4830.2 | 1.00 | 8192 | 1.00 |
| 100 | 64 | raw | (a') N writes of one padded block + one flush | 63 | 6328 | 0.40 | 15045.3 | 29909.1 | 100.00 | 409600 | n/a |
| 100 | 64 | raw | (b') packed: one write of N records + one flush | 376 | 37606 | 2.41 | 2509.5 | 4859.7 | 1.00 | 8192 | n/a |
| 100 | 64 | fsys | append x N + sync_through (window off) | 181 | 18081 | 1.16 | 5262.6 | 20738.3 | 100.00 | 7600 | n/a |
| 100 | 64 | fsys | append_batch(N) + sync_through (window off) | 184 | 18420 | 1.18 | 5156.0 | 20761.9 | 1.00 | 7600 | n/a |
| 100 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 56 | 5570 | 2.85 | 17621.4 | 26793.4 | 100.00 | 409600 | 1.00 |
| 100 | 512 | store-io | (b) AppendBatch: N x append + commit | 368 | 36762 | 18.82 | 2545.3 | 5505.5 | 1.00 | 53248 | 1.00 |
| 100 | 512 | raw | (a') N writes of one padded block + one flush | 59 | 5865 | 3.00 | 16598.1 | 22683.3 | 100.00 | 409600 | n/a |
| 100 | 512 | raw | (b') packed: one write of N records + one flush | 421 | 42115 | 21.56 | 2350.2 | 4121.6 | 1.00 | 53248 | n/a |
| 100 | 512 | fsys | append x N + sync_through (window off) | 189 | 18884 | 9.67 | 5170.8 | 7657.9 | 100.00 | 52400 | n/a |
| 100 | 512 | fsys | append_batch(N) + sync_through (window off) | 177 | 17677 | 9.05 | 5216.1 | 15648.2 | 1.00 | 52400 | n/a |
| 100 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 51 | 5149 | 21.09 | 18698.9 | 33932.8 | 100.00 | 409600 | 1.00 |
| 100 | 4096 | store-io | (b) AppendBatch: N x append + commit | 269 | 26930 | 110.31 | 2720.0 | 18581.7 | 1.00 | 409600 | 1.00 |
| 100 | 4096 | raw | (a') N writes of one padded block + one flush | 56 | 5562 | 22.78 | 17536.2 | 24371.6 | 100.00 | 409600 | n/a |
| 100 | 4096 | raw | (b') packed: one write of N records + one flush | 257 | 25678 | 105.18 | 2843.9 | 18959.9 | 1.00 | 409600 | n/a |
| 100 | 4096 | fsys | append x N + sync_through (window off) | 133 | 13281 | 54.40 | 6382.5 | 24037.1 | 100.00 | 410800 | n/a |
| 100 | 4096 | fsys | append_batch(N) + sync_through (window off) | 153 | 15263 | 62.52 | 5905.8 | 12430.8 | 1.00 | 410800 | n/a |
| 1000 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 6 | 6430 | 0.41 | 155435.0 | n/a | 1000.00 | 4096000 | 1.00 |
| 1000 | 64 | store-io | (b) AppendBatch: N x append + commit | 191 | 190827 | 12.21 | 4066.9 | 15669.9 | 1.00 | 65536 | 1.00 |
| 1000 | 64 | raw | (a') N writes of one padded block + one flush | 7 | 7049 | 0.45 | 140957.7 | n/a | 1000.00 | 4096000 | n/a |
| 1000 | 64 | raw | (b') packed: one write of N records + one flush | 401 | 400520 | 25.63 | 2422.1 | 4384.7 | 1.00 | 65536 | n/a |
| 1000 | 64 | fsys | append x N + sync_through (window off) | 172 | 172050 | 11.01 | 5662.3 | 9134.5 | 1000.00 | 76000 | n/a |
| 1000 | 64 | fsys | append_batch(N) + sync_through (window off) | 187 | 186992 | 11.97 | 5266.5 | 7347.2 | 1.00 | 76000 | n/a |
| 1000 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 7 | 7208 | 3.69 | 137018.8 | n/a | 1000.00 | 4096000 | 1.00 |
| 1000 | 512 | store-io | (b) AppendBatch: N x append + commit | 239 | 238542 | 122.13 | 3769.7 | 9548.2 | 1.00 | 512000 | 1.00 |
| 1000 | 512 | raw | (a') N writes of one padded block + one flush | 7 | 7067 | 3.62 | 141123.6 | n/a | 1000.00 | 4096000 | n/a |
| 1000 | 512 | raw | (b') packed: one write of N records + one flush | 307 | 306907 | 157.14 | 2683.6 | 6235.2 | 1.00 | 512000 | n/a |
| 1000 | 512 | fsys | append x N + sync_through (window off) | 117 | 117494 | 60.16 | 9092.2 | 25156.4 | 1000.00 | 524000 | n/a |
| 1000 | 512 | fsys | append_batch(N) + sync_through (window off) | 118 | 118368 | 60.60 | 8467.4 | 25367.0 | 1.00 | 524000 | n/a |
| 1000 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 7 | 6645 | 27.22 | 146513.4 | n/a | 1000.00 | 4096000 | 1.00 |
| 1000 | 4096 | store-io | (b) AppendBatch: N x append + commit | 179 | 178605 | 731.57 | 4594.4 | 13039.4 | 4.00 | 4096000 | 1.00 |
| 1000 | 4096 | raw | (a') N writes of one padded block + one flush | 7 | 7348 | 30.10 | 136284.5 | n/a | 1000.00 | 4096000 | n/a |
| 1000 | 4096 | raw | (b') packed: one write of N records + one flush | 216 | 216352 | 886.18 | 3715.2 | 9721.2 | 1.00 | 4096000 | n/a |
| 1000 | 4096 | fsys | append x N + sync_through (window off) | 48 | 47667 | 195.24 | 15734.2 | 62184.7 | 1000.00 | 4108000 | n/a |
| 1000 | 4096 | fsys | append_batch(N) + sync_through (window off) | 36 | 36307 | 148.72 | 23031.1 | 64404.3 | 1.00 | 4108000 | n/a |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 1249 records read back and compared byte for byte; all 1249 checked for overlap and alignment | provisioned a 65 MiB append region in 0.06 s (1115 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 1316 records read back and compared byte for byte; all 1316 checked for overlap and alignment | provisioned a 65 MiB append region in 0.04 s (1601 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 596 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 578 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 1257 records read back and compared byte for byte; all 1257 checked for overlap and alignment | provisioned a 65 MiB append region in 0.06 s (1157 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 1278 records read back and compared byte for byte; all 1278 checked for overlap and alignment | provisioned a 65 MiB append region in 0.03 s (2062 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 616 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 627 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 1309 records read back and compared byte for byte; all 1309 checked for overlap and alignment | provisioned a 65 MiB append region in 0.05 s (1247 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 1308 records read back and compared byte for byte; all 1308 checked for overlap and alignment | provisioned a 65 MiB append region in 0.03 s (2517 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 610 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 595 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 8470 records read back and compared byte for byte; all 8470 checked for overlap and alignment | provisioned a 645 MiB append region in 0.21 s (3263 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 12270 records read back and compared byte for byte; all 12270 checked for overlap and alignment | provisioned a 65 MiB append region in 0.03 s (2452 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 6200 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 6130 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 8900 records read back and compared byte for byte; all 8900 checked for overlap and alignment | provisioned a 645 MiB append region in 0.21 s (3295 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 12780 records read back and compared byte for byte; all 12780 checked for overlap and alignment | provisioned a 129 MiB append region in 0.06 s (2356 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 3430 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 6000 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 8690 records read back and compared byte for byte; all 8690 checked for overlap and alignment | provisioned a 645 MiB append region in 0.37 s (1808 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 12090 records read back and compared byte for byte; all 12090 checked for overlap and alignment | provisioned a 645 MiB append region in 0.27 s (2527 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 5980 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 6100 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 20100 records read back and compared byte for byte; all 20100 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.63 s (3429 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 130500 records read back and compared byte for byte; all 130500 checked for overlap and alignment | provisioned a 129 MiB append region in 0.04 s (3124 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 59200 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 60700 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 18600 records read back and compared byte for byte; all 18600 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.72 s (2968 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 120400 records read back and compared byte for byte; all 120400 checked for overlap and alignment | provisioned a 838 MiB append region in 0.35 s (2527 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 62400 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 58900 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 17000 records read back and compared byte for byte; all 17000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.93 s (2299 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 87100 records read back and compared byte for byte; all 87100 checked for overlap and alignment | provisioned a 2048 MiB append region in 1.00 s (2158 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 43700 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 49900 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 22000 records read back and compared byte for byte; all 22000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.78 s (2742 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 635000 records read back and compared byte for byte; all 635000 checked for overlap and alignment | provisioned a 1032 MiB append region in 0.47 s (2324 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 559000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 607000 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 25000 records read back and compared byte for byte; all 25000 checked for overlap and alignment | provisioned a 2048 MiB append region in 1.04 s (2067 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 765000 records read back and compared byte for byte; all 765000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.86 s (2502 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 384000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 391000 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 22000 records read back and compared byte for byte; all 22000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.84 s (2554 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 524000 records read back and compared byte for byte; all 524000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.69 s (3103 MB/s: fill, flush, verify); ended early at region capacity: 472 measured ops in 2.87 s |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 165000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 118000 records replayed with JournalReader and compared byte for byte |  |

## W4: page batch (N random 4 KiB pages, one barrier) (`page-batch`)

Wall time: 43.1 s.

**Method.**

- N in [1, 16, 64, 256] distinct pages chosen uniformly at random from a 256 MiB region / file for every commit; one commit = N page writes + one barrier; commits back to back on one thread.
- store-io: one `PageBatch` reused for every commit: N x `write(pos, page)` then `commit()`.
- raw QD1: N x pwritev2 one after another, then one data flush.
- raw QD-N (Windows only): the N writes issued together on an overlapped NO_BUFFERING handle (one event each), all awaited, then one data flush on that handle.
- fsys: N x `Handle::write_at(path, offset, page)` then one `Handle::sync(path)`.
- Every system rewrites its own ready file / region; page payloads are copied once inside the timed section by every system.

Results:

| N | system | variant | commits/s | pages/s | commit p50 µs | commit p99 µs | commit p99.9 µs | write calls/commit | flushes/commit |
|---|---|---|---|---|---|---|---|---|---|
| 1 | store-io | PageBatch: N x write + commit | 281 | 281 | 2615.6 | 18294.3 | n/a | 1.00 | 1.00 |
| 1 | raw | QD1: N writes one at a time + one flush | 311 | 311 | 2382.9 | 11030.6 | n/a | 1.00 | n/a |
| 1 | fsys | Handle::write_at x N + Handle::sync | 156 | 156 | 5386.7 | 12560.2 | n/a | 1.00 | n/a |
| 16 | store-io | PageBatch: N x write + commit | 256 | 4091 | 2864.6 | 11315.0 | n/a | 16.00 | 1.00 |
| 16 | raw | QD1: N writes one at a time + one flush | 219 | 3502 | 4312.0 | 8540.9 | n/a | 16.00 | n/a |
| 16 | fsys | Handle::write_at x N + Handle::sync | 138 | 2215 | 5652.5 | 42454.7 | n/a | 16.00 | n/a |
| 64 | store-io | PageBatch: N x write + commit | 276 | 17633 | 3528.7 | 5473.3 | n/a | 64.00 | 1.00 |
| 64 | raw | QD1: N writes one at a time + one flush | 99 | 6362 | 9732.3 | 16453.6 | n/a | 64.00 | n/a |
| 64 | fsys | Handle::write_at x N + Handle::sync | 155 | 9909 | 6204.2 | 12619.4 | n/a | 64.00 | n/a |
| 256 | store-io | PageBatch: N x write + commit | 147 | 37647 | 6704.3 | 9296.0 | n/a | 256.00 | 1.00 |
| 256 | raw | QD1: N writes one at a time + one flush | 30 | 7597 | 33354.5 | n/a | n/a | 256.00 | n/a |
| 256 | fsys | Handle::write_at x N + Handle::sync | 121 | 31051 | 8114.5 | 10642.9 | n/a | 256.00 | n/a |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | PageBatch: N x write + commit | passed: 860 of 860 written pages read back (latest write each) and compared byte for byte |  |
| raw | QD1: N writes one at a time + one flush | passed: 986 of 986 written slots read back unbuffered and compared byte for byte |  |
| fsys | Handle::write_at x N + Handle::sync | passed: 505 of 505 written slots read back unbuffered and compared byte for byte |  |
| store-io | PageBatch: N x write + commit | passed: 1025 of 11635 written pages read back (latest write each) and compared byte for byte |  |
| raw | QD1: N writes one at a time + one flush | passed: 1025 of 10081 written slots read back unbuffered and compared byte for byte |  |
| fsys | Handle::write_at x N + Handle::sync | passed: 1025 of 6394 written slots read back unbuffered and compared byte for byte |  |
| store-io | PageBatch: N x write + commit | passed: 1025 of 37157 written pages read back (latest write each) and compared byte for byte |  |
| raw | QD1: N writes one at a time + one flush | passed: 1025 of 17133 written slots read back unbuffered and compared byte for byte |  |
| fsys | Handle::write_at x N + Handle::sync | passed: 1025 of 24602 written slots read back unbuffered and compared byte for byte |  |
| store-io | PageBatch: N x write + commit | passed: 1025 of 54472 written pages read back (latest write each) and compared byte for byte |  |
| raw | QD1: N writes one at a time + one flush | passed: 1025 of 20511 written slots read back unbuffered and compared byte for byte |  |
| fsys | Handle::write_at x N + Handle::sync | passed: 1025 of 51095 written slots read back unbuffered and compared byte for byte |  |

## W5: sequential bandwidth (1, 2 and 8 MiB writes, 1/8 MiB reads) (`sequential`)

Wall time: 39.3 s.

**Method.**

- Write: one thread writes 1 MiB chunks back to back (not durable), then one durability barrier; MB/s = bytes / (time inside the write calls + the barrier); payload generation between writes is excluded (the wall clock is recorded as `wall_s` in the JSON). No warm-up (the region is written once); the point stops at 1 GiB or at the time box.
- store-io: `AppendRegion::append(1 MiB)` into a fresh 1 GiB append region, then `sync_through(last ticket)`. raw: copy into an aligned buffer + pwritev2 per chunk into a ready 1 GiB file, then one data flush. fsys: journal `append(1 MiB)` + one `sync_through` on a fresh journal (buffered and direct, window off; fsys extends its file as it appends, store-io and raw overwrite preallocated space).
- Large writes: the same with 2 and 8 MiB per call (store-io `append` spanning several pool buffers; raw one write per call), each into its own fresh region / file; every 1 MiB of payload is generated and checked as in the 1 MiB case.
- Read: the bytes just written, front to back, repeated until the time box: store-io `AppendRegion::read(offset, &mut out)` with 1 MiB and 8 MiB `out`; raw preadv2 on an O_DIRECT descriptor (one call per request) with the same request sizes. Latency is per request; MB/s = bytes / time inside the read calls (the content spot-check between requests is excluded).
- fsys read: not measured. fsys 1.1.3 has no unbuffered positioned read (`Handle::read_at` opens the file buffered on every call, and `JournalReader` replays through a buffered reader), so it would measure the page cache, not the device.
- MB = 10^6 bytes.

Results:

| system | variant | request | MB/s | bytes | seconds | barrier ms | request p50 µs | request p99 µs | request max µs | write calls/request |
|---|---|---|---|---|---|---|---|---|---|---|
| store-io | AppendRegion::append(1 MiB) x N + sync_through | 1048576 | 1630.9 | 1073741824 | 0.658 | 10.95 | 465.2 | 3100.5 | 5822.6 | 1.00 |
| store-io | AppendRegion::read, 1 MiB requests | 1048576 | 907.0 | 2639265792 | 2.910 | n/a | 840.9 | 4419.5 | 14094.7 | 0.00 |
| store-io | AppendRegion::read, 8 MiB requests | 8388608 | 2712.1 | 8019509248 | 2.957 | n/a | 2510.1 | 10870.8 | 17640.6 | 0.00 |
| store-io | AppendRegion::append(2 MiB) x N + sync_through | 2097152 | 1121.4 | 1073741824 | 0.957 | 4.63 | 1814.0 | 3516.0 | 6470.9 | 2.00 |
| store-io | AppendRegion::append(8 MiB) x N + sync_through | 8388608 | 1076.6 | 1073741824 | 0.997 | 4.66 | 7347.2 | 12996.6 | 13042.4 | 8.00 |
| raw | 1 MiB writes + one data flush | 1048576 | 1188.7 | 1073741824 | 0.903 | 5.97 | 834.5 | 1040.8 | 7068.1 | 1.00 |
| raw | unbuffered read, 1 MiB requests | 1048576 | 1464.7 | 4126146560 | 2.817 | n/a | 687.0 | 2058.0 | 2552.7 | 0.00 |
| raw | unbuffered read, 8 MiB requests | 8388608 | 4236.1 | 12348030976 | 2.915 | n/a | 1869.9 | 3448.1 | 19917.4 | 0.00 |
| raw | diagnostic: unbuffered read into a store-io-buf pool buffer, 1 MiB requests | 1048576 | 1563.2 | 4384096256 | 2.805 | n/a | 658.1 | 1290.9 | 2556.7 | 0.00 |
| raw | diagnostic: unbuffered read into a pool buffer + copy into a caller Vec, 1 MiB requests | 1048576 | 1409.8 | 4053794816 | 2.875 | n/a | 669.1 | 2266.0 | 3191.8 | 0.00 |
| raw | diagnostic: unbuffered read at offsets + 48 KiB (store-io's data alignment), 1 MiB requests | 1048576 | 1571.3 | 4712300544 | 2.999 | n/a | 657.0 | 829.2 | 2848.8 | 0.00 |
| raw | 2 MiB writes + one data flush | 2097152 | 918.1 | 1073741824 | 1.170 | 5.17 | 2177.9 | 4286.4 | 7244.7 | 1.00 |
| raw | 8 MiB writes + one data flush | 8388608 | 895.3 | 1073741824 | 1.199 | 5.42 | 9150.6 | 13660.0 | 14038.7 | 1.00 |
| fsys | journal, buffered, window off: append(1 MiB) x N + sync_through | 1048576 | 498.2 | 1073741824 | 2.155 | 1373.88 | 813.7 | 1028.6 | 1572.7 | 1.00 |
| fsys | journal, direct, window off: append(1 MiB) x N + sync_through | 1048576 | 527.3 | 1073741824 | 2.036 | 12.46 | 1654.0 | 9820.5 | 13501.0 | 1.00 |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append(1 MiB) x N + sync_through | passed: all 1024 MiB read back through AppendRegion::read and compared byte for byte | provisioned a 1024 MiB append region in 0.40 s (2692 MB/s: fill, flush, verify) |
| store-io | AppendRegion::read, 1 MiB requests | passed: first 1 MiB of each of 2764 requests compared against what was written |  |
| store-io | AppendRegion::read, 8 MiB requests | passed: first 1 MiB of each of 1035 requests compared against what was written |  |
| store-io | AppendRegion::append(2 MiB) x N + sync_through | passed: all 1024 MiB read back through AppendRegion::read and compared byte for byte | provisioned a 1024 MiB append region in 0.51 s (2095 MB/s: fill, flush, verify) |
| store-io | AppendRegion::append(8 MiB) x N + sync_through | passed: all 1024 MiB read back through AppendRegion::read and compared byte for byte | provisioned a 1024 MiB append region in 0.87 s (1236 MB/s: fill, flush, verify) |
| raw | 1 MiB writes + one data flush | passed: all 1024 MiB read back unbuffered and compared byte for byte | file made ready (zero-filled unbuffered 1 MiB writes + flushed) in 0.19 s (5739 MB/s); compare store-io's provisioning note |
| raw | unbuffered read, 1 MiB requests | passed: first 1 MiB of each of 4327 requests compared against what was written |  |
| raw | unbuffered read, 8 MiB requests | passed: first 1 MiB of each of 1625 requests compared against what was written |  |
| raw | diagnostic: unbuffered read into a store-io-buf pool buffer, 1 MiB requests | passed: first 1 MiB of each of 4504 requests compared against what was written | not a comparison row: separates pool-memory cost from engine-path cost |
| raw | diagnostic: unbuffered read into a pool buffer + copy into a caller Vec, 1 MiB requests | passed: first 1 MiB of each of 4309 requests compared against what was written | not a comparison row: reproduces store-io's read path (device read into a pooled buffer, then a copy into the caller's buffer) |
| raw | diagnostic: unbuffered read at offsets + 48 KiB (store-io's data alignment), 1 MiB requests | not checked: shifted requests straddle written chunks; the unshifted rows check content | not a comparison row: tests whether store-io's region data offset (48 KiB past a 1 MiB boundary) costs bandwidth on this stack |
| raw | 2 MiB writes + one data flush | passed: all 1024 MiB read back unbuffered and compared byte for byte | file made ready (zero-filled unbuffered 1 MiB writes + flushed) in 0.18 s (6107 MB/s); compare store-io's provisioning note |
| raw | 8 MiB writes + one data flush | passed: all 1024 MiB read back unbuffered and compared byte for byte | file made ready (zero-filled unbuffered 1 MiB writes + flushed) in 0.19 s (5790 MB/s); compare store-io's provisioning note |
| fsys | journal, buffered, window off: append(1 MiB) x N + sync_through | passed: all 1024 records replayed with JournalReader and compared byte for byte |  |
| fsys | journal, direct, window off: append(1 MiB) x N + sync_through | passed: all 1024 records replayed with JournalReader and compared byte for byte |  |

## W6: N in flight from one thread, random 4 KiB (Submitter vs fio) (`submitter`)

Wall time: 58.0 s.

**Method.**

- Queue depth N in [1, 8, 32, 128]; uniformly random 4 KiB-aligned offsets over 256 MiB; one thread keeps N operations in flight, resubmitting as each completes; writes are plain (no per-write durability), reads read back.
- store-io: `Store::submitter(depth N)`, `write(region, pos, buf, Plain)` / `read_page`, `wait(1)`; buffers from the submitter's pool (`take_buf`).
- raw: fio --ioengine=io_uring --direct=1 --bs=4k --iodepth=N --numjobs=1 on a 256 MiB file laid out first; time-based, with the same warm-up as ramp time.
- Latency: per operation, submit to completion observed (store-io); fio's completion latency (clat).

Results:

| pattern | N | system | variant | ops/s | p50 µs | p99 µs | p99.9 µs |
|---|---|---|---|---|---|---|---|
| randwrite | 1 | store-io | Submitter randwrite QD1 | 6433 | 140.6 | 246.0 | 1731.1 |
| randwrite | 1 | raw | fio io_uring randwrite QD1 | 6534 | 124.4 | 220.2 | 2072.6 |
| randwrite | 8 | store-io | Submitter randwrite QD8 | 39738 | 175.9 | 296.0 | 2154.0 |
| randwrite | 8 | raw | fio io_uring randwrite QD8 | 67226 | 107.0 | 268.3 | 2146.3 |
| randwrite | 32 | store-io | Submitter randwrite QD32 | 60182 | 340.9 | 619.3 | 2304.7 |
| randwrite | 32 | raw | fio io_uring randwrite QD32 | 87717 | 329.7 | 2113.5 | 2670.6 |
| randwrite | 128 | store-io | Submitter randwrite QD128 | 159647 | 772.0 | 1180.7 | 4004.3 |
| randwrite | 128 | raw | fio io_uring randwrite QD128 | 175036 | 667.6 | 1253.4 | 2965.5 |
| randread | 1 | store-io | Submitter randread QD1 | 4633 | 205.2 | 345.7 | 1995.2 |
| randread | 1 | raw | fio io_uring randread QD1 | 5380 | 166.9 | 264.2 | 2056.2 |
| randread | 8 | store-io | Submitter randread QD8 | 54697 | 123.2 | 236.8 | 505.5 |
| randread | 8 | raw | fio io_uring randread QD8 | 40436 | 189.4 | 309.2 | 1581.1 |
| randread | 32 | store-io | Submitter randread QD32 | 62647 | 312.2 | 583.0 | 1519.7 |
| randread | 32 | raw | fio io_uring randread QD32 | 143544 | 209.9 | 428.0 | 2277.4 |
| randread | 128 | store-io | Submitter randread QD128 | 56292 | 1161.6 | 3121.7 | 5985.5 |
| randread | 128 | raw | fio io_uring randread QD128 | 145997 | 716.8 | 4554.8 | 11993.1 |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | Submitter randwrite QD1 | not checked: throughput point; correctness is the test suites' job | io: synchronous (chosen) |
| raw | fio io_uring randwrite QD1 | not checked: fio baseline |  |
| store-io | Submitter randwrite QD8 | not checked: throughput point; correctness is the test suites' job | io: synchronous (chosen) |
| raw | fio io_uring randwrite QD8 | not checked: fio baseline |  |
| store-io | Submitter randwrite QD32 | not checked: throughput point; correctness is the test suites' job | io: synchronous (chosen) |
| raw | fio io_uring randwrite QD32 | not checked: fio baseline |  |
| store-io | Submitter randwrite QD128 | not checked: throughput point; correctness is the test suites' job | io: synchronous (chosen) |
| raw | fio io_uring randwrite QD128 | not checked: fio baseline |  |
| store-io | Submitter randread QD1 | not checked: throughput point; correctness is the test suites' job | io: synchronous (chosen) |
| raw | fio io_uring randread QD1 | not checked: fio baseline |  |
| store-io | Submitter randread QD8 | not checked: throughput point; correctness is the test suites' job | io: synchronous (chosen) |
| raw | fio io_uring randread QD8 | not checked: fio baseline |  |
| store-io | Submitter randread QD32 | not checked: throughput point; correctness is the test suites' job | io: synchronous (chosen) |
| raw | fio io_uring randread QD32 | not checked: fio baseline |  |
| store-io | Submitter randread QD128 | not checked: throughput point; correctness is the test suites' job | io: synchronous (chosen) |
| raw | fio io_uring randread QD128 | not checked: fio baseline |  |

