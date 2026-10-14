# store-io performance harness: linux / msft-virtual-disk (2026-10-14)

> **OVERRIDE IN EFFECT.** `Store::create` with default trust was **refused** on this device:
> `device durability is unverified: running under a hypervisor without a certificate for this volume type; missing: device identify data (needs a readable device node); SMART / health log (needs privilege on Linux); volatile write cache presence; volatile write cache enabled state; device atomic-write fields; [Unverified { missing: {Identify, Smart, CachePresence, CacheEnabled, AtomicFields}, reasons: {Hypervisor} }]`
>
> Every store in this run was created with the documented labelled override (`Trust { override_refusal: true, .. }`): the class stays as decided, every receipt is labelled `overridden`, and the most conservative primitive (write + device flush) is used. Nothing else was weakened. The numbers below are therefore **not** gate numbers for a certified durability class; they measure software cost on this stack.

## Run

| item | value |
|---|---|
| date (UTC) | 2026-10-14T08:36:22Z |
| command | submitter |
| store-io commit | e4f90ecec8fe39c18eccbe385c6a10fe51cfce2c (crates/ modified) |
| OS | Ubuntu 24.04.4 LTS |
| kernel / build | Linux version 6.6.87.2-microsoft-standard-WSL2 (root@439a258ad544) (gcc (GCC) 11.2.0, GNU ld (GNU Binutils) 2.37) #1 SMP PREEMPT_DYNAMIC Thu Jun  5 18:30:46 UTC 2025 |
| CPU | AMD Ryzen 9 9950X3D 16-Core Processor |
| logical CPUs | 32 |
| filesystem | ext4 on / (device /dev/sdd 8:48, mount options rw,relatime, super options rw,discard,errors=remount-ro,data=ordered); store-io classified it Ext4 |
| directory under test | /home/james/store-io-harness-51510 |
| backend | store-io-posix (PosixPlatform, O_DIRECT; tier in the report's io line) |
| durability class | unverified (receipt label overridden(running under a hypervisor without a certificate for this volume type), durable open Overridden (Trust::override_refusal), block size 4096 B) |
| time per point | 3.0 s measured after 0.3 s warm-up (time-boxed; a point may end early at region capacity, noted where it does) |
| fsys | fsys 1.1.3 (crates.io), Handle method Sync, durability primitive fsync |
| raw primitives | pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync; preadv2 on an O_DIRECT descriptor (one call per request) |
| harness build | release, lto=fat, codegen-units=1 (same as store-io's release profile) |
| total wall time | 174.0 s |

## Device report (`Store::report()`)

```text
volume           428c2ceb-90c1-c46d-52be-e8ab4bff477e
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
| submitter | randwrite 4 KiB, 1 in flight | ops/s (higher is better) | 8291.4 | 6765.7 | n/a | 1.225 | +22.5% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randwrite 4 KiB, 1 in flight | p99 µs (lower is better) | 243.3 | 214.0 | n/a | 1.137 | -13.7% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randwrite 4 KiB, 8 in flight | ops/s (higher is better) | 38723.9 | 39834.2 | n/a | 0.972 | -2.8% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randwrite 4 KiB, 8 in flight | p99 µs (lower is better) | 3668.3 | 4292.6 | n/a | 0.855 | +14.5% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randwrite 4 KiB, 32 in flight | ops/s (higher is better) | 91451.1 | 91748.1 | n/a | 0.997 | -0.3% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randwrite 4 KiB, 32 in flight | p99 µs (lower is better) | 560.8 | 528.4 | n/a | 1.061 | -6.1% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randwrite 4 KiB, 128 in flight | ops/s (higher is better) | 173483.7 | 172133.0 | n/a | 1.008 | +0.8% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randwrite 4 KiB, 128 in flight | p99 µs (lower is better) | 1267.5 | 1417.2 | n/a | 0.894 | +10.6% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randread 4 KiB, 1 in flight | ops/s (higher is better) | 7518.8 | 5228.6 | n/a | 1.438 | +43.8% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randread 4 KiB, 1 in flight | p99 µs (lower is better) | 217.7 | 272.4 | n/a | 0.799 | +20.1% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randread 4 KiB, 8 in flight | ops/s (higher is better) | 69987.8 | 40245.3 | n/a | 1.739 | +73.9% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randread 4 KiB, 8 in flight | p99 µs (lower is better) | 222.8 | 301.1 | n/a | 0.740 | +26.0% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randread 4 KiB, 32 in flight | ops/s (higher is better) | 150761.4 | 137207.6 | n/a | 1.099 | +9.9% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randread 4 KiB, 32 in flight | p99 µs (lower is better) | 334.8 | 469.0 | n/a | 0.714 | +28.6% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randread 4 KiB, 128 in flight | ops/s (higher is better) | 220131.5 | 210611.5 | n/a | 1.045 | +4.5% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |
| submitter | randread 4 KiB, 128 in flight | p99 µs (lower is better) | 1008.3 | 864.3 | n/a | 1.167 | -16.7% | n/a | n/a | Submitter vs fio io_uring (medians of 3) |

## Anomalies, errors and integrity failures

- None: every measurement completed and every integrity check passed.

## W6: N in flight from one thread, random 4 KiB (Submitter vs fio) (`submitter`)

Wall time: 171.3 s.

**Method.**

- Queue depth N in [1, 8, 32, 128]; uniformly random 4 KiB-aligned offsets over 256 MiB, with repeats (fio: --norandommap); one thread keeps N operations in flight, resubmitting as each completes; writes are plain (no per-write durability), reads read back and are checked for length.
- store-io: `Store::submitter(depth N)`, `write(region, pos, buf, Plain)` / `read_page`, `wait(1)`; buffers from the submitter's pool (`take_buf`); the region is written in full by provisioning.
- raw: fio --ioengine=io_uring --direct=1 --bs=4k --iodepth=N --numjobs=1 --norandommap on one 256 MiB file written in full first (1 MiB direct writes, then fsync) and reused by every point; time-based, the same warm-up as ramp time.
- Latency: per operation, submit call to completion observed (store-io); fio's total latency (lat: submission and completion).
- Each point runs 3 times, alternating which system goes first; the table shows the repetition with the median ops/s, and the notes give every repetition.

Results (median of the repetitions):

| pattern | N | system | variant | ops/s | ops/s min..max | p50 µs | p99 µs | p99.9 µs |
|---|---|---|---|---|---|---|---|---|
| randwrite | 1 | store-io | Submitter randwrite QD1 | 8291 | 6449..8646 | 104.9 | 243.3 | 1108.3 |
| randwrite | 1 | raw | fio io_uring randwrite QD1 | 6766 | 6535..8399 | 146.4 | 214.0 | 1990.7 |
| randwrite | 8 | store-io | Submitter randwrite QD8 | 38724 | 33706..57580 | 117.2 | 3668.3 | 8174.0 |
| randwrite | 8 | raw | fio io_uring randwrite QD8 | 39834 | 16420..66154 | 120.3 | 4292.6 | 7897.1 |
| randwrite | 32 | store-io | Submitter randwrite QD32 | 91451 | 90027..91507 | 326.5 | 560.8 | 2279.6 |
| randwrite | 32 | raw | fio io_uring randwrite QD32 | 91748 | 90673..102127 | 333.8 | 528.4 | 1187.8 |
| randwrite | 128 | store-io | Submitter randwrite QD128 | 173484 | 134121..205281 | 639.5 | 1267.5 | 2822.4 |
| randwrite | 128 | raw | fio io_uring randwrite QD128 | 172133 | 145909..182775 | 667.6 | 1417.2 | 3031.0 |
| randread | 1 | store-io | Submitter randread QD1 | 7519 | 7394..7742 | 126.4 | 217.7 | 362.8 |
| randread | 1 | raw | fio io_uring randread QD1 | 5229 | 5190..5339 | 185.3 | 272.4 | 456.7 |
| randread | 8 | store-io | Submitter randread QD8 | 69988 | 69436..73497 | 101.0 | 222.8 | 2016.9 |
| randread | 8 | raw | fio io_uring randread QD8 | 40245 | 38110..40643 | 193.5 | 301.1 | 585.7 |
| randread | 32 | store-io | Submitter randread QD32 | 150761 | 134712..152717 | 195.2 | 334.8 | 2268.6 |
| randread | 32 | raw | fio io_uring randread QD32 | 137208 | 132566..141423 | 216.1 | 469.0 | 2342.9 |
| randread | 128 | store-io | Submitter randread QD128 | 220132 | 219048..225463 | 534.6 | 1008.3 | 2728.5 |
| randread | 128 | raw | fio io_uring randread QD128 | 210611 | 191221..210765 | 593.9 | 864.3 | 2670.6 |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | Submitter randwrite QD1 | not checked: throughput point; read lengths and tags are checked | io: io_uring (chosen); io_uring fallbacks 0 of 21288 operations; io: io_uring (chosen); io_uring fallbacks 0 of 27155 operations; io: io_uring (chosen); io_uring fallbacks 0 of 28511 operations; repetitions (ops/s, p99 µs): 6449/197, 8291/243, 8646/304 |
| raw | fio io_uring randwrite QD1 | not checked: fio baseline: fio's own checks | repetitions (ops/s, p99 µs): 6535/191, 6766/214, 8399/326 |
| store-io | Submitter randwrite QD8 | not checked: throughput point; read lengths and tags are checked | io: io_uring (chosen); io_uring fallbacks 0 of 111034 operations; io: io_uring (chosen); io_uring fallbacks 0 of 128345 operations; io: io_uring (chosen); io_uring fallbacks 0 of 184951 operations; repetitions (ops/s, p99 µs): 33706/3977, 38724/3668, 57580/299 |
| raw | fio io_uring randwrite QD8 | not checked: fio baseline: fio's own checks | repetitions (ops/s, p99 µs): 16420/6324, 39834/4293, 66154/251 |
| store-io | Submitter randwrite QD32 | not checked: throughput point; read lengths and tags are checked | io: io_uring (chosen); io_uring fallbacks 0 of 304996 operations; io: io_uring (chosen); io_uring fallbacks 0 of 297471 operations; io: io_uring (chosen); io_uring fallbacks 0 of 302187 operations; repetitions (ops/s, p99 µs): 90027/558, 91451/561, 91507/529 |
| raw | fio io_uring randwrite QD32 | not checked: fio baseline: fio's own checks | repetitions (ops/s, p99 µs): 90673/545, 91748/528, 102127/528 |
| store-io | Submitter randwrite QD128 | not checked: throughput point; read lengths and tags are checked | io: io_uring (chosen); io_uring fallbacks 0 of 447243 operations; io: io_uring (chosen); io_uring fallbacks 0 of 674939 operations; io: io_uring (chosen); io_uring fallbacks 0 of 582638 operations; repetitions (ops/s, p99 µs): 134121/5296, 173484/1268, 205281/826 |
| raw | fio io_uring randwrite QD128 | not checked: fio baseline: fio's own checks | repetitions (ops/s, p99 µs): 145909/2867, 172133/1417, 182775/1221 |
| store-io | Submitter randread QD1 | not checked: throughput point; read lengths and tags are checked | io: io_uring (chosen); io_uring fallbacks 0 of 24539 operations; io: io_uring (chosen); io_uring fallbacks 0 of 25449 operations; io: io_uring (chosen); io_uring fallbacks 0 of 24435 operations; repetitions (ops/s, p99 µs): 7394/213, 7519/218, 7742/207 |
| raw | fio io_uring randread QD1 | not checked: fio baseline: fio's own checks | repetitions (ops/s, p99 µs): 5190/272, 5229/272, 5339/268 |
| store-io | Submitter randread QD8 | not checked: throughput point; read lengths and tags are checked | io: io_uring (chosen); io_uring fallbacks 0 of 239268 operations; io: io_uring (chosen); io_uring fallbacks 0 of 229330 operations; io: io_uring (chosen); io_uring fallbacks 0 of 231140 operations; repetitions (ops/s, p99 µs): 69436/219, 69988/223, 73497/211 |
| raw | fio io_uring randread QD8 | not checked: fio baseline: fio's own checks | repetitions (ops/s, p99 µs): 38110/322, 40245/301, 40643/289 |
| store-io | Submitter randread QD32 | not checked: throughput point; read lengths and tags are checked | io: io_uring (chosen); io_uring fallbacks 0 of 490653 operations; io: io_uring (chosen); io_uring fallbacks 0 of 500019 operations; io: io_uring (chosen); io_uring fallbacks 0 of 448342 operations; repetitions (ops/s, p99 µs): 134712/408, 150761/335, 152717/319 |
| raw | fio io_uring randread QD32 | not checked: fio baseline: fio's own checks | repetitions (ops/s, p99 µs): 132566/469, 137208/469, 141423/383 |
| store-io | Submitter randread QD128 | not checked: throughput point; read lengths and tags are checked | io: io_uring (chosen); io_uring fallbacks 0 of 722246 operations; io: io_uring (chosen); io_uring fallbacks 0 of 746583 operations; io: io_uring (chosen); io_uring fallbacks 0 of 730686 operations; repetitions (ops/s, p99 µs): 219048/714, 220132/1008, 225463/858 |
| raw | fio io_uring randread QD128 | not checked: fio baseline: fio's own checks | repetitions (ops/s, p99 µs): 191221/2441, 210611/864, 210765/930 |

