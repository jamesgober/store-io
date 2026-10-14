# store-io performance harness: linux / msft-virtual-disk (2026-10-14)

> **OVERRIDE IN EFFECT.** `Store::create` with default trust was **refused** on this device:
> `device durability is unverified: running under a hypervisor without a certificate for this volume type; missing: device identify data (needs a readable device node); SMART / health log (needs privilege on Linux); volatile write cache presence; volatile write cache enabled state; device atomic-write fields; [Unverified { missing: {Identify, Smart, CachePresence, CacheEnabled, AtomicFields}, reasons: {Hypervisor} }]`
>
> Every store in this run was created with the documented labelled override (`Trust { override_refusal: true, .. }`): the class stays as decided, every receipt is labelled `overridden`, and the most conservative primitive (write + device flush) is used. Nothing else was weakened. The numbers below are therefore **not** gate numbers for a certified durability class; they measure software cost on this stack.

## Run

| item | value |
|---|---|
| date (UTC) | 2026-10-14T08:34:48Z |
| command | concurrent |
| store-io commit | e4f90ecec8fe39c18eccbe385c6a10fe51cfce2c (crates/ modified) |
| OS | Ubuntu 24.04.4 LTS |
| kernel / build | Linux version 6.6.87.2-microsoft-standard-WSL2 (root@439a258ad544) (gcc (GCC) 11.2.0, GNU ld (GNU Binutils) 2.37) #1 SMP PREEMPT_DYNAMIC Thu Jun  5 18:30:46 UTC 2025 |
| CPU | AMD Ryzen 9 9950X3D 16-Core Processor |
| logical CPUs | 32 |
| filesystem | ext4 on / (device /dev/sdd 8:48, mount options rw,relatime, super options rw,discard,errors=remount-ro,data=ordered); store-io classified it Ext4 |
| directory under test | /home/james/store-io-harness-50992 |
| backend | store-io-posix (PosixPlatform, O_DIRECT; tier in the report's io line) |
| durability class | unverified (receipt label overridden(running under a hypervisor without a certificate for this volume type), durable open Overridden (Trust::override_refusal), block size 4096 B) |
| time per point | 3.0 s measured after 0.3 s warm-up (time-boxed; a point may end early at region capacity, noted where it does) |
| fsys | fsys 1.1.3 (crates.io), Handle method Sync, durability primitive fsync |
| raw primitives | pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync; preadv2 on an O_DIRECT descriptor (one call per request) |
| harness build | release, lto=fat, codegen-units=1 (same as store-io's release profile) |
| total wall time | 98.8 s |

## Device report (`Store::report()`)

```text
volume           27387da8-60ee-1b48-0ecc-af24c74c047a
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
| concurrent | 1 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 390.5 | 393.4 | 187.4 | 0.993 | -0.7% | 2.084 | +108.4% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 1 concurrent durable 4 KiB writers | p99 µs (lower is better) | 6381.1 | 6263.3 | 20534.1 | 1.019 | -1.9% | 0.311 | +68.9% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 2 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 427.6 | 501.7 | 328.8 | 0.852 | -14.8% | 1.300 | +30.0% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 2 concurrent durable 4 KiB writers | p99 µs (lower is better) | 19922.2 | 18318.8 | 21066.7 | 1.088 | -8.8% | 0.946 | +5.4% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 4 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 851.0 | 440.1 | 662.8 | 1.934 | +93.4% | 1.284 | +28.4% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 4 concurrent durable 4 KiB writers | p99 µs (lower is better) | 19834.8 | 24098.9 | 11695.6 | 0.823 | +17.7% | 1.696 | -69.6% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 8 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 827.6 | 171.3 | 1129.7 | 4.831 | +383.1% | 0.733 | -26.7% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 8 concurrent durable 4 KiB writers | p99 µs (lower is better) | 35107.2 | 94141.9 | 18285.7 | 0.373 | +62.7% | 1.920 | -92.0% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 16 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 2150.2 | 418.6 | 1604.9 | 5.136 | +413.6% | 1.340 | +34.0% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 16 concurrent durable 4 KiB writers | p99 µs (lower is better) | 18361.5 | 56998.1 | 26392.9 | 0.322 | +67.8% | 0.696 | +30.4% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 32 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 4474.1 | 466.6 | 3093.6 | 9.588 | +858.8% | 1.446 | +44.6% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 32 concurrent durable 4 KiB writers | p99 µs (lower is better) | 9266.0 | 119295.8 | 17631.5 | 0.078 | +92.2% | 0.526 | +47.4% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 64 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 8233.9 | 736.2 | 1124.6 | 11.184 | +1018.4% | 7.322 | +632.2% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 64 concurrent durable 4 KiB writers | p99 µs (lower is better) | 11013.1 | 124182.9 | 162934.3 | 0.089 | +91.1% | 0.068 | +93.2% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |

## Anomalies, errors and integrity failures

- None: every measurement completed and every integrity check passed.

## W2: concurrent durable writers, 4 KiB (`concurrent`)

Wall time: 98.3 s.

**Method.**

- Writer counts [1, 2, 4, 8, 16, 32, 64]; every thread loops a durable 4 KiB write at queue depth 1 for the whole point.
- store-io: all threads call `AppendRegion::append_durable` on one append region of one store (a fresh store per point). Flush sharing is read from `Store::domain_stats()` deltas over the window: `flushes` issued, barriers that `led` a flush, barriers that `joined` one.
- raw: each thread writes its own contiguous range of one ready 256 MiB file and makes it durable itself with pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync (no sharing).
- fsys: all threads call `append` + `sync_through` on one shared journal (fresh per point), window off and default (500 µs group-commit window).
- An operation counts when it started and finished inside the measured window; rate = counted operations / window.

Results:

| threads | system | variant | durable ops/s | p50 µs | p99 µs | p99.9 µs | flushes issued | barriers led | barriers joined | writes per flush |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | store-io | AppendRegion::append_durable (shared region) | 390 | 2399.0 | 6381.1 | 19000.3 | 1186 | 1186 | 0 | 1.00 |
| 1 | raw | own write + own data flush (per thread range) | 393 | 2366.1 | 6263.3 | 18246.8 | n/a | n/a | n/a | n/a |
| 1 | fsys | journal, buffered, window off: shared journal append + sync_through | 187 | 5001.7 | 20534.1 | n/a | n/a | n/a | n/a | n/a |
| 1 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 166 | 5749.0 | 21297.2 | n/a | n/a | n/a | n/a | n/a |
| 2 | store-io | AppendRegion::append_durable (shared region) | 428 | 4521.8 | 19922.2 | 20271.8 | 1304 | 1304 | 1 | 1.00 |
| 2 | raw | own write + own data flush (per thread range) | 502 | 4290.2 | 18318.8 | 20291.5 | n/a | n/a | n/a | n/a |
| 2 | fsys | journal, buffered, window off: shared journal append + sync_through | 329 | 5428.9 | 21066.7 | n/a | n/a | n/a | n/a | n/a |
| 2 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 325 | 5873.0 | 21459.5 | n/a | n/a | n/a | n/a | n/a |
| 4 | store-io | AppendRegion::append_durable (shared region) | 851 | 4499.5 | 19834.8 | 21785.9 | 1282 | 1282 | 1276 | 1.99 |
| 4 | raw | own write + own data flush (per thread range) | 440 | 8471.2 | 24098.9 | 30493.3 | n/a | n/a | n/a | n/a |
| 4 | fsys | journal, buffered, window off: shared journal append + sync_through | 550 | 5293.4 | 20607.5 | 26173.1 | n/a | n/a | n/a | n/a |
| 4 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 663 | 5755.0 | 11695.6 | 21661.9 | n/a | n/a | n/a | n/a |
| 8 | store-io | AppendRegion::append_durable (shared region) | 828 | 7563.2 | 35107.2 | 75654.0 | 675 | 675 | 1846 | 3.72 |
| 8 | raw | own write + own data flush (per thread range) | 171 | 47013.2 | 94141.9 | n/a | n/a | n/a | n/a | n/a |
| 8 | fsys | journal, buffered, window off: shared journal append + sync_through | 823 | 10425.3 | 26401.2 | 30658.9 | n/a | n/a | n/a | n/a |
| 8 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 1130 | 6122.7 | 18285.7 | 22674.6 | n/a | n/a | n/a | n/a |
| 16 | store-io | AppendRegion::append_durable (shared region) | 2150 | 6909.3 | 18361.5 | 31222.0 | 948 | 948 | 5519 | 6.80 |
| 16 | raw | own write + own data flush (per thread range) | 419 | 35861.4 | 56998.1 | 58949.3 | n/a | n/a | n/a | n/a |
| 16 | fsys | journal, buffered, window off: shared journal append + sync_through | 1605 | 10409.3 | 26392.9 | 28144.9 | n/a | n/a | n/a | n/a |
| 16 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 1563 | 10606.6 | 26744.7 | 34915.3 | n/a | n/a | n/a | n/a |
| 32 | store-io | AppendRegion::append_durable (shared region) | 4474 | 7307.2 | 9266.0 | 24009.9 | 1161 | 1161 | 12293 | 11.56 |
| 32 | raw | own write + own data flush (per thread range) | 467 | 54611.1 | 119295.8 | 132100.2 | n/a | n/a | n/a | n/a |
| 32 | fsys | journal, buffered, window off: shared journal append + sync_through | 3047 | 10660.1 | 17631.5 | 28067.2 | n/a | n/a | n/a | n/a |
| 32 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 3094 | 10458.6 | 21038.0 | 26730.3 | n/a | n/a | n/a | n/a |
| 64 | store-io | AppendRegion::append_durable (shared region) | 8234 | 7773.6 | 11013.1 | 12364.2 | 1174 | 1174 | 23595 | 21.04 |
| 64 | raw | own write + own data flush (per thread range) | 736 | 88862.6 | 124182.9 | 125457.4 | n/a | n/a | n/a | n/a |
| 64 | fsys | journal, buffered, window off: shared journal append + sync_through | 899 | 42108.7 | 254082.3 | 290890.2 | n/a | n/a | n/a | n/a |
| 64 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 1125 | 38859.9 | 162934.3 | 176133.9 | n/a | n/a | n/a | n/a |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 1297 records read back and compared byte for byte; all 1297 checked for overlap and alignment | provisioned a 516 MiB append region in 0.37 s (1458 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1025 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 626 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 558 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 1429 records read back and compared byte for byte; all 1429 checked for overlap and alignment | provisioned a 516 MiB append region in 0.24 s (2211 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1026 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 1090 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 1092 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 2810 records read back and compared byte for byte; all 2810 checked for overlap and alignment | provisioned a 516 MiB append region in 0.25 s (2179 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1028 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 1853 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 2204 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 2847 records read back and compared byte for byte; all 2847 checked for overlap and alignment | provisioned a 516 MiB append region in 0.31 s (1767 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 588 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.016 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 2590 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 3907 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 7136 records read back and compared byte for byte; all 7136 checked for overlap and alignment | provisioned a 516 MiB append region in 0.38 s (1436 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1040 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.013 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 5320 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 5201 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 14758 records read back and compared byte for byte; all 14758 checked for overlap and alignment | provisioned a 516 MiB append region in 0.39 s (1404 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.002 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1056 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.022 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 10120 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.004 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 10282 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 27374 records read back and compared byte for byte; all 27374 checked for overlap and alignment | provisioned a 516 MiB append region in 0.35 s (1544 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1088 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.027 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 4532 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.024 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 3636 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.019 (straddling ops make this slightly above 1) |

**Findings.**

- store-io scaling: 390 durable ops/s at 1 writer, 8234 at 64 writers (21.04 writes per flush) [derived from the table].

