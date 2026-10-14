# store-io performance harness: linux / msft-virtual-disk (2026-10-14)

> **OVERRIDE IN EFFECT.** `Store::create` with default trust was **refused** on this device:
> `device durability is unverified: running under a hypervisor without a certificate for this volume type; missing: device identify data (needs a readable device node); SMART / health log (needs privilege on Linux); volatile write cache presence; volatile write cache enabled state; device atomic-write fields; [Unverified { missing: {Identify, Smart, CachePresence, CacheEnabled, AtomicFields}, reasons: {Hypervisor} }]`
>
> Every store in this run was created with the documented labelled override (`Trust { override_refusal: true, .. }`): the class stays as decided, every receipt is labelled `overridden`, and the most conservative primitive (write + device flush) is used. Nothing else was weakened. The numbers below are therefore **not** gate numbers for a certified durability class; they measure software cost on this stack.

## Run

| item | value |
|---|---|
| date (UTC) | 2026-10-14T08:47:31Z |
| command | lone |
| store-io commit | e4f90ecec8fe39c18eccbe385c6a10fe51cfce2c (crates/ modified) |
| OS | Ubuntu 24.04.4 LTS |
| kernel / build | Linux version 6.6.87.2-microsoft-standard-WSL2 (root@439a258ad544) (gcc (GCC) 11.2.0, GNU ld (GNU Binutils) 2.37) #1 SMP PREEMPT_DYNAMIC Thu Jun  5 18:30:46 UTC 2025 |
| CPU | AMD Ryzen 9 9950X3D 16-Core Processor |
| logical CPUs | 32 |
| filesystem | ext4 on / (device /dev/sdd 8:48, mount options rw,relatime, super options rw,discard,errors=remount-ro,data=ordered); store-io classified it Ext4 |
| directory under test | /home/james/store-io-harness-1430 |
| backend | store-io-posix (PosixPlatform, O_DIRECT; tier in the report's io line) |
| durability class | unverified (receipt label overridden(running under a hypervisor without a certificate for this volume type), durable open Overridden (Trust::override_refusal), block size 4096 B) |
| time per point | 2.0 s measured after 0.2 s warm-up (time-boxed; a point may end early at region capacity, noted where it does) |
| fsys | fsys 1.1.3 (crates.io), Handle method Sync, durability primitive fsync |
| raw primitives | pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync; preadv2 on an O_DIRECT descriptor (one call per request) |
| harness build | release, lto=fat, codegen-units=1 (same as store-io's release profile) |
| total wall time | 19.1 s |

## Device report (`Store::report()`)

```text
volume           57ee3ad6-c0b8-77e4-da0c-128c4b38308b
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
| lone | QD1 durable 4 KiB append | durable ops/s (higher is better) | 384.2 | 384.0 | 188.6 | 1.000 | +0.0% | 2.037 | +103.7% | append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config) |
| lone | QD1 durable 4 KiB append | p50 µs (lower is better) | 2435.4 | 2477.1 | 5047.0 | 0.983 | +1.7% | 0.483 | +51.7% | append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config) |
| lone | QD1 durable 4 KiB append | p99 µs (lower is better) | 4661.1 | 4824.9 | 20386.6 | 0.966 | +3.4% | 0.229 | +77.1% | append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config) |
| lone | QD1 durable 4 KiB append, fsys default config | durable ops/s (higher is better) | 384.2 | 384.0 | 159.4 | 1.000 | +0.0% | 2.410 | +141.0% | fsys JournalOptions::new() (500 µs group-commit window) |
| lone | QD1 durable 4 KiB page overwrite | durable ops/s (higher is better) | 383.9 | 384.0 | 192.2 | 1.000 | -0.0% | 1.997 | +99.7% | write_durable vs raw write+flush vs fsys write_at+sync |
| lone | QD1 durable 4 KiB page overwrite | p50 µs (lower is better) | 2464.7 | 2477.1 | 5010.7 | 0.995 | +0.5% | 0.492 | +50.8% | write_durable vs raw write+flush vs fsys write_at+sync |
| lone | QD1 durable 4 KiB page overwrite | p99 µs (lower is better) | 6629.9 | 4824.9 | 20494.2 | 1.374 | -37.4% | 0.324 | +67.6% | write_durable vs raw write+flush vs fsys write_at+sync |

## Anomalies, errors and integrity failures

- None: every measurement completed and every integrity check passed.

## W1: lone writer, QD1 durable 4 KiB (`lone`)

Wall time: 18.6 s.

**Method.**

- One thread, one operation at a time; each operation is a 4 KiB write made durable before the next starts.
- store-io `append_durable` appends to a freshly provisioned append region; `write_durable` overwrites a 256 MiB page region at sequentially cycling 4 KiB offsets.
- raw: copy the payload into an aligned buffer (the copy store-io also makes), then pwritev2(flags=0) on an O_DIRECT descriptor + fdatasync, at sequentially cycling offsets of a ready 256 MiB file.
- fsys: `JournalHandle::append` + `sync_through(lsn)` (default, window off, direct+window off) on a fresh journal file; `Handle::write_at` + `Handle::sync` on a ready 256 MiB file (fsys opens the file on every call; that is its API).
- Latency covers the write and the durability call only; payload generation happens before the timer starts.

Results:

| system | variant | durable ops/s | p50 µs | p90 µs | p99 µs | p99.9 µs | max µs | mean µs | flushes/op | write calls/op | other calls/op |
|---|---|---|---|---|---|---|---|---|---|---|---|
| store-io | AppendRegion::append_durable | 384 | 2435.4 | 2617.7 | 4661.1 | n/a | 18911.2 | 2603.1 | 1.00 | 1.00 | n/a |
| store-io | AppendRegion::append + sync_through (halves timed separately) | 390 | 2418.1 | 2562.3 | 6212.0 | n/a | 18556.0 | 2561.4 | 1.00 | 1.00 | n/a |
| store-io | PageRegion::write_durable | 384 | 2464.7 | 2595.6 | 6629.9 | n/a | 17805.6 | 2605.0 | 1.00 | 1.00 | n/a |
| raw | write + data flush | 384 | 2477.1 | 2682.4 | 4824.9 | n/a | 17944.9 | 2603.9 | n/a | 1.00 | n/a |
| fsys | journal, default (buffered, 500 µs group-commit window): append + sync_through | 159 | 5724.1 | 6146.3 | 20930.4 | n/a | 103516.2 | 6272.8 | n/a | 1.00 | n/a |
| fsys | journal, buffered, window off: append + sync_through | 189 | 5047.0 | 5364.3 | 20386.6 | n/a | 20540.5 | 5302.1 | n/a | 1.00 | n/a |
| fsys | journal, direct, window off: append + sync_through | 186 | 5156.7 | 5470.6 | 20448.4 | n/a | 20723.3 | 5376.0 | n/a | 0.00 | n/a |
| fsys | Handle::write_at + Handle::sync | 192 | 5010.7 | 5307.3 | 20494.2 | n/a | 20661.4 | 5202.7 | n/a | 1.00 | n/a |

Where the time goes (write call vs durability call, timed separately on the same thread):

| system | variant | write p50 µs | write p99 µs | write mean µs | durability p50 µs | durability p99 µs | durability mean µs |
|---|---|---|---|---|---|---|---|
| store-io | AppendRegion::append + sync_through (halves timed separately) | 174.5 | 2042.1 | 203.7 | 2233.4 | 4242.5 | 2338.6 |
| raw | write + data flush | 157.7 | 421.1 | 180.5 | 2310.5 | 4477.8 | 2421.8 |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append_durable | passed: 849 of 849 records read back and compared byte for byte; all 849 checked for overlap and alignment | provisioned a 215 MiB append region in 0.13 s (1717 MB/s: fill, flush, verify) |
| store-io | AppendRegion::append + sync_through (halves timed separately) | passed: 866 of 866 records read back and compared byte for byte; all 866 checked for overlap and alignment | provisioned a 215 MiB append region in 0.13 s (1802 MB/s: fill, flush, verify) |
| store-io | PageRegion::write_durable | passed: 845 of 845 written pages read back (latest write each) and compared byte for byte |  |
| raw | write + data flush | passed: 846 of 846 written slots read back unbuffered and compared byte for byte | file made ready (zero-filled unbuffered + flushed) in 0.05 s (5867 MB/s) |
| fsys | journal, default (buffered, 500 µs group-commit window): append + sync_through | passed: all 351 records replayed with JournalReader and compared byte for byte | journal backend KernelBuffered, direct active false |
| fsys | journal, buffered, window off: append + sync_through | passed: all 418 records replayed with JournalReader and compared byte for byte | journal backend KernelBuffered, direct active false |
| fsys | journal, direct, window off: append + sync_through | passed: all 410 records replayed with JournalReader and compared byte for byte | journal backend KernelDirect, direct active true |
| fsys | Handle::write_at + Handle::sync | passed: 422 of 422 written slots read back unbuffered and compared byte for byte |  |

