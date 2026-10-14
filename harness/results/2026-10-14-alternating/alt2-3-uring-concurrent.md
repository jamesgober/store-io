# store-io performance harness: linux / msft-virtual-disk (2026-10-14)

> **OVERRIDE IN EFFECT.** `Store::create` with default trust was **refused** on this device:
> `device durability is unverified: running under a hypervisor without a certificate for this volume type; missing: device identify data (needs a readable device node); SMART / health log (needs privilege on Linux); volatile write cache presence; volatile write cache enabled state; device atomic-write fields; [Unverified { missing: {Identify, Smart, CachePresence, CacheEnabled, AtomicFields}, reasons: {Hypervisor} }]`
>
> Every store in this run was created with the documented labelled override (`Trust { override_refusal: true, .. }`): the class stays as decided, every receipt is labelled `overridden`, and the most conservative primitive (write + device flush) is used. Nothing else was weakened. The numbers below are therefore **not** gate numbers for a certified durability class; they measure software cost on this stack.

## Run

| item | value |
|---|---|
| date (UTC) | 2026-10-14T08:47:49Z |
| command | concurrent |
| store-io commit | e4f90ecec8fe39c18eccbe385c6a10fe51cfce2c (crates/ modified) |
| OS | Ubuntu 24.04.4 LTS |
| kernel / build | Linux version 6.6.87.2-microsoft-standard-WSL2 (root@439a258ad544) (gcc (GCC) 11.2.0, GNU ld (GNU Binutils) 2.37) #1 SMP PREEMPT_DYNAMIC Thu Jun  5 18:30:46 UTC 2025 |
| CPU | AMD Ryzen 9 9950X3D 16-Core Processor |
| logical CPUs | 32 |
| filesystem | ext4 on / (device /dev/sdd 8:48, mount options rw,relatime, super options rw,discard,errors=remount-ro,data=ordered); store-io classified it Ext4 |
| directory under test | /home/james/store-io-harness-1452 |
| backend | store-io-posix (PosixPlatform, O_DIRECT; tier in the report's io line) |
| durability class | unverified (receipt label overridden(running under a hypervisor without a certificate for this volume type), durable open Overridden (Trust::override_refusal), block size 4096 B) |
| time per point | 2.0 s measured after 0.2 s warm-up (time-boxed; a point may end early at region capacity, noted where it does) |
| fsys | fsys 1.1.3 (crates.io), Handle method Sync, durability primitive fsync |
| raw primitives | pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync; preadv2 on an O_DIRECT descriptor (one call per request) |
| harness build | release, lto=fat, codegen-units=1 (same as store-io's release profile) |
| total wall time | 66.1 s |

## Device report (`Store::report()`)

```text
volume           204c1d52-4241-8720-9f94-abe87bd9a515
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
| concurrent | 1 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 369.5 | 355.0 | 179.0 | 1.041 | +4.1% | 2.064 | +106.4% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 1 concurrent durable 4 KiB writers | p99 µs (lower is better) | 5262.2 | 6862.3 | 20780.2 | 0.767 | +23.3% | 0.253 | +74.7% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 2 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 426.1 | 423.9 | 346.1 | 1.005 | +0.5% | 1.231 | +23.1% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 2 concurrent durable 4 KiB writers | p99 µs (lower is better) | 10437.4 | 20486.3 | 11071.2 | 0.509 | +49.1% | 0.943 | +5.7% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 4 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 829.9 | 427.3 | 652.4 | 1.942 | +94.2% | 1.272 | +27.2% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 4 concurrent durable 4 KiB writers | p99 µs (lower is better) | 19521.1 | 24314.5 | 14631.6 | 0.803 | +19.7% | 1.334 | -33.4% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 8 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 1471.4 | 442.3 | 1273.5 | 3.326 | +232.6% | 1.155 | +15.5% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 8 concurrent durable 4 KiB writers | p99 µs (lower is better) | 19990.5 | 32764.3 | 12507.2 | 0.610 | +39.0% | 1.598 | -59.8% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 16 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 2327.0 | 400.0 | 1557.2 | 5.817 | +481.7% | 1.494 | +49.4% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 16 concurrent durable 4 KiB writers | p99 µs (lower is better) | 20685.8 | 60122.9 | 21625.0 | 0.344 | +65.6% | 0.957 | +4.3% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 32 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 4300.8 | 503.4 | 3189.5 | 8.544 | +754.4% | 1.348 | +34.8% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 32 concurrent durable 4 KiB writers | p99 µs (lower is better) | 22485.5 | 103313.7 | 19488.2 | 0.218 | +78.2% | 1.154 | -15.4% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 64 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 8054.4 | 679.9 | 5676.7 | 11.847 | +1084.7% | 1.419 | +41.9% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 64 concurrent durable 4 KiB writers | p99 µs (lower is better) | 12653.1 | 131787.7 | 26183.2 | 0.096 | +90.4% | 0.483 | +51.7% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |

## Anomalies, errors and integrity failures

- None: every measurement completed and every integrity check passed.

## W2: concurrent durable writers, 4 KiB (`concurrent`)

Wall time: 65.6 s.

**Method.**

- Writer counts [1, 2, 4, 8, 16, 32, 64]; every thread loops a durable 4 KiB write at queue depth 1 for the whole point.
- store-io: all threads call `AppendRegion::append_durable` on one append region of one store (a fresh store per point). Flush sharing is read from `Store::domain_stats()` deltas over the window: `flushes` issued, barriers that `led` a flush, barriers that `joined` one.
- raw: each thread writes its own contiguous range of one ready 256 MiB file and makes it durable itself with pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync (no sharing).
- fsys: all threads call `append` + `sync_through` on one shared journal (fresh per point), window off and default (500 µs group-commit window).
- An operation counts when it started and finished inside the measured window; rate = counted operations / window.

Results:

| threads | system | variant | durable ops/s | p50 µs | p99 µs | p99.9 µs | flushes issued | barriers led | barriers joined | writes per flush |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | store-io | AppendRegion::append_durable (shared region) | 369 | 2554.7 | 5262.2 | n/a | 740 | 740 | 0 | 1.00 |
| 1 | raw | own write + own data flush (per thread range) | 355 | 2587.6 | 6862.3 | n/a | n/a | n/a | n/a | n/a |
| 1 | fsys | journal, buffered, window off: shared journal append + sync_through | 179 | 5269.2 | 20780.2 | n/a | n/a | n/a | n/a | n/a |
| 1 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 160 | 5811.7 | 21416.5 | n/a | n/a | n/a | n/a | n/a |
| 2 | store-io | AppendRegion::append_durable (shared region) | 426 | 4476.0 | 10437.4 | n/a | 867 | 867 | 0 | 1.00 |
| 2 | raw | own write + own data flush (per thread range) | 424 | 4520.0 | 20486.3 | n/a | n/a | n/a | n/a | n/a |
| 2 | fsys | journal, buffered, window off: shared journal append + sync_through | 346 | 5288.6 | 11071.2 | n/a | n/a | n/a | n/a | n/a |
| 2 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 325 | 5912.7 | 21603.7 | n/a | n/a | n/a | n/a | n/a |
| 4 | store-io | AppendRegion::append_durable (shared region) | 830 | 4558.0 | 19521.1 | 21231.2 | 839 | 839 | 825 | 1.98 |
| 4 | raw | own write + own data flush (per thread range) | 427 | 8749.7 | 24314.5 | n/a | n/a | n/a | n/a | n/a |
| 4 | fsys | journal, buffered, window off: shared journal append + sync_through | 517 | 5655.8 | 14631.6 | 21129.9 | n/a | n/a | n/a | n/a |
| 4 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 652 | 5875.8 | 18762.2 | 21328.4 | n/a | n/a | n/a | n/a |
| 8 | store-io | AppendRegion::append_durable (shared region) | 1471 | 4763.7 | 19990.5 | 22514.3 | 839 | 839 | 2125 | 3.52 |
| 8 | raw | own write + own data flush (per thread range) | 442 | 16841.6 | 32764.3 | n/a | n/a | n/a | n/a | n/a |
| 8 | fsys | journal, buffered, window off: shared journal append + sync_through | 907 | 10267.9 | 13740.8 | 26900.7 | n/a | n/a | n/a | n/a |
| 8 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 1274 | 5937.7 | 12507.2 | 25041.8 | n/a | n/a | n/a | n/a |
| 16 | store-io | AppendRegion::append_durable (shared region) | 2327 | 7153.6 | 20685.8 | 24614.6 | 736 | 736 | 3935 | 6.32 |
| 16 | raw | own write + own data flush (per thread range) | 400 | 37046.4 | 60122.9 | n/a | n/a | n/a | n/a | n/a |
| 16 | fsys | journal, buffered, window off: shared journal append + sync_through | 1523 | 10859.1 | 27142.1 | 27436.4 | n/a | n/a | n/a | n/a |
| 16 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 1557 | 10850.8 | 21625.0 | 27861.7 | n/a | n/a | n/a | n/a |
| 32 | store-io | AppendRegion::append_durable (shared region) | 4301 | 7334.8 | 22485.5 | 27489.0 | 752 | 752 | 7885 | 11.44 |
| 32 | raw | own write + own data flush (per thread range) | 503 | 53934.7 | 103313.7 | 106671.6 | n/a | n/a | n/a | n/a |
| 32 | fsys | journal, buffered, window off: shared journal append + sync_through | 2993 | 10693.2 | 19488.2 | 26789.8 | n/a | n/a | n/a | n/a |
| 32 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 3190 | 10217.5 | 19782.7 | 28352.0 | n/a | n/a | n/a | n/a |
| 64 | store-io | AppendRegion::append_durable (shared region) | 8054 | 7707.9 | 12653.1 | 23390.4 | 767 | 767 | 15407 | 21.00 |
| 64 | raw | own write + own data flush (per thread range) | 680 | 92793.7 | 131787.7 | 133922.5 | n/a | n/a | n/a | n/a |
| 64 | fsys | journal, buffered, window off: shared journal append + sync_through | 5594 | 10948.1 | 27224.5 | 29430.5 | n/a | n/a | n/a | n/a |
| 64 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 5677 | 10694.2 | 26183.2 | 29549.4 | n/a | n/a | n/a | n/a |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append_durable (shared region) | passed: 813 of 813 records read back and compared byte for byte; all 813 checked for overlap and alignment | provisioned a 344 MiB append region in 0.13 s (2753 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 784 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 398 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 362 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 955 of 955 records read back and compared byte for byte; all 955 checked for overlap and alignment | provisioned a 344 MiB append region in 0.16 s (2280 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1008 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 810 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 724 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 1825 records read back and compared byte for byte; all 1825 checked for overlap and alignment | provisioned a 344 MiB append region in 0.13 s (2728 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 963 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.005 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 1141 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.004 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 1456 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 3255 records read back and compared byte for byte; all 3255 checked for overlap and alignment | provisioned a 344 MiB append region in 0.15 s (2484 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1001 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.010 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 2022 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.004 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 2816 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 5187 records read back and compared byte for byte; all 5187 checked for overlap and alignment | provisioned a 344 MiB append region in 0.17 s (2091 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 905 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.020 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 3399 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.005 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 3424 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.005 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 9465 records read back and compared byte for byte; all 9465 checked for overlap and alignment | provisioned a 344 MiB append region in 0.15 s (2484 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1036 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.033 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 6628 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.005 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 7118 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.005 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 17972 records read back and compared byte for byte; all 17972 checked for overlap and alignment | provisioned a 344 MiB append region in 0.16 s (2326 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1088 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.048 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 12436 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.006 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 12674 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.006 (straddling ops make this slightly above 1) |

**Findings.**

- store-io scaling: 369 durable ops/s at 1 writer, 8054 at 64 writers (21.00 writes per flush) [derived from the table].

