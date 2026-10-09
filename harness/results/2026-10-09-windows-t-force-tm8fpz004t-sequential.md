# store-io performance harness: windows / t-force-tm8fpz004t (2026-10-09)

## Run

| item | value |
|---|---|
| date (UTC) | 2026-10-09T08:58:23Z |
| command | sequential |
| store-io commit | 6551652186be080c31a2e3ec97d6510d5f0525d9 |
| OS | Microsoft Windows [Version 10.0.26200.9457] |
| kernel / build | NT build 10.0.26200.9457 |
| CPU | AMD64 Family 26 Model 68 Stepping 0, AuthenticAMD |
| logical CPUs | 32 |
| filesystem | Ntfs (volume C:) |
| directory under test | C:\Users\james\AppData\Local\Temp\store-io-harness-97944 |
| backend | store-io-win (WinPlatform: overlapped I/O on IOCP, NO_BUFFERING) |
| durability class | flush-required (receipt label evidence, durable open Allowed, block size 4096 B) |
| time per point | 5.0 s measured after 0.5 s warm-up (time-boxed; a point may end early at region capacity, noted where it does) |
| fsys | fsys 1.1.3 (crates.io), Handle method Sync, durability primitive fsync |
| raw primitives | WriteFile on a FILE_FLAG_NO_BUFFERING handle + NtFlushBuffersFileEx(FLUSH_FLAGS_FILE_DATA_SYNC_ONLY); ReadFile on a FILE_FLAG_NO_BUFFERING handle (one call per request) |
| harness build | release, lto=fat, codegen-units=1 (same as store-io's release profile) |
| total wall time | 48.0 s |

## Device report (`Store::report()`)

```text
volume           c8a45c99-0fc7-c385-1686-2669ec04b6d7
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
| sequential | sequential 1 MiB writes, one barrier at the end | MB/s (higher is better) | 2562.4 | 3135.9 | 761.9 | 0.817 | -18.3% | 3.363 | +236.3% | fsys: the better of its buffered and direct journals |
| sequential | sequential read, 1 MiB requests | MB/s (higher is better) | 2162.3 | 2645.6 | n/a | 0.817 | -18.3% | n/a | n/a | fsys: no comparable unbuffered read (see method) |
| sequential | sequential read, 8 MiB requests | MB/s (higher is better) | 1861.0 | 5323.1 | n/a | 0.350 | -65.0% | n/a | n/a | fsys: no comparable unbuffered read (see method) |

## Anomalies, errors and integrity failures

- diagnostic: NtFlushBuffersFileEx(DATA_SYNC_ONLY) on a FILE_FLAG_OVERLAPPED | NO_BUFFERING handle (the kind store-io-win flushes through) after a dirty 4 KiB write never returned STATUS_PENDING: 0 of 400 idle and 0 of 400 while a second thread wrote the same file [measured]. The latent hazard in store-io-win's flush wrapper (STATUS_PENDING counted as success, IO_STATUS_BLOCK dropped) did not trigger on this machine; that is not proof it cannot

## W5: sequential bandwidth (1 MiB writes, 1/8 MiB reads) (`sequential`)

Wall time: 47.4 s.

**Method.**

- Write: one thread writes 1 MiB chunks back to back (not durable), then one durability barrier; MB/s = bytes / (time inside the write calls + the barrier); payload generation between writes is excluded (the wall clock is recorded as `wall_s` in the JSON). No warm-up (the region is written once); the point stops at 1 GiB or at the time box.
- store-io: `AppendRegion::append(1 MiB)` into a fresh 1 GiB append region, then `sync_through(last ticket)`. raw: copy into an aligned buffer + WriteFile per chunk into a ready 1 GiB file, then one data flush. fsys: journal `append(1 MiB)` + one `sync_through` on a fresh journal (buffered and direct, window off; fsys extends its file as it appends, store-io and raw overwrite preallocated space).
- Read: the bytes just written, front to back, repeated until the time box: store-io `AppendRegion::read(offset, &mut out)` with 1 MiB and 8 MiB `out`; raw ReadFile on a FILE_FLAG_NO_BUFFERING handle (one call per request) with the same request sizes. Latency is per request; MB/s = bytes / time inside the read calls (the content spot-check between requests is excluded).
- fsys read: not measured. fsys 1.1.3 has no unbuffered positioned read (`Handle::read_at` opens the file buffered on every call, and `JournalReader` replays through a buffered reader), so it would measure the page cache, not the device.
- MB = 10^6 bytes.

Results:

| system | variant | request | MB/s | bytes | seconds | barrier ms | request p50 µs | request p99 µs | request max µs | write calls/request |
|---|---|---|---|---|---|---|---|---|---|---|
| store-io | AppendRegion::append(1 MiB) x N + sync_through | 1048576 | 2562.4 | 1073741824 | 0.419 | 0.67 | 369.3 | 849.0 | 5996.2 | 1.00 |
| store-io | AppendRegion::read, 1 MiB requests | 1048576 | 2162.3 | 9492758528 | 4.390 | n/a | 458.2 | 853.0 | 5608.8 | 0.00 |
| store-io | AppendRegion::read, 8 MiB requests | 8388608 | 1861.0 | 8992587776 | 4.832 | n/a | 4324.0 | 9304.8 | 12838.3 | 0.00 |
| raw | 1 MiB writes + one data flush | 1048576 | 3135.9 | 1073741824 | 0.342 | 0.25 | 261.2 | 750.2 | 6338.8 | 1.00 |
| raw | unbuffered read, 1 MiB requests | 1048576 | 2645.6 | 10415505408 | 3.937 | n/a | 357.2 | 700.5 | 17358.2 | 0.00 |
| raw | unbuffered read, 8 MiB requests | 8388608 | 5323.1 | 24679284736 | 4.636 | n/a | 1503.0 | 2131.5 | 8407.0 | 0.00 |
| raw | diagnostic: unbuffered read into a store-io-buf pool buffer, 1 MiB requests | 1048576 | 2371.5 | 9486467072 | 4.000 | n/a | 414.9 | 745.8 | 5689.0 | 0.00 |
| raw | diagnostic: unbuffered read into a pool buffer + copy into a caller Vec, 1 MiB requests | 1048576 | 1173.4 | 5361369088 | 4.569 | n/a | 537.2 | 5452.4 | 12151.2 | 0.00 |
| raw | diagnostic: unbuffered read at offsets + 48 KiB (store-io's data alignment), 1 MiB requests | 1048576 | 2008.2 | 10029629440 | 4.994 | n/a | 508.0 | 816.8 | 5692.2 | 0.00 |
| fsys | journal, buffered, window off: append(1 MiB) x N + sync_through | 1048576 | 761.9 | 1073741824 | 1.409 | 536.58 | 796.6 | 1707.6 | 2728.3 | 1.00 |
| fsys | journal, direct, window off: append(1 MiB) x N + sync_through | 1048576 | 556.8 | 1073741824 | 1.928 | 580.24 | 1034.8 | 4595.9 | 70646.2 | 1.00 |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append(1 MiB) x N + sync_through | passed: all 1024 MiB read back through AppendRegion::read and compared byte for byte | provisioned a 1024 MiB append region in 0.45 s (2401 MB/s: fill, flush, verify) |
| store-io | AppendRegion::read, 1 MiB requests | passed: first 1 MiB of each of 9978 requests compared against what was written |  |
| store-io | AppendRegion::read, 8 MiB requests | passed: first 1 MiB of each of 1187 requests compared against what was written |  |
| raw | 1 MiB writes + one data flush | passed: all 1024 MiB read back unbuffered and compared byte for byte | file made ready (zero-filled unbuffered 1 MiB writes + flushed) in 0.30 s (3552 MB/s); compare store-io's provisioning note |
| raw | unbuffered read, 1 MiB requests | passed: first 1 MiB of each of 10910 requests compared against what was written |  |
| raw | unbuffered read, 8 MiB requests | passed: first 1 MiB of each of 3249 requests compared against what was written |  |
| raw | diagnostic: unbuffered read into a store-io-buf pool buffer, 1 MiB requests | passed: first 1 MiB of each of 9903 requests compared against what was written | not a comparison row: separates pool-memory cost from engine-path cost |
| raw | diagnostic: unbuffered read into a pool buffer + copy into a caller Vec, 1 MiB requests | passed: first 1 MiB of each of 5925 requests compared against what was written | not a comparison row: reproduces store-io's read path (device read into a pooled buffer, then a copy into the caller's buffer) |
| raw | diagnostic: unbuffered read at offsets + 48 KiB (store-io's data alignment), 1 MiB requests | not checked: shifted requests straddle written chunks; the unshifted rows check content | not a comparison row: tests whether store-io's region data offset (48 KiB past a 1 MiB boundary) costs bandwidth on this stack |
| fsys | journal, buffered, window off: append(1 MiB) x N + sync_through | passed: all 1024 records replayed with JournalReader and compared byte for byte |  |
| fsys | journal, direct, window off: append(1 MiB) x N + sync_through | passed: all 1024 records replayed with JournalReader and compared byte for byte |  |

