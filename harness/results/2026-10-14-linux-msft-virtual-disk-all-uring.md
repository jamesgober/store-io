# store-io performance harness: linux / msft-virtual-disk (2026-10-14)

> **OVERRIDE IN EFFECT.** `Store::create` with default trust was **refused** on this device:
> `device durability is unverified: running under a hypervisor without a certificate for this volume type; missing: device identify data (needs a readable device node); SMART / health log (needs privilege on Linux); volatile write cache presence; volatile write cache enabled state; device atomic-write fields; [Unverified { missing: {Identify, Smart, CachePresence, CacheEnabled, AtomicFields}, reasons: {Hypervisor} }]`
>
> Every store in this run was created with the documented labelled override (`Trust { override_refusal: true, .. }`): the class stays as decided, every receipt is labelled `overridden`, and the most conservative primitive (write + device flush) is used. Nothing else was weakened. The numbers below are therefore **not** gate numbers for a certified durability class; they measure software cost on this stack.

## Run

| item | value |
|---|---|
| date (UTC) | 2026-10-14T07:43:02Z |
| command | all |
| store-io commit | e4f90ecec8fe39c18eccbe385c6a10fe51cfce2c |
| OS | Ubuntu 24.04.4 LTS |
| kernel / build | Linux version 6.6.87.2-microsoft-standard-WSL2 (root@439a258ad544) (gcc (GCC) 11.2.0, GNU ld (GNU Binutils) 2.37) #1 SMP PREEMPT_DYNAMIC Thu Jun  5 18:30:46 UTC 2025 |
| CPU | AMD Ryzen 9 9950X3D 16-Core Processor |
| logical CPUs | 32 |
| filesystem | ext4 on / (device /dev/sdd 8:48, mount options rw,relatime, super options rw,discard,errors=remount-ro,data=ordered); store-io classified it Ext4 |
| directory under test | /home/james/store-io-harness-128310 |
| backend | store-io-posix (PosixPlatform, O_DIRECT; tier in the report's io line) |
| durability class | unverified (receipt label overridden(running under a hypervisor without a certificate for this volume type), durable open Overridden (Trust::override_refusal), block size 4096 B) |
| time per point | 3.0 s measured after 0.3 s warm-up (time-boxed; a point may end early at region capacity, noted where it does) |
| fsys | fsys 1.1.3 (crates.io), Handle method Sync, durability primitive fsync |
| raw primitives | pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync; preadv2 on an O_DIRECT descriptor (one call per request) |
| harness build | release, lto=fat, codegen-units=1 (same as store-io's release profile) |
| total wall time | 519.8 s |

## Device report (`Store::report()`)

```text
volume           1dde7a0f-835a-d8b3-f4d5-773cf19b4e5a
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
| lone | QD1 durable 4 KiB append | durable ops/s (higher is better) | 98.4 | 167.4 | 158.5 | 0.588 | -41.2% | 0.621 | -37.9% | append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config) |
| lone | QD1 durable 4 KiB append | p50 µs (lower is better) | 6369.0 | 3399.9 | 5543.3 | 1.873 | -87.3% | 1.149 | -14.9% | append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config) |
| lone | QD1 durable 4 KiB append | p99 µs (lower is better) | 95926.3 | 29320.7 | 22213.4 | 3.272 | -227.2% | 4.318 | -331.8% | append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config) |
| lone | QD1 durable 4 KiB append, fsys default config | durable ops/s (higher is better) | 98.4 | 167.4 | 138.6 | 0.588 | -41.2% | 0.710 | -29.0% | fsys JournalOptions::new() (500 µs group-commit window) |
| lone | QD1 durable 4 KiB page overwrite | durable ops/s (higher is better) | 245.6 | 167.4 | 174.7 | 1.467 | +46.7% | 1.406 | +40.6% | write_durable vs raw write+flush vs fsys write_at+sync |
| lone | QD1 durable 4 KiB page overwrite | p50 µs (lower is better) | 2551.2 | 3399.9 | 5188.7 | 0.750 | +25.0% | 0.492 | +50.8% | write_durable vs raw write+flush vs fsys write_at+sync |
| lone | QD1 durable 4 KiB page overwrite | p99 µs (lower is better) | 24443.9 | 29320.7 | 21050.7 | 0.834 | +16.6% | 1.161 | -16.1% | write_durable vs raw write+flush vs fsys write_at+sync |
| concurrent | 1 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 352.3 | 335.9 | 89.3 | 1.049 | +4.9% | 3.945 | +294.5% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 1 concurrent durable 4 KiB writers | p99 µs (lower is better) | 11072.5 | 11378.9 | 44002.1 | 0.973 | +2.7% | 0.252 | +74.8% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 2 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 261.0 | 378.1 | 293.3 | 0.690 | -31.0% | 0.890 | -11.0% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 2 concurrent durable 4 KiB writers | p99 µs (lower is better) | 47541.3 | 20884.5 | 21997.9 | 2.276 | -127.6% | 2.161 | -116.1% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 4 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 811.1 | 432.3 | 659.9 | 1.876 | +87.6% | 1.229 | +22.9% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 4 concurrent durable 4 KiB writers | p99 µs (lower is better) | 20072.1 | 24358.7 | 9690.9 | 0.824 | +17.6% | 2.071 | -107.1% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 8 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 1389.5 | 163.9 | 1274.7 | 8.478 | +747.8% | 1.090 | +9.0% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 8 concurrent durable 4 KiB writers | p99 µs (lower is better) | 13640.6 | 107918.5 | 11700.5 | 0.126 | +87.4% | 1.166 | -16.6% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 16 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 2414.1 | 380.3 | 1281.8 | 6.347 | +534.7% | 1.883 | +88.3% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 16 concurrent durable 4 KiB writers | p99 µs (lower is better) | 11359.0 | 69324.3 | 30532.8 | 0.164 | +83.6% | 0.372 | +62.8% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 32 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 4055.0 | 451.1 | 2571.6 | 8.989 | +798.9% | 1.577 | +57.7% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 32 concurrent durable 4 KiB writers | p99 µs (lower is better) | 20035.2 | 169481.5 | 43642.6 | 0.118 | +88.2% | 0.459 | +54.1% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 64 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 3325.0 | 784.7 | 2086.5 | 4.237 | +323.7% | 1.594 | +59.4% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 64 concurrent durable 4 KiB writers | p99 µs (lower is better) | 49855.1 | 143756.2 | 232361.3 | 0.347 | +65.3% | 0.215 | +78.5% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| caller-batch | batch of 1 x 64 B, one barrier | records/s (higher is better) | 119.0 | 156.2 | 78.8 | 0.762 | -23.8% | 1.510 | +51.0% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(64 B) + one sync_through | records/s (higher is better) | 148.0 | 146.0 | 70.0 | 1.014 | +1.4% | 2.115 | +111.5% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1 x 512 B, one barrier | records/s (higher is better) | 159.1 | 156.7 | 56.6 | 1.015 | +1.5% | 2.811 | +181.1% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(512 B) + one sync_through | records/s (higher is better) | 177.1 | 166.3 | 71.5 | 1.065 | +6.5% | 2.477 | +147.7% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1 x 4096 B, one barrier | records/s (higher is better) | 387.7 | 367.9 | 183.6 | 1.054 | +5.4% | 2.112 | +111.2% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(4096 B) + one sync_through | records/s (higher is better) | 173.9 | 385.8 | 177.9 | 0.451 | -54.9% | 0.977 | -2.3% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 64 B, one barrier | records/s (higher is better) | 3931.2 | 3954.0 | 1072.9 | 0.994 | -0.6% | 3.664 | +266.4% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(64 B) + one sync_through | records/s (higher is better) | 2618.7 | 2734.7 | 1793.6 | 0.958 | -4.2% | 1.460 | +46.0% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 512 B, one barrier | records/s (higher is better) | 3914.1 | 3750.0 | 1766.8 | 1.044 | +4.4% | 2.215 | +121.5% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(512 B) + one sync_through | records/s (higher is better) | 1519.4 | 2799.8 | 1774.7 | 0.543 | -45.7% | 0.856 | -14.4% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 4096 B, one barrier | records/s (higher is better) | 3860.9 | 4042.5 | 1229.4 | 0.955 | -4.5% | 3.141 | +214.1% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(4096 B) + one sync_through | records/s (higher is better) | 2511.5 | 2771.5 | 1860.3 | 0.906 | -9.4% | 1.350 | +35.0% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 64 B, one barrier | records/s (higher is better) | 38533.7 | 34029.9 | 17055.0 | 1.132 | +13.2% | 2.259 | +125.9% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(64 B) + one sync_through | records/s (higher is better) | 1931.5 | 5822.7 | 16954.4 | 0.332 | -66.8% | 0.114 | -88.6% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 512 B, one barrier | records/s (higher is better) | 33536.1 | 40985.0 | 18878.1 | 0.818 | -18.2% | 1.776 | +77.6% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(512 B) + one sync_through | records/s (higher is better) | 4786.1 | 6012.9 | 15775.8 | 0.796 | -20.4% | 0.303 | -69.7% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 4096 B, one barrier | records/s (higher is better) | 14114.3 | 24283.2 | 14403.7 | 0.581 | -41.9% | 0.980 | -2.0% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(4096 B) + one sync_through | records/s (higher is better) | 5042.2 | 5872.9 | 13030.4 | 0.859 | -14.1% | 0.387 | -61.3% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 64 B, one barrier | records/s (higher is better) | 380426.4 | 395069.3 | 178347.1 | 0.963 | -3.7% | 2.133 | +113.3% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(64 B) + one sync_through | records/s (higher is better) | 7047.1 | 8002.3 | 164328.7 | 0.881 | -11.9% | 0.043 | -95.7% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 512 B, one barrier | records/s (higher is better) | 358671.2 | 318419.3 | 152786.0 | 1.126 | +12.6% | 2.348 | +134.8% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(512 B) + one sync_through | records/s (higher is better) | 6910.5 | 7994.2 | 125297.0 | 0.864 | -13.6% | 0.055 | -94.5% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 4096 B, one barrier | records/s (higher is better) | 114210.0 | 157287.3 | 66128.3 | 0.726 | -27.4% | 1.727 | +72.7% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(4096 B) + one sync_through | records/s (higher is better) | 7375.2 | 7471.6 | 59403.3 | 0.987 | -1.3% | 0.124 | -87.6% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| page-batch | 1 random 4 KiB pages + one barrier | pages/s (higher is better) | 330.7 | 328.1 | 152.4 | 1.008 | +0.8% | 2.170 | +117.0% | PageBatch::commit vs raw QD1 writes + flush vs fsys write_at x N + sync |
| page-batch | 1 random 4 KiB pages + one barrier | commit p99 µs (lower is better) | 6246.7 | 6744.2 | 20938.3 | 0.926 | +7.4% | 0.298 | +70.2% | PageBatch::commit vs raw QD1 writes + flush vs fsys write_at x N + sync |
| page-batch | 16 random 4 KiB pages + one barrier | pages/s (higher is better) | 2914.8 | 2584.6 | 2833.9 | 1.128 | +12.8% | 1.029 | +2.9% | PageBatch::commit vs raw QD1 writes + flush vs fsys write_at x N + sync |
| page-batch | 16 random 4 KiB pages + one barrier | commit p99 µs (lower is better) | 12568.7 | 26687.3 | 11016.0 | 0.471 | +52.9% | 1.141 | -14.1% | PageBatch::commit vs raw QD1 writes + flush vs fsys write_at x N + sync |
| page-batch | 64 random 4 KiB pages + one barrier | pages/s (higher is better) | 19382.8 | 6146.8 | 10782.4 | 3.153 | +215.3% | 1.798 | +79.8% | PageBatch::commit vs raw QD1 writes + flush vs fsys write_at x N + sync |
| page-batch | 64 random 4 KiB pages + one barrier | commit p99 µs (lower is better) | 5065.5 | 17225.6 | 9311.8 | 0.294 | +70.6% | 0.544 | +45.6% | PageBatch::commit vs raw QD1 writes + flush vs fsys write_at x N + sync |
| page-batch | 256 random 4 KiB pages + one barrier | pages/s (higher is better) | 49025.1 | 7274.2 | 25154.9 | 6.740 | +574.0% | 1.949 | +94.9% | PageBatch::commit vs raw QD1 writes + flush vs fsys write_at x N + sync |
| page-batch | 256 random 4 KiB pages + one barrier | commit p99 µs (lower is better) | 6750.5 | n/a | 23754.8 | n/a | n/a | 0.284 | +71.6% | PageBatch::commit vs raw QD1 writes + flush vs fsys write_at x N + sync |
| sequential | sequential 1 MiB writes, one barrier at the end | MB/s (higher is better) | 870.8 | 2809.2 | 822.9 | 0.310 | -69.0% | 1.058 | +5.8% | fsys: the better of its buffered and direct journals |
| sequential | sequential 2 MiB writes, one barrier at the end | MB/s (higher is better) | 2296.8 | 2331.9 | n/a | 0.985 | -1.5% | n/a | n/a | raw: one 2 MiB write per call |
| sequential | sequential 8 MiB writes, one barrier at the end | MB/s (higher is better) | 2904.1 | 2680.3 | n/a | 1.084 | +8.4% | n/a | n/a | raw: one 8 MiB write per call |
| sequential | sequential read, 1 MiB requests | MB/s (higher is better) | 996.2 | 1551.0 | n/a | 0.642 | -35.8% | n/a | n/a | fsys: no comparable unbuffered read (see method) |
| sequential | sequential read, 8 MiB requests | MB/s (higher is better) | 3188.3 | 4294.1 | n/a | 0.742 | -25.8% | n/a | n/a | fsys: no comparable unbuffered read (see method) |
| submitter | randwrite 4 KiB, 1 in flight | ops/s (higher is better) | 9857.6 | 4313.7 | n/a | 2.285 | +128.5% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randwrite 4 KiB, 1 in flight | p99 µs (lower is better) | 165.4 | 228.4 | n/a | 0.724 | +27.6% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randwrite 4 KiB, 8 in flight | ops/s (higher is better) | 32257.1 | 67165.6 | n/a | 0.480 | -52.0% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randwrite 4 KiB, 8 in flight | p99 µs (lower is better) | 254.9 | 259.1 | n/a | 0.984 | +1.6% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randwrite 4 KiB, 32 in flight | ops/s (higher is better) | 99490.6 | 109294.2 | n/a | 0.910 | -9.0% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randwrite 4 KiB, 32 in flight | p99 µs (lower is better) | 516.6 | 522.2 | n/a | 0.989 | +1.1% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randwrite 4 KiB, 128 in flight | ops/s (higher is better) | 188752.0 | 159451.1 | n/a | 1.184 | +18.4% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randwrite 4 KiB, 128 in flight | p99 µs (lower is better) | 1401.6 | 2146.3 | n/a | 0.653 | +34.7% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randread 4 KiB, 1 in flight | ops/s (higher is better) | 11604.4 | 5283.9 | n/a | 2.196 | +119.6% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randread 4 KiB, 1 in flight | p99 µs (lower is better) | 160.8 | 250.9 | n/a | 0.641 | +35.9% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randread 4 KiB, 8 in flight | ops/s (higher is better) | 86564.2 | 39438.5 | n/a | 2.195 | +119.5% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randread 4 KiB, 8 in flight | p99 µs (lower is better) | 134.0 | 297.0 | n/a | 0.451 | +54.9% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randread 4 KiB, 32 in flight | ops/s (higher is better) | 230434.9 | 138759.4 | n/a | 1.661 | +66.1% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randread 4 KiB, 32 in flight | p99 µs (lower is better) | 242.1 | 440.3 | n/a | 0.550 | +45.0% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randread 4 KiB, 128 in flight | ops/s (higher is better) | 450853.2 | 206614.1 | n/a | 2.182 | +118.2% | n/a | n/a | Submitter vs fio io_uring |
| submitter | randread 4 KiB, 128 in flight | p99 µs (lower is better) | 464.7 | 897.0 | n/a | 0.518 | +48.2% | n/a | n/a | Submitter vs fio io_uring |

## Anomalies, errors and integrity failures

- None: every measurement completed and every integrity check passed.

## W1: lone writer, QD1 durable 4 KiB (`lone`)

Wall time: 28.0 s.

**Method.**

- One thread, one operation at a time; each operation is a 4 KiB write made durable before the next starts.
- store-io `append_durable` appends to a freshly provisioned append region; `write_durable` overwrites a 256 MiB page region at sequentially cycling 4 KiB offsets.
- raw: copy the payload into an aligned buffer (the copy store-io also makes), then pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync, at sequentially cycling offsets of a ready 256 MiB file.
- fsys: `JournalHandle::append` + `sync_through(lsn)` (default, window off, direct+window off) on a fresh journal file; `Handle::write_at` + `Handle::sync` on a ready 256 MiB file (fsys opens the file on every call; that is its API).
- Latency covers the write and the durability call only; payload generation happens before the timer starts.

Results:

| system | variant | durable ops/s | p50 µs | p90 µs | p99 µs | p99.9 µs | max µs | mean µs | flushes/op | write calls/op | other calls/op |
|---|---|---|---|---|---|---|---|---|---|---|---|
| store-io | AppendRegion::append_durable | 98 | 6369.0 | 13019.7 | 95926.3 | n/a | 179301.1 | 10165.6 | 1.00 | 0.00 | n/a |
| store-io | AppendRegion::append + sync_through (halves timed separately) | 140 | 6579.2 | 10880.1 | 20666.5 | n/a | 43056.3 | 7156.3 | 1.00 | 0.00 | n/a |
| store-io | PageRegion::write_durable | 246 | 2551.2 | 7510.9 | 24443.9 | n/a | 64672.2 | 4072.1 | 1.00 | 0.00 | n/a |
| raw | write + data flush | 167 | 3399.9 | 11595.5 | 29320.7 | n/a | 55035.7 | 5973.8 | n/a | 1.00 | n/a |
| fsys | journal, default (buffered, 500 µs group-commit window): append + sync_through | 139 | 6375.0 | 7608.4 | 23623.1 | n/a | 41504.1 | 7213.4 | n/a | 1.00 | n/a |
| fsys | journal, buffered, window off: append + sync_through | 159 | 5543.3 | 7087.7 | 22213.4 | n/a | 26747.7 | 6308.7 | n/a | 1.00 | n/a |
| fsys | journal, direct, window off: append + sync_through | 119 | 5437.6 | 5847.0 | 70495.9 | n/a | 271393.2 | 8408.1 | n/a | 0.00 | n/a |
| fsys | Handle::write_at + Handle::sync | 175 | 5188.7 | 5407.2 | 21050.7 | n/a | 28094.9 | 5725.4 | n/a | 1.00 | n/a |

Where the time goes (write call vs durability call, timed separately on the same thread):

| system | variant | write p50 µs | write p99 µs | write mean µs | durability p50 µs | durability p99 µs | durability mean µs |
|---|---|---|---|---|---|---|---|
| store-io | AppendRegion::append + sync_through (halves timed separately) | 260.3 | 6418.4 | 577.5 | 6069.9 | 15846.8 | 6630.9 |
| raw | write + data flush | 250.4 | 6264.7 | 595.6 | 3174.3 | 26869.7 | 5472.2 |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append_durable | passed: 414 of 414 records read back and compared byte for byte; all 414 checked for overlap and alignment | provisioned a 323 MiB append region in 0.15 s (2195 MB/s: fill, flush, verify) |
| store-io | AppendRegion::append + sync_through (halves timed separately) | passed: 459 of 459 records read back and compared byte for byte; all 459 checked for overlap and alignment | provisioned a 323 MiB append region in 0.38 s (894 MB/s: fill, flush, verify) |
| store-io | PageRegion::write_durable | passed: 783 of 783 written pages read back (latest write each) and compared byte for byte |  |
| raw | write + data flush | passed: 545 of 545 written slots read back unbuffered and compared byte for byte | file made ready (zero-filled unbuffered + flushed) in 0.08 s (3559 MB/s) |
| fsys | journal, default (buffered, 500 µs group-commit window): append + sync_through | passed: all 455 records replayed with JournalReader and compared byte for byte | journal backend KernelBuffered, direct active false |
| fsys | journal, buffered, window off: append + sync_through | passed: all 503 records replayed with JournalReader and compared byte for byte | journal backend KernelBuffered, direct active false |
| fsys | journal, direct, window off: append + sync_through | passed: all 413 records replayed with JournalReader and compared byte for byte | journal backend KernelDirect, direct active true |
| fsys | Handle::write_at + Handle::sync | passed: 581 of 581 written slots read back unbuffered and compared byte for byte |  |

## W2: concurrent durable writers, 4 KiB (`concurrent`)

Wall time: 99.3 s.

**Method.**

- Writer counts [1, 2, 4, 8, 16, 32, 64]; every thread loops a durable 4 KiB write at queue depth 1 for the whole point.
- store-io: all threads call `AppendRegion::append_durable` on one append region of one store (a fresh store per point). Flush sharing is read from `Store::domain_stats()` deltas over the window: `flushes` issued, barriers that `led` a flush, barriers that `joined` one.
- raw: each thread writes its own contiguous range of one ready 256 MiB file and makes it durable itself with pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync (no sharing).
- fsys: all threads call `append` + `sync_through` on one shared journal (fresh per point), window off and default (500 µs group-commit window).
- An operation counts when it started and finished inside the measured window; rate = counted operations / window.

Results:

| threads | system | variant | durable ops/s | p50 µs | p99 µs | p99.9 µs | flushes issued | barriers led | barriers joined | writes per flush |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | store-io | AppendRegion::append_durable (shared region) | 352 | 2434.4 | 11072.5 | 18466.1 | 1081 | 1081 | 0 | 1.00 |
| 1 | raw | own write + own data flush (per thread range) | 336 | 2487.9 | 11378.9 | 18608.1 | n/a | n/a | n/a | n/a |
| 1 | fsys | journal, buffered, window off: shared journal append + sync_through | 89 | 7788.2 | 45681.7 | n/a | n/a | n/a | n/a | n/a |
| 1 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 79 | 9434.7 | 44002.1 | n/a | n/a | n/a | n/a | n/a |
| 2 | store-io | AppendRegion::append_durable (shared region) | 261 | 5090.7 | 47541.3 | n/a | 784 | 784 | 1 | 1.00 |
| 2 | raw | own write + own data flush (per thread range) | 378 | 4840.3 | 20884.5 | 24452.6 | n/a | n/a | n/a | n/a |
| 2 | fsys | journal, buffered, window off: shared journal append + sync_through | 286 | 5883.2 | 22160.9 | n/a | n/a | n/a | n/a | n/a |
| 2 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 293 | 6276.8 | 21997.9 | n/a | n/a | n/a | n/a | n/a |
| 4 | store-io | AppendRegion::append_durable (shared region) | 811 | 4627.8 | 20072.1 | 20507.1 | 1255 | 1255 | 1243 | 1.99 |
| 4 | raw | own write + own data flush (per thread range) | 432 | 8572.6 | 24358.7 | 26246.0 | n/a | n/a | n/a | n/a |
| 4 | fsys | journal, buffered, window off: shared journal append + sync_through | 518 | 5415.1 | 20958.9 | 41647.0 | n/a | n/a | n/a | n/a |
| 4 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 660 | 5885.7 | 9690.9 | 21449.1 | n/a | n/a | n/a | n/a |
| 8 | store-io | AppendRegion::append_durable (shared region) | 1390 | 5005.8 | 13640.6 | 24985.4 | 1178 | 1178 | 2999 | 3.54 |
| 8 | raw | own write + own data flush (per thread range) | 164 | 45477.0 | 107918.5 | n/a | n/a | n/a | n/a | n/a |
| 8 | fsys | journal, buffered, window off: shared journal append + sync_through | 583 | 10654.7 | 135444.5 | 162647.8 | n/a | n/a | n/a | n/a |
| 8 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 1275 | 5985.4 | 11700.5 | 22062.1 | n/a | n/a | n/a | n/a |
| 16 | store-io | AppendRegion::append_durable (shared region) | 2414 | 6944.1 | 11359.0 | 23424.5 | 1175 | 1175 | 6084 | 6.16 |
| 16 | raw | own write + own data flush (per thread range) | 380 | 38988.9 | 69324.3 | 88437.4 | n/a | n/a | n/a | n/a |
| 16 | fsys | journal, buffered, window off: shared journal append + sync_through | 1101 | 12564.3 | 36269.2 | 116537.4 | n/a | n/a | n/a | n/a |
| 16 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 1282 | 11783.2 | 30532.8 | 184602.3 | n/a | n/a | n/a | n/a |
| 32 | store-io | AppendRegion::append_durable (shared region) | 4055 | 7586.3 | 20035.2 | 24551.1 | 1045 | 1045 | 11153 | 11.64 |
| 32 | raw | own write + own data flush (per thread range) | 451 | 55197.5 | 169481.5 | 301180.7 | n/a | n/a | n/a | n/a |
| 32 | fsys | journal, buffered, window off: shared journal append + sync_through | 2572 | 10957.7 | 47044.1 | 151711.4 | n/a | n/a | n/a | n/a |
| 32 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 2224 | 11930.9 | 43642.6 | 56070.7 | n/a | n/a | n/a | n/a |
| 64 | store-io | AppendRegion::append_durable (shared region) | 3325 | 17467.2 | 49855.1 | 65632.7 | 441 | 441 | 9599 | 22.62 |
| 64 | raw | own write + own data flush (per thread range) | 785 | 76550.2 | 143756.2 | 149315.4 | n/a | n/a | n/a | n/a |
| 64 | fsys | journal, buffered, window off: shared journal append + sync_through | 1412 | 17081.8 | 544480.6 | 544734.7 | n/a | n/a | n/a | n/a |
| 64 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 2086 | 12251.8 | 232361.3 | 397870.7 | n/a | n/a | n/a | n/a |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 1197 records read back and compared byte for byte; all 1197 checked for overlap and alignment | provisioned a 516 MiB append region in 0.33 s (1648 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1025 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 318 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.004 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 253 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.004 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 901 of 901 records read back and compared byte for byte; all 901 checked for overlap and alignment | provisioned a 516 MiB append region in 0.38 s (1416 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1026 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 992 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 960 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 2684 records read back and compared byte for byte; all 2684 checked for overlap and alignment | provisioned a 516 MiB append region in 0.30 s (1824 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1028 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 1718 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 2196 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 4597 records read back and compared byte for byte; all 4597 checked for overlap and alignment | provisioned a 516 MiB append region in 0.19 s (2805 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 573 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.016 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 1778 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.005 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 4368 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 8017 records read back and compared byte for byte; all 8017 checked for overlap and alignment | provisioned a 516 MiB append region in 0.29 s (1854 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1040 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.014 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 3478 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.005 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 4274 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.004 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 13501 records read back and compared byte for byte; all 13501 checked for overlap and alignment | provisioned a 516 MiB append region in 0.32 s (1688 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1056 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.014 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 8220 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.004 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 7627 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.005 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 11258 records read back and compared byte for byte; all 11258 checked for overlap and alignment | provisioned a 516 MiB append region in 0.26 s (2080 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1088 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.028 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 4891 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.024 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 7832 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.010 (straddling ops make this slightly above 1) |

**Findings.**

- store-io scaling: 352 durable ops/s at 1 writer, 3325 at 64 writers (22.62 writes per flush) [derived from the table].

## W3: caller batch (N records, one barrier) (`caller-batch`)

Wall time: 254.8 s.

**Method.**

- N in [1, 10, 100, 1000], record sizes [64, 512, 4096] bytes; one commit = N records made durable by one barrier; commits run back to back on one thread. Each point ends at the time box or after 3000 measured commits, whichever comes first.
- store-io (a) `N x append + sync_through`: each append is its own write, padded to a block (documented behaviour). (b) `AppendBatch`: records packed back to back into pooled buffers, `commit()` = one reservation, the fewest writes (largest pooled buffer 1 MiB), one barrier. A fresh store per (N, size).
- raw (a') N writes of one zero-padded block each, then one flush; (b') the N records packed into one aligned buffer padded to a block, one write, one flush. Primitive: pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync. A ready 1 GiB file, offsets cycling.
- fsys: `append` x N + `sync_through(last lsn)`, and `append_batch(&records)` + `sync_through`, on a fresh journal per point, window off.
- records/s = commits/s x N; payload MB/s counts caller bytes only (10^6 bytes); `write calls/commit` and `bytes written/commit` come from the OS counters and show the device writes each design issues per commit.

Results:

| N | record B | system | variant | commits/s | records/s | payload MB/s | commit p50 µs | commit p99 µs | write calls/commit | bytes written/commit | flushes/commit |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 148 | 148 | 0.01 | 2761.1 | 121821.2 | 0.00 | 0 | 1.00 |
| 1 | 64 | store-io | (b) AppendBatch: N x append + commit | 119 | 119 | 0.01 | 2853.1 | 118499.0 | 0.00 | 0 | 1.00 |
| 1 | 64 | raw | (a') N writes of one padded block + one flush | 146 | 146 | 0.01 | 2775.8 | 87138.9 | 1.00 | 4096 | n/a |
| 1 | 64 | raw | (b') packed: one write of N records + one flush | 156 | 156 | 0.01 | 2559.3 | 120615.8 | 1.00 | 4096 | n/a |
| 1 | 64 | fsys | append x N + sync_through (window off) | 70 | 70 | 0.00 | 5416.1 | 229646.6 | 1.00 | 76 | n/a |
| 1 | 64 | fsys | append_batch(N) + sync_through (window off) | 79 | 79 | 0.01 | 5277.8 | 147103.8 | 1.00 | 76 | n/a |
| 1 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 177 | 177 | 0.09 | 2485.1 | 90997.8 | 0.00 | 0 | 1.00 |
| 1 | 512 | store-io | (b) AppendBatch: N x append + commit | 159 | 159 | 0.08 | 2436.4 | 102326.4 | 0.00 | 0 | 1.00 |
| 1 | 512 | raw | (a') N writes of one padded block + one flush | 166 | 166 | 0.09 | 2442.3 | 86664.8 | 1.00 | 4096 | n/a |
| 1 | 512 | raw | (b') packed: one write of N records + one flush | 157 | 157 | 0.08 | 2439.4 | 124952.5 | 1.00 | 4096 | n/a |
| 1 | 512 | fsys | append x N + sync_through (window off) | 72 | 72 | 0.04 | 5393.0 | 187134.0 | 1.00 | 524 | n/a |
| 1 | 512 | fsys | append_batch(N) + sync_through (window off) | 57 | 57 | 0.03 | 5830.5 | 199117.7 | 1.00 | 524 | n/a |
| 1 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 174 | 174 | 0.71 | 4560.4 | 18749.3 | 0.00 | 0 | 1.00 |
| 1 | 4096 | store-io | (b) AppendBatch: N x append + commit | 388 | 388 | 1.59 | 2434.8 | 5537.5 | 0.00 | 0 | 1.00 |
| 1 | 4096 | raw | (a') N writes of one padded block + one flush | 386 | 386 | 1.58 | 2427.1 | 5349.3 | 1.00 | 4096 | n/a |
| 1 | 4096 | raw | (b') packed: one write of N records + one flush | 368 | 368 | 1.51 | 2563.1 | 4724.3 | 1.00 | 4096 | n/a |
| 1 | 4096 | fsys | append x N + sync_through (window off) | 178 | 178 | 0.73 | 5353.4 | 20777.5 | 1.00 | 4108 | n/a |
| 1 | 4096 | fsys | append_batch(N) + sync_through (window off) | 184 | 184 | 0.75 | 5143.1 | 20308.9 | 1.00 | 4108 | n/a |
| 10 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 262 | 2619 | 0.17 | 3594.1 | 6738.5 | 0.00 | 0 | 1.00 |
| 10 | 64 | store-io | (b) AppendBatch: N x append + commit | 393 | 3931 | 0.25 | 2380.1 | 6176.0 | 0.00 | 0 | 1.00 |
| 10 | 64 | raw | (a') N writes of one padded block + one flush | 273 | 2735 | 0.18 | 3491.5 | 6176.9 | 10.00 | 40960 | n/a |
| 10 | 64 | raw | (b') packed: one write of N records + one flush | 395 | 3954 | 0.25 | 2374.9 | 6380.6 | 1.00 | 4096 | n/a |
| 10 | 64 | fsys | append x N + sync_through (window off) | 179 | 1794 | 0.11 | 5283.5 | 21066.3 | 10.00 | 760 | n/a |
| 10 | 64 | fsys | append_batch(N) + sync_through (window off) | 107 | 1073 | 0.07 | 7669.3 | 33423.1 | 1.00 | 760 | n/a |
| 10 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 152 | 1519 | 0.78 | 5313.5 | 21019.5 | 0.00 | 0 | 1.00 |
| 10 | 512 | store-io | (b) AppendBatch: N x append + commit | 391 | 3914 | 2.00 | 2395.4 | 6180.0 | 0.00 | 0 | 1.00 |
| 10 | 512 | raw | (a') N writes of one padded block + one flush | 280 | 2800 | 1.43 | 3477.0 | 5417.4 | 10.00 | 40960 | n/a |
| 10 | 512 | raw | (b') packed: one write of N records + one flush | 375 | 3750 | 1.92 | 2491.4 | 6569.9 | 1.00 | 8192 | n/a |
| 10 | 512 | fsys | append x N + sync_through (window off) | 177 | 1775 | 0.91 | 5395.9 | 20974.1 | 10.00 | 5240 | n/a |
| 10 | 512 | fsys | append_batch(N) + sync_through (window off) | 177 | 1767 | 0.90 | 5321.3 | 20768.7 | 1.00 | 5240 | n/a |
| 10 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 251 | 2512 | 10.29 | 3763.2 | 7974.3 | 0.00 | 0 | 1.00 |
| 10 | 4096 | store-io | (b) AppendBatch: N x append + commit | 386 | 3861 | 15.81 | 2423.0 | 5687.5 | 0.00 | 71 | 1.00 |
| 10 | 4096 | raw | (a') N writes of one padded block + one flush | 277 | 2771 | 11.35 | 3504.6 | 5540.4 | 10.00 | 40960 | n/a |
| 10 | 4096 | raw | (b') packed: one write of N records + one flush | 404 | 4043 | 16.56 | 2405.1 | 4767.6 | 1.00 | 40960 | n/a |
| 10 | 4096 | fsys | append x N + sync_through (window off) | 186 | 1860 | 7.62 | 5106.9 | 9431.4 | 10.00 | 41080 | n/a |
| 10 | 4096 | fsys | append_batch(N) + sync_through (window off) | 123 | 1229 | 5.04 | 5444.9 | 81178.2 | 1.00 | 41080 | n/a |
| 100 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 19 | 1932 | 0.12 | 24172.3 | n/a | 0.00 | 0 | 1.00 |
| 100 | 64 | store-io | (b) AppendBatch: N x append + commit | 385 | 38534 | 2.47 | 2448.6 | 5028.3 | 0.00 | 0 | 1.00 |
| 100 | 64 | raw | (a') N writes of one padded block + one flush | 58 | 5823 | 0.37 | 16094.7 | 33265.6 | 100.00 | 409600 | n/a |
| 100 | 64 | raw | (b') packed: one write of N records + one flush | 340 | 34030 | 2.18 | 2447.4 | 18213.2 | 1.00 | 8192 | n/a |
| 100 | 64 | fsys | append x N + sync_through (window off) | 170 | 16954 | 1.09 | 5540.8 | 12640.0 | 100.00 | 7600 | n/a |
| 100 | 64 | fsys | append_batch(N) + sync_through (window off) | 171 | 17055 | 1.09 | 5544.3 | 14020.4 | 1.00 | 7600 | n/a |
| 100 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 48 | 4786 | 2.45 | 20083.2 | 33298.0 | 0.00 | 0 | 1.00 |
| 100 | 512 | store-io | (b) AppendBatch: N x append + commit | 335 | 33536 | 17.17 | 2628.1 | 10477.8 | 0.00 | 215 | 1.00 |
| 100 | 512 | raw | (a') N writes of one padded block + one flush | 60 | 6013 | 3.08 | 15174.5 | 36448.2 | 100.00 | 409600 | n/a |
| 100 | 512 | raw | (b') packed: one write of N records + one flush | 410 | 40985 | 20.98 | 2365.9 | 4315.5 | 1.00 | 53248 | n/a |
| 100 | 512 | fsys | append x N + sync_through (window off) | 158 | 15776 | 8.08 | 5699.7 | 15457.6 | 100.00 | 52400 | n/a |
| 100 | 512 | fsys | append_batch(N) + sync_through (window off) | 189 | 18878 | 9.67 | 5124.2 | 8744.5 | 1.00 | 52400 | n/a |
| 100 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 50 | 5042 | 20.65 | 16346.7 | 138408.1 | 0.00 | 0 | 1.00 |
| 100 | 4096 | store-io | (b) AppendBatch: N x append + commit | 141 | 14114 | 57.81 | 5709.4 | 19937.3 | 0.00 | 0 | 1.00 |
| 100 | 4096 | raw | (a') N writes of one padded block + one flush | 59 | 5873 | 24.06 | 15275.6 | 39470.7 | 100.00 | 409600 | n/a |
| 100 | 4096 | raw | (b') packed: one write of N records + one flush | 243 | 24283 | 99.46 | 2900.7 | 15695.7 | 1.00 | 409600 | n/a |
| 100 | 4096 | fsys | append x N + sync_through (window off) | 130 | 13030 | 53.37 | 6457.5 | 17858.3 | 100.00 | 410800 | n/a |
| 100 | 4096 | fsys | append_batch(N) + sync_through (window off) | 144 | 14404 | 59.00 | 5722.0 | 24321.5 | 1.00 | 410800 | n/a |
| 1000 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 7 | 7047 | 0.45 | 140391.2 | n/a | 0.00 | 0 | 1.00 |
| 1000 | 64 | store-io | (b) AppendBatch: N x append + commit | 380 | 380426 | 24.35 | 2368.8 | 5517.3 | 0.00 | 0 | 1.00 |
| 1000 | 64 | raw | (a') N writes of one padded block + one flush | 8 | 8002 | 0.51 | 122504.1 | n/a | 1000.00 | 4096000 | n/a |
| 1000 | 64 | raw | (b') packed: one write of N records + one flush | 395 | 395069 | 25.28 | 2446.7 | 4522.6 | 1.00 | 65536 | n/a |
| 1000 | 64 | fsys | append x N + sync_through (window off) | 164 | 164329 | 10.52 | 5780.1 | 9488.6 | 1000.00 | 76000 | n/a |
| 1000 | 64 | fsys | append_batch(N) + sync_through (window off) | 178 | 178347 | 11.41 | 5306.0 | 9289.6 | 1.00 | 76000 | n/a |
| 1000 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 7 | 6911 | 3.54 | 142870.2 | n/a | 0.00 | 0 | 1.00 |
| 1000 | 512 | store-io | (b) AppendBatch: N x append + commit | 359 | 358671 | 183.64 | 2702.9 | 6471.2 | 0.00 | 0 | 1.00 |
| 1000 | 512 | raw | (a') N writes of one padded block + one flush | 8 | 7994 | 4.09 | 124597.9 | n/a | 1000.00 | 4096000 | n/a |
| 1000 | 512 | raw | (b') packed: one write of N records + one flush | 318 | 318419 | 163.03 | 2633.2 | 6805.1 | 1.00 | 512000 | n/a |
| 1000 | 512 | fsys | append x N + sync_through (window off) | 125 | 125297 | 64.15 | 8225.9 | 24614.2 | 1000.00 | 524000 | n/a |
| 1000 | 512 | fsys | append_batch(N) + sync_through (window off) | 153 | 152786 | 78.23 | 5828.1 | 10230.5 | 1.00 | 524000 | n/a |
| 1000 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 7 | 7375 | 30.21 | 133701.2 | n/a | 0.00 | 0 | 1.00 |
| 1000 | 4096 | store-io | (b) AppendBatch: N x append + commit | 114 | 114210 | 467.80 | 6222.3 | 23811.8 | 0.00 | 0 | 1.00 |
| 1000 | 4096 | raw | (a') N writes of one padded block + one flush | 7 | 7472 | 30.60 | 127977.2 | n/a | 1000.00 | 4096000 | n/a |
| 1000 | 4096 | raw | (b') packed: one write of N records + one flush | 157 | 157287 | 644.25 | 4969.1 | 14776.1 | 1.00 | 4096000 | n/a |
| 1000 | 4096 | fsys | append x N + sync_through (window off) | 59 | 59403 | 243.32 | 16441.2 | 32379.6 | 1000.00 | 4108000 | n/a |
| 1000 | 4096 | fsys | append_batch(N) + sync_through (window off) | 66 | 66128 | 270.86 | 14342.5 | 29197.2 | 1.00 | 4108000 | n/a |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | (a) N x AppendRegion::append + sync_through | passed: 548 of 548 records read back and compared byte for byte; all 548 checked for overlap and alignment | provisioned a 65 MiB append region in 0.09 s (778 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 462 of 462 records read back and compared byte for byte; all 462 checked for overlap and alignment | provisioned a 65 MiB append region in 0.17 s (395 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 212 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 289 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 618 of 618 records read back and compared byte for byte; all 618 checked for overlap and alignment | provisioned a 65 MiB append region in 0.10 s (661 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 593 of 593 records read back and compared byte for byte; all 593 checked for overlap and alignment | provisioned a 65 MiB append region in 0.46 s (149 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 273 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 178 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 552 of 552 records read back and compared byte for byte; all 552 checked for overlap and alignment | provisioned a 65 MiB append region in 0.12 s (590 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 1275 records read back and compared byte for byte; all 1275 checked for overlap and alignment | provisioned a 65 MiB append region in 0.03 s (2139 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 585 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 606 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 8690 records read back and compared byte for byte; all 8690 checked for overlap and alignment | provisioned a 645 MiB append region in 0.28 s (2374 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 13000 records read back and compared byte for byte; all 13000 checked for overlap and alignment | provisioned a 65 MiB append region in 0.03 s (2364 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 5930 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 3790 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 5030 records read back and compared byte for byte; all 5030 checked for overlap and alignment | provisioned a 645 MiB append region in 0.31 s (2167 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 12870 records read back and compared byte for byte; all 12870 checked for overlap and alignment | provisioned a 129 MiB append region in 0.06 s (2445 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 5840 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 5800 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 8330 records read back and compared byte for byte; all 8330 checked for overlap and alignment | provisioned a 645 MiB append region in 0.30 s (2284 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 12700 records read back and compared byte for byte; all 12700 checked for overlap and alignment | provisioned a 645 MiB append region in 0.22 s (3113 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 6120 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 4260 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 6100 records read back and compared byte for byte; all 6100 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.63 s (3400 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 125400 records read back and compared byte for byte; all 125400 checked for overlap and alignment | provisioned a 129 MiB append region in 0.19 s (702 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 56100 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 55300 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 16300 records read back and compared byte for byte; all 16300 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.55 s (3937 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 109900 records read back and compared byte for byte; all 109900 checked for overlap and alignment | provisioned a 838 MiB append region in 0.24 s (3588 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 51800 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 62300 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 17200 records read back and compared byte for byte; all 17200 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.54 s (3956 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 49900 records read back and compared byte for byte; all 49900 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.59 s (3642 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 42600 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 46900 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 25000 records read back and compared byte for byte; all 25000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.57 s (3739 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 1206000 records read back and compared byte for byte; all 1206000 checked for overlap and alignment | provisioned a 1032 MiB append region in 0.28 s (3900 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 540000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 577000 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 24000 records read back and compared byte for byte; all 24000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.62 s (3471 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 1140000 records read back and compared byte for byte; all 1140000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.59 s (3658 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 413000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 494000 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 26000 records read back and compared byte for byte; all 26000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.58 s (3680 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 350000 records read back and compared byte for byte; all 350000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.62 s (3463 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 194000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 223000 records replayed with JournalReader and compared byte for byte |  |

## W4: page batch (N random 4 KiB pages, one barrier) (`page-batch`)

Wall time: 43.3 s.

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
| 1 | store-io | PageBatch: N x write + commit | 331 | 331 | 2386.2 | 6246.7 | n/a | 0.00 | 1.00 |
| 1 | raw | QD1: N writes one at a time + one flush | 328 | 328 | 2350.1 | 6744.2 | n/a | 1.00 | n/a |
| 1 | fsys | Handle::write_at x N + Handle::sync | 152 | 152 | 5442.7 | 20938.3 | n/a | 1.00 | n/a |
| 16 | store-io | PageBatch: N x write + commit | 182 | 2915 | 4296.4 | 12568.7 | n/a | 0.00 | 1.00 |
| 16 | raw | QD1: N writes one at a time + one flush | 162 | 2585 | 4274.6 | 26687.3 | n/a | 16.00 | n/a |
| 16 | fsys | Handle::write_at x N + Handle::sync | 177 | 2834 | 5281.8 | 11016.0 | n/a | 16.00 | n/a |
| 64 | store-io | PageBatch: N x write + commit | 303 | 19383 | 3213.8 | 5065.5 | n/a | 0.00 | 1.00 |
| 64 | raw | QD1: N writes one at a time + one flush | 96 | 6147 | 10109.8 | 17225.6 | n/a | 64.00 | n/a |
| 64 | fsys | Handle::write_at x N + Handle::sync | 168 | 10782 | 5827.0 | 9311.8 | n/a | 64.00 | n/a |
| 256 | store-io | PageBatch: N x write + commit | 192 | 49025 | 5127.5 | 6750.5 | n/a | 0.00 | 1.00 |
| 256 | raw | QD1: N writes one at a time + one flush | 28 | 7274 | 34456.5 | n/a | n/a | 256.00 | n/a |
| 256 | fsys | Handle::write_at x N + Handle::sync | 98 | 25155 | 8675.1 | 23754.8 | n/a | 256.00 | n/a |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | PageBatch: N x write + commit | passed: 1025 of 1045 written pages read back (latest write each) and compared byte for byte |  |
| raw | QD1: N writes one at a time + one flush | passed: 1025 of 1037 written slots read back unbuffered and compared byte for byte |  |
| fsys | Handle::write_at x N + Handle::sync | passed: 496 of 496 written slots read back unbuffered and compared byte for byte |  |
| store-io | PageBatch: N x write + commit | passed: 1025 of 8535 written pages read back (latest write each) and compared byte for byte |  |
| raw | QD1: N writes one at a time + one flush | passed: 1025 of 7467 written slots read back unbuffered and compared byte for byte |  |
| fsys | Handle::write_at x N + Handle::sync | passed: 1025 of 8341 written slots read back unbuffered and compared byte for byte |  |
| store-io | PageBatch: N x write + commit | passed: 1025 of 39223 written pages read back (latest write each) and compared byte for byte |  |
| raw | QD1: N writes one at a time + one flush | passed: 1025 of 16686 written slots read back unbuffered and compared byte for byte |  |
| fsys | Handle::write_at x N + Handle::sync | passed: 1025 of 26170 written slots read back unbuffered and compared byte for byte |  |
| store-io | PageBatch: N x write + commit | passed: 1025 of 58987 written pages read back (latest write each) and compared byte for byte |  |
| raw | QD1: N writes one at a time + one flush | passed: 1025 of 19244 written slots read back unbuffered and compared byte for byte |  |
| fsys | Handle::write_at x N + Handle::sync | passed: 1025 of 45893 written slots read back unbuffered and compared byte for byte |  |

## W5: sequential bandwidth (1, 2 and 8 MiB writes, 1/8 MiB reads) (`sequential`)

Wall time: 35.1 s.

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
| store-io | AppendRegion::append(1 MiB) x N + sync_through | 1048576 | 870.8 | 1073741824 | 1.233 | 12.87 | 928.0 | 4189.3 | 18390.5 | 0.00 |
| store-io | AppendRegion::read, 1 MiB requests | 1048576 | 996.2 | 2889875456 | 2.901 | n/a | 825.9 | 3173.8 | 9540.5 | 0.00 |
| store-io | AppendRegion::read, 8 MiB requests | 8388608 | 3188.3 | 9412018176 | 2.952 | n/a | 2603.1 | 3691.7 | 4636.3 | 0.00 |
| store-io | AppendRegion::append(2 MiB) x N + sync_through | 2097152 | 2296.8 | 1073741824 | 0.467 | 4.79 | 816.2 | 2389.9 | 6082.9 | 0.00 |
| store-io | AppendRegion::append(8 MiB) x N + sync_through | 8388608 | 2904.1 | 1073741824 | 0.370 | 5.58 | 2637.1 | 7176.6 | 7802.2 | 0.00 |
| raw | 1 MiB writes + one data flush | 1048576 | 2809.2 | 1073741824 | 0.382 | 4.74 | 337.6 | 501.3 | 6006.7 | 1.00 |
| raw | unbuffered read, 1 MiB requests | 1048576 | 1551.0 | 4320133120 | 2.785 | n/a | 655.7 | 852.0 | 3270.2 | 0.00 |
| raw | unbuffered read, 8 MiB requests | 8388608 | 4294.1 | 12490637312 | 2.909 | n/a | 1873.2 | 2952.5 | 4449.3 | 0.00 |
| raw | diagnostic: unbuffered read into a store-io-buf pool buffer, 1 MiB requests | 1048576 | 1451.0 | 4045406208 | 2.788 | n/a | 717.0 | 915.5 | 2691.0 | 0.00 |
| raw | diagnostic: unbuffered read into a pool buffer + copy into a caller Vec, 1 MiB requests | 1048576 | 1367.3 | 3932160000 | 2.876 | n/a | 753.2 | 1006.0 | 2599.4 | 0.00 |
| raw | diagnostic: unbuffered read at offsets + 48 KiB (store-io's data alignment), 1 MiB requests | 1048576 | 1440.3 | 4320133120 | 2.999 | n/a | 725.8 | 904.7 | 2733.1 | 0.00 |
| raw | 2 MiB writes + one data flush | 2097152 | 2331.9 | 1073741824 | 0.460 | 5.64 | 818.9 | 3834.6 | 8159.8 | 1.00 |
| raw | 8 MiB writes + one data flush | 8388608 | 2680.3 | 1073741824 | 0.401 | 5.80 | 2893.6 | 8083.1 | 8446.9 | 1.00 |
| fsys | journal, buffered, window off: append(1 MiB) x N + sync_through | 1048576 | 822.9 | 1073741824 | 1.305 | 562.66 | 808.5 | 1128.4 | 1486.1 | 1.00 |
| fsys | journal, direct, window off: append(1 MiB) x N + sync_through | 1048576 | 743.9 | 1073741824 | 1.443 | 12.57 | 1278.9 | 5332.0 | 12811.7 | 1.00 |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append(1 MiB) x N + sync_through | passed: all 1024 MiB read back through AppendRegion::read and compared byte for byte | provisioned a 1024 MiB append region in 0.62 s (1736 MB/s: fill, flush, verify) |
| store-io | AppendRegion::read, 1 MiB requests | passed: first 1 MiB of each of 2922 requests compared against what was written |  |
| store-io | AppendRegion::read, 8 MiB requests | passed: first 1 MiB of each of 1234 requests compared against what was written |  |
| store-io | AppendRegion::append(2 MiB) x N + sync_through | passed: all 1024 MiB read back through AppendRegion::read and compared byte for byte | provisioned a 1024 MiB append region in 0.49 s (2205 MB/s: fill, flush, verify) |
| store-io | AppendRegion::append(8 MiB) x N + sync_through | passed: all 1024 MiB read back through AppendRegion::read and compared byte for byte | provisioned a 1024 MiB append region in 0.42 s (2538 MB/s: fill, flush, verify) |
| raw | 1 MiB writes + one data flush | passed: all 1024 MiB read back unbuffered and compared byte for byte | file made ready (zero-filled unbuffered 1 MiB writes + flushed) in 0.19 s (5755 MB/s); compare store-io's provisioning note |
| raw | unbuffered read, 1 MiB requests | passed: first 1 MiB of each of 4541 requests compared against what was written |  |
| raw | unbuffered read, 8 MiB requests | passed: first 1 MiB of each of 1639 requests compared against what was written |  |
| raw | diagnostic: unbuffered read into a store-io-buf pool buffer, 1 MiB requests | passed: first 1 MiB of each of 4226 requests compared against what was written | not a comparison row: separates pool-memory cost from engine-path cost |
| raw | diagnostic: unbuffered read into a pool buffer + copy into a caller Vec, 1 MiB requests | passed: first 1 MiB of each of 4151 requests compared against what was written | not a comparison row: reproduces store-io's read path (device read into a pooled buffer, then a copy into the caller's buffer) |
| raw | diagnostic: unbuffered read at offsets + 48 KiB (store-io's data alignment), 1 MiB requests | not checked: shifted requests straddle written chunks; the unshifted rows check content | not a comparison row: tests whether store-io's region data offset (48 KiB past a 1 MiB boundary) costs bandwidth on this stack |
| raw | 2 MiB writes + one data flush | passed: all 1024 MiB read back unbuffered and compared byte for byte | file made ready (zero-filled unbuffered 1 MiB writes + flushed) in 0.19 s (5783 MB/s); compare store-io's provisioning note |
| raw | 8 MiB writes + one data flush | passed: all 1024 MiB read back unbuffered and compared byte for byte | file made ready (zero-filled unbuffered 1 MiB writes + flushed) in 0.19 s (5770 MB/s); compare store-io's provisioning note |
| fsys | journal, buffered, window off: append(1 MiB) x N + sync_through | passed: all 1024 records replayed with JournalReader and compared byte for byte |  |
| fsys | journal, direct, window off: append(1 MiB) x N + sync_through | passed: all 1024 records replayed with JournalReader and compared byte for byte |  |

## W6: N in flight from one thread, random 4 KiB (Submitter vs fio) (`submitter`)

Wall time: 58.7 s.

**Method.**

- Queue depth N in [1, 8, 32, 128]; uniformly random 4 KiB-aligned offsets over 256 MiB; one thread keeps N operations in flight, resubmitting as each completes; writes are plain (no per-write durability), reads read back.
- store-io: `Store::submitter(depth N)`, `write(region, pos, buf, Plain)` / `read_page`, `wait(1)`; buffers from the submitter's pool (`take_buf`).
- raw: fio --ioengine=io_uring --direct=1 --bs=4k --iodepth=N --numjobs=1 on a 256 MiB file laid out first; time-based, with the same warm-up as ramp time.
- Latency: per operation, submit to completion observed (store-io); fio's completion latency (clat).

Results:

| pattern | N | system | variant | ops/s | p50 µs | p99 µs | p99.9 µs |
|---|---|---|---|---|---|---|---|
| randwrite | 1 | store-io | Submitter randwrite QD1 | 9858 | 88.1 | 165.4 | 1937.4 |
| randwrite | 1 | raw | fio io_uring randwrite QD1 | 4314 | 125.4 | 228.4 | 2072.6 |
| randwrite | 8 | store-io | Submitter randwrite QD8 | 32257 | 106.4 | 254.9 | 2093.6 |
| randwrite | 8 | raw | fio io_uring randwrite QD8 | 67166 | 108.0 | 259.1 | 428.0 |
| randwrite | 32 | store-io | Submitter randwrite QD32 | 99491 | 314.5 | 516.6 | 2327.7 |
| randwrite | 32 | raw | fio io_uring randwrite QD32 | 109294 | 284.7 | 522.2 | 2179.1 |
| randwrite | 128 | store-io | Submitter randwrite QD128 | 188752 | 621.0 | 1401.6 | 4631.8 |
| randwrite | 128 | raw | fio io_uring randwrite QD128 | 159451 | 684.0 | 2146.3 | 9895.9 |
| randread | 1 | store-io | Submitter randread QD1 | 11604 | 81.8 | 160.8 | 257.6 |
| randread | 1 | raw | fio io_uring randread QD1 | 5284 | 173.1 | 250.9 | 1990.7 |
| randread | 8 | store-io | Submitter randread QD8 | 86564 | 75.0 | 134.0 | 212.2 |
| randread | 8 | raw | fio io_uring randread QD8 | 39439 | 193.5 | 297.0 | 2113.5 |
| randread | 32 | store-io | Submitter randread QD32 | 230435 | 123.5 | 242.1 | 499.0 |
| randread | 32 | raw | fio io_uring randread QD32 | 138759 | 216.1 | 440.3 | 2408.4 |
| randread | 128 | store-io | Submitter randread QD128 | 450853 | 251.7 | 464.7 | 688.4 |
| randread | 128 | raw | fio io_uring randread QD128 | 206614 | 602.1 | 897.0 | 2637.8 |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | Submitter randwrite QD1 | not checked: throughput point; correctness is the test suites' job | io: io_uring (chosen) |
| raw | fio io_uring randwrite QD1 | not checked: fio baseline |  |
| store-io | Submitter randwrite QD8 | not checked: throughput point; correctness is the test suites' job | io: io_uring (chosen) |
| raw | fio io_uring randwrite QD8 | not checked: fio baseline |  |
| store-io | Submitter randwrite QD32 | not checked: throughput point; correctness is the test suites' job | io: io_uring (chosen) |
| raw | fio io_uring randwrite QD32 | not checked: fio baseline |  |
| store-io | Submitter randwrite QD128 | not checked: throughput point; correctness is the test suites' job | io: io_uring (chosen) |
| raw | fio io_uring randwrite QD128 | not checked: fio baseline |  |
| store-io | Submitter randread QD1 | not checked: throughput point; correctness is the test suites' job | io: io_uring (chosen) |
| raw | fio io_uring randread QD1 | not checked: fio baseline |  |
| store-io | Submitter randread QD8 | not checked: throughput point; correctness is the test suites' job | io: io_uring (chosen) |
| raw | fio io_uring randread QD8 | not checked: fio baseline |  |
| store-io | Submitter randread QD32 | not checked: throughput point; correctness is the test suites' job | io: io_uring (chosen) |
| raw | fio io_uring randread QD32 | not checked: fio baseline |  |
| store-io | Submitter randread QD128 | not checked: throughput point; correctness is the test suites' job | io: io_uring (chosen) |
| raw | fio io_uring randread QD128 | not checked: fio baseline |  |

