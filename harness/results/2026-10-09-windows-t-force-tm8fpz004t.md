# store-io performance harness: windows / t-force-tm8fpz004t (2026-10-09)

## Run

| item | value |
|---|---|
| date (UTC) | 2026-10-09T08:10:12Z |
| command | all |
| store-io commit | 6551652186be080c31a2e3ec97d6510d5f0525d9 |
| OS | Microsoft Windows [Version 10.0.26200.9457] |
| kernel / build | NT build 10.0.26200.9457 |
| CPU | AMD64 Family 26 Model 68 Stepping 0, AuthenticAMD |
| logical CPUs | 32 |
| filesystem | Ntfs (volume C:) |
| directory under test | C:\Users\james\AppData\Local\Temp\store-io-harness-41844 |
| backend | store-io-win (WinPlatform: overlapped I/O on IOCP, NO_BUFFERING) |
| durability class | flush-required (receipt label evidence, durable open Allowed, block size 4096 B) |
| time per point | 5.0 s measured after 0.5 s warm-up (time-boxed; a point may end early at region capacity, noted where it does) |
| fsys | fsys 1.1.3 (crates.io), Handle method Sync, durability primitive fsync |
| raw primitives | WriteFile on a FILE_FLAG_NO_BUFFERING handle + NtFlushBuffersFileEx(FLUSH_FLAGS_FILE_DATA_SYNC_ONLY); ReadFile on a FILE_FLAG_NO_BUFFERING handle (one call per request) |
| harness build | release, lto=fat, codegen-units=1 (same as store-io's release profile) |
| total wall time | 556.1 s |

## Device report (`Store::report()`)

```text
volume           71b44d8c-28ac-340b-9ca5-39f6d95ff554
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
| lone | QD1 durable 4 KiB append | durable ops/s (higher is better) | 2804.2 | 2643.9 | 2179.0 | 1.061 | +6.1% | 1.287 | +28.7% | append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config) |
| lone | QD1 durable 4 KiB append | p50 µs (lower is better) | 323.9 | 342.6 | 422.5 | 0.945 | +5.5% | 0.767 | +23.3% | append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config) |
| lone | QD1 durable 4 KiB append | p99 µs (lower is better) | 454.0 | 595.4 | 727.5 | 0.763 | +23.7% | 0.624 | +37.6% | append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config) |
| lone | QD1 durable 4 KiB append, fsys default config | durable ops/s (higher is better) | 2804.2 | 2643.9 | 64.2 | 1.061 | +6.1% | 43.685 | +4268.5% | fsys JournalOptions::new() (500 µs group-commit window) |
| lone | QD1 durable 4 KiB page overwrite | durable ops/s (higher is better) | 2496.5 | 2643.9 | 1519.0 | 0.944 | -5.6% | 1.643 | +64.3% | write_durable vs raw write+flush vs fsys write_at+sync |
| lone | QD1 durable 4 KiB page overwrite | p50 µs (lower is better) | 351.9 | 342.6 | 619.3 | 1.027 | -2.7% | 0.568 | +43.2% | write_durable vs raw write+flush vs fsys write_at+sync |
| lone | QD1 durable 4 KiB page overwrite | p99 µs (lower is better) | 760.5 | 595.4 | 1034.1 | 1.277 | -27.7% | 0.735 | +26.5% | write_durable vs raw write+flush vs fsys write_at+sync |
| concurrent | 1 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 2592.6 | 2701.7 | 2165.7 | 0.960 | -4.0% | 1.197 | +19.7% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 1 concurrent durable 4 KiB writers | p99 µs (lower is better) | 583.4 | 482.0 | 726.7 | 1.210 | -21.0% | 0.803 | +19.7% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 2 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 2904.2 | 2615.3 | 2983.8 | 1.110 | +11.0% | 0.973 | -2.7% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 2 concurrent durable 4 KiB writers | p99 µs (lower is better) | 1597.7 | 5375.4 | 1268.3 | 0.297 | +70.3% | 1.260 | -26.0% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 4 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 4935.2 | 2635.0 | 4150.5 | 1.873 | +87.3% | 1.189 | +18.9% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 4 concurrent durable 4 KiB writers | p99 µs (lower is better) | 5097.7 | 12008.7 | 2165.3 | 0.425 | +57.5% | 2.354 | -135.4% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 8 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 10508.9 | 2871.5 | 7674.5 | 3.660 | +266.0% | 1.369 | +36.9% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 8 concurrent durable 4 KiB writers | p99 µs (lower is better) | 1311.6 | 26850.3 | 2116.8 | 0.049 | +95.1% | 0.620 | +38.0% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 16 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 20627.7 | 2613.2 | 11422.0 | 7.894 | +689.4% | 1.806 | +80.6% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 16 concurrent durable 4 KiB writers | p99 µs (lower is better) | 1191.5 | 55399.9 | 2523.6 | 0.022 | +97.8% | 0.472 | +52.8% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 32 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 28315.4 | 2476.9 | 20629.9 | 11.432 | +1043.2% | 1.373 | +37.3% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 32 concurrent durable 4 KiB writers | p99 µs (lower is better) | 2702.5 | 137055.4 | 2534.4 | 0.020 | +98.0% | 1.066 | -6.6% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 64 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 63246.4 | 2721.1 | 35875.9 | 23.243 | +2224.3% | 1.763 | +76.3% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 64 concurrent durable 4 KiB writers | p99 µs (lower is better) | 1649.3 | 321049.5 | 2811.0 | 0.005 | +99.5% | 0.587 | +41.3% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| caller-batch | batch of 1 x 64 B, one barrier | records/s (higher is better) | 2692.5 | 2726.7 | 2302.0 | 0.987 | -1.3% | 1.170 | +17.0% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(64 B) + one sync_through | records/s (higher is better) | 2727.0 | 2765.7 | 2333.6 | 0.986 | -1.4% | 1.169 | +16.9% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1 x 512 B, one barrier | records/s (higher is better) | 2420.7 | 2407.0 | 2230.2 | 1.006 | +0.6% | 1.085 | +8.5% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(512 B) + one sync_through | records/s (higher is better) | 2707.4 | 2298.8 | 2182.6 | 1.178 | +17.8% | 1.240 | +24.0% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1 x 4096 B, one barrier | records/s (higher is better) | 2652.3 | 2752.3 | 2180.1 | 0.964 | -3.6% | 1.217 | +21.7% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(4096 B) + one sync_through | records/s (higher is better) | 2659.2 | 2603.2 | 2171.8 | 1.022 | +2.2% | 1.224 | +22.4% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 64 B, one barrier | records/s (higher is better) | 26286.3 | 26571.7 | 20751.2 | 0.989 | -1.1% | 1.267 | +26.7% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(64 B) + one sync_through | records/s (higher is better) | 18099.1 | 13872.1 | 21475.4 | 1.305 | +30.5% | 0.843 | -15.7% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 512 B, one barrier | records/s (higher is better) | 26713.2 | 22914.1 | 20475.2 | 1.166 | +16.6% | 1.305 | +30.5% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(512 B) + one sync_through | records/s (higher is better) | 12944.2 | 15437.4 | 20607.2 | 0.838 | -16.2% | 0.628 | -37.2% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 4096 B, one barrier | records/s (higher is better) | 29755.7 | 29332.7 | 22549.8 | 1.014 | +1.4% | 1.320 | +32.0% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(4096 B) + one sync_through | records/s (higher is better) | 14206.4 | 16847.8 | 19996.4 | 0.843 | -15.7% | 0.710 | -29.0% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 64 B, one barrier | records/s (higher is better) | 260400.0 | 270635.1 | 213089.4 | 0.962 | -3.8% | 1.222 | +22.2% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(64 B) + one sync_through | records/s (higher is better) | 27780.0 | 28625.5 | 150919.4 | 0.970 | -3.0% | 0.184 | -81.6% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 512 B, one barrier | records/s (higher is better) | 332782.7 | 332030.1 | 236561.6 | 1.002 | +0.2% | 1.407 | +40.7% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(512 B) + one sync_through | records/s (higher is better) | 31280.6 | 30443.0 | 134718.7 | 1.028 | +2.8% | 0.232 | -76.8% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 4096 B, one barrier | records/s (higher is better) | 184325.4 | 185282.3 | 115607.5 | 0.995 | -0.5% | 1.594 | +59.4% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(4096 B) + one sync_through | records/s (higher is better) | 28070.5 | 24112.1 | 73449.6 | 1.164 | +16.4% | 0.382 | -61.8% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 64 B, one barrier | records/s (higher is better) | 3755762.3 | 1462559.0 | 1231004.3 | 2.568 | +156.8% | 3.051 | +205.1% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(64 B) + one sync_through | records/s (higher is better) | 26859.6 | 21125.2 | 309311.6 | 1.271 | +27.1% | 0.087 | -91.3% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 512 B, one barrier | records/s (higher is better) | 1780413.2 | 1801246.1 | 888751.4 | 0.988 | -1.2% | 2.003 | +100.3% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(512 B) + one sync_through | records/s (higher is better) | 20636.8 | 24831.2 | 213089.1 | 0.831 | -16.9% | 0.097 | -90.3% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 4096 B, one barrier | records/s (higher is better) | 666283.6 | 533685.7 | 163213.5 | 1.248 | +24.8% | 4.082 | +308.2% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(4096 B) + one sync_through | records/s (higher is better) | 27021.8 | 25751.5 | 102497.4 | 1.049 | +4.9% | 0.264 | -73.6% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| page-batch | 1 random 4 KiB pages + one barrier | pages/s (higher is better) | 1656.4 | 2361.7 | 1302.0 | 0.701 | -29.9% | 1.272 | +27.2% | PageBatch::commit vs raw QD-N writes + flush vs fsys write_at x N + sync |
| page-batch | 1 random 4 KiB pages + one barrier | commit p99 µs (lower is better) | 3688.4 | 874.1 | 1490.6 | 4.220 | -322.0% | 2.474 | -147.4% | PageBatch::commit vs raw QD-N writes + flush vs fsys write_at x N + sync |
| page-batch | 16 random 4 KiB pages + one barrier | pages/s (higher is better) | 33552.0 | 31663.9 | 3136.8 | 1.060 | +6.0% | 10.696 | +969.6% | PageBatch::commit vs raw QD-N writes + flush vs fsys write_at x N + sync |
| page-batch | 16 random 4 KiB pages + one barrier | commit p99 µs (lower is better) | 983.7 | 926.4 | 9380.1 | 1.062 | -6.2% | 0.105 | +89.5% | PageBatch::commit vs raw QD-N writes + flush vs fsys write_at x N + sync |
| page-batch | 64 random 4 KiB pages + one barrier | pages/s (higher is better) | 48244.1 | 65802.4 | 4344.4 | 0.733 | -26.7% | 11.105 | +1010.5% | PageBatch::commit vs raw QD-N writes + flush vs fsys write_at x N + sync |
| page-batch | 64 random 4 KiB pages + one barrier | commit p99 µs (lower is better) | 4432.6 | 1638.6 | 20983.5 | 2.705 | -170.5% | 0.211 | +78.9% | PageBatch::commit vs raw QD-N writes + flush vs fsys write_at x N + sync |
| page-batch | 256 random 4 KiB pages + one barrier | pages/s (higher is better) | 55461.8 | 69608.7 | 4458.5 | 0.797 | -20.3% | 12.440 | +1144.0% | PageBatch::commit vs raw QD-N writes + flush vs fsys write_at x N + sync |
| page-batch | 256 random 4 KiB pages + one barrier | commit p99 µs (lower is better) | 18539.4 | 5879.2 | n/a | 3.153 | -215.3% | n/a | n/a | PageBatch::commit vs raw QD-N writes + flush vs fsys write_at x N + sync |
| sequential | sequential 1 MiB writes, one barrier at the end | MB/s (higher is better) | 3031.6 | 2866.6 | 867.8 | 1.058 | +5.8% | 3.494 | +249.4% | fsys: the better of its buffered and direct journals |
| sequential | sequential read, 1 MiB requests | MB/s (higher is better) | 2302.2 | 2989.7 | n/a | 0.770 | -23.0% | n/a | n/a | fsys: no comparable unbuffered read (see method) |
| sequential | sequential read, 8 MiB requests | MB/s (higher is better) | 1800.1 | 5455.6 | n/a | 0.330 | -67.0% | n/a | n/a | fsys: no comparable unbuffered read (see method) |

## Anomalies, errors and integrity failures

- diagnostic: NtFlushBuffersFileEx(DATA_SYNC_ONLY) on a FILE_FLAG_OVERLAPPED | NO_BUFFERING handle (the kind store-io-win flushes through) after a dirty 4 KiB write never returned STATUS_PENDING: 0 of 400 idle and 0 of 400 while a second thread wrote the same file [measured]. The latent hazard in store-io-win's flush wrapper (STATUS_PENDING counted as success, IO_STATUS_BLOCK dropped) did not trigger on this machine; that is not proof it cannot

## W1: lone writer, QD1 durable 4 KiB (`lone`)

Wall time: 45.5 s.

**Method.**

- One thread, one operation at a time; each operation is a 4 KiB write made durable before the next starts.
- store-io `append_durable` appends to a freshly provisioned append region; `write_durable` overwrites a 256 MiB page region at sequentially cycling 4 KiB offsets.
- raw: copy the payload into an aligned buffer (the copy store-io also makes), then WriteFile on a FILE_FLAG_NO_BUFFERING handle + NtFlushBuffersFileEx(FLUSH_FLAGS_FILE_DATA_SYNC_ONLY), at sequentially cycling offsets of a ready 256 MiB file.
- fsys: `JournalHandle::append` + `sync_through(lsn)` (default, window off, direct+window off) on a fresh journal file; `Handle::write_at` + `Handle::sync` on a ready 256 MiB file (fsys opens the file on every call; that is its API).
- Latency covers the write and the durability call only; payload generation happens before the timer starts.

Results:

| system | variant | durable ops/s | p50 µs | p90 µs | p99 µs | p99.9 µs | max µs | mean µs | flushes/op | write calls/op | other calls/op |
|---|---|---|---|---|---|---|---|---|---|---|---|
| store-io | AppendRegion::append_durable | 2804 | 323.9 | 356.8 | 454.0 | 5308.4 | 6162.9 | 356.6 | 1.00 | 1.00 | 1.00 |
| store-io | AppendRegion::append + sync_through (halves timed separately) | 2797 | 326.1 | 359.7 | 442.9 | 5295.8 | 6022.8 | 357.5 | 1.00 | 1.00 | 1.00 |
| store-io | PageRegion::write_durable | 2497 | 351.9 | 447.0 | 760.5 | 5372.8 | 6580.8 | 400.6 | 1.00 | 1.00 | 1.00 |
| raw | write + data flush | 2644 | 342.6 | 398.2 | 595.4 | 5324.6 | 6001.6 | 378.2 | n/a | 1.00 | 1.00 |
| fsys | journal, default (buffered, 500 µs group-commit window): append + sync_through | 64 | 15552.1 | 15903.0 | 17115.7 | n/a | 19352.5 | 15578.6 | n/a | 1.00 | 1.00 |
| fsys | journal, buffered, window off: append + sync_through | 2179 | 422.5 | 493.9 | 727.5 | 5436.9 | 6059.4 | 458.9 | n/a | 1.00 | 1.00 |
| fsys | journal, direct, window off: append + sync_through | 2178 | 417.6 | 496.6 | 763.9 | 5435.6 | 30135.9 | 459.1 | n/a | 1.00 | 1.00 |
| fsys | Handle::write_at + Handle::sync | 1519 | 619.3 | 722.8 | 1034.1 | 5648.9 | 11144.3 | 658.3 | n/a | 1.00 | 14.00 |

Where the time goes (write call vs durability call, timed separately on the same thread):

| system | variant | write p50 µs | write p99 µs | write mean µs | durability p50 µs | durability p99 µs | durability mean µs |
|---|---|---|---|---|---|---|---|
| store-io | AppendRegion::append + sync_through (halves timed separately) | 40.1 | 89.1 | 37.7 | 287.5 | 377.1 | 320.2 |
| raw | write + data flush | 47.1 | 120.7 | 47.8 | 295.7 | 493.0 | 332.2 |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append_durable | passed: 2049 of 15410 records read back and compared byte for byte; all 15410 checked for overlap and alignment | provisioned a 538 MiB append region in 0.26 s (2152 MB/s: fill, flush, verify) |
| store-io | AppendRegion::append + sync_through (halves timed separately) | passed: 2049 of 15348 records read back and compared byte for byte; all 15348 checked for overlap and alignment | provisioned a 538 MiB append region in 0.15 s (3869 MB/s: fill, flush, verify) |
| store-io | PageRegion::write_durable | passed: 2049 of 13700 written pages read back (latest write each) and compared byte for byte |  |
| raw | write + data flush | passed: 2049 of 14439 written slots read back unbuffered and compared byte for byte | file made ready (zero-filled unbuffered + flushed) in 0.06 s (4851 MB/s) |
| fsys | journal, default (buffered, 500 µs group-commit window): append + sync_through | passed: all 354 records replayed with JournalReader and compared byte for byte | journal backend KernelBuffered, direct active false |
| fsys | journal, buffered, window off: append + sync_through | passed: all 11977 records replayed with JournalReader and compared byte for byte | journal backend KernelBuffered, direct active false |
| fsys | journal, direct, window off: append + sync_through | passed: all 11982 records replayed with JournalReader and compared byte for byte | journal backend KernelBuffered, direct active false |
| fsys | Handle::write_at + Handle::sync | passed: 2049 of 8390 written slots read back unbuffered and compared byte for byte |  |

## W2: concurrent durable writers, 4 KiB (`concurrent`)

Wall time: 158.7 s.

**Method.**

- Writer counts [1, 2, 4, 8, 16, 32, 64]; every thread loops a durable 4 KiB write at queue depth 1 for the whole point.
- store-io: all threads call `AppendRegion::append_durable` on one append region of one store (a fresh store per point). Flush sharing is read from `Store::domain_stats()` deltas over the window: `flushes` issued, barriers that `led` a flush, barriers that `joined` one.
- raw: each thread writes its own contiguous range of one ready 256 MiB file and makes it durable itself with WriteFile on a FILE_FLAG_NO_BUFFERING handle + NtFlushBuffersFileEx(FLUSH_FLAGS_FILE_DATA_SYNC_ONLY) (no sharing).
- fsys: all threads call `append` + `sync_through` on one shared journal (fresh per point), window off and default (500 µs group-commit window).
- An operation counts when it started and finished inside the measured window; rate = counted operations / window.

Results:

| threads | system | variant | durable ops/s | p50 µs | p99 µs | p99.9 µs | flushes issued | barriers led | barriers joined | writes per flush |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | store-io | AppendRegion::append_durable (shared region) | 2593 | 348.6 | 583.4 | 5328.9 | 12965 | 12965 | 0 | 1.00 |
| 1 | raw | own write + own data flush (per thread range) | 2702 | 332.6 | 482.0 | 5312.6 | n/a | n/a | n/a | n/a |
| 1 | fsys | journal, buffered, window off: shared journal append + sync_through | 2166 | 424.7 | 726.7 | 5425.8 | n/a | n/a | n/a | n/a |
| 1 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 64 | 15533.5 | 25044.6 | n/a | n/a | n/a | n/a | n/a |
| 2 | store-io | AppendRegion::append_durable (shared region) | 2904 | 626.7 | 1597.7 | 5831.5 | 14503 | 14503 | 21 | 1.00 |
| 2 | raw | own write + own data flush (per thread range) | 2615 | 372.5 | 5375.4 | 7983.4 | n/a | n/a | n/a | n/a |
| 2 | fsys | journal, buffered, window off: shared journal append + sync_through | 2984 | 483.5 | 1268.3 | 5740.7 | n/a | n/a | n/a | n/a |
| 2 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 105 | 15540.0 | 140660.2 | n/a | n/a | n/a | n/a | n/a |
| 4 | store-io | AppendRegion::append_durable (shared region) | 4935 | 683.3 | 5097.7 | 6271.8 | 13596 | 13596 | 11085 | 1.82 |
| 4 | raw | own write + own data flush (per thread range) | 2635 | 363.1 | 12008.7 | 20667.2 | n/a | n/a | n/a | n/a |
| 4 | fsys | journal, buffered, window off: shared journal append + sync_through | 4151 | 973.9 | 2165.3 | 9251.5 | n/a | n/a | n/a | n/a |
| 4 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 257 | 15535.9 | 20710.7 | 25635.1 | n/a | n/a | n/a | n/a |
| 8 | store-io | AppendRegion::append_durable (shared region) | 10509 | 717.9 | 1311.6 | 6088.6 | 15946 | 15946 | 36610 | 3.30 |
| 8 | raw | own write + own data flush (per thread range) | 2871 | 327.5 | 26850.3 | 45234.7 | n/a | n/a | n/a | n/a |
| 8 | fsys | journal, buffered, window off: shared journal append + sync_through | 7674 | 1002.5 | 2116.8 | 5310.5 | n/a | n/a | n/a | n/a |
| 8 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 512 | 15533.8 | 16159.6 | 16334.8 | n/a | n/a | n/a | n/a |
| 16 | store-io | AppendRegion::append_durable (shared region) | 20628 | 801.0 | 1191.5 | 5904.5 | 16938 | 16938 | 86219 | 6.09 |
| 16 | raw | own write + own data flush (per thread range) | 2613 | 362.8 | 55399.9 | 84331.0 | n/a | n/a | n/a | n/a |
| 16 | fsys | journal, buffered, window off: shared journal append + sync_through | 11422 | 1414.1 | 2523.6 | 6990.6 | n/a | n/a | n/a | n/a |
| 16 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 10891 | 1468.9 | 2576.5 | 7071.5 | n/a | n/a | n/a | n/a |
| 32 | store-io | AppendRegion::append_durable (shared region) | 28315 | 978.6 | 2702.5 | 6655.4 | 12287 | 12287 | 129317 | 11.52 |
| 32 | raw | own write + own data flush (per thread range) | 2477 | 382.0 | 137055.4 | 213214.0 | n/a | n/a | n/a | n/a |
| 32 | fsys | journal, buffered, window off: shared journal append + sync_through | 19881 | 1562.3 | 2555.2 | 6493.6 | n/a | n/a | n/a | n/a |
| 32 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 20630 | 1508.7 | 2534.4 | 6546.1 | n/a | n/a | n/a | n/a |
| 64 | store-io | AppendRegion::append_durable (shared region) | 63246 | 978.8 | 1649.3 | 6182.6 | 9101 | 9101 | 182758 | 21.07 |
| 64 | raw | own write + own data flush (per thread range) | 2721 | 337.9 | 321049.5 | 521866.5 | n/a | n/a | n/a | n/a |
| 64 | fsys | journal, buffered, window off: shared journal append + sync_through | 35876 | 1765.0 | 2811.0 | 7206.9 | n/a | n/a | n/a | n/a |
| 64 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 33680 | 1930.7 | 3052.5 | 7320.8 | n/a | n/a | n/a | n/a |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 14242 records read back and compared byte for byte; all 14242 checked for overlap and alignment | provisioned a 860 MiB append region in 0.24 s (3826 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1025 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 11968 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 352 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 15514 records read back and compared byte for byte; all 15514 checked for overlap and alignment | provisioned a 860 MiB append region in 0.25 s (3626 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1026 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 16818 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 570 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.004 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 27526 records read back and compared byte for byte; all 27526 checked for overlap and alignment | provisioned a 860 MiB append region in 0.23 s (3887 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1028 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 22697 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 1423 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 57116 records read back and compared byte for byte; all 57116 checked for overlap and alignment | provisioned a 860 MiB append region in 0.22 s (4179 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1032 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 41971 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 2832 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 110578 records read back and compared byte for byte; all 110578 checked for overlap and alignment | provisioned a 860 MiB append region in 0.22 s (4120 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1040 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 61333 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 59877 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 148834 records read back and compared byte for byte; all 148834 checked for overlap and alignment | provisioned a 860 MiB append region in 0.28 s (3207 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1); domain counters: 148826 barriers for 148834 durable appends |
| raw | own write + own data flush (per thread range) | passed: 1056 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 109206 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 113396 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 220096 records read back and compared byte for byte; all 220096 checked for overlap and alignment | provisioned a 860 MiB append region in 0.22 s (4022 MB/s: fill, flush, verify); window cut at region capacity: 191768 ops in 3.03 s; OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1); domain counters: 220059 barriers for 220096 durable appends |
| raw | own write + own data flush (per thread range) | passed: 1088 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.005 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 196619 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 184839 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |

**Findings.**

- store-io scaling: 2593 durable ops/s at 1 writer, 63246 at 64 writers (21.07 writes per flush) [derived from the table].

## W3: caller batch (N records, one barrier) (`caller-batch`)

Wall time: 230.6 s.

**Method.**

- N in [1, 10, 100, 1000], record sizes [64, 512, 4096] bytes; one commit = N records made durable by one barrier; commits run back to back on one thread. Each point ends at the time box or after 3000 measured commits, whichever comes first.
- store-io (a) `N x append + sync_through`: each append is its own write, padded to a block (documented behaviour). (b) `AppendBatch`: records packed back to back into pooled buffers, `commit()` = one reservation, the fewest writes (largest pooled buffer 1 MiB), one barrier. A fresh store per (N, size).
- raw (a') N writes of one zero-padded block each, then one flush; (b') the N records packed into one aligned buffer padded to a block, one write, one flush. Primitive: WriteFile on a FILE_FLAG_NO_BUFFERING handle + NtFlushBuffersFileEx(FLUSH_FLAGS_FILE_DATA_SYNC_ONLY). A ready 1 GiB file, offsets cycling.
- fsys: `append` x N + `sync_through(last lsn)`, and `append_batch(&records)` + `sync_through`, on a fresh journal per point, window off.
- records/s = commits/s x N; payload MB/s counts caller bytes only (10^6 bytes); `write calls/commit` and `bytes written/commit` come from the OS counters and show the device writes each design issues per commit.

Results:

| N | record B | system | variant | commits/s | records/s | payload MB/s | commit p50 µs | commit p99 µs | write calls/commit | bytes written/commit | flushes/commit |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 2727 | 2727 | 0.17 | 335.6 | 490.7 | 1.00 | 4096 | 1.00 |
| 1 | 64 | store-io | (b) AppendBatch: N x append + commit | 2693 | 2693 | 0.17 | 338.8 | 533.8 | 1.00 | 4096 | 1.00 |
| 1 | 64 | raw | (a') N writes of one padded block + one flush | 2766 | 2766 | 0.18 | 329.5 | 476.6 | 1.00 | 4096 | n/a |
| 1 | 64 | raw | (b') packed: one write of N records + one flush | 2727 | 2727 | 0.17 | 334.3 | 495.1 | 1.00 | 4096 | n/a |
| 1 | 64 | fsys | append x N + sync_through (window off) | 2334 | 2334 | 0.15 | 391.9 | 685.6 | 1.00 | 76 | n/a |
| 1 | 64 | fsys | append_batch(N) + sync_through (window off) | 2302 | 2302 | 0.15 | 399.7 | 631.8 | 1.00 | 76 | n/a |
| 1 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 2707 | 2707 | 1.39 | 335.0 | 478.7 | 1.00 | 4096 | 1.00 |
| 1 | 512 | store-io | (b) AppendBatch: N x append + commit | 2421 | 2421 | 1.24 | 375.2 | 726.0 | 1.00 | 4096 | 1.00 |
| 1 | 512 | raw | (a') N writes of one padded block + one flush | 2299 | 2299 | 1.18 | 385.3 | 921.0 | 1.00 | 4096 | n/a |
| 1 | 512 | raw | (b') packed: one write of N records + one flush | 2407 | 2407 | 1.23 | 357.5 | 769.9 | 1.00 | 4096 | n/a |
| 1 | 512 | fsys | append x N + sync_through (window off) | 2183 | 2183 | 1.12 | 422.3 | 698.0 | 1.00 | 524 | n/a |
| 1 | 512 | fsys | append_batch(N) + sync_through (window off) | 2230 | 2230 | 1.14 | 413.0 | 755.7 | 1.00 | 524 | n/a |
| 1 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 2659 | 2659 | 10.89 | 338.9 | 507.1 | 1.00 | 4096 | 1.00 |
| 1 | 4096 | store-io | (b) AppendBatch: N x append + commit | 2652 | 2652 | 10.86 | 343.8 | 507.3 | 1.00 | 4096 | 1.00 |
| 1 | 4096 | raw | (a') N writes of one padded block + one flush | 2603 | 2603 | 10.66 | 348.9 | 532.1 | 1.00 | 4096 | n/a |
| 1 | 4096 | raw | (b') packed: one write of N records + one flush | 2752 | 2752 | 11.27 | 329.2 | 516.6 | 1.00 | 4096 | n/a |
| 1 | 4096 | fsys | append x N + sync_through (window off) | 2172 | 2172 | 8.90 | 408.2 | 686.3 | 1.00 | 4108 | n/a |
| 1 | 4096 | fsys | append_batch(N) + sync_through (window off) | 2180 | 2180 | 8.93 | 419.2 | 717.7 | 1.00 | 4108 | n/a |
| 10 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 1810 | 18099 | 1.16 | 523.8 | 903.1 | 10.00 | 40960 | 1.00 |
| 10 | 64 | store-io | (b) AppendBatch: N x append + commit | 2629 | 26286 | 1.68 | 340.4 | 704.8 | 1.00 | 4096 | 1.00 |
| 10 | 64 | raw | (a') N writes of one padded block + one flush | 1387 | 13872 | 0.89 | 672.6 | 1181.2 | 10.00 | 40960 | n/a |
| 10 | 64 | raw | (b') packed: one write of N records + one flush | 2657 | 26572 | 1.70 | 340.7 | 565.1 | 1.00 | 4096 | n/a |
| 10 | 64 | fsys | append x N + sync_through (window off) | 2148 | 21475 | 1.37 | 433.7 | 665.9 | 10.00 | 760 | n/a |
| 10 | 64 | fsys | append_batch(N) + sync_through (window off) | 2075 | 20751 | 1.33 | 442.5 | 774.5 | 1.00 | 760 | n/a |
| 10 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 1294 | 12944 | 6.63 | 688.1 | 1159.3 | 10.00 | 40960 | 1.00 |
| 10 | 512 | store-io | (b) AppendBatch: N x append + commit | 2671 | 26713 | 13.68 | 339.4 | 506.2 | 1.00 | 8192 | 1.00 |
| 10 | 512 | raw | (a') N writes of one padded block + one flush | 1544 | 15437 | 7.90 | 629.8 | 978.6 | 10.00 | 40960 | n/a |
| 10 | 512 | raw | (b') packed: one write of N records + one flush | 2291 | 22914 | 11.73 | 397.5 | 775.4 | 1.00 | 8192 | n/a |
| 10 | 512 | fsys | append x N + sync_through (window off) | 2061 | 20607 | 10.55 | 453.2 | 764.6 | 10.00 | 5240 | n/a |
| 10 | 512 | fsys | append_batch(N) + sync_through (window off) | 2048 | 20475 | 10.48 | 437.3 | 1147.5 | 1.00 | 5240 | n/a |
| 10 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 1421 | 14206 | 58.19 | 677.6 | 1146.0 | 10.00 | 40960 | 1.00 |
| 10 | 4096 | store-io | (b) AppendBatch: N x append + commit | 2976 | 29756 | 121.88 | 327.2 | 471.8 | 1.00 | 40960 | 1.00 |
| 10 | 4096 | raw | (a') N writes of one padded block + one flush | 1685 | 16848 | 69.01 | 555.7 | 964.9 | 10.00 | 40960 | n/a |
| 10 | 4096 | raw | (b') packed: one write of N records + one flush | 2933 | 29333 | 120.15 | 333.7 | 484.9 | 1.00 | 40960 | n/a |
| 10 | 4096 | fsys | append x N + sync_through (window off) | 2000 | 19996 | 81.91 | 484.2 | 784.7 | 10.00 | 41080 | n/a |
| 10 | 4096 | fsys | append_batch(N) + sync_through (window off) | 2255 | 22550 | 92.36 | 433.2 | 678.3 | 1.00 | 41080 | n/a |
| 100 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 278 | 27780 | 1.78 | 3497.5 | 6032.1 | 100.00 | 409600 | 1.00 |
| 100 | 64 | store-io | (b) AppendBatch: N x append + commit | 2604 | 260400 | 16.67 | 347.6 | 516.3 | 1.00 | 8192 | 1.00 |
| 100 | 64 | raw | (a') N writes of one padded block + one flush | 286 | 28625 | 1.83 | 3385.0 | 5597.1 | 100.00 | 409600 | n/a |
| 100 | 64 | raw | (b') packed: one write of N records + one flush | 2706 | 270635 | 17.32 | 329.8 | 523.9 | 1.00 | 8192 | n/a |
| 100 | 64 | fsys | append x N + sync_through (window off) | 1509 | 150919 | 9.66 | 629.3 | 979.2 | 100.00 | 7600 | n/a |
| 100 | 64 | fsys | append_batch(N) + sync_through (window off) | 2131 | 213089 | 13.64 | 438.2 | 735.7 | 1.00 | 7600 | n/a |
| 100 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 313 | 31281 | 16.02 | 2992.4 | 5783.4 | 100.00 | 409600 | 1.00 |
| 100 | 512 | store-io | (b) AppendBatch: N x append + commit | 3328 | 332783 | 170.38 | 290.2 | 432.2 | 1.00 | 53248 | 1.00 |
| 100 | 512 | raw | (a') N writes of one padded block + one flush | 304 | 30443 | 15.59 | 3141.3 | 5587.6 | 100.00 | 409600 | n/a |
| 100 | 512 | raw | (b') packed: one write of N records + one flush | 3320 | 332030 | 170.00 | 291.3 | 501.8 | 1.00 | 53248 | n/a |
| 100 | 512 | fsys | append x N + sync_through (window off) | 1347 | 134719 | 68.98 | 719.9 | 1191.4 | 100.00 | 52400 | n/a |
| 100 | 512 | fsys | append_batch(N) + sync_through (window off) | 2366 | 236562 | 121.12 | 408.6 | 743.1 | 1.00 | 52400 | n/a |
| 100 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 281 | 28071 | 114.98 | 3477.8 | 5506.6 | 100.00 | 409600 | 1.00 |
| 100 | 4096 | store-io | (b) AppendBatch: N x append + commit | 1843 | 184325 | 755.00 | 509.6 | 836.2 | 1.00 | 409600 | 1.00 |
| 100 | 4096 | raw | (a') N writes of one padded block + one flush | 241 | 24112 | 98.76 | 4056.3 | 7058.4 | 100.00 | 409600 | n/a |
| 100 | 4096 | raw | (b') packed: one write of N records + one flush | 1853 | 185282 | 758.92 | 496.4 | 907.4 | 1.00 | 409600 | n/a |
| 100 | 4096 | fsys | append x N + sync_through (window off) | 734 | 73450 | 300.85 | 1328.0 | 2005.5 | 100.00 | 410800 | n/a |
| 100 | 4096 | fsys | append_batch(N) + sync_through (window off) | 1156 | 115607 | 473.53 | 835.2 | 1317.9 | 1.00 | 410800 | n/a |
| 1000 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 27 | 26860 | 1.72 | 36841.0 | 49074.5 | 1000.00 | 4096000 | 1.00 |
| 1000 | 64 | store-io | (b) AppendBatch: N x append + commit | 3756 | 3755762 | 240.37 | 240.0 | 556.0 | 1.00 | 65536 | 1.00 |
| 1000 | 64 | raw | (a') N writes of one padded block + one flush | 21 | 21125 | 1.35 | 40818.3 | 159364.9 | 1000.00 | 4096000 | n/a |
| 1000 | 64 | raw | (b') packed: one write of N records + one flush | 1463 | 1462559 | 93.60 | 447.4 | 1248.6 | 1.00 | 65536 | n/a |
| 1000 | 64 | fsys | append x N + sync_through (window off) | 309 | 309312 | 19.80 | 3165.2 | 4470.9 | 1000.00 | 76000 | n/a |
| 1000 | 64 | fsys | append_batch(N) + sync_through (window off) | 1231 | 1231004 | 78.78 | 558.0 | 2387.1 | 1.00 | 76000 | n/a |
| 1000 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 21 | 20637 | 10.57 | 43233.6 | 137630.4 | 1000.00 | 4096000 | 1.00 |
| 1000 | 512 | store-io | (b) AppendBatch: N x append + commit | 1780 | 1780413 | 911.57 | 530.1 | 927.2 | 1.00 | 512000 | 1.00 |
| 1000 | 512 | raw | (a') N writes of one padded block + one flush | 25 | 24831 | 12.71 | 34628.5 | 84055.3 | 1000.00 | 4096000 | n/a |
| 1000 | 512 | raw | (b') packed: one write of N records + one flush | 1801 | 1801246 | 922.24 | 510.4 | 1233.7 | 1.00 | 512000 | n/a |
| 1000 | 512 | fsys | append x N + sync_through (window off) | 213 | 213089 | 109.10 | 4543.3 | 6741.8 | 1000.00 | 524000 | n/a |
| 1000 | 512 | fsys | append_batch(N) + sync_through (window off) | 889 | 888751 | 455.04 | 1058.0 | 2071.1 | 1.00 | 524000 | n/a |
| 1000 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 27 | 27022 | 110.68 | 36277.9 | 50796.1 | 1000.00 | 4096000 | 1.00 |
| 1000 | 4096 | store-io | (b) AppendBatch: N x append + commit | 666 | 666284 | 2729.10 | 1440.2 | 2343.0 | 4.00 | 4096000 | 1.00 |
| 1000 | 4096 | raw | (a') N writes of one padded block + one flush | 26 | 25752 | 105.48 | 36707.1 | 73334.7 | 1000.00 | 4096000 | n/a |
| 1000 | 4096 | raw | (b') packed: one write of N records + one flush | 534 | 533686 | 2185.98 | 1515.7 | 6481.8 | 1.00 | 4096000 | n/a |
| 1000 | 4096 | fsys | append x N + sync_through (window off) | 102 | 102497 | 419.83 | 9618.7 | 12786.6 | 1000.00 | 4108000 | n/a |
| 1000 | 4096 | fsys | append_batch(N) + sync_through (window off) | 163 | 163214 | 668.52 | 5935.2 | 10420.1 | 1.00 | 4108000 | n/a |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 4392 records read back and compared byte for byte; all 4392 checked for overlap and alignment | provisioned a 108 MiB append region in 0.04 s (3017 MB/s: fill, flush, verify); ended at the 3000-operation cap after 1.10 s |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 4392 records read back and compared byte for byte; all 4392 checked for overlap and alignment | provisioned a 108 MiB append region in 0.03 s (4047 MB/s: fill, flush, verify); ended at the 3000-operation cap after 1.12 s |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 1.09 s |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 1.10 s |
| fsys | append x N + sync_through (window off) | passed: all 4142 records replayed with JournalReader and compared byte for byte | ended at the 3000-operation cap after 1.29 s |
| fsys | append_batch(N) + sync_through (window off) | passed: all 4149 records replayed with JournalReader and compared byte for byte | ended at the 3000-operation cap after 1.31 s |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 4320 records read back and compared byte for byte; all 4320 checked for overlap and alignment | provisioned a 108 MiB append region in 0.04 s (2985 MB/s: fill, flush, verify); ended at the 3000-operation cap after 1.11 s |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 4274 records read back and compared byte for byte; all 4274 checked for overlap and alignment | provisioned a 108 MiB append region in 0.03 s (4238 MB/s: fill, flush, verify); ended at the 3000-operation cap after 1.24 s |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 1.31 s |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 1.25 s |
| fsys | append x N + sync_through (window off) | passed: all 4092 records replayed with JournalReader and compared byte for byte | ended at the 3000-operation cap after 1.38 s |
| fsys | append_batch(N) + sync_through (window off) | passed: all 4102 records replayed with JournalReader and compared byte for byte | ended at the 3000-operation cap after 1.35 s |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 4329 records read back and compared byte for byte; all 4329 checked for overlap and alignment | provisioned a 108 MiB append region in 0.04 s (2955 MB/s: fill, flush, verify); ended at the 3000-operation cap after 1.13 s |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 4359 records read back and compared byte for byte; all 4359 checked for overlap and alignment | provisioned a 108 MiB append region in 0.03 s (3853 MB/s: fill, flush, verify); ended at the 3000-operation cap after 1.13 s |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 1.16 s |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 1.09 s |
| fsys | append x N + sync_through (window off) | passed: all 4133 records replayed with JournalReader and compared byte for byte | ended at the 3000-operation cap after 1.38 s |
| fsys | append_batch(N) + sync_through (window off) | passed: all 4125 records replayed with JournalReader and compared byte for byte | ended at the 3000-operation cap after 1.38 s |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 38710 records read back and compared byte for byte; all 38710 checked for overlap and alignment | provisioned a 1075 MiB append region in 0.29 s (3822 MB/s: fill, flush, verify); ended at the 3000-operation cap after 1.66 s |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 43710 records read back and compared byte for byte; all 43710 checked for overlap and alignment | provisioned a 108 MiB append region in 0.03 s (4112 MB/s: fill, flush, verify); ended at the 3000-operation cap after 1.15 s |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 2.17 s |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 1.13 s |
| fsys | append x N + sync_through (window off) | passed: all 41030 records replayed with JournalReader and compared byte for byte | ended at the 3000-operation cap after 1.40 s |
| fsys | append_batch(N) + sync_through (window off) | passed: all 40360 records replayed with JournalReader and compared byte for byte | ended at the 3000-operation cap after 1.45 s |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 36810 records read back and compared byte for byte; all 36810 checked for overlap and alignment | provisioned a 1075 MiB append region in 0.35 s (3234 MB/s: fill, flush, verify); ended at the 3000-operation cap after 2.33 s |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 43040 records read back and compared byte for byte; all 43040 checked for overlap and alignment | provisioned a 215 MiB append region in 0.06 s (4039 MB/s: fill, flush, verify); ended at the 3000-operation cap after 1.13 s |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 1.95 s |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 1.32 s |
| fsys | append x N + sync_through (window off) | passed: all 39960 records replayed with JournalReader and compared byte for byte | ended at the 3000-operation cap after 1.46 s |
| fsys | append_batch(N) + sync_through (window off) | passed: all 40770 records replayed with JournalReader and compared byte for byte | ended at the 3000-operation cap after 1.47 s |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 36930 records read back and compared byte for byte; all 36930 checked for overlap and alignment | provisioned a 1075 MiB append region in 0.33 s (3381 MB/s: fill, flush, verify); ended at the 3000-operation cap after 2.13 s |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 44610 records read back and compared byte for byte; all 44610 checked for overlap and alignment | provisioned a 1075 MiB append region in 0.32 s (3530 MB/s: fill, flush, verify); ended at the 3000-operation cap after 1.02 s |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 1.80 s |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 1.04 s |
| fsys | append x N + sync_through (window off) | passed: all 39690 records replayed with JournalReader and compared byte for byte | ended at the 3000-operation cap after 1.52 s |
| fsys | append_batch(N) + sync_through (window off) | passed: all 41630 records replayed with JournalReader and compared byte for byte | ended at the 3000-operation cap after 1.35 s |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 151800 records read back and compared byte for byte; all 151800 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.63 s (3411 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 429600 records read back and compared byte for byte; all 429600 checked for overlap and alignment | provisioned a 215 MiB append region in 0.06 s (3596 MB/s: fill, flush, verify); ended at the 3000-operation cap after 1.19 s |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 1.13 s |
| fsys | append x N + sync_through (window off) | passed: all 377900 records replayed with JournalReader and compared byte for byte | ended at the 3000-operation cap after 2.02 s |
| fsys | append_batch(N) + sync_through (window off) | passed: all 404700 records replayed with JournalReader and compared byte for byte | ended at the 3000-operation cap after 1.44 s |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 167400 records read back and compared byte for byte; all 167400 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.63 s (3389 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 461100 records read back and compared byte for byte; all 461100 checked for overlap and alignment | provisioned a 1397 MiB append region in 0.39 s (3709 MB/s: fill, flush, verify); ended at the 3000-operation cap after 0.95 s |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 0.95 s |
| fsys | append x N + sync_through (window off) | passed: all 365500 records replayed with JournalReader and compared byte for byte | ended at the 3000-operation cap after 2.28 s |
| fsys | append_batch(N) + sync_through (window off) | passed: all 417500 records replayed with JournalReader and compared byte for byte | ended at the 3000-operation cap after 1.32 s |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 151700 records read back and compared byte for byte; all 151700 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.64 s (3381 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 387900 records read back and compared byte for byte; all 387900 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.59 s (3629 MB/s: fill, flush, verify); ended at the 3000-operation cap after 1.82 s |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 1.80 s |
| fsys | append x N + sync_through (window off) | passed: all 333300 records replayed with JournalReader and compared byte for byte | ended at the 3000-operation cap after 4.28 s |
| fsys | append_batch(N) + sync_through (window off) | passed: all 356300 records replayed with JournalReader and compared byte for byte | ended at the 3000-operation cap after 2.78 s |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 146000 records read back and compared byte for byte; all 146000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.65 s (3315 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 4400000 records read back and compared byte for byte; all 4400000 checked for overlap and alignment | provisioned a 1719 MiB append region in 0.51 s (3522 MB/s: fill, flush, verify); ended at the 3000-operation cap after 1.13 s |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 2.35 s |
| fsys | append x N + sync_through (window off) | passed: all 1657000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 3758000 records replayed with JournalReader and compared byte for byte | ended at the 3000-operation cap after 2.64 s |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 118000 records read back and compared byte for byte; all 118000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.59 s (3638 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 3643000 records read back and compared byte for byte; all 3643000 checked for overlap and alignment | provisioned a 2048 MiB append region in 1.05 s (2041 MB/s: fill, flush, verify); ended at the 3000-operation cap after 2.06 s |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) | ended at the 3000-operation cap after 2.07 s |
| fsys | append x N + sync_through (window off) | passed: all 1122000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 3392000 records replayed with JournalReader and compared byte for byte | ended at the 3000-operation cap after 3.91 s |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 147000 records read back and compared byte for byte; all 147000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.81 s (2665 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 524000 records read back and compared byte for byte; all 524000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.62 s (3444 MB/s: fill, flush, verify); ended early at region capacity: 289 measured ops in 0.61 s |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 519000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 802000 records replayed with JournalReader and compared byte for byte |  |

## W4: page batch (N random 4 KiB pages, one barrier) (`page-batch`)

Wall time: 91.2 s.

**Method.**

- N in [1, 16, 64, 256] distinct pages chosen uniformly at random from a 256 MiB region / file for every commit; one commit = N page writes + one barrier; commits back to back on one thread.
- store-io: one `PageBatch` reused for every commit: N x `write(pos, page)` then `commit()`.
- raw QD1: N x WriteFile one after another, then one data flush.
- raw QD-N (Windows only): the N writes issued together on an overlapped NO_BUFFERING handle (one event each), all awaited, then one data flush on that handle.
- fsys: N x `Handle::write_at(path, offset, page)` then one `Handle::sync(path)`.
- Every system rewrites its own ready file / region; page payloads are copied once inside the timed section by every system.

Results:

| N | system | variant | commits/s | pages/s | commit p50 µs | commit p99 µs | commit p99.9 µs | write calls/commit | flushes/commit |
|---|---|---|---|---|---|---|---|---|---|
| 1 | store-io | PageBatch: N x write + commit | 1656 | 1656 | 389.1 | 3688.4 | 9145.9 | 1.00 | 1.00 |
| 1 | raw | QD1: N writes one at a time + one flush | 2249 | 2249 | 401.3 | 785.7 | 5498.3 | 1.00 | n/a |
| 1 | raw | QD-N: N overlapped writes in flight + one flush | 2362 | 2362 | 357.7 | 874.1 | 5451.8 | 1.00 | n/a |
| 1 | fsys | Handle::write_at x N + Handle::sync | 1302 | 1302 | 713.3 | 1490.6 | 5816.2 | 1.00 | n/a |
| 16 | store-io | PageBatch: N x write + commit | 2097 | 33552 | 416.3 | 983.7 | 5592.1 | 16.00 | 1.00 |
| 16 | raw | QD1: N writes one at a time + one flush | 831 | 13292 | 961.3 | 5152.2 | 11555.6 | 16.00 | n/a |
| 16 | raw | QD-N: N overlapped writes in flight + one flush | 1979 | 31664 | 458.1 | 926.4 | 5631.5 | 16.00 | n/a |
| 16 | fsys | Handle::write_at x N + Handle::sync | 196 | 3137 | 4973.3 | 9380.1 | n/a | 16.00 | n/a |
| 64 | store-io | PageBatch: N x write + commit | 754 | 48244 | 1214.9 | 4432.6 | 6105.0 | 64.00 | 1.00 |
| 64 | raw | QD1: N writes one at a time + one flush | 338 | 21627 | 2896.4 | 5635.4 | 13130.2 | 64.00 | n/a |
| 64 | raw | QD-N: N overlapped writes in flight + one flush | 1028 | 65802 | 914.5 | 1638.6 | 3660.0 | 64.00 | n/a |
| 64 | fsys | Handle::write_at x N + Handle::sync | 68 | 4344 | 14432.6 | 20983.5 | n/a | 64.00 | n/a |
| 256 | store-io | PageBatch: N x write + commit | 217 | 55462 | 4147.0 | 18539.4 | 21295.4 | 256.00 | 1.00 |
| 256 | raw | QD1: N writes one at a time + one flush | 61 | 15715 | 15741.2 | 30492.7 | n/a | 256.00 | n/a |
| 256 | raw | QD-N: N overlapped writes in flight + one flush | 272 | 69609 | 3494.5 | 5879.2 | 7640.7 | 256.00 | n/a |
| 256 | fsys | Handle::write_at x N + Handle::sync | 17 | 4458 | 55725.4 | n/a | n/a | 256.00 | n/a |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | PageBatch: N x write + commit | passed: 1025 of 8798 written pages read back (latest write each) and compared byte for byte |  |
| raw | QD1: N writes one at a time + one flush | passed: 1025 of 11121 written slots read back unbuffered and compared byte for byte |  |
| raw | QD-N: N overlapped writes in flight + one flush | passed: 1025 of 11713 written slots read back unbuffered and compared byte for byte | NtFlushBuffersFileEx on this overlapped handle returned STATUS_PENDING 0 times in 12930 flushes |
| fsys | Handle::write_at x N + Handle::sync | passed: 1025 of 6715 written slots read back unbuffered and compared byte for byte |  |
| store-io | PageBatch: N x write + commit | passed: 1025 of 60767 written pages read back (latest write each) and compared byte for byte |  |
| raw | QD1: N writes one at a time + one flush | passed: 1025 of 43145 written slots read back unbuffered and compared byte for byte |  |
| raw | QD-N: N overlapped writes in flight + one flush | passed: 1025 of 60535 written slots read back unbuffered and compared byte for byte | NtFlushBuffersFileEx on this overlapped handle returned STATUS_PENDING 0 times in 10545 flushes |
| fsys | Handle::write_at x N + Handle::sync | passed: 1025 of 15212 written slots read back unbuffered and compared byte for byte |  |
| store-io | PageBatch: N x write + commit | passed: 1025 of 63839 written pages read back (latest write each) and compared byte for byte |  |
| raw | QD1: N writes one at a time + one flush | passed: 1025 of 54149 written slots read back unbuffered and compared byte for byte |  |
| raw | QD-N: N overlapped writes in flight + one flush | passed: 1025 of 65178 written slots read back unbuffered and compared byte for byte | NtFlushBuffersFileEx on this overlapped handle returned STATUS_PENDING 0 times in 5363 flushes |
| fsys | Handle::write_at x N + Handle::sync | passed: 1025 of 20101 written slots read back unbuffered and compared byte for byte |  |
| store-io | PageBatch: N x write + commit | passed: 1025 of 64468 written pages read back (latest write each) and compared byte for byte |  |
| raw | QD1: N writes one at a time + one flush | passed: 1025 of 47445 written slots read back unbuffered and compared byte for byte |  |
| raw | QD-N: N overlapped writes in flight + one flush | passed: 1025 of 65278 written slots read back unbuffered and compared byte for byte | NtFlushBuffersFileEx on this overlapped handle returned STATUS_PENDING 0 times in 1411 flushes |
| fsys | Handle::write_at x N + Handle::sync | passed: 1025 of 20329 written slots read back unbuffered and compared byte for byte |  |

## W5: sequential bandwidth (1 MiB writes, 1/8 MiB reads) (`sequential`)

Wall time: 29.5 s.

**Method.**

- Write: one thread writes 1 MiB chunks back to back (not durable), then one durability barrier; MB/s = bytes / (time inside the write calls + the barrier); payload generation between writes is excluded (the wall clock is recorded as `wall_s` in the JSON). No warm-up (the region is written once); the point stops at 1 GiB or at the time box.
- store-io: `AppendRegion::append(1 MiB)` into a fresh 1 GiB append region, then `sync_through(last ticket)`. raw: copy into an aligned buffer + WriteFile per chunk into a ready 1 GiB file, then one data flush. fsys: journal `append(1 MiB)` + one `sync_through` on a fresh journal (buffered and direct, window off; fsys extends its file as it appends, store-io and raw overwrite preallocated space).
- Read: the bytes just written, front to back, repeated until the time box: store-io `AppendRegion::read(offset, &mut out)` with 1 MiB and 8 MiB `out`; raw ReadFile on a FILE_FLAG_NO_BUFFERING handle (one call per request) with the same request sizes. Latency is per request; MB/s = bytes / time inside the read calls (the content spot-check between requests is excluded).
- fsys read: not measured. fsys 1.1.3 has no unbuffered positioned read (`Handle::read_at` opens the file buffered on every call, and `JournalReader` replays through a buffered reader), so it would measure the page cache, not the device.
- MB = 10^6 bytes.

Results:

| system | variant | request | MB/s | bytes | seconds | barrier ms | request p50 µs | request p99 µs | request max µs | write calls/request |
|---|---|---|---|---|---|---|---|---|---|---|
| store-io | AppendRegion::append(1 MiB) x N + sync_through | 1048576 | 3031.6 | 1073741824 | 0.354 | 0.90 | 323.9 | 546.0 | 5266.7 | 1.00 |
| store-io | AppendRegion::read, 1 MiB requests | 1048576 | 2302.2 | 9834594304 | 4.272 | n/a | 441.3 | 727.0 | 1523.4 | 0.00 |
| store-io | AppendRegion::read, 8 MiB requests | 8388608 | 1800.1 | 8615100416 | 4.786 | n/a | 4333.5 | 11833.7 | 25116.9 | 0.00 |
| raw | 1 MiB writes + one data flush | 1048576 | 2866.6 | 1073741824 | 0.375 | 6.04 | 319.1 | 764.6 | 5333.5 | 1.00 |
| raw | unbuffered read, 1 MiB requests | 1048576 | 2989.7 | 10694426624 | 3.577 | n/a | 345.1 | 486.4 | 2425.5 | 0.00 |
| raw | unbuffered read, 8 MiB requests | 8388608 | 5455.6 | 24872222720 | 4.559 | n/a | 1521.2 | 1777.6 | 8125.6 | 0.00 |
| fsys | journal, buffered, window off: append(1 MiB) x N + sync_through | 1048576 | 867.8 | 1073741824 | 1.237 | 378.12 | 825.2 | 1403.5 | 2219.8 | 1.00 |
| fsys | journal, direct, window off: append(1 MiB) x N + sync_through | 1048576 | 773.0 | 1073741824 | 1.389 | 346.31 | 984.7 | 1748.2 | 2640.7 | 1.00 |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append(1 MiB) x N + sync_through | passed: all 1024 MiB read back through AppendRegion::read and compared byte for byte | provisioned a 1024 MiB append region in 0.34 s (3135 MB/s: fill, flush, verify) |
| store-io | AppendRegion::read, 1 MiB requests | passed: first 1 MiB of each of 10260 requests compared against what was written |  |
| store-io | AppendRegion::read, 8 MiB requests | passed: first 1 MiB of each of 1151 requests compared against what was written |  |
| raw | 1 MiB writes + one data flush | passed: all 1024 MiB read back unbuffered and compared byte for byte | file made ready (zero-filled unbuffered 1 MiB writes + flushed) in 0.26 s (4105 MB/s); compare store-io's provisioning note |
| raw | unbuffered read, 1 MiB requests | passed: first 1 MiB of each of 11166 requests compared against what was written |  |
| raw | unbuffered read, 8 MiB requests | passed: first 1 MiB of each of 3252 requests compared against what was written |  |
| fsys | journal, buffered, window off: append(1 MiB) x N + sync_through | passed: all 1024 records replayed with JournalReader and compared byte for byte |  |
| fsys | journal, direct, window off: append(1 MiB) x N + sync_through | passed: all 1024 records replayed with JournalReader and compared byte for byte |  |

