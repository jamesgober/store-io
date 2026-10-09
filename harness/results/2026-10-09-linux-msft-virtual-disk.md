# store-io performance harness: linux / msft-virtual-disk (2026-10-09)

> **OVERRIDE IN EFFECT.** `Store::create` with default trust was **refused** on this device:
> `device durability is unverified: running under a hypervisor without a certificate for this volume type; missing: device identify data (needs a readable device node); SMART / health log (needs privilege on Linux); volatile write cache presence; volatile write cache enabled state; device atomic-write fields; [Unverified { missing: {Identify, Smart, CachePresence, CacheEnabled, AtomicFields}, reasons: {Hypervisor} }]`
>
> Every store in this run was created with the documented labelled override (`Trust { override_refusal: true, .. }`): the class stays as decided, every receipt is labelled `overridden`, and the most conservative primitive (write + device flush) is used. Nothing else was weakened. The numbers below are therefore **not** gate numbers for a certified durability class; they measure software cost on this stack.

## Run

| item | value |
|---|---|
| date (UTC) | 2026-10-09T08:24:28Z |
| command | all |
| store-io commit | 6551652186be080c31a2e3ec97d6510d5f0525d9 |
| OS | Ubuntu 24.04.4 LTS |
| kernel / build | Linux version 6.6.87.2-microsoft-standard-WSL2 (root@439a258ad544) (gcc (GCC) 11.2.0, GNU ld (GNU Binutils) 2.37) #1 SMP PREEMPT_DYNAMIC Thu Jun  5 18:30:46 UTC 2025 |
| CPU | AMD Ryzen 9 9950X3D 16-Core Processor |
| logical CPUs | 32 |
| filesystem | ext4 on / (device /dev/sdd 8:48, mount options rw,relatime, super options rw,discard,errors=remount-ro,data=ordered); store-io classified it Ext4 |
| directory under test | /home/james/store-io-harness-582 |
| backend | store-io-posix (PosixPlatform: synchronous O_DIRECT, tier T3) |
| durability class | unverified (receipt label overridden(running under a hypervisor without a certificate for this volume type), durable open Overridden (Trust::override_refusal), block size 4096 B) |
| time per point | 5.0 s measured after 0.5 s warm-up (time-boxed; a point may end early at region capacity, noted where it does) |
| fsys | fsys 1.1.3 (crates.io), Handle method Sync, durability primitive fsync |
| raw primitives | pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync; preadv2 on an O_DIRECT descriptor (one call per request) |
| harness build | release, lto=fat, codegen-units=1 (same as store-io's release profile) |
| total wall time | 657.4 s |

## Device report (`Store::report()`)

```text
volume           d9608732-df90-f3a7-c219-95f85a9b0f45
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
    queues: 0,
    queue_depth: 64,
    append_ring: 4096,
    keyed_fill: false,
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
| lone | QD1 durable 4 KiB append | durable ops/s (higher is better) | 1300.5 | 1225.5 | 426.3 | 1.061 | +6.1% | 3.050 | +205.0% | append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config) |
| lone | QD1 durable 4 KiB append | p50 µs (lower is better) | 720.4 | 747.2 | 2121.3 | 0.964 | +3.6% | 0.340 | +66.0% | append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config) |
| lone | QD1 durable 4 KiB append | p99 µs (lower is better) | 1305.0 | 2036.6 | 7919.2 | 0.641 | +35.9% | 0.165 | +83.5% | append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config) |
| lone | QD1 durable 4 KiB append, fsys default config | durable ops/s (higher is better) | 1300.5 | 1225.5 | 258.3 | 1.061 | +6.1% | 5.035 | +403.5% | fsys JournalOptions::new() (500 µs group-commit window) |
| lone | QD1 durable 4 KiB page overwrite | durable ops/s (higher is better) | 1097.4 | 1225.5 | 455.7 | 0.895 | -10.5% | 2.408 | +140.8% | write_durable vs raw write+flush vs fsys write_at+sync |
| lone | QD1 durable 4 KiB page overwrite | p50 µs (lower is better) | 789.8 | 747.2 | 1299.3 | 1.057 | -5.7% | 0.608 | +39.2% | write_durable vs raw write+flush vs fsys write_at+sync |
| lone | QD1 durable 4 KiB page overwrite | p99 µs (lower is better) | 3254.0 | 2036.6 | 15955.5 | 1.598 | -59.8% | 0.204 | +79.6% | write_durable vs raw write+flush vs fsys write_at+sync |
| concurrent | 1 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 460.3 | 907.3 | 354.2 | 0.507 | -49.3% | 1.300 | +30.0% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 1 concurrent durable 4 KiB writers | p99 µs (lower is better) | 23519.5 | 5785.0 | 7527.5 | 4.066 | -306.6% | 3.124 | -212.4% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 2 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 1602.8 | 1912.9 | 805.2 | 0.838 | -16.2% | 1.991 | +99.1% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 2 concurrent durable 4 KiB writers | p99 µs (lower is better) | 6100.9 | 5876.4 | 4123.2 | 1.038 | -3.8% | 1.480 | -48.0% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 4 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 2912.7 | 2303.9 | 1449.4 | 1.264 | +26.4% | 2.010 | +101.0% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 4 concurrent durable 4 KiB writers | p99 µs (lower is better) | 5952.5 | 6751.1 | 4138.3 | 0.882 | +11.8% | 1.438 | -43.8% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 8 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 5220.8 | 1988.0 | 1677.6 | 2.626 | +162.6% | 3.112 | +211.2% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 8 concurrent durable 4 KiB writers | p99 µs (lower is better) | 2750.5 | 9571.9 | 37725.2 | 0.287 | +71.3% | 0.073 | +92.7% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 16 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 8616.0 | 2226.3 | 4121.9 | 3.870 | +287.0% | 2.090 | +109.0% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 16 concurrent durable 4 KiB writers | p99 µs (lower is better) | 3163.6 | 16307.3 | 9002.5 | 0.194 | +80.6% | 0.351 | +64.9% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 32 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 12599.9 | 2232.1 | 5581.4 | 5.645 | +464.5% | 2.258 | +125.8% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 32 concurrent durable 4 KiB writers | p99 µs (lower is better) | 6501.9 | 32867.9 | 52989.5 | 0.198 | +80.2% | 0.123 | +87.7% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 64 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 8148.8 | 3296.5 | 8590.1 | 2.472 | +147.2% | 0.949 | -5.1% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 64 concurrent durable 4 KiB writers | p99 µs (lower is better) | 52466.1 | 111869.9 | 38674.9 | 0.469 | +53.1% | 1.357 | -35.7% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| caller-batch | batch of 1 x 64 B, one barrier | records/s (higher is better) | 1291.3 | 1301.2 | 587.4 | 0.992 | -0.8% | 2.198 | +119.8% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(64 B) + one sync_through | records/s (higher is better) | 1483.0 | 1540.9 | 553.5 | 0.962 | -3.8% | 2.679 | +167.9% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1 x 512 B, one barrier | records/s (higher is better) | 1499.4 | 1413.8 | 632.0 | 1.061 | +6.1% | 2.373 | +137.3% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(512 B) + one sync_through | records/s (higher is better) | 1339.1 | 1290.9 | 626.3 | 1.037 | +3.7% | 2.138 | +113.8% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1 x 4096 B, one barrier | records/s (higher is better) | 1179.3 | 1357.3 | 584.2 | 0.869 | -13.1% | 2.019 | +101.9% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(4096 B) + one sync_through | records/s (higher is better) | 1362.6 | 1319.9 | 553.2 | 1.032 | +3.2% | 2.463 | +146.3% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 64 B, one barrier | records/s (higher is better) | 14727.5 | 14596.6 | 4281.2 | 1.009 | +0.9% | 3.440 | +244.0% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(64 B) + one sync_through | records/s (higher is better) | 5078.4 | 3567.4 | 5793.0 | 1.424 | +42.4% | 0.877 | -12.3% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 512 B, one barrier | records/s (higher is better) | 10383.9 | 13395.1 | 4048.1 | 0.775 | -22.5% | 2.565 | +156.5% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(512 B) + one sync_through | records/s (higher is better) | 4375.7 | 4505.7 | 4349.5 | 0.971 | -2.9% | 1.006 | +0.6% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 4096 B, one barrier | records/s (higher is better) | 13237.6 | 11538.0 | 5192.9 | 1.147 | +14.7% | 2.549 | +154.9% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(4096 B) + one sync_through | records/s (higher is better) | 4499.4 | 2913.0 | 3374.8 | 1.545 | +54.5% | 1.333 | +33.3% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 64 B, one barrier | records/s (higher is better) | 87756.3 | 139526.8 | 48255.9 | 0.629 | -37.1% | 1.819 | +81.9% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(64 B) + one sync_through | records/s (higher is better) | 6245.7 | 6512.7 | 48191.8 | 0.959 | -4.1% | 0.130 | -87.0% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 512 B, one barrier | records/s (higher is better) | 143132.5 | 150756.2 | 51045.8 | 0.949 | -5.1% | 2.804 | +180.4% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(512 B) + one sync_through | records/s (higher is better) | 5843.0 | 6686.7 | 46813.0 | 0.874 | -12.6% | 0.125 | -87.5% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 4096 B, one barrier | records/s (higher is better) | 107836.8 | 95949.2 | 26663.0 | 1.124 | +12.4% | 4.044 | +304.4% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(4096 B) + one sync_through | records/s (higher is better) | 6801.2 | 6860.9 | 31187.3 | 0.991 | -0.9% | 0.218 | -78.2% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 64 B, one barrier | records/s (higher is better) | 1165174.1 | 744967.8 | 445748.7 | 1.564 | +56.4% | 2.614 | +161.4% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(64 B) + one sync_through | records/s (higher is better) | 6254.6 | 6757.8 | 286583.6 | 0.926 | -7.4% | 0.022 | -97.8% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 512 B, one barrier | records/s (higher is better) | 969082.2 | 1108633.1 | 294375.8 | 0.874 | -12.6% | 3.292 | +229.2% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(512 B) + one sync_through | records/s (higher is better) | 6357.5 | 6357.5 | 236174.5 | 1.000 | -0.0% | 0.027 | -97.3% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 4096 B, one barrier | records/s (higher is better) | 274446.4 | 397929.0 | 126112.9 | 0.690 | -31.0% | 2.176 | +117.6% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(4096 B) + one sync_through | records/s (higher is better) | 5665.6 | 5522.5 | 96795.6 | 1.026 | +2.6% | 0.059 | -94.1% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| page-batch | 1 random 4 KiB pages + one barrier | pages/s (higher is better) | 704.6 | 1025.0 | 472.8 | 0.687 | -31.3% | 1.490 | +49.0% | PageBatch::commit vs raw QD1 writes + flush vs fsys write_at x N + sync |
| page-batch | 1 random 4 KiB pages + one barrier | commit p99 µs (lower is better) | 7708.9 | 4360.5 | 9038.1 | 1.768 | -76.8% | 0.853 | +14.7% | PageBatch::commit vs raw QD1 writes + flush vs fsys write_at x N + sync |
| page-batch | 16 random 4 KiB pages + one barrier | pages/s (higher is better) | 2190.5 | 3481.8 | 2226.1 | 0.629 | -37.1% | 0.984 | -1.6% | PageBatch::commit vs raw QD1 writes + flush vs fsys write_at x N + sync |
| page-batch | 16 random 4 KiB pages + one barrier | commit p99 µs (lower is better) | 111929.8 | 20911.5 | 45499.0 | 5.353 | -435.3% | 2.460 | -146.0% | PageBatch::commit vs raw QD1 writes + flush vs fsys write_at x N + sync |
| page-batch | 64 random 4 KiB pages + one barrier | pages/s (higher is better) | 5494.0 | 3025.6 | 14259.3 | 1.816 | +81.6% | 0.385 | -61.5% | PageBatch::commit vs raw QD1 writes + flush vs fsys write_at x N + sync |
| page-batch | 64 random 4 KiB pages + one barrier | commit p99 µs (lower is better) | 16310.2 | 140218.7 | 27770.7 | 0.116 | +88.4% | 0.587 | +41.3% | PageBatch::commit vs raw QD1 writes + flush vs fsys write_at x N + sync |
| page-batch | 256 random 4 KiB pages + one barrier | pages/s (higher is better) | 4269.9 | 4624.0 | 34099.0 | 0.923 | -7.7% | 0.125 | -87.5% | PageBatch::commit vs raw QD1 writes + flush vs fsys write_at x N + sync |
| page-batch | 256 random 4 KiB pages + one barrier | commit p99 µs (lower is better) | n/a | n/a | 18510.3 | n/a | n/a | n/a | n/a | PageBatch::commit vs raw QD1 writes + flush vs fsys write_at x N + sync |
| sequential | sequential 1 MiB writes, one barrier at the end | MB/s (higher is better) | 1230.5 | 2114.8 | 1291.5 | 0.582 | -41.8% | 0.953 | -4.7% | fsys: the better of its buffered and direct journals |
| sequential | sequential read, 1 MiB requests | MB/s (higher is better) | 1227.9 | 1940.3 | n/a | 0.633 | -36.7% | n/a | n/a | fsys: no comparable unbuffered read (see method) |
| sequential | sequential read, 8 MiB requests | MB/s (higher is better) | 1233.1 | 4726.3 | n/a | 0.261 | -73.9% | n/a | n/a | fsys: no comparable unbuffered read (see method) |

## Anomalies, errors and integrity failures

- None: every measurement completed and every integrity check passed.

## W1: lone writer, QD1 durable 4 KiB (`lone`)

Wall time: 47.1 s.

**Method.**

- One thread, one operation at a time; each operation is a 4 KiB write made durable before the next starts.
- store-io `append_durable` appends to a freshly provisioned append region; `write_durable` overwrites a 256 MiB page region at sequentially cycling 4 KiB offsets.
- raw: copy the payload into an aligned buffer (the copy store-io also makes), then pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync, at sequentially cycling offsets of a ready 256 MiB file.
- fsys: `JournalHandle::append` + `sync_through(lsn)` (default, window off, direct+window off) on a fresh journal file; `Handle::write_at` + `Handle::sync` on a ready 256 MiB file (fsys opens the file on every call; that is its API).
- Latency covers the write and the durability call only; payload generation happens before the timer starts.

Results:

| system | variant | durable ops/s | p50 µs | p90 µs | p99 µs | p99.9 µs | max µs | mean µs | flushes/op | write calls/op | other calls/op |
|---|---|---|---|---|---|---|---|---|---|---|---|
| store-io | AppendRegion::append_durable | 1301 | 720.4 | 839.8 | 1305.0 | 5940.6 | 35374.3 | 768.9 | 1.00 | 1.00 | n/a |
| store-io | AppendRegion::append + sync_through (halves timed separately) | 1270 | 709.6 | 834.2 | 1963.0 | 12607.9 | 28559.5 | 787.4 | 1.00 | 1.00 | n/a |
| store-io | PageRegion::write_durable | 1097 | 789.8 | 993.0 | 3254.0 | 19575.4 | 48135.2 | 911.3 | 1.00 | 1.00 | n/a |
| raw | write + data flush | 1226 | 747.2 | 940.1 | 2036.6 | 5923.7 | 15395.5 | 816.0 | n/a | 1.00 | n/a |
| fsys | journal, default (buffered, 500 µs group-commit window): append + sync_through | 258 | 3194.3 | 4575.9 | 22365.4 | 37514.4 | 45493.3 | 3871.7 | n/a | 1.00 | n/a |
| fsys | journal, buffered, window off: append + sync_through | 426 | 2121.3 | 2826.7 | 7919.2 | 12410.1 | 24739.3 | 2345.6 | n/a | 1.00 | n/a |
| fsys | journal, direct, window off: append + sync_through | 209 | 2390.3 | 4686.7 | 125400.9 | 151560.9 | 152029.4 | 4779.0 | n/a | 0.00 | n/a |
| fsys | Handle::write_at + Handle::sync | 456 | 1299.3 | 3554.0 | 15955.5 | 29029.3 | 80377.9 | 2194.6 | n/a | 1.00 | n/a |

Where the time goes (write call vs durability call, timed separately on the same thread):

| system | variant | write p50 µs | write p99 µs | write mean µs | durability p50 µs | durability p99 µs | durability mean µs |
|---|---|---|---|---|---|---|---|
| store-io | AppendRegion::append + sync_through (halves timed separately) | 176.6 | 386.2 | 186.8 | 529.6 | 1706.8 | 599.5 |
| raw | write + data flush | 192.2 | 404.8 | 202.1 | 556.2 | 1695.4 | 618.4 |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append_durable | passed: 2049 of 7124 records read back and compared byte for byte; all 7124 checked for overlap and alignment | provisioned a 538 MiB append region in 0.31 s (1832 MB/s: fill, flush, verify) |
| store-io | AppendRegion::append + sync_through (halves timed separately) | passed: 2049 of 6974 records read back and compared byte for byte; all 6974 checked for overlap and alignment | provisioned a 538 MiB append region in 0.26 s (2205 MB/s: fill, flush, verify) |
| store-io | PageRegion::write_durable | passed: 2049 of 6034 written pages read back (latest write each) and compared byte for byte |  |
| raw | write + data flush | passed: 2049 of 6684 written slots read back unbuffered and compared byte for byte | file made ready (zero-filled unbuffered + flushed) in 0.06 s (4278 MB/s) |
| fsys | journal, default (buffered, 500 µs group-commit window): append + sync_through | passed: all 1472 records replayed with JournalReader and compared byte for byte | journal backend KernelBuffered, direct active false |
| fsys | journal, buffered, window off: append + sync_through | passed: all 2320 records replayed with JournalReader and compared byte for byte | journal backend KernelBuffered, direct active false |
| fsys | journal, direct, window off: append + sync_through | passed: all 1161 records replayed with JournalReader and compared byte for byte | journal backend KernelDirect, direct active true |
| fsys | Handle::write_at + Handle::sync | passed: 2049 of 2366 written slots read back unbuffered and compared byte for byte |  |

## W2: concurrent durable writers, 4 KiB (`concurrent`)

Wall time: 161.0 s.

**Method.**

- Writer counts [1, 2, 4, 8, 16, 32, 64]; every thread loops a durable 4 KiB write at queue depth 1 for the whole point.
- store-io: all threads call `AppendRegion::append_durable` on one append region of one store (a fresh store per point). Flush sharing is read from `Store::domain_stats()` deltas over the window: `flushes` issued, barriers that `led` a flush, barriers that `joined` one.
- raw: each thread writes its own contiguous range of one ready 256 MiB file and makes it durable itself with pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync (no sharing).
- fsys: all threads call `append` + `sync_through` on one shared journal (fresh per point), window off and default (500 µs group-commit window).
- An operation counts when it started and finished inside the measured window; rate = counted operations / window.

Results:

| threads | system | variant | durable ops/s | p50 µs | p99 µs | p99.9 µs | flushes issued | barriers led | barriers joined | writes per flush |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | store-io | AppendRegion::append_durable (shared region) | 460 | 1084.4 | 23519.5 | 54333.4 | 2304 | 2304 | 0 | 1.00 |
| 1 | raw | own write + own data flush (per thread range) | 907 | 902.4 | 5785.0 | 8570.2 | n/a | n/a | n/a | n/a |
| 1 | fsys | journal, buffered, window off: shared journal append + sync_through | 354 | 2415.8 | 13229.6 | 25077.5 | n/a | n/a | n/a | n/a |
| 1 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 312 | 3027.7 | 7527.5 | 19812.4 | n/a | n/a | n/a | n/a |
| 2 | store-io | AppendRegion::append_durable (shared region) | 1603 | 1137.3 | 6100.9 | 6572.4 | 8010 | 8010 | 7 | 1.00 |
| 2 | raw | own write + own data flush (per thread range) | 1913 | 948.1 | 5876.4 | 6451.2 | n/a | n/a | n/a | n/a |
| 2 | fsys | journal, buffered, window off: shared journal append + sync_through | 805 | 2141.2 | 5339.2 | 9648.0 | n/a | n/a | n/a | n/a |
| 2 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 706 | 2765.7 | 4123.2 | 7899.9 | n/a | n/a | n/a | n/a |
| 4 | store-io | AppendRegion::append_durable (shared region) | 2913 | 1238.9 | 5952.5 | 7168.2 | 7461 | 7461 | 7107 | 1.95 |
| 4 | raw | own write + own data flush (per thread range) | 2304 | 1583.8 | 6751.1 | 7505.3 | n/a | n/a | n/a | n/a |
| 4 | fsys | journal, buffered, window off: shared journal append + sync_through | 1449 | 2265.0 | 5552.2 | 11786.5 | n/a | n/a | n/a | n/a |
| 4 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 1441 | 2687.7 | 4138.3 | 11921.5 | n/a | n/a | n/a | n/a |
| 8 | store-io | AppendRegion::append_durable (shared region) | 5221 | 1477.5 | 2750.5 | 7239.5 | 7922 | 7922 | 18194 | 3.30 |
| 8 | raw | own write + own data flush (per thread range) | 1988 | 3375.0 | 9571.9 | 96655.2 | n/a | n/a | n/a | n/a |
| 8 | fsys | journal, buffered, window off: shared journal append + sync_through | 1283 | 4861.4 | 39160.1 | 92695.2 | n/a | n/a | n/a | n/a |
| 8 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 1678 | 3142.2 | 37725.2 | 70088.5 | n/a | n/a | n/a | n/a |
| 16 | store-io | AppendRegion::append_durable (shared region) | 8616 | 1819.2 | 3163.6 | 8099.9 | 7583 | 7583 | 35514 | 5.68 |
| 16 | raw | own write + own data flush (per thread range) | 2226 | 6475.6 | 16307.3 | 127403.1 | n/a | n/a | n/a | n/a |
| 16 | fsys | journal, buffered, window off: shared journal append + sync_through | 4122 | 3896.9 | 9002.5 | 14090.1 | n/a | n/a | n/a | n/a |
| 16 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 3680 | 4264.7 | 11476.8 | 40827.7 | n/a | n/a | n/a | n/a |
| 32 | store-io | AppendRegion::append_durable (shared region) | 12600 | 2223.1 | 6501.9 | 29715.2 | 6300 | 6300 | 56730 | 10.00 |
| 32 | raw | own write + own data flush (per thread range) | 2232 | 11139.3 | 32867.9 | 48123.1 | n/a | n/a | n/a | n/a |
| 32 | fsys | journal, buffered, window off: shared journal append + sync_through | 5581 | 4092.6 | 52989.5 | 116935.4 | n/a | n/a | n/a | n/a |
| 32 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 2379 | 6912.1 | 107143.1 | 198146.9 | n/a | n/a | n/a | n/a |
| 64 | store-io | AppendRegion::append_durable (shared region) | 8149 | 4555.6 | 52466.1 | 137858.9 | 2490 | 2490 | 38113 | 16.36 |
| 64 | raw | own write + own data flush (per thread range) | 3297 | 17395.2 | 111869.9 | 159388.9 | n/a | n/a | n/a | n/a |
| 64 | fsys | journal, buffered, window off: shared journal append + sync_through | 1906 | 17076.1 | 171583.6 | 214962.5 | n/a | n/a | n/a | n/a |
| 64 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 8590 | 5186.3 | 38674.9 | 54832.0 | n/a | n/a | n/a | n/a |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 2708 records read back and compared byte for byte; all 2708 checked for overlap and alignment | provisioned a 860 MiB append region in 0.48 s (1877 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1025 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 1941 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 1690 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 8834 records read back and compared byte for byte; all 8834 checked for overlap and alignment | provisioned a 860 MiB append region in 0.38 s (2350 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1026 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 4393 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 3894 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 16042 records read back and compared byte for byte; all 16042 checked for overlap and alignment | provisioned a 860 MiB append region in 0.46 s (1953 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1028 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 7963 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 7972 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 28969 records read back and compared byte for byte; all 28969 checked for overlap and alignment | provisioned a 860 MiB append region in 0.36 s (2508 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1032 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 7279 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 8667 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 46062 records read back and compared byte for byte; all 46062 checked for overlap and alignment | provisioned a 860 MiB append region in 0.35 s (2556 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1040 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 22630 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 20514 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 69405 records read back and compared byte for byte; all 69405 checked for overlap and alignment | provisioned a 860 MiB append region in 0.42 s (2154 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1); domain counters: 69400 barriers for 69405 durable appends |
| raw | own write + own data flush (per thread range) | passed: 1056 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 31443 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 12244 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 48498 records read back and compared byte for byte; all 48498 checked for overlap and alignment | provisioned a 860 MiB append region in 0.51 s (1758 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1); domain counters: 48277 barriers for 48498 durable appends |
| raw | own write + own data flush (per thread range) | passed: 1088 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.004 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 9921 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.004 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 47028 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |

**Findings.**

- store-io scaling: 460 durable ops/s at 1 writer, 8149 at 64 writers (16.36 writes per flush) [derived from the table].

## W3: caller batch (N records, one barrier) (`caller-batch`)

Wall time: 348.5 s.

**Method.**

- N in [1, 10, 100, 1000], record sizes [64, 512, 4096] bytes; one commit = N records made durable by one barrier; commits run back to back on one thread. Each point ends at the time box or after 3000 measured commits, whichever comes first.
- store-io (a) `N x append + sync_through`: each append is its own write, padded to a block (documented behaviour). (b) `AppendBatch`: records packed back to back into pooled buffers, `commit()` = one reservation, the fewest writes (largest pooled buffer 1 MiB), one barrier. A fresh store per (N, size).
- raw (a') N writes of one zero-padded block each, then one flush; (b') the N records packed into one aligned buffer padded to a block, one write, one flush. Primitive: pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync. A ready 1 GiB file, offsets cycling.
- fsys: `append` x N + `sync_through(last lsn)`, and `append_batch(&records)` + `sync_through`, on a fresh journal per point, window off.
- records/s = commits/s x N; payload MB/s counts caller bytes only (10^6 bytes); `write calls/commit` and `bytes written/commit` come from the OS counters and show the device writes each design issues per commit.

Results:

| N | record B | system | variant | commits/s | records/s | payload MB/s | commit p50 µs | commit p99 µs | write calls/commit | bytes written/commit | flushes/commit |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 1483 | 1483 | 0.09 | 626.6 | 1242.3 | 1.00 | 4096 | 1.00 |
| 1 | 64 | store-io | (b) AppendBatch: N x append + commit | 1291 | 1291 | 0.08 | 626.4 | 5554.7 | 1.00 | 4096 | 1.00 |
| 1 | 64 | raw | (a') N writes of one padded block + one flush | 1541 | 1541 | 0.10 | 610.2 | 1061.5 | 1.00 | 4096 | n/a |
| 1 | 64 | raw | (b') packed: one write of N records + one flush | 1301 | 1301 | 0.08 | 637.9 | 5438.0 | 1.00 | 4096 | n/a |
| 1 | 64 | fsys | append x N + sync_through (window off) | 554 | 554 | 0.04 | 1673.3 | 3768.2 | 1.00 | 76 | n/a |
| 1 | 64 | fsys | append_batch(N) + sync_through (window off) | 587 | 587 | 0.04 | 1533.7 | 6499.0 | 1.00 | 76 | n/a |
| 1 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 1339 | 1339 | 0.69 | 634.9 | 1615.0 | 1.00 | 4096 | 1.00 |
| 1 | 512 | store-io | (b) AppendBatch: N x append + commit | 1499 | 1499 | 0.77 | 625.4 | 1252.5 | 1.00 | 4096 | 1.00 |
| 1 | 512 | raw | (a') N writes of one padded block + one flush | 1291 | 1291 | 0.66 | 643.4 | 5519.9 | 1.00 | 4096 | n/a |
| 1 | 512 | raw | (b') packed: one write of N records + one flush | 1414 | 1414 | 0.72 | 632.5 | 1494.2 | 1.00 | 4096 | n/a |
| 1 | 512 | fsys | append x N + sync_through (window off) | 626 | 626 | 0.32 | 1519.3 | 6286.9 | 1.00 | 524 | n/a |
| 1 | 512 | fsys | append_batch(N) + sync_through (window off) | 632 | 632 | 0.32 | 1512.1 | 3986.6 | 1.00 | 524 | n/a |
| 1 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 1363 | 1363 | 5.58 | 629.2 | 2887.7 | 1.00 | 4096 | 1.00 |
| 1 | 4096 | store-io | (b) AppendBatch: N x append + commit | 1179 | 1179 | 4.83 | 724.1 | 4376.0 | 1.00 | 4096 | 1.00 |
| 1 | 4096 | raw | (a') N writes of one padded block + one flush | 1320 | 1320 | 5.41 | 691.6 | 2083.6 | 1.00 | 4096 | n/a |
| 1 | 4096 | raw | (b') packed: one write of N records + one flush | 1357 | 1357 | 5.56 | 673.9 | 1607.5 | 1.00 | 4096 | n/a |
| 1 | 4096 | fsys | append x N + sync_through (window off) | 553 | 553 | 2.27 | 1643.2 | 5312.7 | 1.00 | 4108 | n/a |
| 1 | 4096 | fsys | append_batch(N) + sync_through (window off) | 584 | 584 | 2.39 | 1596.1 | 4331.5 | 1.00 | 4108 | n/a |
| 10 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 508 | 5078 | 0.33 | 1845.3 | 3216.9 | 10.00 | 40960 | 1.00 |
| 10 | 64 | store-io | (b) AppendBatch: N x append + commit | 1473 | 14728 | 0.94 | 630.4 | 1327.6 | 1.00 | 4096 | 1.00 |
| 10 | 64 | raw | (a') N writes of one padded block + one flush | 357 | 3567 | 0.23 | 1993.8 | 15487.8 | 10.00 | 40960 | n/a |
| 10 | 64 | raw | (b') packed: one write of N records + one flush | 1460 | 14597 | 0.93 | 638.1 | 1221.0 | 1.00 | 4096 | n/a |
| 10 | 64 | fsys | append x N + sync_through (window off) | 579 | 5793 | 0.37 | 1584.3 | 6460.7 | 10.00 | 760 | n/a |
| 10 | 64 | fsys | append_batch(N) + sync_through (window off) | 428 | 4281 | 0.27 | 2035.8 | 8376.9 | 1.00 | 760 | n/a |
| 10 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 438 | 4376 | 2.24 | 2224.3 | 3663.6 | 10.00 | 40960 | 1.00 |
| 10 | 512 | store-io | (b) AppendBatch: N x append + commit | 1038 | 10384 | 5.32 | 792.6 | 3356.7 | 1.00 | 8192 | 1.00 |
| 10 | 512 | raw | (a') N writes of one padded block + one flush | 451 | 4506 | 2.31 | 2186.8 | 3176.1 | 10.00 | 40960 | n/a |
| 10 | 512 | raw | (b') packed: one write of N records + one flush | 1340 | 13395 | 6.86 | 689.0 | 1701.6 | 1.00 | 8192 | n/a |
| 10 | 512 | fsys | append x N + sync_through (window off) | 435 | 4350 | 2.23 | 1958.8 | 4141.7 | 10.00 | 5240 | n/a |
| 10 | 512 | fsys | append_batch(N) + sync_through (window off) | 405 | 4048 | 2.07 | 1992.5 | 6992.1 | 1.00 | 5240 | n/a |
| 10 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 450 | 4499 | 18.43 | 2148.5 | 3456.3 | 10.00 | 40960 | 1.00 |
| 10 | 4096 | store-io | (b) AppendBatch: N x append + commit | 1324 | 13238 | 54.22 | 733.1 | 1148.0 | 1.00 | 40960 | 1.00 |
| 10 | 4096 | raw | (a') N writes of one padded block + one flush | 291 | 2913 | 11.93 | 2383.3 | 33130.6 | 10.00 | 40960 | n/a |
| 10 | 4096 | raw | (b') packed: one write of N records + one flush | 1154 | 11538 | 47.26 | 747.4 | 1446.5 | 1.00 | 40960 | n/a |
| 10 | 4096 | fsys | append x N + sync_through (window off) | 337 | 3375 | 13.82 | 2019.6 | 7941.9 | 10.00 | 41080 | n/a |
| 10 | 4096 | fsys | append_batch(N) + sync_through (window off) | 519 | 5193 | 21.27 | 1770.8 | 5236.2 | 1.00 | 41080 | n/a |
| 100 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 62 | 6246 | 0.40 | 15712.6 | 21267.4 | 100.00 | 409600 | 1.00 |
| 100 | 64 | store-io | (b) AppendBatch: N x append + commit | 878 | 87756 | 5.62 | 776.8 | 2090.2 | 1.00 | 8192 | 1.00 |
| 100 | 64 | raw | (a') N writes of one padded block + one flush | 65 | 6513 | 0.42 | 14948.1 | 22186.7 | 100.00 | 409600 | n/a |
| 100 | 64 | raw | (b') packed: one write of N records + one flush | 1395 | 139527 | 8.93 | 669.6 | 1154.4 | 1.00 | 8192 | n/a |
| 100 | 64 | fsys | append x N + sync_through (window off) | 482 | 48192 | 3.08 | 1851.2 | 6774.4 | 100.00 | 7600 | n/a |
| 100 | 64 | fsys | append_batch(N) + sync_through (window off) | 483 | 48256 | 3.09 | 2004.7 | 3490.5 | 1.00 | 7600 | n/a |
| 100 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 58 | 5843 | 2.99 | 16632.0 | 26223.4 | 100.00 | 409600 | 1.00 |
| 100 | 512 | store-io | (b) AppendBatch: N x append + commit | 1431 | 143132 | 73.28 | 676.6 | 1006.0 | 1.00 | 53248 | 1.00 |
| 100 | 512 | raw | (a') N writes of one padded block + one flush | 67 | 6687 | 3.42 | 14447.1 | 22256.9 | 100.00 | 409600 | n/a |
| 100 | 512 | raw | (b') packed: one write of N records + one flush | 1508 | 150756 | 77.19 | 640.4 | 1171.1 | 1.00 | 53248 | n/a |
| 100 | 512 | fsys | append x N + sync_through (window off) | 468 | 46813 | 23.97 | 1997.1 | 4370.8 | 100.00 | 52400 | n/a |
| 100 | 512 | fsys | append_batch(N) + sync_through (window off) | 510 | 51046 | 26.14 | 1842.2 | 6877.9 | 1.00 | 52400 | n/a |
| 100 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 68 | 6801 | 27.86 | 14491.9 | 18230.4 | 100.00 | 409600 | 1.00 |
| 100 | 4096 | store-io | (b) AppendBatch: N x append + commit | 1078 | 107837 | 441.70 | 898.9 | 1387.7 | 1.00 | 409600 | 1.00 |
| 100 | 4096 | raw | (a') N writes of one padded block + one flush | 69 | 6861 | 28.10 | 14356.1 | 18389.6 | 100.00 | 409600 | n/a |
| 100 | 4096 | raw | (b') packed: one write of N records + one flush | 959 | 95949 | 393.01 | 849.6 | 1935.7 | 1.00 | 409600 | n/a |
| 100 | 4096 | fsys | append x N + sync_through (window off) | 312 | 31187 | 127.74 | 2915.6 | 10086.9 | 100.00 | 410800 | n/a |
| 100 | 4096 | fsys | append_batch(N) + sync_through (window off) | 267 | 26663 | 109.21 | 3497.7 | 9624.5 | 1.00 | 410800 | n/a |
| 1000 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 6 | 6255 | 0.40 | 156588.7 | n/a | 1000.00 | 4096000 | 1.00 |
| 1000 | 64 | store-io | (b) AppendBatch: N x append + commit | 1165 | 1165174 | 74.57 | 769.8 | 1835.3 | 1.00 | 65536 | 1.00 |
| 1000 | 64 | raw | (a') N writes of one padded block + one flush | 7 | 6758 | 0.43 | 145223.6 | n/a | 1000.00 | 4096000 | n/a |
| 1000 | 64 | raw | (b') packed: one write of N records + one flush | 745 | 744968 | 47.68 | 739.0 | 5757.1 | 1.00 | 65536 | n/a |
| 1000 | 64 | fsys | append x N + sync_through (window off) | 287 | 286584 | 18.34 | 2652.6 | 9521.0 | 1000.00 | 76000 | n/a |
| 1000 | 64 | fsys | append_batch(N) + sync_through (window off) | 446 | 445749 | 28.53 | 2085.5 | 4151.2 | 1.00 | 76000 | n/a |
| 1000 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 6 | 6357 | 3.26 | 157440.3 | n/a | 1000.00 | 4096000 | 1.00 |
| 1000 | 512 | store-io | (b) AppendBatch: N x append + commit | 969 | 969082 | 496.17 | 983.5 | 1860.0 | 1.00 | 512000 | 1.00 |
| 1000 | 512 | raw | (a') N writes of one padded block + one flush | 6 | 6358 | 3.26 | 157884.4 | n/a | 1000.00 | 4096000 | n/a |
| 1000 | 512 | raw | (b') packed: one write of N records + one flush | 1109 | 1108633 | 567.62 | 879.8 | 1311.6 | 1.00 | 512000 | n/a |
| 1000 | 512 | fsys | append x N + sync_through (window off) | 236 | 236174 | 120.92 | 4136.5 | 8063.4 | 1000.00 | 524000 | n/a |
| 1000 | 512 | fsys | append_batch(N) + sync_through (window off) | 294 | 294376 | 150.72 | 3075.4 | 7613.6 | 1.00 | 524000 | n/a |
| 1000 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 6 | 5666 | 23.21 | 173875.9 | n/a | 1000.00 | 4096000 | 1.00 |
| 1000 | 4096 | store-io | (b) AppendBatch: N x append + commit | 274 | 274446 | 1124.13 | 3293.9 | 7691.9 | 4.00 | 4096000 | 1.00 |
| 1000 | 4096 | raw | (a') N writes of one padded block + one flush | 6 | 5522 | 22.62 | 181308.9 | n/a | 1000.00 | 4096000 | n/a |
| 1000 | 4096 | raw | (b') packed: one write of N records + one flush | 398 | 397929 | 1629.92 | 2323.6 | 7080.0 | 1.00 | 4096000 | n/a |
| 1000 | 4096 | fsys | append x N + sync_through (window off) | 97 | 96796 | 396.47 | 9891.9 | 19180.7 | 1000.00 | 4108000 | n/a |
| 1000 | 4096 | fsys | append_batch(N) + sync_through (window off) | 126 | 126113 | 516.56 | 7362.1 | 15294.4 | 1.00 | 4108000 | n/a |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 3606 records read back and compared byte for byte; all 3606 checked for overlap and alignment | provisioned a 108 MiB append region in 0.08 s (1492 MB/s: fill, flush, verify); ended at the 3000-operation cap after 2.03 s |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 3696 records read back and compared byte for byte; all 3696 checked for overlap and alignment | provisioned a 108 MiB append region in 0.04 s (2827 MB/s: fill, flush, verify); ended at the 3000-operation cap after 2.33 s |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 1.95 s |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 2.31 s |
| fsys | append x N + sync_through (window off) | passed: all 3043 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 3238 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 3756 records read back and compared byte for byte; all 3756 checked for overlap and alignment | provisioned a 108 MiB append region in 0.07 s (1589 MB/s: fill, flush, verify); ended at the 3000-operation cap after 2.24 s |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 3692 records read back and compared byte for byte; all 3692 checked for overlap and alignment | provisioned a 108 MiB append region in 0.03 s (3646 MB/s: fill, flush, verify); ended at the 3000-operation cap after 2.00 s |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 2.33 s |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 2.12 s |
| fsys | append x N + sync_through (window off) | passed: all 3310 records replayed with JournalReader and compared byte for byte | ended at the 3000-operation cap after 4.79 s |
| fsys | append_batch(N) + sync_through (window off) | passed: all 3306 records replayed with JournalReader and compared byte for byte | ended at the 3000-operation cap after 4.75 s |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 3735 records read back and compared byte for byte; all 3735 checked for overlap and alignment | provisioned a 108 MiB append region in 0.07 s (1637 MB/s: fill, flush, verify); ended at the 3000-operation cap after 2.21 s |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 3628 records read back and compared byte for byte; all 3628 checked for overlap and alignment | provisioned a 108 MiB append region in 0.04 s (2741 MB/s: fill, flush, verify); ended at the 3000-operation cap after 2.55 s |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 2.28 s |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 2.21 s |
| fsys | append x N + sync_through (window off) | passed: all 3039 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 3221 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 27920 records read back and compared byte for byte; all 27920 checked for overlap and alignment | provisioned a 1075 MiB append region in 0.38 s (2965 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 37400 records read back and compared byte for byte; all 37400 checked for overlap and alignment | provisioned a 108 MiB append region in 0.03 s (3481 MB/s: fill, flush, verify); ended at the 3000-operation cap after 2.04 s |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 2.06 s |
| fsys | append x N + sync_through (window off) | passed: all 31970 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 24340 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 24140 records read back and compared byte for byte; all 24140 checked for overlap and alignment | provisioned a 1075 MiB append region in 0.50 s (2238 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 34430 records read back and compared byte for byte; all 34430 checked for overlap and alignment | provisioned a 215 MiB append region in 0.09 s (2471 MB/s: fill, flush, verify); ended at the 3000-operation cap after 2.90 s |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 2.25 s |
| fsys | append x N + sync_through (window off) | passed: all 24330 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 21230 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 24380 records read back and compared byte for byte; all 24380 checked for overlap and alignment | provisioned a 1075 MiB append region in 0.46 s (2449 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 35180 records read back and compared byte for byte; all 35180 checked for overlap and alignment | provisioned a 1075 MiB append region in 0.32 s (3491 MB/s: fill, flush, verify); ended at the 3000-operation cap after 2.30 s |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 2.64 s |
| fsys | append x N + sync_through (window off) | passed: all 17790 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 28080 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 34400 records read back and compared byte for byte; all 34400 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.71 s (3036 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 355100 records read back and compared byte for byte; all 355100 checked for overlap and alignment | provisioned a 215 MiB append region in 0.07 s (3009 MB/s: fill, flush, verify); ended at the 3000-operation cap after 3.52 s |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 2.19 s |
| fsys | append x N + sync_through (window off) | passed: all 266000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 262100 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 32200 records read back and compared byte for byte; all 32200 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.79 s (2718 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 367100 records read back and compared byte for byte; all 367100 checked for overlap and alignment | provisioned a 1397 MiB append region in 0.43 s (3440 MB/s: fill, flush, verify); ended at the 3000-operation cap after 2.18 s |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 2.05 s |
| fsys | append x N + sync_through (window off) | passed: all 256300 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 278600 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 37400 records read back and compared byte for byte; all 37400 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.64 s (3337 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 351300 records read back and compared byte for byte; all 351300 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.66 s (3235 MB/s: fill, flush, verify); ended at the 3000-operation cap after 3.02 s |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 3.35 s |
| fsys | append x N + sync_through (window off) | passed: all 169000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 145300 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 35000 records read back and compared byte for byte; all 35000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.82 s (2617 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 3426000 records read back and compared byte for byte; all 3426000 checked for overlap and alignment | provisioned a 1719 MiB append region in 0.62 s (2907 MB/s: fill, flush, verify); ended at the 3000-operation cap after 3.07 s |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 4.34 s |
| fsys | append x N + sync_through (window off) | passed: all 1470000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 2328000 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 36000 records read back and compared byte for byte; all 36000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.79 s (2712 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 3328000 records read back and compared byte for byte; all 3328000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.71 s (3027 MB/s: fill, flush, verify); ended at the 3000-operation cap after 3.72 s |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 3.17 s |
| fsys | append x N + sync_through (window off) | passed: all 1252000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 1576000 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 33000 records read back and compared byte for byte; all 33000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.71 s (3016 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 524000 records read back and compared byte for byte; all 524000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.97 s (2206 MB/s: fill, flush, verify); ended early at region capacity: 409 measured ops in 1.82 s |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 495000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 624000 records replayed with JournalReader and compared byte for byte |  |

## W4: page batch (N random 4 KiB pages, one barrier) (`page-batch`)

Wall time: 71.1 s.

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
| 1 | store-io | PageBatch: N x write + commit | 705 | 705 | 925.9 | 7708.9 | 24645.2 | 1.00 | 1.00 |
| 1 | raw | QD1: N writes one at a time + one flush | 1025 | 1025 | 811.6 | 4360.5 | 7403.4 | 1.00 | n/a |
| 1 | fsys | Handle::write_at x N + Handle::sync | 473 | 473 | 1017.9 | 9038.1 | 134089.4 | 1.00 | n/a |
| 16 | store-io | PageBatch: N x write + commit | 137 | 2190 | 3867.4 | 111929.8 | n/a | 16.00 | 1.00 |
| 16 | raw | QD1: N writes one at a time + one flush | 218 | 3482 | 3766.9 | 20911.5 | 78191.0 | 16.00 | n/a |
| 16 | fsys | Handle::write_at x N + Handle::sync | 139 | 2226 | 4515.0 | 45499.0 | n/a | 16.00 | n/a |
| 64 | store-io | PageBatch: N x write + commit | 86 | 5494 | 11110.8 | 16310.2 | n/a | 64.00 | 1.00 |
| 64 | raw | QD1: N writes one at a time + one flush | 47 | 3026 | 13154.4 | 140218.7 | n/a | 64.00 | n/a |
| 64 | fsys | Handle::write_at x N + Handle::sync | 223 | 14259 | 3879.7 | 27770.7 | 55796.8 | 64.00 | n/a |
| 256 | store-io | PageBatch: N x write + commit | 17 | 4270 | 46472.4 | n/a | n/a | 256.00 | 1.00 |
| 256 | raw | QD1: N writes one at a time + one flush | 18 | 4624 | 46287.1 | n/a | n/a | 256.00 | n/a |
| 256 | fsys | Handle::write_at x N + Handle::sync | 133 | 34099 | 6657.2 | 18510.3 | n/a | 256.00 | n/a |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | PageBatch: N x write + commit | passed: 1025 of 3771 written pages read back (latest write each) and compared byte for byte |  |
| raw | QD1: N writes one at a time + one flush | passed: 1025 of 5232 written slots read back unbuffered and compared byte for byte |  |
| fsys | Handle::write_at x N + Handle::sync | passed: 1025 of 2560 written slots read back unbuffered and compared byte for byte |  |
| store-io | PageBatch: N x write + commit | passed: 1025 of 10435 written pages read back (latest write each) and compared byte for byte |  |
| raw | QD1: N writes one at a time + one flush | passed: 1025 of 16403 written slots read back unbuffered and compared byte for byte |  |
| fsys | Handle::write_at x N + Handle::sync | passed: 1025 of 10463 written slots read back unbuffered and compared byte for byte |  |
| store-io | PageBatch: N x write + commit | passed: 1025 of 23785 written pages read back (latest write each) and compared byte for byte |  |
| raw | QD1: N writes one at a time + one flush | passed: 1025 of 14436 written slots read back unbuffered and compared byte for byte |  |
| fsys | Handle::write_at x N + Handle::sync | passed: 1025 of 44057 written slots read back unbuffered and compared byte for byte |  |
| store-io | PageBatch: N x write + commit | passed: 1025 of 19429 written pages read back (latest write each) and compared byte for byte |  |
| raw | QD1: N writes one at a time + one flush | passed: 1025 of 21196 written slots read back unbuffered and compared byte for byte |  |
| fsys | Handle::write_at x N + Handle::sync | passed: 1025 of 61404 written slots read back unbuffered and compared byte for byte |  |

## W5: sequential bandwidth (1 MiB writes, 1/8 MiB reads) (`sequential`)

Wall time: 29.6 s.

**Method.**

- Write: one thread writes 1 MiB chunks back to back (not durable), then one durability barrier; MB/s = bytes / (time inside the write calls + the barrier); payload generation between writes is excluded (the wall clock is recorded as `wall_s` in the JSON). No warm-up (the region is written once); the point stops at 1 GiB or at the time box.
- store-io: `AppendRegion::append(1 MiB)` into a fresh 1 GiB append region, then `sync_through(last ticket)`. raw: copy into an aligned buffer + pwritev2 per chunk into a ready 1 GiB file, then one data flush. fsys: journal `append(1 MiB)` + one `sync_through` on a fresh journal (buffered and direct, window off; fsys extends its file as it appends, store-io and raw overwrite preallocated space).
- Read: the bytes just written, front to back, repeated until the time box: store-io `AppendRegion::read(offset, &mut out)` with 1 MiB and 8 MiB `out`; raw preadv2 on an O_DIRECT descriptor (one call per request) with the same request sizes. Latency is per request; MB/s = bytes / time inside the read calls (the content spot-check between requests is excluded).
- fsys read: not measured. fsys 1.1.3 has no unbuffered positioned read (`Handle::read_at` opens the file buffered on every call, and `JournalReader` replays through a buffered reader), so it would measure the page cache, not the device.
- MB = 10^6 bytes.

Results:

| system | variant | request | MB/s | bytes | seconds | barrier ms | request p50 µs | request p99 µs | request max µs | write calls/request |
|---|---|---|---|---|---|---|---|---|---|---|
| store-io | AppendRegion::append(1 MiB) x N + sync_through | 1048576 | 1230.5 | 1073741824 | 0.873 | 1.40 | 825.5 | 1506.2 | 5089.9 | 1.00 |
| store-io | AppendRegion::read, 1 MiB requests | 1048576 | 1227.9 | 5413797888 | 4.409 | n/a | 831.3 | 1374.4 | 6370.6 | 0.00 |
| store-io | AppendRegion::read, 8 MiB requests | 8388608 | 1233.1 | 6006243328 | 4.871 | n/a | 6729.5 | 10298.8 | 12121.1 | 0.00 |
| raw | 1 MiB writes + one data flush | 1048576 | 2114.8 | 1073741824 | 0.508 | 1.57 | 471.4 | 794.8 | 2349.8 | 1.00 |
| raw | unbuffered read, 1 MiB requests | 1048576 | 1940.3 | 7645167616 | 3.940 | n/a | 531.2 | 759.5 | 6731.5 | 0.00 |
| raw | unbuffered read, 8 MiB requests | 8388608 | 4726.3 | 21734883328 | 4.599 | n/a | 1739.0 | 2491.5 | 4246.1 | 0.00 |
| fsys | journal, buffered, window off: append(1 MiB) x N + sync_through | 1048576 | 787.8 | 1073741824 | 1.363 | 196.75 | 1161.8 | 2268.5 | 3734.5 | 1.00 |
| fsys | journal, direct, window off: append(1 MiB) x N + sync_through | 1048576 | 1291.5 | 1073741824 | 0.831 | 2.62 | 790.5 | 1617.0 | 6026.3 | 1.00 |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append(1 MiB) x N + sync_through | passed: all 1024 MiB read back through AppendRegion::read and compared byte for byte | provisioned a 1024 MiB append region in 0.69 s (1554 MB/s: fill, flush, verify) |
| store-io | AppendRegion::read, 1 MiB requests | passed: first 1 MiB of each of 5785 requests compared against what was written |  |
| store-io | AppendRegion::read, 8 MiB requests | passed: first 1 MiB of each of 783 requests compared against what was written |  |
| raw | 1 MiB writes + one data flush | passed: all 1024 MiB read back unbuffered and compared byte for byte | file made ready (zero-filled unbuffered 1 MiB writes + flushed) in 0.26 s (4073 MB/s); compare store-io's provisioning note |
| raw | unbuffered read, 1 MiB requests | passed: first 1 MiB of each of 8052 requests compared against what was written |  |
| raw | unbuffered read, 8 MiB requests | passed: first 1 MiB of each of 2856 requests compared against what was written |  |
| fsys | journal, buffered, window off: append(1 MiB) x N + sync_through | passed: all 1024 records replayed with JournalReader and compared byte for byte |  |
| fsys | journal, direct, window off: append(1 MiB) x N + sync_through | passed: all 1024 records replayed with JournalReader and compared byte for byte |  |

