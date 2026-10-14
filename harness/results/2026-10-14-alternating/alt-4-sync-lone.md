# store-io performance harness: linux / msft-virtual-disk (2026-10-14)

> **OVERRIDE IN EFFECT.** `Store::create` with default trust was **refused** on this device:
> `device durability is unverified: running under a hypervisor without a certificate for this volume type; missing: device identify data (needs a readable device node); SMART / health log (needs privilege on Linux); volatile write cache presence; volatile write cache enabled state; device atomic-write fields; [Unverified { missing: {Identify, Smart, CachePresence, CacheEnabled, AtomicFields}, reasons: {Hypervisor} }]`
>
> Every store in this run was created with the documented labelled override (`Trust { override_refusal: true, .. }`): the class stays as decided, every receipt is labelled `overridden`, and the most conservative primitive (write + device flush) is used. Nothing else was weakened. The numbers below are therefore **not** gate numbers for a certified durability class; they measure software cost on this stack.

## Run

| item | value |
|---|---|
| date (UTC) | 2026-10-14T08:19:51Z |
| command | lone |
| store-io commit | e4f90ecec8fe39c18eccbe385c6a10fe51cfce2c (crates/ modified) |
| OS | Ubuntu 24.04.4 LTS |
| kernel / build | Linux version 6.6.87.2-microsoft-standard-WSL2 (root@439a258ad544) (gcc (GCC) 11.2.0, GNU ld (GNU Binutils) 2.37) #1 SMP PREEMPT_DYNAMIC Thu Jun  5 18:30:46 UTC 2025 |
| CPU | AMD Ryzen 9 9950X3D 16-Core Processor |
| logical CPUs | 32 |
| filesystem | ext4 on / (device /dev/sdd 8:48, mount options rw,relatime, super options rw,discard,errors=remount-ro,data=ordered); store-io classified it Ext4 |
| directory under test | /home/james/store-io-harness-59555 |
| backend | store-io-posix (PosixPlatform, O_DIRECT; tier in the report's io line) |
| durability class | unverified (receipt label overridden(running under a hypervisor without a certificate for this volume type), durable open Overridden (Trust::override_refusal), block size 4096 B) |
| time per point | 3.0 s measured after 0.3 s warm-up (time-boxed; a point may end early at region capacity, noted where it does) |
| fsys | fsys 1.1.3 (crates.io), Handle method Sync, durability primitive fsync |
| raw primitives | pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync; preadv2 on an O_DIRECT descriptor (one call per request) |
| harness build | release, lto=fat, codegen-units=1 (same as store-io's release profile) |
| total wall time | 28.6 s |

## Device report (`Store::report()`)

```text
volume           8b052866-0a6a-cd72-c293-a916a9bfffd2
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
| lone | QD1 durable 4 KiB append | durable ops/s (higher is better) | 386.4 | 372.4 | 169.4 | 1.037 | +3.7% | 2.280 | +128.0% | append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config) |
| lone | QD1 durable 4 KiB append | p50 µs (lower is better) | 2390.4 | 2544.0 | 5594.6 | 0.940 | +6.0% | 0.427 | +57.3% | append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config) |
| lone | QD1 durable 4 KiB append | p99 µs (lower is better) | 6367.6 | 5279.4 | 21084.8 | 1.206 | -20.6% | 0.302 | +69.8% | append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config) |
| lone | QD1 durable 4 KiB append, fsys default config | durable ops/s (higher is better) | 386.4 | 372.4 | 156.2 | 1.037 | +3.7% | 2.474 | +147.4% | fsys JournalOptions::new() (500 µs group-commit window) |
| lone | QD1 durable 4 KiB page overwrite | durable ops/s (higher is better) | 380.2 | 372.4 | 183.6 | 1.021 | +2.1% | 2.071 | +107.1% | write_durable vs raw write+flush vs fsys write_at+sync |
| lone | QD1 durable 4 KiB page overwrite | p50 µs (lower is better) | 2439.4 | 2544.0 | 5124.9 | 0.959 | +4.1% | 0.476 | +52.4% | write_durable vs raw write+flush vs fsys write_at+sync |
| lone | QD1 durable 4 KiB page overwrite | p99 µs (lower is better) | 5363.7 | 5279.4 | 21125.1 | 1.016 | -1.6% | 0.254 | +74.6% | write_durable vs raw write+flush vs fsys write_at+sync |

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
| store-io | AppendRegion::append_durable | 386 | 2390.4 | 2588.6 | 6367.6 | 18346.0 | 20161.7 | 2588.2 | 1.00 | 1.00 | n/a |
| store-io | AppendRegion::append + sync_through (halves timed separately) | 353 | 2492.1 | 2684.0 | 18141.3 | 29476.3 | 55565.0 | 2833.6 | 1.00 | 1.00 | n/a |
| store-io | PageRegion::write_durable | 380 | 2439.4 | 2658.6 | 5363.7 | 18570.8 | 18744.1 | 2629.9 | 1.00 | 1.00 | n/a |
| raw | write + data flush | 372 | 2544.0 | 2721.0 | 5279.4 | 18476.1 | 18611.0 | 2684.9 | n/a | 1.00 | n/a |
| fsys | journal, default (buffered, 500 µs group-commit window): append + sync_through | 156 | 6109.8 | 6525.5 | 21937.9 | n/a | 22419.0 | 6402.6 | n/a | 1.00 | n/a |
| fsys | journal, buffered, window off: append + sync_through | 169 | 5594.6 | 6123.7 | 21084.8 | n/a | 21840.9 | 5902.4 | n/a | 1.00 | n/a |
| fsys | journal, direct, window off: append + sync_through | 170 | 5296.8 | 6999.6 | 17236.8 | n/a | 21138.9 | 5875.6 | n/a | 0.00 | n/a |
| fsys | Handle::write_at + Handle::sync | 184 | 5124.9 | 5758.1 | 21125.1 | n/a | 23307.4 | 5446.3 | n/a | 1.00 | n/a |

Where the time goes (write call vs durability call, timed separately on the same thread):

| system | variant | write p50 µs | write p99 µs | write mean µs | durability p50 µs | durability p99 µs | durability mean µs |
|---|---|---|---|---|---|---|---|
| store-io | AppendRegion::append + sync_through (halves timed separately) | 160.2 | 1145.3 | 200.6 | 2331.0 | 18054.6 | 2759.6 |
| raw | write + data flush | 157.3 | 778.0 | 191.1 | 2366.8 | 4586.6 | 2484.6 |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append_durable | passed: 1277 of 1277 records read back and compared byte for byte; all 1277 checked for overlap and alignment | provisioned a 323 MiB append region in 0.22 s (1561 MB/s: fill, flush, verify) |
| store-io | AppendRegion::append + sync_through (halves timed separately) | passed: 1126 of 1126 records read back and compared byte for byte; all 1126 checked for overlap and alignment | provisioned a 323 MiB append region in 0.27 s (1263 MB/s: fill, flush, verify) |
| store-io | PageRegion::write_durable | passed: 1245 of 1245 written pages read back (latest write each) and compared byte for byte |  |
| raw | write + data flush | passed: 1233 of 1233 written slots read back unbuffered and compared byte for byte | file made ready (zero-filled unbuffered + flushed) in 0.05 s (5525 MB/s) |
| fsys | journal, default (buffered, 500 µs group-commit window): append + sync_through | passed: all 515 records replayed with JournalReader and compared byte for byte | journal backend KernelBuffered, direct active false |
| fsys | journal, buffered, window off: append + sync_through | passed: all 564 records replayed with JournalReader and compared byte for byte | journal backend KernelBuffered, direct active false |
| fsys | journal, direct, window off: append + sync_through | passed: all 566 records replayed with JournalReader and compared byte for byte | journal backend KernelDirect, direct active true |
| fsys | Handle::write_at + Handle::sync | passed: 610 of 610 written slots read back unbuffered and compared byte for byte |  |

