# store-io performance harness: linux / msft-virtual-disk (2026-10-14)

> **OVERRIDE IN EFFECT.** `Store::create` with default trust was **refused** on this device:
> `device durability is unverified: running under a hypervisor without a certificate for this volume type; missing: device identify data (needs a readable device node); SMART / health log (needs privilege on Linux); volatile write cache presence; volatile write cache enabled state; device atomic-write fields; [Unverified { missing: {Identify, Smart, CachePresence, CacheEnabled, AtomicFields}, reasons: {Hypervisor} }]`
>
> Every store in this run was created with the documented labelled override (`Trust { override_refusal: true, .. }`): the class stays as decided, every receipt is labelled `overridden`, and the most conservative primitive (write + device flush) is used. Nothing else was weakened. The numbers below are therefore **not** gate numbers for a certified durability class; they measure software cost on this stack.

## Run

| item | value |
|---|---|
| date (UTC) | 2026-10-14T08:39:08Z |
| command | submitter |
| store-io commit | e4f90ecec8fe39c18eccbe385c6a10fe51cfce2c (crates/ modified) |
| OS | Ubuntu 24.04.4 LTS |
| kernel / build | Linux version 6.6.87.2-microsoft-standard-WSL2 (root@439a258ad544) (gcc (GCC) 11.2.0, GNU ld (GNU Binutils) 2.37) #1 SMP PREEMPT_DYNAMIC Thu Jun  5 18:30:46 UTC 2025 |
| CPU | AMD Ryzen 9 9950X3D 16-Core Processor |
| logical CPUs | 32 |
| filesystem | ext4 on / (device /dev/sdd 8:48, mount options rw,relatime, super options rw,discard,errors=remount-ro,data=ordered); store-io classified it Ext4 |
| directory under test | /home/james/store-io-harness-52397 |
| backend | store-io-posix (PosixPlatform, O_DIRECT; tier in the report's io line) |
| durability class | unverified (receipt label overridden(running under a hypervisor without a certificate for this volume type), durable open Overridden (Trust::override_refusal), block size 4096 B) |
| time per point | 3.0 s measured after 0.3 s warm-up (time-boxed; a point may end early at region capacity, noted where it does) |
| fsys | fsys 1.1.3 (crates.io), Handle method Sync, durability primitive fsync |
| raw primitives | pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync; preadv2 on an O_DIRECT descriptor (one call per request) |
| harness build | release, lto=fat, codegen-units=1 (same as store-io's release profile) |
| total wall time | 171.0 s |

## Device report (`Store::report()`)

```text
volume           79488136-c2ea-7683-62fc-75ff49c92c9e
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
| submitter | randwrite 4 KiB, 1 in flight | ops/s (higher is better) | 5324.6 | 7481.5 | n/a | 0.712 | -28.8% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randwrite 4 KiB, 1 in flight | p99 µs (lower is better) | 278.8 | 199.7 | n/a | 1.396 | -39.6% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randwrite 4 KiB, 8 in flight | ops/s (higher is better) | 39652.2 | 69828.1 | n/a | 0.568 | -43.2% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randwrite 4 KiB, 8 in flight | p99 µs (lower is better) | 290.0 | 236.5 | n/a | 1.226 | -22.6% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randwrite 4 KiB, 32 in flight | ops/s (higher is better) | 58699.3 | 136485.2 | n/a | 0.430 | -57.0% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randwrite 4 KiB, 32 in flight | p99 µs (lower is better) | 623.7 | 383.0 | n/a | 1.628 | -62.8% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randwrite 4 KiB, 128 in flight | ops/s (higher is better) | 125739.5 | 170956.7 | n/a | 0.736 | -26.4% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randwrite 4 KiB, 128 in flight | p99 µs (lower is better) | 2194.9 | 1351.7 | n/a | 1.624 | -62.4% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randread 4 KiB, 1 in flight | ops/s (higher is better) | 5053.3 | 5355.5 | n/a | 0.944 | -5.6% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randread 4 KiB, 1 in flight | p99 µs (lower is better) | 289.5 | 272.4 | n/a | 1.063 | -6.3% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randread 4 KiB, 8 in flight | ops/s (higher is better) | 39911.1 | 39943.4 | n/a | 0.999 | -0.1% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randread 4 KiB, 8 in flight | p99 µs (lower is better) | 282.7 | 292.9 | n/a | 0.965 | +3.5% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randread 4 KiB, 32 in flight | ops/s (higher is better) | 57661.7 | 143383.2 | n/a | 0.402 | -59.8% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randread 4 KiB, 32 in flight | p99 µs (lower is better) | 652.8 | 391.2 | n/a | 1.669 | -66.9% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randread 4 KiB, 128 in flight | ops/s (higher is better) | 124899.3 | 204061.6 | n/a | 0.612 | -38.8% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randread 4 KiB, 128 in flight | p99 µs (lower is better) | 2220.9 | 954.4 | n/a | 2.327 | -132.7% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |

## Anomalies, errors and integrity failures

- None: every measurement completed and every integrity check passed.

## W6: N in flight from one thread, random 4 KiB (Submitter vs fio) (`submitter`)

Wall time: 170.5 s.

**Method.**

- Queue depth N in [1, 8, 32, 128]; uniformly random 4 KiB-aligned offsets over 256 MiB, with repeats (fio: --norandommap); one thread keeps N operations in flight, resubmitting as each completes; writes are plain (no per-write durability), reads read back and are checked for length.
- store-io: `Store::submitter(depth N)`, `write(region, pos, buf, Plain)` / `read_page`, `wait(1)`; buffers from the submitter's pool (`take_buf`); the region is written in full by provisioning.
- raw: fio --ioengine=io_uring --direct=1 --bs=4k --iodepth=N --numjobs=1 --norandommap on one 256 MiB file written in full first (1 MiB direct writes, then fsync) and reused by every point; time-based, the same warm-up as ramp time.
- Latency: per operation, submit call to completion observed (store-io); fio's total latency (lat: submission and completion).
- Each point runs 3 times, alternating which system goes first; the table shows the repetition with the median ops/s, and the notes give every repetition.

Results (median of the repetitions):

| pattern | N | system | variant | ops/s | ops/s min..max | p50 µs | p99 µs | p99.9 µs |
|---|---|---|---|---|---|---|---|---|
| randwrite | 1 | store-io | Submitter randwrite QD1 | 5325 | 5133..6188 | 177.6 | 278.8 | 3364.9 |
| randwrite | 1 | raw | fio io_uring randwrite QD1 | 7482 | 7349..7494 | 126.5 | 199.7 | 292.9 |
| randwrite | 8 | store-io | Submitter randwrite QD8 | 39652 | 39460..40872 | 178.9 | 290.0 | 2093.6 |
| randwrite | 8 | raw | fio io_uring randwrite QD8 | 69828 | 67720..70689 | 113.2 | 236.5 | 428.0 |
| randwrite | 32 | store-io | Submitter randwrite QD32 | 58699 | 55289..58889 | 351.3 | 623.7 | 2003.8 |
| randwrite | 32 | raw | fio io_uring randwrite QD32 | 136485 | 88572..144738 | 228.4 | 383.0 | 1876.0 |
| randwrite | 128 | store-io | Submitter randwrite QD128 | 125739 | 123452..126397 | 793.1 | 2194.9 | 4869.1 |
| randwrite | 128 | raw | fio io_uring randwrite QD128 | 170957 | 170956..177581 | 651.3 | 1351.7 | 4620.3 |
| randread | 1 | store-io | Submitter randread QD1 | 5053 | 4916..5225 | 190.1 | 289.5 | 538.2 |
| randread | 1 | raw | fio io_uring randread QD1 | 5356 | 5255..5381 | 177.2 | 272.4 | 2072.6 |
| randread | 8 | store-io | Submitter randread QD8 | 39911 | 39474..40026 | 177.5 | 282.7 | 2033.8 |
| randread | 8 | raw | fio io_uring randread QD8 | 39943 | 39133..40078 | 195.6 | 292.9 | 2146.3 |
| randread | 32 | store-io | Submitter randread QD32 | 57662 | 57257..58106 | 356.1 | 652.8 | 2356.5 |
| randread | 32 | raw | fio io_uring randread QD32 | 143383 | 139506..148681 | 212.0 | 391.2 | 2277.4 |
| randread | 128 | store-io | Submitter randread QD128 | 124899 | 118555..134738 | 786.4 | 2220.9 | 2996.5 |
| randread | 128 | raw | fio io_uring randread QD128 | 204062 | 198560..209849 | 610.3 | 954.4 | 2768.9 |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | Submitter randwrite QD1 | not checked: throughput point; read lengths and tags are checked | io: synchronous (chosen); repetitions (ops/s, p99 µs): 5133/280, 5325/279, 6188/247 |
| raw | fio io_uring randwrite QD1 | not checked: fio baseline: fio's own checks | repetitions (ops/s, p99 µs): 7349/204, 7482/200, 7494/218 |
| store-io | Submitter randwrite QD8 | not checked: throughput point; read lengths and tags are checked | io: synchronous (chosen); repetitions (ops/s, p99 µs): 39460/284, 39652/290, 40872/267 |
| raw | fio io_uring randwrite QD8 | not checked: fio baseline: fio's own checks | repetitions (ops/s, p99 µs): 67720/241, 69828/237, 70689/232 |
| store-io | Submitter randwrite QD32 | not checked: throughput point; read lengths and tags are checked | io: synchronous (chosen); repetitions (ops/s, p99 µs): 55289/690, 58699/624, 58889/624 |
| raw | fio io_uring randwrite QD32 | not checked: fio baseline: fio's own checks | repetitions (ops/s, p99 µs): 88572/602, 136485/383, 144738/387 |
| store-io | Submitter randwrite QD128 | not checked: throughput point; read lengths and tags are checked | io: synchronous (chosen); repetitions (ops/s, p99 µs): 123452/2560, 125739/2195, 126397/2173 |
| raw | fio io_uring randwrite QD128 | not checked: fio baseline: fio's own checks | repetitions (ops/s, p99 µs): 170956/1647, 170957/1352, 177581/1253 |
| store-io | Submitter randread QD1 | not checked: throughput point; read lengths and tags are checked | io: synchronous (chosen); repetitions (ops/s, p99 µs): 4916/305, 5053/290, 5225/308 |
| raw | fio io_uring randread QD1 | not checked: fio baseline: fio's own checks | repetitions (ops/s, p99 µs): 5255/261, 5356/272, 5381/259 |
| store-io | Submitter randread QD8 | not checked: throughput point; read lengths and tags are checked | io: synchronous (chosen); repetitions (ops/s, p99 µs): 39474/290, 39911/283, 40026/286 |
| raw | fio io_uring randread QD8 | not checked: fio baseline: fio's own checks | repetitions (ops/s, p99 µs): 39133/297, 39943/293, 40078/289 |
| store-io | Submitter randread QD32 | not checked: throughput point; read lengths and tags are checked | io: synchronous (chosen); repetitions (ops/s, p99 µs): 57257/671, 57662/653, 58106/647 |
| raw | fio io_uring randread QD32 | not checked: fio baseline: fio's own checks | repetitions (ops/s, p99 µs): 139506/399, 143383/391, 148681/391 |
| store-io | Submitter randread QD128 | not checked: throughput point; read lengths and tags are checked | io: synchronous (chosen); repetitions (ops/s, p99 µs): 118555/2402, 124899/2221, 134738/2123 |
| raw | fio io_uring randread QD128 | not checked: fio baseline: fio's own checks | repetitions (ops/s, p99 µs): 198560/2277, 204062/954, 209849/848 |

