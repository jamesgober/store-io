# store-io performance harness: linux / msft-virtual-disk (2026-10-14)

> **OVERRIDE IN EFFECT.** `Store::create` with default trust was **refused** on this device:
> `device durability is unverified: running under a hypervisor without a certificate for this volume type; missing: device identify data (needs a readable device node); SMART / health log (needs privilege on Linux); volatile write cache presence; volatile write cache enabled state; device atomic-write fields; [Unverified { missing: {Identify, Smart, CachePresence, CacheEnabled, AtomicFields}, reasons: {Hypervisor} }]`
>
> Every store in this run was created with the documented labelled override (`Trust { override_refusal: true, .. }`): the class stays as decided, every receipt is labelled `overridden`, and the most conservative primitive (write + device flush) is used. Nothing else was weakened. The numbers below are therefore **not** gate numbers for a certified durability class; they measure software cost on this stack.

## Run

| item | value |
|---|---|
| date (UTC) | 2026-10-14T08:01:42Z |
| command | sequential |
| store-io commit | e4f90ecec8fe39c18eccbe385c6a10fe51cfce2c (crates/ modified) |
| OS | Ubuntu 24.04.4 LTS |
| kernel / build | Linux version 6.6.87.2-microsoft-standard-WSL2 (root@439a258ad544) (gcc (GCC) 11.2.0, GNU ld (GNU Binutils) 2.37) #1 SMP PREEMPT_DYNAMIC Thu Jun  5 18:30:46 UTC 2025 |
| CPU | AMD Ryzen 9 9950X3D 16-Core Processor |
| logical CPUs | 32 |
| filesystem | ext4 on / (device /dev/sdd 8:48, mount options rw,relatime, super options rw,discard,errors=remount-ro,data=ordered); store-io classified it Ext4 |
| directory under test | /home/james/store-io-harness-838 |
| backend | store-io-posix (PosixPlatform, O_DIRECT; tier in the report's io line) |
| durability class | unverified (receipt label overridden(running under a hypervisor without a certificate for this volume type), durable open Overridden (Trust::override_refusal), block size 4096 B) |
| time per point | 3.0 s measured after 0.3 s warm-up (time-boxed; a point may end early at region capacity, noted where it does) |
| fsys | fsys 1.1.3 (crates.io), Handle method Sync, durability primitive fsync |
| raw primitives | pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync; preadv2 on an O_DIRECT descriptor (one call per request) |
| harness build | release, lto=fat, codegen-units=1 (same as store-io's release profile) |
| total wall time | 47.0 s |

## Device report (`Store::report()`)

```text
volume           e5e3d67c-7811-6707-60b1-6c07a49f80c3
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
| sequential | sequential 1 MiB writes, one barrier at the end | MB/s (higher is better) | 798.8 | 563.3 | 812.3 | 1.418 | +41.8% | 0.983 | -1.7% | fsys: the better of its buffered and direct journals |
| sequential | sequential 2 MiB writes, one barrier at the end | MB/s (higher is better) | 371.7 | 1726.0 | n/a | 0.215 | -78.5% | n/a | n/a | raw: one 2 MiB write per call |
| sequential | sequential 8 MiB writes, one barrier at the end | MB/s (higher is better) | 447.8 | 1090.4 | n/a | 0.411 | -58.9% | n/a | n/a | raw: one 8 MiB write per call |
| sequential | sequential read, 1 MiB requests | MB/s (higher is better) | 784.8 | 1386.9 | n/a | 0.566 | -43.4% | n/a | n/a | fsys: no comparable unbuffered read (see method) |
| sequential | sequential read, 8 MiB requests | MB/s (higher is better) | 4357.2 | 3686.7 | n/a | 1.182 | +18.2% | n/a | n/a | fsys: no comparable unbuffered read (see method) |

## Anomalies, errors and integrity failures

- None: every measurement completed and every integrity check passed.

## W5: sequential bandwidth (1, 2 and 8 MiB writes, 1/8 MiB reads) (`sequential`)

Wall time: 46.4 s.

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
| store-io | AppendRegion::append(1 MiB) x N + sync_through | 1048576 | 798.8 | 1073741824 | 1.344 | 15.76 | 1461.5 | 4111.6 | 13876.3 | 0.00 |
| store-io | AppendRegion::read, 1 MiB requests | 1048576 | 784.8 | 2297430016 | 2.928 | n/a | 700.2 | 6047.4 | 27046.4 | 0.00 |
| store-io | AppendRegion::read, 8 MiB requests | 8388608 | 4357.2 | 12801015808 | 2.938 | n/a | 1903.9 | 2314.7 | 2449.1 | 0.00 |
| store-io | AppendRegion::append(2 MiB) x N + sync_through | 2097152 | 371.7 | 1073741824 | 2.889 | 4.46 | 5347.1 | 10826.0 | 16070.4 | 0.00 |
| store-io | AppendRegion::append(8 MiB) x N + sync_through | 8388608 | 447.8 | 1073741824 | 2.398 | 5.53 | 14501.9 | 46140.5 | 50126.1 | 0.00 |
| raw | 1 MiB writes + one data flush | 1048576 | 563.3 | 1073741824 | 1.906 | 5.06 | 1871.8 | 3973.9 | 6763.4 | 1.00 |
| raw | unbuffered read, 1 MiB requests | 1048576 | 1386.9 | 3876585472 | 2.795 | n/a | 717.1 | 2403.2 | 18450.3 | 0.00 |
| raw | unbuffered read, 8 MiB requests | 8388608 | 3686.7 | 10561257472 | 2.865 | n/a | 2150.3 | 3868.6 | 5564.1 | 0.00 |
| raw | diagnostic: unbuffered read into a store-io-buf pool buffer, 1 MiB requests | 1048576 | 1312.9 | 3578789888 | 2.726 | n/a | 733.6 | 2452.7 | 4125.4 | 0.00 |
| raw | diagnostic: unbuffered read into a pool buffer + copy into a caller Vec, 1 MiB requests | 1048576 | 1382.3 | 3977248768 | 2.877 | n/a | 717.5 | 2422.0 | 18424.6 | 0.00 |
| raw | diagnostic: unbuffered read at offsets + 48 KiB (store-io's data alignment), 1 MiB requests | 1048576 | 1478.5 | 4434427904 | 2.999 | n/a | 694.5 | 998.7 | 4034.7 | 0.00 |
| raw | 2 MiB writes + one data flush | 2097152 | 1726.0 | 1073741824 | 0.622 | 7.67 | 895.9 | 3152.8 | 6897.6 | 1.00 |
| raw | 8 MiB writes + one data flush | 8388608 | 1090.4 | 1073741824 | 0.985 | 12.21 | 5551.7 | 21743.8 | 24413.9 | 1.00 |
| fsys | journal, buffered, window off: append(1 MiB) x N + sync_through | 1048576 | 644.2 | 1073741824 | 1.667 | 885.91 | 799.1 | 1017.4 | 1497.5 | 1.00 |
| fsys | journal, direct, window off: append(1 MiB) x N + sync_through | 1048576 | 812.3 | 1073741824 | 1.322 | 19.85 | 997.6 | 5350.5 | 19256.8 | 1.00 |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append(1 MiB) x N + sync_through | passed: all 1024 MiB read back through AppendRegion::read and compared byte for byte | provisioned a 1024 MiB append region in 0.40 s (2670 MB/s: fill, flush, verify) |
| store-io | AppendRegion::read, 1 MiB requests | passed: first 1 MiB of each of 2366 requests compared against what was written |  |
| store-io | AppendRegion::read, 8 MiB requests | passed: first 1 MiB of each of 1639 requests compared against what was written |  |
| store-io | AppendRegion::append(2 MiB) x N + sync_through | passed: all 1024 MiB read back through AppendRegion::read and compared byte for byte | provisioned a 1024 MiB append region in 2.10 s (511 MB/s: fill, flush, verify) |
| store-io | AppendRegion::append(8 MiB) x N + sync_through | passed: all 1024 MiB read back through AppendRegion::read and compared byte for byte | provisioned a 1024 MiB append region in 2.62 s (409 MB/s: fill, flush, verify) |
| raw | 1 MiB writes + one data flush | passed: all 1024 MiB read back unbuffered and compared byte for byte | file made ready (zero-filled unbuffered 1 MiB writes + flushed) in 0.20 s (5490 MB/s); compare store-io's provisioning note |
| raw | unbuffered read, 1 MiB requests | passed: first 1 MiB of each of 4071 requests compared against what was written |  |
| raw | unbuffered read, 8 MiB requests | passed: first 1 MiB of each of 1399 requests compared against what was written |  |
| raw | diagnostic: unbuffered read into a store-io-buf pool buffer, 1 MiB requests | passed: first 1 MiB of each of 3698 requests compared against what was written | not a comparison row: separates pool-memory cost from engine-path cost |
| raw | diagnostic: unbuffered read into a pool buffer + copy into a caller Vec, 1 MiB requests | passed: first 1 MiB of each of 4154 requests compared against what was written | not a comparison row: reproduces store-io's read path (device read into a pooled buffer, then a copy into the caller's buffer) |
| raw | diagnostic: unbuffered read at offsets + 48 KiB (store-io's data alignment), 1 MiB requests | not checked: shifted requests straddle written chunks; the unshifted rows check content | not a comparison row: tests whether store-io's region data offset (48 KiB past a 1 MiB boundary) costs bandwidth on this stack |
| raw | 2 MiB writes + one data flush | passed: all 1024 MiB read back unbuffered and compared byte for byte | file made ready (zero-filled unbuffered 1 MiB writes + flushed) in 0.19 s (5761 MB/s); compare store-io's provisioning note |
| raw | 8 MiB writes + one data flush | passed: all 1024 MiB read back unbuffered and compared byte for byte | file made ready (zero-filled unbuffered 1 MiB writes + flushed) in 0.21 s (5056 MB/s); compare store-io's provisioning note |
| fsys | journal, buffered, window off: append(1 MiB) x N + sync_through | passed: all 1024 records replayed with JournalReader and compared byte for byte |  |
| fsys | journal, direct, window off: append(1 MiB) x N + sync_through | passed: all 1024 records replayed with JournalReader and compared byte for byte |  |

