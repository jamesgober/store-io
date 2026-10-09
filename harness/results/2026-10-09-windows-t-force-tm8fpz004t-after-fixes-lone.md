# store-io performance harness: windows / t-force-tm8fpz004t (2026-10-09)

## Run

| item | value |
|---|---|
| date (UTC) | 2026-10-09T09:15:21Z |
| command | lone |
| store-io commit | a51493bda14df48e5fe64575dfdf88192cdde941 (crates/ modified) |
| OS | Microsoft Windows [Version 10.0.26200.9457] |
| kernel / build | NT build 10.0.26200.9457 |
| CPU | AMD64 Family 26 Model 68 Stepping 0, AuthenticAMD |
| logical CPUs | 32 |
| filesystem | Ntfs (volume C:) |
| directory under test | C:\Users\james\AppData\Local\Temp\store-io-harness-107916 |
| backend | store-io-win (WinPlatform: overlapped I/O on IOCP, NO_BUFFERING) |
| durability class | flush-required (receipt label evidence, durable open Allowed, block size 4096 B) |
| time per point | 5.0 s measured after 0.5 s warm-up (time-boxed; a point may end early at region capacity, noted where it does) |
| fsys | fsys 1.1.3 (crates.io), Handle method Sync, durability primitive fsync |
| raw primitives | WriteFile on a FILE_FLAG_NO_BUFFERING handle + NtFlushBuffersFileEx(FLUSH_FLAGS_FILE_DATA_SYNC_ONLY); ReadFile on a FILE_FLAG_NO_BUFFERING handle (one call per request) |
| harness build | release, lto=fat, codegen-units=1 (same as store-io's release profile) |
| total wall time | 45.7 s |

## Device report (`Store::report()`)

```text
volume           32e3918a-de98-713a-fffa-e7007e9c5993
class            flush-required
label            evidence
durable open     Allowed
missing          storage stack layers below the filesystem
missing          page-cache residency of a range (Linux `cachestat`)
flush execution  inline (leader)
block size       4096
table capacity   253 regions
filesystem       Ntfs
bus              Nvme
model            T-FORCE TM8FPZ004T
mode             read-write
```

## Store options (every store in this run)

```text
StoreOptions {
    trust: Trust {
        certificate: None,
        attestation: None,
        override_refusal: false,
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
    platform: Windows,
    device: DeviceFacts {
        model: Some(
            "T-FORCE TM8FPZ004T",
        ),
        firmware: Some(
            "EIFM70.3",
        ),
        bus: Nvme,
        cache_present: Yes,
        cache_enabled: Yes,
        os_cache_mode: WriteBack,
        os_flush_supported: Yes,
        user_power_protection: No,
        backup_failed: No,
        fua_supported: Yes,
        logical_block: 512,
        physical_block: 4096,
        optimal_write: 0,
        max_transfer: 524288,
        atomic: AtomicFields {
            awupf: Some(
                0,
            ),
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
        kind: Ntfs,
        data_journal: false,
        no_barrier: false,
        sync_disabled: false,
        weak_error_mode: false,
        transformed: false,
        cow: No,
        direct_io: Yes,
        full_flush: Yes,
        dir_flush: Yes,
        os_atomic_unit: 4096,
        dio_mem_align: 4096,
        dio_offset_align: 512,
    },
    stack: [],
    hypervisor: None,
    privilege: Full,
    kernel: Some(
        (
            10,
            0,
            26200,
        ),
    ),
    timings: Timings {
        flush_idle_ns: None,
        flush_loaded_ns: None,
        fua_extra_ns: None,
        flush_blocks_writes_pct: None,
    },
    missing: {
        Stack,
        PageCache,
    },
}
```
</details>

## How to read the numbers

- Every value in the workload tables is **[measured]** by this harness in this run; ratios and percentages are **[derived]** from those measurements.
- Latencies are per operation (per commit for batches), in microseconds, nearest-rank percentiles over every measured operation; p99 needs at least 100 samples and p99.9 at least 1000, otherwise `n/a`.
- Throughput is operations completed inside the measured window divided by the window (wall clock).
- `write calls` / `other calls` are the OS per-process counters (GetProcessIoCounters (WriteOperationCount: NtWriteFile calls; OtherOperationCount includes flushes)), read before and after the measured window; they count system calls, not device commands.
- **store-io advantage**: positive means store-io is better (more throughput, or less latency) by that percentage; negative means worse.

## Summary: store-io vs raw vs fsys

| workload | case | metric | store-io | raw | fsys | store-io ÷ raw | store-io advantage vs raw | store-io ÷ fsys | store-io advantage vs fsys | compared |
|---|---|---|---|---|---|---|---|---|---|---|
| lone | QD1 durable 4 KiB append | durable ops/s (higher is better) | 2811.1 | 2720.5 | 2154.0 | 1.033 | +3.3% | 1.305 | +30.5% | append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config) |
| lone | QD1 durable 4 KiB append | p50 µs (lower is better) | 316.5 | 327.7 | 410.5 | 0.966 | +3.4% | 0.771 | +22.9% | append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config) |
| lone | QD1 durable 4 KiB append | p99 µs (lower is better) | 504.8 | 558.3 | 874.3 | 0.904 | +9.6% | 0.577 | +42.3% | append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config) |
| lone | QD1 durable 4 KiB append, fsys default config | durable ops/s (higher is better) | 2811.1 | 2720.5 | 64.5 | 1.033 | +3.3% | 43.601 | +4260.1% | fsys JournalOptions::new() (500 µs group-commit window) |
| lone | QD1 durable 4 KiB page overwrite | durable ops/s (higher is better) | 2723.6 | 2720.5 | 1477.3 | 1.001 | +0.1% | 1.844 | +84.4% | write_durable vs raw write+flush vs fsys write_at+sync |
| lone | QD1 durable 4 KiB page overwrite | p50 µs (lower is better) | 327.1 | 327.7 | 634.4 | 0.998 | +0.2% | 0.516 | +48.4% | write_durable vs raw write+flush vs fsys write_at+sync |
| lone | QD1 durable 4 KiB page overwrite | p99 µs (lower is better) | 572.3 | 558.3 | 1111.4 | 1.025 | -2.5% | 0.515 | +48.5% | write_durable vs raw write+flush vs fsys write_at+sync |

## Anomalies, errors and integrity failures

- diagnostic: NtFlushBuffersFileEx(DATA_SYNC_ONLY) on a FILE_FLAG_OVERLAPPED | NO_BUFFERING handle (the kind store-io-win flushes through) after a dirty 4 KiB write never returned STATUS_PENDING: 0 of 400 idle and 0 of 400 while a second thread wrote the same file [measured]. The latent hazard in store-io-win's flush wrapper (STATUS_PENDING counted as success, IO_STATUS_BLOCK dropped) did not trigger on this machine; that is not proof it cannot

## W1: lone writer, QD1 durable 4 KiB (`lone`)

Wall time: 45.3 s.

**Method.**

- One thread, one operation at a time; each operation is a 4 KiB write made durable before the next starts.
- store-io `append_durable` appends to a freshly provisioned append region; `write_durable` overwrites a 256 MiB page region at sequentially cycling 4 KiB offsets.
- raw: copy the payload into an aligned buffer (the copy store-io also makes), then WriteFile on a FILE_FLAG_NO_BUFFERING handle + NtFlushBuffersFileEx(FLUSH_FLAGS_FILE_DATA_SYNC_ONLY), at sequentially cycling offsets of a ready 256 MiB file.
- fsys: `JournalHandle::append` + `sync_through(lsn)` (default, window off, direct+window off) on a fresh journal file; `Handle::write_at` + `Handle::sync` on a ready 256 MiB file (fsys opens the file on every call; that is its API).
- Latency covers the write and the durability call only; payload generation happens before the timer starts.

Results:

| system | variant | durable ops/s | p50 µs | p90 µs | p99 µs | p99.9 µs | max µs | mean µs | flushes/op | write calls/op | other calls/op |
|---|---|---|---|---|---|---|---|---|---|---|---|
| store-io | AppendRegion::append_durable | 2811 | 316.5 | 363.9 | 504.8 | 5303.4 | 6269.3 | 355.7 | 1.00 | 1.00 | 1.00 |
| store-io | AppendRegion::append + sync_through (halves timed separately) | 2699 | 329.2 | 385.3 | 629.2 | 5331.1 | 6216.4 | 370.5 | 1.00 | 1.00 | 1.00 |
| store-io | PageRegion::write_durable | 2724 | 327.1 | 377.8 | 572.3 | 5310.9 | 6392.2 | 367.2 | 1.00 | 1.00 | 1.00 |
| raw | write + data flush | 2720 | 327.7 | 382.6 | 558.3 | 5332.3 | 6021.0 | 367.6 | n/a | 1.00 | 1.00 |
| fsys | journal, default (buffered, 500 µs group-commit window): append + sync_through | 64 | 15505.1 | 15736.4 | 16628.1 | n/a | 19316.8 | 15510.1 | n/a | 1.00 | 1.00 |
| fsys | journal, buffered, window off: append + sync_through | 2154 | 410.5 | 509.7 | 874.3 | 5445.9 | 47192.8 | 464.2 | n/a | 1.00 | 1.00 |
| fsys | journal, direct, window off: append + sync_through | 2086 | 423.9 | 571.6 | 1245.3 | 5484.4 | 6898.5 | 479.4 | n/a | 1.00 | 1.00 |
| fsys | Handle::write_at + Handle::sync | 1477 | 634.4 | 763.8 | 1111.4 | 5663.7 | 6043.6 | 676.9 | n/a | 1.00 | 14.00 |

Where the time goes (write call vs durability call, timed separately on the same thread):

| system | variant | write p50 µs | write p99 µs | write mean µs | durability p50 µs | durability p99 µs | durability mean µs |
|---|---|---|---|---|---|---|---|
| store-io | AppendRegion::append + sync_through (halves timed separately) | 36.9 | 104.3 | 41.9 | 290.5 | 480.7 | 328.2 |
| raw | write + data flush | 34.9 | 101.9 | 41.2 | 288.7 | 442.5 | 324.8 |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append_durable | passed: 2049 of 15468 records read back and compared byte for byte; all 15468 checked for overlap and alignment | provisioned a 538 MiB append region in 0.15 s (3735 MB/s: fill, flush, verify) |
| store-io | AppendRegion::append + sync_through (halves timed separately) | passed: 2049 of 14831 records read back and compared byte for byte; all 14831 checked for overlap and alignment | provisioned a 538 MiB append region in 0.14 s (4093 MB/s: fill, flush, verify) |
| store-io | PageRegion::write_durable | passed: 2049 of 14969 written pages read back (latest write each) and compared byte for byte |  |
| raw | write + data flush | passed: 2049 of 14994 written slots read back unbuffered and compared byte for byte | file made ready (zero-filled unbuffered + flushed) in 0.05 s (4917 MB/s) |
| fsys | journal, default (buffered, 500 µs group-commit window): append + sync_through | passed: all 356 records replayed with JournalReader and compared byte for byte | journal backend KernelBuffered, direct active false |
| fsys | journal, buffered, window off: append + sync_through | passed: all 11853 records replayed with JournalReader and compared byte for byte | journal backend KernelBuffered, direct active false |
| fsys | journal, direct, window off: append + sync_through | passed: all 11476 records replayed with JournalReader and compared byte for byte | journal backend KernelBuffered, direct active false |
| fsys | Handle::write_at + Handle::sync | passed: 2049 of 8165 written slots read back unbuffered and compared byte for byte |  |

