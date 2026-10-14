# store-io performance harness: linux / msft-virtual-disk (2026-10-14)

> **OVERRIDE IN EFFECT.** `Store::create` with default trust was **refused** on this device:
> `device durability is unverified: running under a hypervisor without a certificate for this volume type; missing: device identify data (needs a readable device node); SMART / health log (needs privilege on Linux); volatile write cache presence; volatile write cache enabled state; device atomic-write fields; [Unverified { missing: {Identify, Smart, CachePresence, CacheEnabled, AtomicFields}, reasons: {Hypervisor} }]`
>
> Every store in this run was created with the documented labelled override (`Trust { override_refusal: true, .. }`): the class stays as decided, every receipt is labelled `overridden`, and the most conservative primitive (write + device flush) is used. Nothing else was weakened. The numbers below are therefore **not** gate numbers for a certified durability class; they measure software cost on this stack.

## Run

| item | value |
|---|---|
| date (UTC) | 2026-10-14T08:45:01Z |
| command | concurrent |
| store-io commit | e4f90ecec8fe39c18eccbe385c6a10fe51cfce2c (crates/ modified) |
| OS | Ubuntu 24.04.4 LTS |
| kernel / build | Linux version 6.6.87.2-microsoft-standard-WSL2 (root@439a258ad544) (gcc (GCC) 11.2.0, GNU ld (GNU Binutils) 2.37) #1 SMP PREEMPT_DYNAMIC Thu Jun  5 18:30:46 UTC 2025 |
| CPU | AMD Ryzen 9 9950X3D 16-Core Processor |
| logical CPUs | 32 |
| filesystem | ext4 on / (device /dev/sdd 8:48, mount options rw,relatime, super options rw,discard,errors=remount-ro,data=ordered); store-io classified it Ext4 |
| directory under test | /home/james/store-io-harness-376 |
| backend | store-io-posix (PosixPlatform, O_DIRECT; tier in the report's io line) |
| durability class | unverified (receipt label overridden(running under a hypervisor without a certificate for this volume type), durable open Overridden (Trust::override_refusal), block size 4096 B) |
| time per point | 2.0 s measured after 0.2 s warm-up (time-boxed; a point may end early at region capacity, noted where it does) |
| fsys | fsys 1.1.3 (crates.io), Handle method Sync, durability primitive fsync |
| raw primitives | pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync; preadv2 on an O_DIRECT descriptor (one call per request) |
| harness build | release, lto=fat, codegen-units=1 (same as store-io's release profile) |
| total wall time | 68.0 s |

## Device report (`Store::report()`)

```text
volume           966700f1-7744-0ac0-9e16-2c2ec0a17b28
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
| concurrent | 1 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 372.5 | 404.5 | 344.5 | 0.921 | -7.9% | 1.081 | +8.1% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 1 concurrent durable 4 KiB writers | p99 µs (lower is better) | 6085.2 | 4609.0 | 7468.2 | 1.320 | -32.0% | 0.815 | +18.5% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 2 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 2041.4 | 2316.3 | 359.0 | 0.881 | -11.9% | 5.687 | +468.7% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 2 concurrent durable 4 KiB writers | p99 µs (lower is better) | 5818.1 | 5766.1 | 13639.8 | 1.009 | -0.9% | 0.427 | +57.3% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 4 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 469.5 | 2244.9 | 1861.9 | 0.209 | -79.1% | 0.252 | -74.8% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 4 concurrent durable 4 KiB writers | p99 µs (lower is better) | 20229.3 | 7129.8 | 2837.7 | 2.837 | -183.7% | 7.129 | -612.9% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 8 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 6303.5 | 2529.6 | 2166.0 | 2.492 | +149.2% | 2.910 | +191.0% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 8 concurrent durable 4 KiB writers | p99 µs (lower is better) | 2510.3 | 8026.0 | 10674.1 | 0.313 | +68.7% | 0.235 | +76.5% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 16 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 2630.4 | 452.5 | 1593.4 | 5.813 | +481.3% | 1.651 | +65.1% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 16 concurrent durable 4 KiB writers | p99 µs (lower is better) | 19906.0 | 50130.9 | 20102.2 | 0.397 | +60.3% | 0.990 | +1.0% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 32 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 4467.8 | 495.5 | 3245.9 | 9.017 | +801.7% | 1.376 | +37.6% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 32 concurrent durable 4 KiB writers | p99 µs (lower is better) | 10226.2 | 110072.6 | 15937.0 | 0.093 | +90.7% | 0.642 | +35.8% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 64 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 8689.6 | 724.6 | 5723.8 | 11.992 | +1099.2% | 1.518 | +51.8% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 64 concurrent durable 4 KiB writers | p99 µs (lower is better) | 10512.4 | 130908.5 | 26896.6 | 0.080 | +92.0% | 0.391 | +60.9% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |

## Anomalies, errors and integrity failures

- None: every measurement completed and every integrity check passed.

## W2: concurrent durable writers, 4 KiB (`concurrent`)

Wall time: 67.0 s.

**Method.**

- Writer counts [1, 2, 4, 8, 16, 32, 64]; every thread loops a durable 4 KiB write at queue depth 1 for the whole point.
- store-io: all threads call `AppendRegion::append_durable` on one append region of one store (a fresh store per point). Flush sharing is read from `Store::domain_stats()` deltas over the window: `flushes` issued, barriers that `led` a flush, barriers that `joined` one.
- raw: each thread writes its own contiguous range of one ready 256 MiB file and makes it durable itself with pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync (no sharing).
- fsys: all threads call `append` + `sync_through` on one shared journal (fresh per point), window off and default (500 µs group-commit window).
- An operation counts when it started and finished inside the measured window; rate = counted operations / window.

Results:

| threads | system | variant | durable ops/s | p50 µs | p99 µs | p99.9 µs | flushes issued | barriers led | barriers joined | writes per flush |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | store-io | AppendRegion::append_durable (shared region) | 372 | 2299.0 | 6085.2 | n/a | 746 | 746 | 0 | 1.00 |
| 1 | raw | own write + own data flush (per thread range) | 404 | 2319.0 | 4609.0 | n/a | n/a | n/a | n/a | n/a |
| 1 | fsys | journal, buffered, window off: shared journal append + sync_through | 185 | 5156.1 | 20103.5 | n/a | n/a | n/a | n/a | n/a |
| 1 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 344 | 2022.0 | 7468.2 | n/a | n/a | n/a | n/a | n/a |
| 2 | store-io | AppendRegion::append_durable (shared region) | 2041 | 912.2 | 5818.1 | 5977.2 | 4085 | 4085 | 0 | 1.00 |
| 2 | raw | own write + own data flush (per thread range) | 2316 | 797.4 | 5766.1 | 6011.5 | n/a | n/a | n/a | n/a |
| 2 | fsys | journal, buffered, window off: shared journal append + sync_through | 359 | 4906.0 | 13639.8 | n/a | n/a | n/a | n/a | n/a |
| 2 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 349 | 4992.2 | 19102.0 | n/a | n/a | n/a | n/a | n/a |
| 4 | store-io | AppendRegion::append_durable (shared region) | 469 | 8141.5 | 20229.3 | n/a | 477 | 477 | 466 | 1.97 |
| 4 | raw | own write + own data flush (per thread range) | 2245 | 1443.5 | 7129.8 | 22450.2 | n/a | n/a | n/a | n/a |
| 4 | fsys | journal, buffered, window off: shared journal append + sync_through | 1817 | 1678.5 | 4122.7 | 8477.2 | n/a | n/a | n/a | n/a |
| 4 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 1862 | 2079.6 | 2837.7 | 8156.0 | n/a | n/a | n/a | n/a |
| 8 | store-io | AppendRegion::append_durable (shared region) | 6304 | 1182.1 | 2510.3 | 6648.6 | 3545 | 3545 | 9073 | 3.56 |
| 8 | raw | own write + own data flush (per thread range) | 2530 | 2867.5 | 8026.0 | 10191.4 | n/a | n/a | n/a | n/a |
| 8 | fsys | journal, buffered, window off: shared journal append + sync_through | 2166 | 2954.0 | 11362.6 | 25337.9 | n/a | n/a | n/a | n/a |
| 8 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 1336 | 5750.1 | 10674.1 | 21156.8 | n/a | n/a | n/a | n/a |
| 16 | store-io | AppendRegion::append_durable (shared region) | 2630 | 6396.9 | 19906.0 | 22802.9 | 849 | 849 | 4428 | 6.20 |
| 16 | raw | own write + own data flush (per thread range) | 452 | 32632.1 | 50130.9 | n/a | n/a | n/a | n/a | n/a |
| 16 | fsys | journal, buffered, window off: shared journal append + sync_through | 1588 | 10594.9 | 20102.2 | 25897.6 | n/a | n/a | n/a | n/a |
| 16 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 1593 | 10458.6 | 25304.7 | 27334.6 | n/a | n/a | n/a | n/a |
| 32 | store-io | AppendRegion::append_durable (shared region) | 4468 | 7194.9 | 10226.2 | 23122.7 | 784 | 784 | 8185 | 11.40 |
| 32 | raw | own write + own data flush (per thread range) | 495 | 49664.2 | 110072.6 | n/a | n/a | n/a | n/a | n/a |
| 32 | fsys | journal, buffered, window off: shared journal append + sync_through | 3246 | 9991.3 | 15937.0 | 25370.1 | n/a | n/a | n/a | n/a |
| 32 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 3126 | 10417.5 | 17864.8 | 25848.5 | n/a | n/a | n/a | n/a |
| 64 | store-io | AppendRegion::append_durable (shared region) | 8690 | 7315.7 | 10512.4 | 12569.2 | 839 | 839 | 16607 | 20.72 |
| 64 | raw | own write + own data flush (per thread range) | 725 | 83444.7 | 130908.5 | 135355.6 | n/a | n/a | n/a | n/a |
| 64 | fsys | journal, buffered, window off: shared journal append + sync_through | 5724 | 11096.7 | 26896.6 | 28761.0 | n/a | n/a | n/a | n/a |
| 64 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 5391 | 11489.1 | 27885.8 | 28283.2 | n/a | n/a | n/a | n/a |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append_durable (shared region) | passed: 787 of 787 records read back and compared byte for byte; all 787 checked for overlap and alignment | provisioned a 344 MiB append region in 0.77 s (468 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 890 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 413 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 723 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 4502 records read back and compared byte for byte; all 4502 checked for overlap and alignment | provisioned a 344 MiB append region in 0.16 s (2211 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1026 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 811 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 780 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 1032 records read back and compared byte for byte; all 1032 checked for overlap and alignment | provisioned a 344 MiB append region in 0.37 s (987 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1028 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 3993 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 4072 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 13891 records read back and compared byte for byte; all 13891 checked for overlap and alignment | provisioned a 344 MiB append region in 0.45 s (801 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1032 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 4922 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 2968 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 5840 records read back and compared byte for byte; all 5840 checked for overlap and alignment | provisioned a 344 MiB append region in 0.36 s (1006 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1016 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.018 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 3549 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.005 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 3548 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.005 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 9955 records read back and compared byte for byte; all 9955 checked for overlap and alignment | provisioned a 344 MiB append region in 0.27 s (1329 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1009 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.031 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 7194 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.005 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 6953 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.005 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 19267 records read back and compared byte for byte; all 19267 checked for overlap and alignment | provisioned a 344 MiB append region in 0.26 s (1412 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1088 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.049 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 12702 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.006 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 12026 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.007 (straddling ops make this slightly above 1) |

**Findings.**

- store-io scaling: 372 durable ops/s at 1 writer, 8690 at 64 writers (20.72 writes per flush) [derived from the table].

