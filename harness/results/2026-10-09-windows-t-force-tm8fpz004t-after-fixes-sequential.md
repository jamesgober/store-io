# store-io performance harness: windows / t-force-tm8fpz004t (2026-10-09)

## Run

| item | value |
|---|---|
| date (UTC) | 2026-10-09T09:12:23Z |
| command | sequential |
| store-io commit | a51493bda14df48e5fe64575dfdf88192cdde941 (crates/ modified) |
| OS | Microsoft Windows [Version 10.0.26200.9457] |
| kernel / build | NT build 10.0.26200.9457 |
| CPU | AMD64 Family 26 Model 68 Stepping 0, AuthenticAMD |
| logical CPUs | 32 |
| filesystem | Ntfs (volume C:) |
| directory under test | C:\Users\james\AppData\Local\Temp\store-io-harness-102840 |
| backend | store-io-win (WinPlatform: overlapped I/O on IOCP, NO_BUFFERING) |
| durability class | flush-required (receipt label evidence, durable open Allowed, block size 4096 B) |
| time per point | 5.0 s measured after 0.5 s warm-up (time-boxed; a point may end early at region capacity, noted where it does) |
| fsys | fsys 1.1.3 (crates.io), Handle method Sync, durability primitive fsync |
| raw primitives | WriteFile on a FILE_FLAG_NO_BUFFERING handle + NtFlushBuffersFileEx(FLUSH_FLAGS_FILE_DATA_SYNC_ONLY); ReadFile on a FILE_FLAG_NO_BUFFERING handle (one call per request) |
| harness build | release, lto=fat, codegen-units=1 (same as store-io's release profile) |
| total wall time | 44.5 s |

## Device report (`Store::report()`)

```text
volume           5acfd942-8871-6ac5-c2cd-be068324ccaa
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
| sequential | sequential 1 MiB writes, one barrier at the end | MB/s (higher is better) | 3373.0 | 3712.6 | 1154.1 | 0.909 | -9.1% | 2.923 | +192.3% | fsys: the better of its buffered and direct journals |
| sequential | sequential read, 1 MiB requests | MB/s (higher is better) | 2385.4 | 3115.7 | n/a | 0.766 | -23.4% | n/a | n/a | fsys: no comparable unbuffered read (see method) |
| sequential | sequential read, 8 MiB requests | MB/s (higher is better) | 5296.0 | 5116.6 | n/a | 1.035 | +3.5% | n/a | n/a | fsys: no comparable unbuffered read (see method) |

## Anomalies, errors and integrity failures

- diagnostic: NtFlushBuffersFileEx(DATA_SYNC_ONLY) on a FILE_FLAG_OVERLAPPED | NO_BUFFERING handle (the kind store-io-win flushes through) after a dirty 4 KiB write never returned STATUS_PENDING: 0 of 400 idle and 0 of 400 while a second thread wrote the same file [measured]. The latent hazard in store-io-win's flush wrapper (STATUS_PENDING counted as success, IO_STATUS_BLOCK dropped) did not trigger on this machine; that is not proof it cannot

## W5: sequential bandwidth (1 MiB writes, 1/8 MiB reads) (`sequential`)

Wall time: 43.9 s.

**Method.**

- Write: one thread writes 1 MiB chunks back to back (not durable), then one durability barrier; MB/s = bytes / (time inside the write calls + the barrier); payload generation between writes is excluded (the wall clock is recorded as `wall_s` in the JSON). No warm-up (the region is written once); the point stops at 1 GiB or at the time box.
- store-io: `AppendRegion::append(1 MiB)` into a fresh 1 GiB append region, then `sync_through(last ticket)`. raw: copy into an aligned buffer + WriteFile per chunk into a ready 1 GiB file, then one data flush. fsys: journal `append(1 MiB)` + one `sync_through` on a fresh journal (buffered and direct, window off; fsys extends its file as it appends, store-io and raw overwrite preallocated space).
- Read: the bytes just written, front to back, repeated until the time box: store-io `AppendRegion::read(offset, &mut out)` with 1 MiB and 8 MiB `out`; raw ReadFile on a FILE_FLAG_NO_BUFFERING handle (one call per request) with the same request sizes. Latency is per request; MB/s = bytes / time inside the read calls (the content spot-check between requests is excluded).
- fsys read: not measured. fsys 1.1.3 has no unbuffered positioned read (`Handle::read_at` opens the file buffered on every call, and `JournalReader` replays through a buffered reader), so it would measure the page cache, not the device.
- MB = 10^6 bytes.

Results:

| system | variant | request | MB/s | bytes | seconds | barrier ms | request p50 µs | request p99 µs | request max µs | write calls/request |
|---|---|---|---|---|---|---|---|---|---|---|
| store-io | AppendRegion::append(1 MiB) x N + sync_through | 1048576 | 3373.0 | 1073741824 | 0.318 | 0.49 | 292.6 | 508.0 | 5155.3 | 1.00 |
| store-io | AppendRegion::read, 1 MiB requests | 1048576 | 2385.4 | 10757341184 | 4.510 | n/a | 407.2 | 1048.7 | 33160.7 | 0.00 |
| store-io | AppendRegion::read, 8 MiB requests | 8388608 | 5296.0 | 24956108800 | 4.712 | n/a | 1473.8 | 2827.3 | 8466.1 | 0.00 |
| raw | 1 MiB writes + one data flush | 1048576 | 3712.6 | 1073741824 | 0.289 | 0.62 | 264.5 | 477.8 | 5196.1 | 1.00 |
| raw | unbuffered read, 1 MiB requests | 1048576 | 3115.7 | 12367953920 | 3.970 | n/a | 335.3 | 457.9 | 2056.8 | 0.00 |
| raw | unbuffered read, 8 MiB requests | 8388608 | 5116.6 | 24100470784 | 4.710 | n/a | 1607.9 | 2322.5 | 9116.9 | 0.00 |
| raw | diagnostic: unbuffered read into a store-io-buf pool buffer, 1 MiB requests | 1048576 | 3184.3 | 13861126144 | 4.353 | n/a | 327.2 | 434.0 | 6044.7 | 0.00 |
| raw | diagnostic: unbuffered read into a pool buffer + copy into a caller Vec, 1 MiB requests | 1048576 | 2650.2 | 12272533504 | 4.631 | n/a | 385.6 | 574.0 | 5509.9 | 0.00 |
| raw | diagnostic: unbuffered read at offsets + 48 KiB (store-io's data alignment), 1 MiB requests | 1048576 | 3019.0 | 15078522880 | 4.994 | n/a | 343.9 | 504.3 | 4069.4 | 0.00 |
| fsys | journal, buffered, window off: append(1 MiB) x N + sync_through | 1048576 | 1154.1 | 1073741824 | 0.930 | 317.69 | 584.1 | 860.1 | 1822.3 | 1.00 |
| fsys | journal, direct, window off: append(1 MiB) x N + sync_through | 1048576 | 1144.2 | 1073741824 | 0.938 | 339.97 | 578.3 | 856.9 | 1422.1 | 1.00 |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append(1 MiB) x N + sync_through | passed: all 1024 MiB read back through AppendRegion::read and compared byte for byte | provisioned a 1024 MiB append region in 0.36 s (2984 MB/s: fill, flush, verify) |
| store-io | AppendRegion::read, 1 MiB requests | passed: first 1 MiB of each of 11450 requests compared against what was written |  |
| store-io | AppendRegion::read, 8 MiB requests | passed: first 1 MiB of each of 3166 requests compared against what was written |  |
| raw | 1 MiB writes + one data flush | passed: all 1024 MiB read back unbuffered and compared byte for byte | file made ready (zero-filled unbuffered 1 MiB writes + flushed) in 0.25 s (4334 MB/s); compare store-io's provisioning note |
| raw | unbuffered read, 1 MiB requests | passed: first 1 MiB of each of 13042 requests compared against what was written |  |
| raw | unbuffered read, 8 MiB requests | passed: first 1 MiB of each of 3164 requests compared against what was written |  |
| raw | diagnostic: unbuffered read into a store-io-buf pool buffer, 1 MiB requests | passed: first 1 MiB of each of 14420 requests compared against what was written | not a comparison row: separates pool-memory cost from engine-path cost |
| raw | diagnostic: unbuffered read into a pool buffer + copy into a caller Vec, 1 MiB requests | passed: first 1 MiB of each of 12978 requests compared against what was written | not a comparison row: reproduces store-io's read path (device read into a pooled buffer, then a copy into the caller's buffer) |
| raw | diagnostic: unbuffered read at offsets + 48 KiB (store-io's data alignment), 1 MiB requests | not checked: shifted requests straddle written chunks; the unshifted rows check content | not a comparison row: tests whether store-io's region data offset (48 KiB past a 1 MiB boundary) costs bandwidth on this stack |
| fsys | journal, buffered, window off: append(1 MiB) x N + sync_through | passed: all 1024 records replayed with JournalReader and compared byte for byte |  |
| fsys | journal, direct, window off: append(1 MiB) x N + sync_through | passed: all 1024 records replayed with JournalReader and compared byte for byte |  |

