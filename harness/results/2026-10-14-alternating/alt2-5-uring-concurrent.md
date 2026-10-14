# store-io performance harness: linux / msft-virtual-disk (2026-10-14)

> **OVERRIDE IN EFFECT.** `Store::create` with default trust was **refused** on this device:
> `device durability is unverified: running under a hypervisor without a certificate for this volume type; missing: device identify data (needs a readable device node); SMART / health log (needs privilege on Linux); volatile write cache presence; volatile write cache enabled state; device atomic-write fields; [Unverified { missing: {Identify, Smart, CachePresence, CacheEnabled, AtomicFields}, reasons: {Hypervisor} }]`
>
> Every store in this run was created with the documented labelled override (`Trust { override_refusal: true, .. }`): the class stays as decided, every receipt is labelled `overridden`, and the most conservative primitive (write + device flush) is used. Nothing else was weakened. The numbers below are therefore **not** gate numbers for a certified durability class; they measure software cost on this stack.

## Run

| item | value |
|---|---|
| date (UTC) | 2026-10-14T08:50:32Z |
| command | concurrent |
| store-io commit | e4f90ecec8fe39c18eccbe385c6a10fe51cfce2c (crates/ modified) |
| OS | Ubuntu 24.04.4 LTS |
| kernel / build | Linux version 6.6.87.2-microsoft-standard-WSL2 (root@439a258ad544) (gcc (GCC) 11.2.0, GNU ld (GNU Binutils) 2.37) #1 SMP PREEMPT_DYNAMIC Thu Jun  5 18:30:46 UTC 2025 |
| CPU | AMD Ryzen 9 9950X3D 16-Core Processor |
| logical CPUs | 32 |
| filesystem | ext4 on / (device /dev/sdd 8:48, mount options rw,relatime, super options rw,discard,errors=remount-ro,data=ordered); store-io classified it Ext4 |
| directory under test | /home/james/store-io-harness-2524 |
| backend | store-io-posix (PosixPlatform, O_DIRECT; tier in the report's io line) |
| durability class | unverified (receipt label overridden(running under a hypervisor without a certificate for this volume type), durable open Overridden (Trust::override_refusal), block size 4096 B) |
| time per point | 2.0 s measured after 0.2 s warm-up (time-boxed; a point may end early at region capacity, noted where it does) |
| fsys | fsys 1.1.3 (crates.io), Handle method Sync, durability primitive fsync |
| raw primitives | pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync; preadv2 on an O_DIRECT descriptor (one call per request) |
| harness build | release, lto=fat, codegen-units=1 (same as store-io's release profile) |
| total wall time | 66.3 s |

## Device report (`Store::report()`)

```text
volume           98c9725e-0de6-8819-0c1f-4ad6c3172405
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
| concurrent | 1 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 386.5 | 406.0 | 174.5 | 0.952 | -4.8% | 2.215 | +121.5% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 1 concurrent durable 4 KiB writers | p99 µs (lower is better) | 4597.3 | 4539.8 | 12268.9 | 1.013 | -1.3% | 0.375 | +62.5% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 2 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 394.5 | 436.0 | 331.4 | 0.905 | -9.5% | 1.191 | +19.1% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 2 concurrent durable 4 KiB writers | p99 µs (lower is better) | 19526.2 | 19322.2 | 9174.0 | 1.011 | -1.1% | 2.128 | -112.8% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 4 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 867.5 | 465.0 | 659.3 | 1.866 | +86.6% | 1.316 | +31.6% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 4 concurrent durable 4 KiB writers | p99 µs (lower is better) | 19119.8 | 23085.4 | 9193.9 | 0.828 | +17.2% | 2.080 | -108.0% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 8 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 1449.9 | 420.1 | 1319.9 | 3.451 | +245.1% | 1.098 | +9.8% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 8 concurrent durable 4 KiB writers | p99 µs (lower is better) | 9784.9 | 36494.5 | 10900.3 | 0.268 | +73.2% | 0.898 | +10.2% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 16 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 2638.9 | 404.8 | 1547.4 | 6.519 | +551.9% | 1.705 | +70.5% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 16 concurrent durable 4 KiB writers | p99 µs (lower is better) | 10421.8 | 55773.6 | 21999.5 | 0.187 | +81.3% | 0.474 | +52.6% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 32 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 4508.6 | 496.2 | 3209.4 | 9.087 | +808.7% | 1.405 | +40.5% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 32 concurrent durable 4 KiB writers | p99 µs (lower is better) | 12331.0 | 102166.7 | 15020.0 | 0.121 | +87.9% | 0.821 | +17.9% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 64 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 8672.6 | 677.7 | 6148.2 | 12.797 | +1179.7% | 1.411 | +41.1% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 64 concurrent durable 4 KiB writers | p99 µs (lower is better) | 11102.7 | 142752.9 | 24577.5 | 0.078 | +92.2% | 0.452 | +54.8% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |

## Anomalies, errors and integrity failures

- None: every measurement completed and every integrity check passed.

## W2: concurrent durable writers, 4 KiB (`concurrent`)

Wall time: 65.7 s.

**Method.**

- Writer counts [1, 2, 4, 8, 16, 32, 64]; every thread loops a durable 4 KiB write at queue depth 1 for the whole point.
- store-io: all threads call `AppendRegion::append_durable` on one append region of one store (a fresh store per point). Flush sharing is read from `Store::domain_stats()` deltas over the window: `flushes` issued, barriers that `led` a flush, barriers that `joined` one.
- raw: each thread writes its own contiguous range of one ready 256 MiB file and makes it durable itself with pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync (no sharing).
- fsys: all threads call `append` + `sync_through` on one shared journal (fresh per point), window off and default (500 µs group-commit window).
- An operation counts when it started and finished inside the measured window; rate = counted operations / window.

Results:

| threads | system | variant | durable ops/s | p50 µs | p99 µs | p99.9 µs | flushes issued | barriers led | barriers joined | writes per flush |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | store-io | AppendRegion::append_durable (shared region) | 386 | 2460.3 | 4597.3 | n/a | 838 | 838 | 0 | 1.00 |
| 1 | raw | own write + own data flush (per thread range) | 406 | 2319.9 | 4539.8 | n/a | n/a | n/a | n/a | n/a |
| 1 | fsys | journal, buffered, window off: shared journal append + sync_through | 174 | 5369.6 | 12268.9 | n/a | n/a | n/a | n/a | n/a |
| 1 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 144 | 6347.8 | 22510.5 | n/a | n/a | n/a | n/a | n/a |
| 2 | store-io | AppendRegion::append_durable (shared region) | 395 | 4902.5 | 19526.2 | n/a | 829 | 829 | 0 | 1.00 |
| 2 | raw | own write + own data flush (per thread range) | 436 | 4363.5 | 19322.2 | n/a | n/a | n/a | n/a | n/a |
| 2 | fsys | journal, buffered, window off: shared journal append + sync_through | 327 | 5270.6 | 21467.2 | n/a | n/a | n/a | n/a | n/a |
| 2 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 331 | 5815.4 | 9174.0 | n/a | n/a | n/a | n/a | n/a |
| 4 | store-io | AppendRegion::append_durable (shared region) | 867 | 4333.9 | 19119.8 | 19508.0 | 877 | 877 | 862 | 1.98 |
| 4 | raw | own write + own data flush (per thread range) | 465 | 8066.2 | 23085.4 | n/a | n/a | n/a | n/a | n/a |
| 4 | fsys | journal, buffered, window off: shared journal append + sync_through | 545 | 5400.0 | 14207.0 | 26793.7 | n/a | n/a | n/a | n/a |
| 4 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 659 | 5825.4 | 9193.9 | 29188.8 | n/a | n/a | n/a | n/a |
| 8 | store-io | AppendRegion::append_durable (shared region) | 1450 | 4787.9 | 9784.9 | 21604.5 | 825 | 825 | 2083 | 3.52 |
| 8 | raw | own write + own data flush (per thread range) | 420 | 18040.7 | 36494.5 | n/a | n/a | n/a | n/a | n/a |
| 8 | fsys | journal, buffered, window off: shared journal append + sync_through | 852 | 10654.6 | 17241.3 | 27831.3 | n/a | n/a | n/a | n/a |
| 8 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 1320 | 5877.0 | 10900.3 | 23419.7 | n/a | n/a | n/a | n/a |
| 16 | store-io | AppendRegion::append_durable (shared region) | 2639 | 6390.9 | 10421.8 | 24596.1 | 855 | 855 | 4439 | 6.17 |
| 16 | raw | own write + own data flush (per thread range) | 405 | 36892.8 | 55773.6 | n/a | n/a | n/a | n/a | n/a |
| 16 | fsys | journal, buffered, window off: shared journal append + sync_through | 1473 | 11365.0 | 21999.5 | 28082.6 | n/a | n/a | n/a | n/a |
| 16 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 1547 | 10528.7 | 25826.4 | 27623.3 | n/a | n/a | n/a | n/a |
| 32 | store-io | AppendRegion::append_durable (shared region) | 4509 | 7108.8 | 12331.0 | 22504.8 | 784 | 784 | 8266 | 11.50 |
| 32 | raw | own write + own data flush (per thread range) | 496 | 57067.6 | 102166.7 | 108117.6 | n/a | n/a | n/a | n/a |
| 32 | fsys | journal, buffered, window off: shared journal append + sync_through | 2980 | 10740.2 | 15020.0 | 18498.9 | n/a | n/a | n/a | n/a |
| 32 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 3209 | 9868.9 | 16433.7 | 27744.6 | n/a | n/a | n/a | n/a |
| 64 | store-io | AppendRegion::append_durable (shared region) | 8673 | 7246.7 | 11102.7 | 12809.6 | 828 | 828 | 16585 | 20.95 |
| 64 | raw | own write + own data flush (per thread range) | 678 | 91017.4 | 142752.9 | 144950.0 | n/a | n/a | n/a | n/a |
| 64 | fsys | journal, buffered, window off: shared journal append + sync_through | 5255 | 11120.6 | 29539.2 | 35289.6 | n/a | n/a | n/a | n/a |
| 64 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 6148 | 10099.5 | 24577.5 | 27480.4 | n/a | n/a | n/a | n/a |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append_durable (shared region) | passed: 918 of 918 records read back and compared byte for byte; all 918 checked for overlap and alignment | provisioned a 344 MiB append region in 0.13 s (2698 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 889 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 388 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 309 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 914 of 914 records read back and compared byte for byte; all 914 checked for overlap and alignment | provisioned a 344 MiB append region in 0.14 s (2564 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 964 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 736 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 736 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 1907 records read back and compared byte for byte; all 1907 checked for overlap and alignment | provisioned a 344 MiB append region in 0.14 s (2568 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1027 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.004 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 1292 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 1464 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 3198 records read back and compared byte for byte; all 3198 checked for overlap and alignment | provisioned a 344 MiB append region in 0.18 s (1991 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 951 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.009 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 1899 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.005 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 2920 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 5798 records read back and compared byte for byte; all 5798 checked for overlap and alignment | provisioned a 344 MiB append region in 0.17 s (2130 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 932 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.020 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 3274 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.005 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 3436 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.005 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 10046 records read back and compared byte for byte; all 10046 checked for overlap and alignment | provisioned a 344 MiB append region in 0.13 s (2818 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1039 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.031 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 6600 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.005 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 7097 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.005 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 19216 records read back and compared byte for byte; all 19216 checked for overlap and alignment | provisioned a 344 MiB append region in 0.13 s (2731 MB/s: fill, flush, verify); OS write calls in window / counted ops = 0.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1088 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.046 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 11816 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.006 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 13684 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.005 (straddling ops make this slightly above 1) |

**Findings.**

- store-io scaling: 386 durable ops/s at 1 writer, 8673 at 64 writers (20.95 writes per flush) [derived from the table].

