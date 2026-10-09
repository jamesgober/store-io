# store-io performance harness: linux / msft-virtual-disk (2026-10-09)

> **OVERRIDE IN EFFECT.** `Store::create` with default trust was **refused** on this device:
> `device durability is unverified: running under a hypervisor without a certificate for this volume type; missing: device identify data (needs a readable device node); SMART / health log (needs privilege on Linux); volatile write cache presence; volatile write cache enabled state; device atomic-write fields; [Unverified { missing: {Identify, Smart, CachePresence, CacheEnabled, AtomicFields}, reasons: {Hypervisor} }]`
>
> Every store in this run was created with the documented labelled override (`Trust { override_refusal: true, .. }`): the class stays as decided, every receipt is labelled `overridden`, and the most conservative primitive (write + device flush) is used. Nothing else was weakened. The numbers below are therefore **not** gate numbers for a certified durability class; they measure software cost on this stack.

## Run

| item | value |
|---|---|
| date (UTC) | 2026-10-09T08:59:19Z |
| command | sequential |
| store-io commit | 6551652186be080c31a2e3ec97d6510d5f0525d9 |
| OS | Ubuntu 24.04.4 LTS |
| kernel / build | Linux version 6.6.87.2-microsoft-standard-WSL2 (root@439a258ad544) (gcc (GCC) 11.2.0, GNU ld (GNU Binutils) 2.37) #1 SMP PREEMPT_DYNAMIC Thu Jun  5 18:30:46 UTC 2025 |
| CPU | AMD Ryzen 9 9950X3D 16-Core Processor |
| logical CPUs | 32 |
| filesystem | ext4 on / (device /dev/sdd 8:48, mount options rw,relatime, super options rw,discard,errors=remount-ro,data=ordered); store-io classified it Ext4 |
| directory under test | /home/james/store-io-harness-4491 |
| backend | store-io-posix (PosixPlatform: synchronous O_DIRECT, tier T3) |
| durability class | unverified (receipt label overridden(running under a hypervisor without a certificate for this volume type), durable open Overridden (Trust::override_refusal), block size 4096 B) |
| time per point | 5.0 s measured after 0.5 s warm-up (time-boxed; a point may end early at region capacity, noted where it does) |
| fsys | fsys 1.1.3 (crates.io), Handle method Sync, durability primitive fsync |
| raw primitives | pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync; preadv2 on an O_DIRECT descriptor (one call per request) |
| harness build | release, lto=fat, codegen-units=1 (same as store-io's release profile) |
| total wall time | 47.4 s |

## Device report (`Store::report()`)

```text
volume           82326a31-1385-7ae3-2369-ec92f5385d88
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
| sequential | sequential 1 MiB writes, one barrier at the end | MB/s (higher is better) | 1364.9 | 1714.9 | 1095.0 | 0.796 | -20.4% | 1.246 | +24.6% | fsys: the better of its buffered and direct journals |
| sequential | sequential read, 1 MiB requests | MB/s (higher is better) | 1189.5 | 1693.2 | n/a | 0.703 | -29.7% | n/a | n/a | fsys: no comparable unbuffered read (see method) |
| sequential | sequential read, 8 MiB requests | MB/s (higher is better) | 1282.1 | 3751.1 | n/a | 0.342 | -65.8% | n/a | n/a | fsys: no comparable unbuffered read (see method) |

## Anomalies, errors and integrity failures

- None: every measurement completed and every integrity check passed.

## W5: sequential bandwidth (1 MiB writes, 1/8 MiB reads) (`sequential`)

Wall time: 47.4 s.

**Method.**

- Write: one thread writes 1 MiB chunks back to back (not durable), then one durability barrier; MB/s = bytes / (time inside the write calls + the barrier); payload generation between writes is excluded (the wall clock is recorded as `wall_s` in the JSON). No warm-up (the region is written once); the point stops at 1 GiB or at the time box.
- store-io: `AppendRegion::append(1 MiB)` into a fresh 1 GiB append region, then `sync_through(last ticket)`. raw: copy into an aligned buffer + pwritev2 per chunk into a ready 1 GiB file, then one data flush. fsys: journal `append(1 MiB)` + one `sync_through` on a fresh journal (buffered and direct, window off; fsys extends its file as it appends, store-io and raw overwrite preallocated space).
- Read: the bytes just written, front to back, repeated until the time box: store-io `AppendRegion::read(offset, &mut out)` with 1 MiB and 8 MiB `out`; raw preadv2 on an O_DIRECT descriptor (one call per request) with the same request sizes. Latency is per request; MB/s = bytes / time inside the read calls (the content spot-check between requests is excluded).
- fsys read: not measured. fsys 1.1.3 has no unbuffered positioned read (`Handle::read_at` opens the file buffered on every call, and `JournalReader` replays through a buffered reader), so it would measure the page cache, not the device.
- MB = 10^6 bytes.

Results:

| system | variant | request | MB/s | bytes | seconds | barrier ms | request p50 µs | request p99 µs | request max µs | write calls/request |
|---|---|---|---|---|---|---|---|---|---|---|
| store-io | AppendRegion::append(1 MiB) x N + sync_through | 1048576 | 1364.9 | 1073741824 | 0.787 | 2.03 | 739.8 | 1283.4 | 5226.7 | 1.00 |
| store-io | AppendRegion::read, 1 MiB requests | 1048576 | 1189.5 | 5338300416 | 4.488 | n/a | 843.2 | 1583.2 | 5639.9 | 0.00 |
| store-io | AppendRegion::read, 8 MiB requests | 8388608 | 1282.1 | 6291456000 | 4.907 | n/a | 6218.5 | 13304.7 | 38353.8 | 0.00 |
| raw | 1 MiB writes + one data flush | 1048576 | 1714.9 | 1073741824 | 0.626 | 66.00 | 452.8 | 1135.6 | 39610.0 | 1.00 |
| raw | unbuffered read, 1 MiB requests | 1048576 | 1693.2 | 6870269952 | 4.058 | n/a | 595.6 | 983.2 | 5662.9 | 0.00 |
| raw | unbuffered read, 8 MiB requests | 8388608 | 3751.1 | 17708351488 | 4.721 | n/a | 2187.7 | 3393.6 | 6931.5 | 0.00 |
| raw | diagnostic: unbuffered read into a store-io-buf pool buffer, 1 MiB requests | 1048576 | 1699.6 | 6882852864 | 4.050 | n/a | 529.9 | 2312.7 | 17916.0 | 0.00 |
| raw | diagnostic: unbuffered read into a pool buffer + copy into a caller Vec, 1 MiB requests | 1048576 | 1337.9 | 5930745856 | 4.433 | n/a | 746.4 | 1353.3 | 6768.9 | 0.00 |
| raw | diagnostic: unbuffered read at offsets + 48 KiB (store-io's data alignment), 1 MiB requests | 1048576 | 1582.5 | 7907311616 | 4.997 | n/a | 570.3 | 2155.7 | 11869.0 | 0.00 |
| fsys | journal, buffered, window off: append(1 MiB) x N + sync_through | 1048576 | 570.4 | 1073741824 | 1.883 | 930.49 | 848.3 | 3196.7 | 7343.3 | 1.00 |
| fsys | journal, direct, window off: append(1 MiB) x N + sync_through | 1048576 | 1095.0 | 1073741824 | 0.981 | 3.82 | 842.0 | 1658.9 | 79234.8 | 1.00 |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append(1 MiB) x N + sync_through | passed: all 1024 MiB read back through AppendRegion::read and compared byte for byte | provisioned a 1024 MiB append region in 0.75 s (1426 MB/s: fill, flush, verify) |
| store-io | AppendRegion::read, 1 MiB requests | passed: first 1 MiB of each of 5548 requests compared against what was written |  |
| store-io | AppendRegion::read, 8 MiB requests | passed: first 1 MiB of each of 827 requests compared against what was written |  |
| raw | 1 MiB writes + one data flush | passed: all 1024 MiB read back unbuffered and compared byte for byte | file made ready (zero-filled unbuffered 1 MiB writes + flushed) in 0.34 s (3202 MB/s); compare store-io's provisioning note |
| raw | unbuffered read, 1 MiB requests | passed: first 1 MiB of each of 7169 requests compared against what was written |  |
| raw | unbuffered read, 8 MiB requests | passed: first 1 MiB of each of 2320 requests compared against what was written |  |
| raw | diagnostic: unbuffered read into a store-io-buf pool buffer, 1 MiB requests | passed: first 1 MiB of each of 7267 requests compared against what was written | not a comparison row: separates pool-memory cost from engine-path cost |
| raw | diagnostic: unbuffered read into a pool buffer + copy into a caller Vec, 1 MiB requests | passed: first 1 MiB of each of 6217 requests compared against what was written | not a comparison row: reproduces store-io's read path (device read into a pooled buffer, then a copy into the caller's buffer) |
| raw | diagnostic: unbuffered read at offsets + 48 KiB (store-io's data alignment), 1 MiB requests | not checked: shifted requests straddle written chunks; the unshifted rows check content | not a comparison row: tests whether store-io's region data offset (48 KiB past a 1 MiB boundary) costs bandwidth on this stack |
| fsys | journal, buffered, window off: append(1 MiB) x N + sync_through | passed: all 1024 records replayed with JournalReader and compared byte for byte |  |
| fsys | journal, direct, window off: append(1 MiB) x N + sync_through | passed: all 1024 records replayed with JournalReader and compared byte for byte |  |

