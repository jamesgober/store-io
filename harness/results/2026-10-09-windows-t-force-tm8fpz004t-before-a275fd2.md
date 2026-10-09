# store-io performance harness: windows / t-force-tm8fpz004t (2026-10-09)

## Run

| item | value |
|---|---|
| date (UTC) | 2026-10-09T07:54:17Z |
| command | all |
| store-io commit | 23b38f8e6686c7c1b603edddac05b8bf6260e176 |
| OS | Microsoft Windows [Version 10.0.26200.9457] |
| kernel / build | NT build 10.0.26200.9457 |
| CPU | AMD64 Family 26 Model 68 Stepping 0, AuthenticAMD |
| logical CPUs | 32 |
| filesystem | Ntfs (volume C:) |
| directory under test | C:\Users\james\AppData\Local\Temp\store-io-harness-36316 |
| backend | store-io-win (WinPlatform: overlapped I/O on IOCP, NO_BUFFERING) |
| durability class | flush-required (receipt label evidence, durable open Allowed, block size 4096 B) |
| time per point | 5.0 s measured after 0.5 s warm-up (time-boxed; a point may end early at region capacity, noted where it does) |
| fsys | fsys 1.1.3 (crates.io), Handle method Sync, durability primitive fsync |
| raw primitives | WriteFile on a FILE_FLAG_NO_BUFFERING handle + NtFlushBuffersFileEx(FLUSH_FLAGS_FILE_DATA_SYNC_ONLY); ReadFile on a FILE_FLAG_NO_BUFFERING handle (one call per request) |
| harness build | release, lto=fat, codegen-units=1 (same as store-io's release profile) |
| total wall time | 729.3 s |

## Device report (`Store::report()`)

```text
volume           842302e6-5e62-4335-476f-0b810a67ef91
class            flush-required
label            evidence
durable open     Allowed
missing          storage stack layers below the filesystem
missing          page-cache residency (cachestat)
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
| lone | QD1 durable 4 KiB append | durable ops/s (higher is better) | 2658.1 | 2850.4 | 2334.7 | 0.933 | -6.7% | 1.139 | +13.9% | append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config) |
| lone | QD1 durable 4 KiB append | p50 µs (lower is better) | 325.8 | 318.0 | 392.8 | 1.025 | -2.5% | 0.829 | +17.1% | append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config) |
| lone | QD1 durable 4 KiB append | p99 µs (lower is better) | 737.8 | 438.0 | 781.0 | 1.684 | -68.4% | 0.945 | +5.5% | append_durable vs raw write+flush vs fsys journal append+sync_through (window off: fsys's best QD1 config) |
| lone | QD1 durable 4 KiB append, fsys default config | durable ops/s (higher is better) | 2658.1 | 2850.4 | 64.3 | 0.933 | -6.7% | 41.371 | +4037.1% | fsys JournalOptions::new() (500 µs group-commit window) |
| lone | QD1 durable 4 KiB page overwrite | durable ops/s (higher is better) | 2870.2 | 2850.4 | 1668.5 | 1.007 | +0.7% | 1.720 | +72.0% | write_durable vs raw write+flush vs fsys write_at+sync |
| lone | QD1 durable 4 KiB page overwrite | p50 µs (lower is better) | 310.6 | 318.0 | 559.4 | 0.977 | +2.3% | 0.555 | +44.5% | write_durable vs raw write+flush vs fsys write_at+sync |
| lone | QD1 durable 4 KiB page overwrite | p99 µs (lower is better) | 450.7 | 438.0 | 932.5 | 1.029 | -2.9% | 0.483 | +51.7% | write_durable vs raw write+flush vs fsys write_at+sync |
| concurrent | 1 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 2761.7 | 2897.5 | 2319.0 | 0.953 | -4.7% | 1.191 | +19.1% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 1 concurrent durable 4 KiB writers | p99 µs (lower is better) | 559.5 | 440.6 | 662.7 | 1.270 | -27.0% | 0.844 | +15.6% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 2 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 3549.7 | 2854.3 | 4101.9 | 1.244 | +24.4% | 0.865 | -13.5% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 2 concurrent durable 4 KiB writers | p99 µs (lower is better) | 934.3 | 6289.0 | 1236.4 | 0.149 | +85.1% | 0.756 | +24.4% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 4 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 5857.7 | 2851.0 | 3626.1 | 2.055 | +105.5% | 1.615 | +61.5% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 4 concurrent durable 4 KiB writers | p99 µs (lower is better) | 5506.0 | 13319.2 | 2267.7 | 0.413 | +58.7% | 2.428 | -142.8% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 8 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 9616.1 | 2739.1 | 6085.3 | 3.511 | +251.1% | 1.580 | +58.0% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 8 concurrent durable 4 KiB writers | p99 µs (lower is better) | 1308.3 | 20665.0 | 2356.2 | 0.063 | +93.7% | 0.555 | +44.5% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 16 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 16561.0 | 2705.3 | 11100.0 | 6.122 | +512.2% | 1.492 | +49.2% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 16 concurrent durable 4 KiB writers | p99 µs (lower is better) | 1749.2 | 46826.4 | 2423.9 | 0.037 | +96.3% | 0.722 | +27.8% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 32 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 28872.2 | 2315.7 | 18199.7 | 12.468 | +1146.8% | 1.586 | +58.6% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 32 concurrent durable 4 KiB writers | p99 µs (lower is better) | 2233.5 | 110857.7 | 2921.2 | 0.020 | +98.0% | 0.765 | +23.5% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| concurrent | 64 concurrent durable 4 KiB writers | durable ops/s (higher is better) | 35434.4 | 2587.8 | 31726.5 | 13.693 | +1269.3% | 1.117 | +11.7% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (default window, the better fsys config here) |
| concurrent | 64 concurrent durable 4 KiB writers | p99 µs (lower is better) | 22262.7 | 280552.2 | 3070.4 | 0.079 | +92.1% | 7.251 | -625.1% | store-io shared flushes vs raw own flush per writer vs fsys shared journal (window off, the better fsys config here) |
| caller-batch | batch of 1 x 64 B, one barrier | records/s (higher is better) | 2054.0 | 2607.5 | 2297.9 | 0.788 | -21.2% | 0.894 | -10.6% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(64 B) + one sync_through | records/s (higher is better) | 1353.8 | 1813.4 | 2312.1 | 0.747 | -25.3% | 0.586 | -41.4% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1 x 512 B, one barrier | records/s (higher is better) | 2728.5 | 2736.4 | 2286.7 | 0.997 | -0.3% | 1.193 | +19.3% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(512 B) + one sync_through | records/s (higher is better) | 2697.3 | 2706.8 | 2332.6 | 0.996 | -0.4% | 1.156 | +15.6% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1 x 4096 B, one barrier | records/s (higher is better) | 2719.7 | 2761.6 | 1981.1 | 0.985 | -1.5% | 1.373 | +37.3% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1 x append(4096 B) + one sync_through | records/s (higher is better) | 2700.8 | 2729.6 | 2312.7 | 0.989 | -1.1% | 1.168 | +16.8% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 64 B, one barrier | records/s (higher is better) | 26742.6 | 27599.9 | 20331.6 | 0.969 | -3.1% | 1.315 | +31.5% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(64 B) + one sync_through | records/s (higher is better) | 16933.3 | 15966.1 | 21476.8 | 1.061 | +6.1% | 0.788 | -21.2% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 512 B, one barrier | records/s (higher is better) | 24049.6 | 13080.6 | 21311.0 | 1.839 | +83.9% | 1.129 | +12.9% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(512 B) + one sync_through | records/s (higher is better) | 15440.5 | 12544.2 | 20072.2 | 1.231 | +23.1% | 0.769 | -23.1% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 10 x 4096 B, one barrier | records/s (higher is better) | 29964.2 | 30474.0 | 24035.0 | 0.983 | -1.7% | 1.247 | +24.7% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 10 x append(4096 B) + one sync_through | records/s (higher is better) | 12825.3 | 16284.3 | 21560.2 | 0.788 | -21.2% | 0.595 | -40.5% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 64 B, one barrier | records/s (higher is better) | 271601.2 | 272073.8 | 228087.7 | 0.998 | -0.2% | 1.191 | +19.1% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(64 B) + one sync_through | records/s (higher is better) | 30774.6 | 29675.0 | 155521.7 | 1.037 | +3.7% | 0.198 | -80.2% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 512 B, one barrier | records/s (higher is better) | 334187.0 | 274740.7 | 255766.3 | 1.216 | +21.6% | 1.307 | +30.7% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(512 B) + one sync_through | records/s (higher is better) | 32416.0 | 30285.4 | 147438.5 | 1.070 | +7.0% | 0.220 | -78.0% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 100 x 4096 B, one barrier | records/s (higher is better) | 194096.9 | 149058.4 | 141362.6 | 1.302 | +30.2% | 1.373 | +37.3% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 100 x append(4096 B) + one sync_through | records/s (higher is better) | 31827.0 | 21734.0 | 86630.0 | 1.464 | +46.4% | 0.367 | -63.3% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 64 B, one barrier | records/s (higher is better) | 4290601.5 | 4509942.7 | 2154420.2 | 0.951 | -4.9% | 1.992 | +99.2% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(64 B) + one sync_through | records/s (higher is better) | 34079.5 | 34494.9 | 459943.1 | 0.988 | -1.2% | 0.074 | -92.6% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 512 B, one barrier | records/s (higher is better) | 1765591.6 | 2051846.8 | 1085887.5 | 0.860 | -14.0% | 1.626 | +62.6% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(512 B) + one sync_through | records/s (higher is better) | 36981.8 | 34754.6 | 304940.8 | 1.064 | +6.4% | 0.121 | -87.9% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| caller-batch | batch of 1000 x 4096 B, one barrier | records/s (higher is better) | 665710.7 | 306987.5 | 210933.5 | 2.169 | +116.9% | 3.156 | +215.6% | AppendBatch::commit vs raw packed write+flush vs fsys append_batch+sync_through |
| caller-batch | 1000 x append(4096 B) + one sync_through | records/s (higher is better) | 31097.3 | 27827.4 | 145060.6 | 1.118 | +11.8% | 0.214 | -78.6% | N x append + sync_through vs raw N padded writes + flush vs fsys N x append + sync_through |
| page-batch | 1 random 4 KiB pages + one barrier | pages/s (higher is better) | 2757.4 | 2689.8 | 1667.7 | 1.025 | +2.5% | 1.653 | +65.3% | PageBatch::commit vs raw QD-N writes + flush vs fsys write_at x N + sync |
| page-batch | 1 random 4 KiB pages + one barrier | commit p99 µs (lower is better) | 505.1 | 550.0 | 912.3 | 0.918 | +8.2% | 0.554 | +44.6% | PageBatch::commit vs raw QD-N writes + flush vs fsys write_at x N + sync |
| page-batch | 16 random 4 KiB pages + one barrier | pages/s (higher is better) | 45554.0 | 42219.5 | 6309.9 | 1.079 | +7.9% | 7.219 | +621.9% | PageBatch::commit vs raw QD-N writes + flush vs fsys write_at x N + sync |
| page-batch | 16 random 4 KiB pages + one barrier | commit p99 µs (lower is better) | 764.2 | 1241.3 | 3961.0 | 0.616 | +38.4% | 0.193 | +80.7% | PageBatch::commit vs raw QD-N writes + flush vs fsys write_at x N + sync |
| page-batch | 64 random 4 KiB pages + one barrier | pages/s (higher is better) | 71828.5 | 74442.0 | 5703.5 | 0.965 | -3.5% | 12.594 | +1159.4% | PageBatch::commit vs raw QD-N writes + flush vs fsys write_at x N + sync |
| page-batch | 64 random 4 KiB pages + one barrier | commit p99 µs (lower is better) | 2943.0 | 1418.1 | 15082.0 | 2.075 | -107.5% | 0.195 | +80.5% | PageBatch::commit vs raw QD-N writes + flush vs fsys write_at x N + sync |
| page-batch | 256 random 4 KiB pages + one barrier | pages/s (higher is better) | 134360.9 | 139825.0 | 8247.5 | 0.961 | -3.9% | 16.291 | +1529.1% | PageBatch::commit vs raw QD-N writes + flush vs fsys write_at x N + sync |
| page-batch | 256 random 4 KiB pages + one barrier | commit p99 µs (lower is better) | 6977.6 | 2729.5 | 38640.0 | 2.556 | -155.6% | 0.181 | +81.9% | PageBatch::commit vs raw QD-N writes + flush vs fsys write_at x N + sync |
| sequential | sequential 1 MiB writes, one barrier at the end | MB/s (higher is better) | 4394.1 | 4831.1 | 1367.3 | 0.910 | -9.0% | 3.214 | +221.4% | fsys: the better of its buffered and direct journals |
| sequential | sequential read, 1 MiB requests | MB/s (higher is better) | 3079.3 | 3420.4 | n/a | 0.900 | -10.0% | n/a | n/a | fsys: no comparable unbuffered read (see method) |
| sequential | sequential read, 8 MiB requests | MB/s (higher is better) | 3282.1 | 5924.3 | n/a | 0.554 | -44.6% | n/a | n/a | fsys: no comparable unbuffered read (see method) |

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
| store-io | AppendRegion::append_durable | 2658 | 325.8 | 422.8 | 737.8 | 5333.6 | 6162.5 | 376.2 | 1.00 | 1.00 | 1.00 |
| store-io | AppendRegion::append + sync_through (halves timed separately) | 2769 | 319.5 | 391.4 | 531.7 | 5323.6 | 6225.7 | 361.2 | 1.00 | 1.00 | 1.00 |
| store-io | PageRegion::write_durable | 2870 | 310.6 | 352.6 | 450.7 | 5295.7 | 6195.7 | 348.4 | 1.00 | 1.00 | 1.00 |
| raw | write + data flush | 2850 | 318.0 | 350.6 | 438.0 | 5285.2 | 6204.7 | 350.8 | n/a | 1.00 | 1.00 |
| fsys | journal, default (buffered, 500 µs group-commit window): append + sync_through | 64 | 15528.7 | 15941.7 | 16253.1 | n/a | 20985.6 | 15563.8 | n/a | 1.00 | 1.00 |
| fsys | journal, buffered, window off: append + sync_through | 2335 | 392.8 | 457.1 | 781.0 | 5384.4 | 6168.0 | 428.3 | n/a | 1.00 | 1.00 |
| fsys | journal, direct, window off: append + sync_through | 2356 | 389.5 | 459.2 | 735.9 | 5386.5 | 5764.2 | 424.4 | n/a | 1.00 | 1.00 |
| fsys | Handle::write_at + Handle::sync | 1669 | 559.4 | 647.3 | 932.5 | 5540.7 | 7602.0 | 599.3 | n/a | 1.00 | 14.00 |

Where the time goes (write call vs durability call, timed separately on the same thread):

| system | variant | write p50 µs | write p99 µs | write mean µs | durability p50 µs | durability p99 µs | durability mean µs |
|---|---|---|---|---|---|---|---|
| store-io | AppendRegion::append + sync_through (halves timed separately) | 30.1 | 89.0 | 34.0 | 284.2 | 504.2 | 326.4 |
| raw | write + data flush | 33.5 | 86.8 | 34.4 | 282.4 | 380.7 | 315.3 |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append_durable | passed: 2049 of 14605 records read back and compared byte for byte; all 14605 checked for overlap and alignment | provisioned a 538 MiB append region in 0.39 s (1452 MB/s: fill, flush, verify) |
| store-io | AppendRegion::append + sync_through (halves timed separately) | passed: 2049 of 15232 records read back and compared byte for byte; all 15232 checked for overlap and alignment | provisioned a 538 MiB append region in 0.15 s (3827 MB/s: fill, flush, verify) |
| store-io | PageRegion::write_durable | passed: 2049 of 15757 written pages read back (latest write each) and compared byte for byte |  |
| raw | write + data flush | passed: 2049 of 15699 written slots read back unbuffered and compared byte for byte | file made ready (zero-filled unbuffered + flushed) in 0.05 s (5338 MB/s) |
| fsys | journal, default (buffered, 500 µs group-commit window): append + sync_through | passed: all 355 records replayed with JournalReader and compared byte for byte | journal backend KernelBuffered, direct active false |
| fsys | journal, buffered, window off: append + sync_through | passed: all 12821 records replayed with JournalReader and compared byte for byte | journal backend KernelBuffered, direct active false |
| fsys | journal, direct, window off: append + sync_through | passed: all 12904 records replayed with JournalReader and compared byte for byte | journal backend KernelBuffered, direct active false |
| fsys | Handle::write_at + Handle::sync | passed: 2049 of 9155 written slots read back unbuffered and compared byte for byte |  |

## W2: concurrent durable writers, 4 KiB (`concurrent`)

Wall time: 158.4 s.

**Method.**

- Writer counts [1, 2, 4, 8, 16, 32, 64]; every thread loops a durable 4 KiB write at queue depth 1 for the whole point.
- store-io: all threads call `AppendRegion::append_durable` on one append region of one store (a fresh store per point). Flush sharing is read from `Store::domain_stats()` deltas over the window: `flushes` issued, barriers that `led` a flush, barriers that `joined` one.
- raw: each thread writes its own contiguous range of one ready 256 MiB file and makes it durable itself with WriteFile on a FILE_FLAG_NO_BUFFERING handle + NtFlushBuffersFileEx(FLUSH_FLAGS_FILE_DATA_SYNC_ONLY) (no sharing).
- fsys: all threads call `append` + `sync_through` on one shared journal (fresh per point), window off and default (500 µs group-commit window).
- An operation counts when it started and finished inside the measured window; rate = counted operations / window.

Results:

| threads | system | variant | durable ops/s | p50 µs | p99 µs | p99.9 µs | flushes issued | barriers led | barriers joined | writes per flush |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | store-io | AppendRegion::append_durable (shared region) | 2762 | 324.3 | 559.5 | 5291.4 | 13810 | 13810 | 0 | 1.00 |
| 1 | raw | own write + own data flush (per thread range) | 2898 | 305.0 | 440.6 | 5293.4 | n/a | n/a | n/a | n/a |
| 1 | fsys | journal, buffered, window off: shared journal append + sync_through | 2319 | 395.7 | 662.7 | 5395.3 | n/a | n/a | n/a | n/a |
| 1 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 64 | 15523.7 | 16350.3 | n/a | n/a | n/a | n/a | n/a |
| 2 | store-io | AppendRegion::append_durable (shared region) | 3550 | 554.6 | 934.3 | 5622.3 | 17746 | 17746 | 5 | 1.00 |
| 2 | raw | own write + own data flush (per thread range) | 2854 | 326.5 | 6289.0 | 10940.9 | n/a | n/a | n/a | n/a |
| 2 | fsys | journal, buffered, window off: shared journal append + sync_through | 4102 | 400.5 | 1236.4 | 5331.3 | n/a | n/a | n/a | n/a |
| 2 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 117 | 15533.7 | 93165.1 | n/a | n/a | n/a | n/a | n/a |
| 4 | store-io | AppendRegion::append_durable (shared region) | 5858 | 594.3 | 5506.0 | 5882.7 | 16734 | 16734 | 12559 | 1.75 |
| 4 | raw | own write + own data flush (per thread range) | 2851 | 331.0 | 13319.2 | 24001.0 | n/a | n/a | n/a | n/a |
| 4 | fsys | journal, buffered, window off: shared journal append + sync_through | 3626 | 1039.7 | 2267.7 | 6139.1 | n/a | n/a | n/a | n/a |
| 4 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 207 | 15533.7 | 124133.6 | 139985.6 | n/a | n/a | n/a | n/a |
| 8 | store-io | AppendRegion::append_durable (shared region) | 9616 | 706.9 | 1308.3 | 6254.3 | 14309 | 14309 | 33780 | 3.36 |
| 8 | raw | own write + own data flush (per thread range) | 2739 | 366.4 | 20665.0 | 32421.7 | n/a | n/a | n/a | n/a |
| 8 | fsys | journal, buffered, window off: shared journal append + sync_through | 6085 | 1100.4 | 2356.2 | 15187.8 | n/a | n/a | n/a | n/a |
| 8 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 512 | 15526.5 | 16170.0 | 16648.5 | n/a | n/a | n/a | n/a |
| 16 | store-io | AppendRegion::append_durable (shared region) | 16561 | 926.7 | 1749.2 | 6294.7 | 13290 | 13290 | 69539 | 6.23 |
| 16 | raw | own write + own data flush (per thread range) | 2705 | 364.4 | 46826.4 | 78621.8 | n/a | n/a | n/a | n/a |
| 16 | fsys | journal, buffered, window off: shared journal append + sync_through | 10678 | 1450.0 | 2455.8 | 7084.6 | n/a | n/a | n/a | n/a |
| 16 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 11100 | 1439.7 | 2423.9 | 7073.1 | n/a | n/a | n/a | n/a |
| 32 | store-io | AppendRegion::append_durable (shared region) | 28872 | 964.7 | 2233.5 | 6786.3 | 12512 | 12512 | 131879 | 11.54 |
| 32 | raw | own write + own data flush (per thread range) | 2316 | 500.6 | 110857.7 | 201962.8 | n/a | n/a | n/a | n/a |
| 32 | fsys | journal, buffered, window off: shared journal append + sync_through | 18200 | 1829.6 | 2921.2 | 7204.3 | n/a | n/a | n/a | n/a |
| 32 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 14637 | 2192.2 | 3574.1 | 4840.6 | n/a | n/a | n/a | n/a |
| 64 | store-io | AppendRegion::append_durable (shared region) | 35434 | 911.1 | 22262.7 | 169246.4 | 15589 | 15589 | 161640 | 11.37 |
| 64 | raw | own write + own data flush (per thread range) | 2588 | 368.9 | 280552.2 | 480978.3 | n/a | n/a | n/a | n/a |
| 64 | fsys | journal, buffered, window off: shared journal append + sync_through | 31431 | 2030.9 | 3070.4 | 7679.5 | n/a | n/a | n/a | n/a |
| 64 | fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | 31726 | 2008.9 | 3260.6 | 7387.6 | n/a | n/a | n/a | n/a |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 15118 records read back and compared byte for byte; all 15118 checked for overlap and alignment | provisioned a 860 MiB append region in 0.22 s (4072 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1025 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 12793 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 355 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 19528 records read back and compared byte for byte; all 19528 checked for overlap and alignment | provisioned a 860 MiB append region in 0.22 s (4176 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1026 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 22458 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 656 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 32299 records read back and compared byte for byte; all 32299 checked for overlap and alignment | provisioned a 860 MiB append region in 0.24 s (3754 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1028 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 20154 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 1112 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.004 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 53249 records read back and compared byte for byte; all 53249 checked for overlap and alignment | provisioned a 860 MiB append region in 0.24 s (3744 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1032 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 33757 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 2832 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 89435 records read back and compared byte for byte; all 89435 checked for overlap and alignment | provisioned a 860 MiB append region in 0.25 s (3671 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| raw | own write + own data flush (per thread range) | passed: 1040 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 58989 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 61015 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 153589 records read back and compared byte for byte; all 153589 checked for overlap and alignment | provisioned a 860 MiB append region in 0.23 s (3945 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1); domain counters: 153582 barriers for 153589 durable appends |
| raw | own write + own data flush (per thread range) | passed: 1056 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.003 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 100368 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 81634 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| store-io | AppendRegion::append_durable (shared region) | passed: 1025 of 190537 records read back and compared byte for byte; all 190537 checked for overlap and alignment | provisioned a 860 MiB append region in 0.24 s (3787 MB/s: fill, flush, verify); OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1); domain counters: 190525 barriers for 190537 durable appends |
| raw | own write + own data flush (per thread range) | passed: 1088 latest writes (sampled across all threads) read back unbuffered and compared | OS write calls in window / counted ops = 1.005 (straddling ops make this slightly above 1) |
| fsys | journal, buffered, window off: shared journal append + sync_through | passed: all 173165 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.000 (straddling ops make this slightly above 1) |
| fsys | journal, default (buffered, 500 µs group-commit window): shared journal append + sync_through | passed: all 174798 records replayed with JournalReader and compared byte for byte | OS write calls in window / counted ops = 1.001 (straddling ops make this slightly above 1) |

**Findings.**

- store-io scaling: 2762 durable ops/s at 1 writer, 35434 at 64 writers (11.37 writes per flush) [derived from the table].

## W3: caller batch (N records, one barrier) (`caller-batch`)

Wall time: 407.4 s.

**Method.**

- N in [1, 10, 100, 1000], record sizes [64, 512, 4096] bytes; one commit = N records made durable by one barrier; commits run back to back on one thread.
- store-io (a) `N x append + sync_through`: each append is its own write, padded to a block (documented behaviour). (b) `AppendBatch`: records packed back to back into pooled buffers, `commit()` = one reservation, the fewest writes (largest pooled buffer 1 MiB), one barrier. A fresh store per (N, size).
- raw (a') N writes of one zero-padded block each, then one flush; (b') the N records packed into one aligned buffer padded to a block, one write, one flush. Primitive: WriteFile on a FILE_FLAG_NO_BUFFERING handle + NtFlushBuffersFileEx(FLUSH_FLAGS_FILE_DATA_SYNC_ONLY). A ready 1 GiB file, offsets cycling.
- fsys: `append` x N + `sync_through(last lsn)`, and `append_batch(&records)` + `sync_through`, on a fresh journal per point, window off.
- records/s = commits/s x N; payload MB/s counts caller bytes only (10^6 bytes); `write calls/commit` and `bytes written/commit` come from the OS counters and show the device writes each design issues per commit.

Results:

| N | record B | system | variant | commits/s | records/s | payload MB/s | commit p50 µs | commit p99 µs | write calls/commit | bytes written/commit | flushes/commit |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 1354 | 1354 | 0.09 | 390.2 | 1538.5 | 1.00 | 4096 | 1.00 |
| 1 | 64 | store-io | (b) AppendBatch: N x append + commit | 2054 | 2054 | 0.13 | 343.8 | 1403.0 | 1.00 | 4096 | 1.00 |
| 1 | 64 | raw | (a') N writes of one padded block + one flush | 1813 | 1813 | 0.12 | 357.7 | 828.8 | 1.00 | 4096 | n/a |
| 1 | 64 | raw | (b') packed: one write of N records + one flush | 2607 | 2607 | 0.17 | 342.6 | 722.7 | 1.00 | 4096 | n/a |
| 1 | 64 | fsys | append x N + sync_through (window off) | 2312 | 2312 | 0.15 | 398.9 | 671.5 | 1.00 | 76 | n/a |
| 1 | 64 | fsys | append_batch(N) + sync_through (window off) | 2298 | 2298 | 0.15 | 401.2 | 645.7 | 1.00 | 76 | n/a |
| 1 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 2697 | 2697 | 1.38 | 337.6 | 541.4 | 1.00 | 4096 | 1.00 |
| 1 | 512 | store-io | (b) AppendBatch: N x append + commit | 2729 | 2729 | 1.40 | 333.6 | 496.8 | 1.00 | 4096 | 1.00 |
| 1 | 512 | raw | (a') N writes of one padded block + one flush | 2707 | 2707 | 1.39 | 337.1 | 488.5 | 1.00 | 4096 | n/a |
| 1 | 512 | raw | (b') packed: one write of N records + one flush | 2736 | 2736 | 1.40 | 331.9 | 493.9 | 1.00 | 4096 | n/a |
| 1 | 512 | fsys | append x N + sync_through (window off) | 2333 | 2333 | 1.19 | 392.5 | 630.9 | 1.00 | 524 | n/a |
| 1 | 512 | fsys | append_batch(N) + sync_through (window off) | 2287 | 2287 | 1.17 | 402.9 | 608.7 | 1.00 | 524 | n/a |
| 1 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 2701 | 2701 | 11.06 | 336.4 | 473.3 | 1.00 | 4096 | 1.00 |
| 1 | 4096 | store-io | (b) AppendBatch: N x append + commit | 2720 | 2720 | 11.14 | 335.4 | 454.8 | 1.00 | 4096 | 1.00 |
| 1 | 4096 | raw | (a') N writes of one padded block + one flush | 2730 | 2730 | 11.18 | 333.0 | 494.4 | 1.00 | 4096 | n/a |
| 1 | 4096 | raw | (b') packed: one write of N records + one flush | 2762 | 2762 | 11.31 | 328.5 | 448.3 | 1.00 | 4096 | n/a |
| 1 | 4096 | fsys | append x N + sync_through (window off) | 2313 | 2313 | 9.47 | 395.5 | 608.5 | 1.00 | 4108 | n/a |
| 1 | 4096 | fsys | append_batch(N) + sync_through (window off) | 1981 | 1981 | 8.11 | 420.1 | 812.5 | 1.00 | 4108 | n/a |
| 10 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 1693 | 16933 | 1.08 | 567.7 | 901.8 | 10.00 | 40960 | 1.00 |
| 10 | 64 | store-io | (b) AppendBatch: N x append + commit | 2674 | 26743 | 1.71 | 337.6 | 489.0 | 1.00 | 4096 | 1.00 |
| 10 | 64 | raw | (a') N writes of one padded block + one flush | 1597 | 15966 | 1.02 | 609.6 | 961.1 | 10.00 | 40960 | n/a |
| 10 | 64 | raw | (b') packed: one write of N records + one flush | 2760 | 27600 | 1.77 | 328.0 | 462.0 | 1.00 | 4096 | n/a |
| 10 | 64 | fsys | append x N + sync_through (window off) | 2148 | 21477 | 1.37 | 430.6 | 669.4 | 10.00 | 760 | n/a |
| 10 | 64 | fsys | append_batch(N) + sync_through (window off) | 2033 | 20332 | 1.30 | 429.1 | 1408.0 | 1.00 | 760 | n/a |
| 10 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 1544 | 15441 | 7.91 | 619.1 | 1135.5 | 10.00 | 40960 | 1.00 |
| 10 | 512 | store-io | (b) AppendBatch: N x append + commit | 2405 | 24050 | 12.31 | 371.4 | 777.6 | 1.00 | 8192 | 1.00 |
| 10 | 512 | raw | (a') N writes of one padded block + one flush | 1254 | 12544 | 6.42 | 780.1 | 1305.3 | 10.00 | 40960 | n/a |
| 10 | 512 | raw | (b') packed: one write of N records + one flush | 1308 | 13081 | 6.70 | 418.4 | 885.8 | 1.00 | 8192 | n/a |
| 10 | 512 | fsys | append x N + sync_through (window off) | 2007 | 20072 | 10.28 | 453.6 | 867.6 | 10.00 | 5240 | n/a |
| 10 | 512 | fsys | append_batch(N) + sync_through (window off) | 2131 | 21311 | 10.91 | 426.1 | 1333.4 | 1.00 | 5240 | n/a |
| 10 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 1283 | 12825 | 52.53 | 644.5 | 2544.2 | 10.00 | 40960 | 1.00 |
| 10 | 4096 | store-io | (b) AppendBatch: N x append + commit | 2996 | 29964 | 122.73 | 328.4 | 453.2 | 1.00 | 40960 | 1.00 |
| 10 | 4096 | raw | (a') N writes of one padded block + one flush | 1628 | 16284 | 66.70 | 595.8 | 967.3 | 10.00 | 40960 | n/a |
| 10 | 4096 | raw | (b') packed: one write of N records + one flush | 3047 | 30474 | 124.82 | 322.0 | 447.0 | 1.00 | 40960 | n/a |
| 10 | 4096 | fsys | append x N + sync_through (window off) | 2156 | 21560 | 88.31 | 450.1 | 732.7 | 10.00 | 41080 | n/a |
| 10 | 4096 | fsys | append_batch(N) + sync_through (window off) | 2403 | 24035 | 98.45 | 406.8 | 632.4 | 1.00 | 41080 | n/a |
| 100 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 308 | 30775 | 1.97 | 3141.6 | 5100.9 | 100.00 | 409600 | 1.00 |
| 100 | 64 | store-io | (b) AppendBatch: N x append + commit | 2716 | 271601 | 17.38 | 333.7 | 480.3 | 1.00 | 8192 | 1.00 |
| 100 | 64 | raw | (a') N writes of one padded block + one flush | 297 | 29675 | 1.90 | 3203.6 | 6276.5 | 100.00 | 409600 | n/a |
| 100 | 64 | raw | (b') packed: one write of N records + one flush | 2721 | 272074 | 17.41 | 332.7 | 497.1 | 1.00 | 8192 | n/a |
| 100 | 64 | fsys | append x N + sync_through (window off) | 1555 | 155522 | 9.95 | 608.6 | 987.7 | 100.00 | 7600 | n/a |
| 100 | 64 | fsys | append_batch(N) + sync_through (window off) | 2281 | 228088 | 14.60 | 407.8 | 647.2 | 1.00 | 7600 | n/a |
| 100 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 324 | 32416 | 16.60 | 2992.0 | 4901.5 | 100.00 | 409600 | 1.00 |
| 100 | 512 | store-io | (b) AppendBatch: N x append + commit | 3342 | 334187 | 171.10 | 294.1 | 410.7 | 1.00 | 53248 | 1.00 |
| 100 | 512 | raw | (a') N writes of one padded block + one flush | 303 | 30285 | 15.51 | 3230.9 | 5058.6 | 100.00 | 409600 | n/a |
| 100 | 512 | raw | (b') packed: one write of N records + one flush | 2747 | 274741 | 140.67 | 294.4 | 1703.5 | 1.00 | 53248 | n/a |
| 100 | 512 | fsys | append x N + sync_through (window off) | 1474 | 147439 | 75.49 | 661.3 | 1054.0 | 100.00 | 52400 | n/a |
| 100 | 512 | fsys | append_batch(N) + sync_through (window off) | 2558 | 255766 | 130.95 | 384.3 | 619.7 | 1.00 | 52400 | n/a |
| 100 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 318 | 31827 | 130.36 | 3057.9 | 4807.9 | 100.00 | 409600 | 1.00 |
| 100 | 4096 | store-io | (b) AppendBatch: N x append + commit | 1941 | 194097 | 795.02 | 465.1 | 5333.1 | 1.00 | 409600 | 1.00 |
| 100 | 4096 | raw | (a') N writes of one padded block + one flush | 217 | 21734 | 89.02 | 4470.8 | 7317.2 | 100.00 | 409600 | n/a |
| 100 | 4096 | raw | (b') packed: one write of N records + one flush | 1491 | 149058 | 610.54 | 454.3 | 5325.5 | 1.00 | 409600 | n/a |
| 100 | 4096 | fsys | append x N + sync_through (window off) | 866 | 86630 | 354.84 | 1125.8 | 1650.9 | 100.00 | 410800 | n/a |
| 100 | 4096 | fsys | append_batch(N) + sync_through (window off) | 1414 | 141363 | 579.02 | 685.6 | 1005.2 | 1.00 | 410800 | n/a |
| 1000 | 64 | store-io | (a) N x AppendRegion::append + sync_through | 34 | 34080 | 2.18 | 28873.6 | 40952.0 | 1000.00 | 4096000 | 1.00 |
| 1000 | 64 | store-io | (b) AppendBatch: N x append + commit | 4291 | 4290601 | 274.60 | 223.0 | 405.6 | 1.00 | 65536 | 1.00 |
| 1000 | 64 | raw | (a') N writes of one padded block + one flush | 34 | 34495 | 2.21 | 28894.9 | 39129.9 | 1000.00 | 4096000 | n/a |
| 1000 | 64 | raw | (b') packed: one write of N records + one flush | 4510 | 4509943 | 288.64 | 212.8 | 392.4 | 1.00 | 65536 | n/a |
| 1000 | 64 | fsys | append x N + sync_through (window off) | 460 | 459943 | 29.44 | 2125.5 | 2723.7 | 1000.00 | 76000 | n/a |
| 1000 | 64 | fsys | append_batch(N) + sync_through (window off) | 2154 | 2154420 | 137.88 | 442.2 | 675.0 | 1.00 | 76000 | n/a |
| 1000 | 512 | store-io | (a) N x AppendRegion::append + sync_through | 37 | 36982 | 18.93 | 24073.4 | 54328.4 | 1000.00 | 4096000 | 1.00 |
| 1000 | 512 | store-io | (b) AppendBatch: N x append + commit | 1766 | 1765592 | 903.98 | 441.7 | 2118.7 | 1.00 | 512000 | 1.00 |
| 1000 | 512 | raw | (a') N writes of one padded block + one flush | 35 | 34755 | 17.79 | 26643.6 | 56015.0 | 1000.00 | 4096000 | n/a |
| 1000 | 512 | raw | (b') packed: one write of N records + one flush | 2052 | 2051847 | 1050.55 | 448.9 | 856.4 | 1.00 | 512000 | n/a |
| 1000 | 512 | fsys | append x N + sync_through (window off) | 305 | 304941 | 156.13 | 3141.6 | 5607.1 | 1000.00 | 524000 | n/a |
| 1000 | 512 | fsys | append_batch(N) + sync_through (window off) | 1086 | 1085887 | 555.97 | 760.5 | 2938.6 | 1.00 | 524000 | n/a |
| 1000 | 4096 | store-io | (a) N x AppendRegion::append + sync_through | 31 | 31097 | 127.37 | 30867.4 | 77096.2 | 1000.00 | 4096000 | 1.00 |
| 1000 | 4096 | store-io | (b) AppendBatch: N x append + commit | 666 | 665711 | 2726.75 | 1433.2 | 2312.0 | 4.00 | 4096000 | 1.00 |
| 1000 | 4096 | raw | (a') N writes of one padded block + one flush | 28 | 27827 | 113.98 | 34064.3 | 57707.6 | 1000.00 | 4096000 | n/a |
| 1000 | 4096 | raw | (b') packed: one write of N records + one flush | 307 | 306988 | 1257.42 | 1534.6 | 93968.3 | 1.00 | 4096000 | n/a |
| 1000 | 4096 | fsys | append x N + sync_through (window off) | 145 | 145061 | 594.17 | 6199.7 | 12386.7 | 1000.00 | 4108000 | n/a |
| 1000 | 4096 | fsys | append_batch(N) + sync_through (window off) | 211 | 210933 | 863.98 | 4047.5 | 10696.5 | 1.00 | 4108000 | n/a |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 7494 records read back and compared byte for byte; all 7494 checked for overlap and alignment | provisioned a 108 MiB append region in 0.04 s (2882 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 11588 records read back and compared byte for byte; all 11588 checked for overlap and alignment | provisioned a 108 MiB append region in 0.03 s (4010 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 12666 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 12617 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 14817 records read back and compared byte for byte; all 14817 checked for overlap and alignment | provisioned a 108 MiB append region in 0.04 s (2776 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 15004 records read back and compared byte for byte; all 15004 checked for overlap and alignment | provisioned a 108 MiB append region in 0.03 s (4035 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 12816 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 12530 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 14761 records read back and compared byte for byte; all 14761 checked for overlap and alignment | provisioned a 108 MiB append region in 0.04 s (2890 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 14944 records read back and compared byte for byte; all 14944 checked for overlap and alignment | provisioned a 108 MiB append region in 0.03 s (3930 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 12667 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 11049 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 92900 records read back and compared byte for byte; all 92900 checked for overlap and alignment | provisioned a 1075 MiB append region in 0.29 s (3891 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 146730 records read back and compared byte for byte; all 146730 checked for overlap and alignment | provisioned a 108 MiB append region in 0.03 s (3990 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 118110 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 112050 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 84670 records read back and compared byte for byte; all 84670 checked for overlap and alignment | provisioned a 1075 MiB append region in 0.29 s (3949 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 130900 records read back and compared byte for byte; all 130900 checked for overlap and alignment | provisioned a 215 MiB append region in 0.07 s (3399 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 104770 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 116750 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 71310 records read back and compared byte for byte; all 71310 checked for overlap and alignment | provisioned a 1075 MiB append region in 0.30 s (3801 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 162620 records read back and compared byte for byte; all 162620 checked for overlap and alignment | provisioned a 1075 MiB append region in 0.29 s (3888 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 10 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 117200 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 131170 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 166700 records read back and compared byte for byte; all 166700 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.55 s (3888 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 1466600 records read back and compared byte for byte; all 1466600 checked for overlap and alignment | provisioned a 215 MiB append region in 0.05 s (4287 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 852100 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 1240800 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 176400 records read back and compared byte for byte; all 176400 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.54 s (3944 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 1786400 records read back and compared byte for byte; all 1786400 checked for overlap and alignment | provisioned a 1397 MiB append region in 0.34 s (4257 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 797300 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 1380300 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 170400 records read back and compared byte for byte; all 170400 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.52 s (4124 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 524200 records read back and compared byte for byte; all 524200 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.50 s (4329 MB/s: fill, flush, verify); ended early at region capacity: 4252 measured ops in 2.31 s |
| raw | (a') N writes of one padded block + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 100 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 461100 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 746800 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 187000 records read back and compared byte for byte; all 187000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.53 s (4034 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 19006000 records read back and compared byte for byte; all 19006000 checked for overlap and alignment | provisioned a 1719 MiB append region in 0.42 s (4257 MB/s: fill, flush, verify) |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 2480000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 10664000 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 202000 records read back and compared byte for byte; all 202000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.53 s (4046 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 4194000 records read back and compared byte for byte; all 4194000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.49 s (4376 MB/s: fill, flush, verify); ended early at region capacity: 3695 measured ops in 2.31 s |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 1649000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 5717000 records replayed with JournalReader and compared byte for byte |  |
| store-io | (a) N x AppendRegion::append + sync_through | passed: 1025 of 173000 records read back and compared byte for byte; all 173000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.53 s (4083 MB/s: fill, flush, verify) |
| store-io | (b) AppendBatch: N x append + commit | passed: 1025 of 524000 records read back and compared byte for byte; all 524000 checked for overlap and alignment | provisioned a 2048 MiB append region in 0.58 s (3710 MB/s: fill, flush, verify); ended early at region capacity: 243 measured ops in 0.48 s |
| raw | (a') N writes of one padded block + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| raw | (b') packed: one write of N records + one flush | passed: all 1000 records of the last commit read back unbuffered and compared (earlier commits are overwritten as offsets cycle) |  |
| fsys | append x N + sync_through (window off) | passed: all 719000 records replayed with JournalReader and compared byte for byte |  |
| fsys | append_batch(N) + sync_through (window off) | passed: all 1118000 records replayed with JournalReader and compared byte for byte |  |

## W4: page batch (N random 4 KiB pages, one barrier) (`page-batch`)

Wall time: 90.4 s.

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
| 1 | store-io | PageBatch: N x write + commit | 2757 | 2757 | 328.0 | 505.1 | 5331.6 | 1.00 | 1.00 |
| 1 | raw | QD1: N writes one at a time + one flush | 2738 | 2738 | 331.0 | 532.8 | 5317.0 | 1.00 | n/a |
| 1 | raw | QD-N: N overlapped writes in flight + one flush | 2690 | 2690 | 335.2 | 550.0 | 5324.8 | 1.00 | n/a |
| 1 | fsys | Handle::write_at x N + Handle::sync | 1668 | 1668 | 567.2 | 912.3 | 5561.1 | 1.00 | n/a |
| 16 | store-io | PageBatch: N x write + commit | 2847 | 45554 | 320.6 | 764.2 | 961.1 | 16.00 | 1.00 |
| 16 | raw | QD1: N writes one at a time + one flush | 1638 | 26201 | 538.3 | 1408.8 | 2648.2 | 16.00 | n/a |
| 16 | raw | QD-N: N overlapped writes in flight + one flush | 2639 | 42219 | 322.9 | 1241.3 | 2336.3 | 16.00 | n/a |
| 16 | fsys | Handle::write_at x N + Handle::sync | 394 | 6310 | 2437.3 | 3961.0 | 9478.6 | 16.00 | n/a |
| 64 | store-io | PageBatch: N x write + commit | 1122 | 71829 | 694.1 | 2943.0 | 3550.4 | 64.00 | 1.00 |
| 64 | raw | QD1: N writes one at a time + one flush | 465 | 29792 | 2043.7 | 3657.6 | 5862.7 | 64.00 | n/a |
| 64 | raw | QD-N: N overlapped writes in flight + one flush | 1163 | 74442 | 833.3 | 1418.1 | 1730.9 | 64.00 | n/a |
| 64 | fsys | Handle::write_at x N + Handle::sync | 89 | 5704 | 11150.3 | 15082.0 | n/a | 64.00 | n/a |
| 256 | store-io | PageBatch: N x write + commit | 525 | 134361 | 1631.2 | 6977.6 | 8157.4 | 256.00 | 1.00 |
| 256 | raw | QD1: N writes one at a time + one flush | 159 | 40672 | 5968.5 | 12688.5 | n/a | 256.00 | n/a |
| 256 | raw | QD-N: N overlapped writes in flight + one flush | 546 | 139825 | 1724.0 | 2729.5 | 3616.6 | 256.00 | n/a |
| 256 | fsys | Handle::write_at x N + Handle::sync | 32 | 8248 | 30691.8 | 38640.0 | n/a | 256.00 | n/a |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | PageBatch: N x write + commit | passed: 1025 of 13444 written pages read back (latest write each) and compared byte for byte |  |
| raw | QD1: N writes one at a time + one flush | passed: 1025 of 13435 written slots read back unbuffered and compared byte for byte |  |
| raw | QD-N: N overlapped writes in flight + one flush | passed: 1025 of 13176 written slots read back unbuffered and compared byte for byte | NtFlushBuffersFileEx on this overlapped handle returned STATUS_PENDING 0 times in 14754 flushes |
| fsys | Handle::write_at x N + Handle::sync | passed: 1025 of 8581 written slots read back unbuffered and compared byte for byte |  |
| store-io | PageBatch: N x write + commit | passed: 1025 of 63786 written pages read back (latest write each) and compared byte for byte |  |
| raw | QD1: N writes one at a time + one flush | passed: 1025 of 58229 written slots read back unbuffered and compared byte for byte |  |
| raw | QD-N: N overlapped writes in flight + one flush | passed: 1025 of 63055 written slots read back unbuffered and compared byte for byte | NtFlushBuffersFileEx on this overlapped handle returned STATUS_PENDING 0 times in 13405 flushes |
| fsys | Handle::write_at x N + Handle::sync | passed: 1025 of 27054 written slots read back unbuffered and compared byte for byte |  |
| store-io | PageBatch: N x write + commit | passed: 1025 of 65272 written pages read back (latest write each) and compared byte for byte |  |
| raw | QD1: N writes one at a time + one flush | passed: 1025 of 60211 written slots read back unbuffered and compared byte for byte |  |
| raw | QD-N: N overlapped writes in flight + one flush | passed: 1025 of 65409 written slots read back unbuffered and compared byte for byte | NtFlushBuffersFileEx on this overlapped handle returned STATUS_PENDING 0 times in 6335 flushes |
| fsys | Handle::write_at x N + Handle::sync | passed: 1025 of 25039 written slots read back unbuffered and compared byte for byte |  |
| store-io | PageBatch: N x write + commit | passed: 1025 of 65535 written pages read back (latest write each) and compared byte for byte |  |
| raw | QD1: N writes one at a time + one flush | passed: 1025 of 62935 written slots read back unbuffered and compared byte for byte |  |
| raw | QD-N: N overlapped writes in flight + one flush | passed: 1025 of 65535 written slots read back unbuffered and compared byte for byte | NtFlushBuffersFileEx on this overlapped handle returned STATUS_PENDING 0 times in 2801 flushes |
| fsys | Handle::write_at x N + Handle::sync | passed: 1025 of 32442 written slots read back unbuffered and compared byte for byte |  |

## W5: sequential bandwidth (1 MiB writes, 1/8 MiB reads) (`sequential`)

Wall time: 26.6 s.

**Method.**

- Write: one thread writes 1 MiB chunks back to back (not durable), then one durability barrier; MB/s = bytes / (time inside the write calls + the barrier); payload generation between writes is excluded (the wall clock is recorded as `wall_s` in the JSON). No warm-up (the region is written once); the point stops at 1 GiB or at the time box.
- store-io: `AppendRegion::append(1 MiB)` into a fresh 1 GiB append region, then `sync_through(last ticket)`. raw: copy into an aligned buffer + WriteFile per chunk into a ready 1 GiB file, then one data flush. fsys: journal `append(1 MiB)` + one `sync_through` on a fresh journal (buffered and direct, window off; fsys extends its file as it appends, store-io and raw overwrite preallocated space).
- Read: the bytes just written, front to back, repeated until the time box: store-io `AppendRegion::read(offset, &mut out)` with 1 MiB and 8 MiB `out`; raw ReadFile on a FILE_FLAG_NO_BUFFERING handle (one call per request) with the same request sizes. Latency is per request; MB/s = bytes / time inside the read calls (the content spot-check between requests is excluded).
- fsys read: not measured. fsys 1.1.3 has no unbuffered positioned read (`Handle::read_at` opens the file buffered on every call, and `JournalReader` replays through a buffered reader), so it would measure the page cache, not the device.
- MB = 10^6 bytes.

Results:

| system | variant | request | MB/s | bytes | seconds | barrier ms | request p50 µs | request p99 µs | request max µs | write calls/request |
|---|---|---|---|---|---|---|---|---|---|---|
| store-io | AppendRegion::append(1 MiB) x N + sync_through | 1048576 | 4394.1 | 1073741824 | 0.244 | 0.36 | 235.5 | 322.7 | 471.3 | 1.00 |
| store-io | AppendRegion::read, 1 MiB requests | 1048576 | 3079.3 | 14440988672 | 4.690 | n/a | 340.2 | 438.7 | 1040.2 | 0.00 |
| store-io | AppendRegion::read, 8 MiB requests | 8388608 | 3282.1 | 16231956480 | 4.946 | n/a | 2525.8 | 3119.7 | 3968.2 | 0.00 |
| raw | 1 MiB writes + one data flush | 1048576 | 4831.1 | 1073741824 | 0.222 | 0.43 | 206.5 | 312.7 | 5307.8 | 1.00 |
| raw | unbuffered read, 1 MiB requests | 1048576 | 3420.4 | 15241052160 | 4.456 | n/a | 307.3 | 391.4 | 702.2 | 0.00 |
| raw | unbuffered read, 8 MiB requests | 8388608 | 5924.3 | 28630319104 | 4.833 | n/a | 1400.7 | 1564.4 | 7937.1 | 0.00 |
| fsys | journal, buffered, window off: append(1 MiB) x N + sync_through | 1048576 | 1367.3 | 1073741824 | 0.785 | 270.48 | 488.0 | 762.6 | 960.5 | 1.00 |
| fsys | journal, direct, window off: append(1 MiB) x N + sync_through | 1048576 | 1183.3 | 1073741824 | 0.907 | 274.83 | 580.7 | 1270.5 | 4697.1 | 1.00 |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | AppendRegion::append(1 MiB) x N + sync_through | passed: all 1024 MiB read back through AppendRegion::read and compared byte for byte | provisioned a 1024 MiB append region in 0.26 s (4091 MB/s: fill, flush, verify) |
| store-io | AppendRegion::read, 1 MiB requests | passed: first 1 MiB of each of 15165 requests compared against what was written |  |
| store-io | AppendRegion::read, 8 MiB requests | passed: first 1 MiB of each of 2139 requests compared against what was written |  |
| raw | 1 MiB writes + one data flush | passed: all 1024 MiB read back unbuffered and compared byte for byte | file made ready (zero-filled unbuffered 1 MiB writes + flushed) in 0.20 s (5377 MB/s); compare store-io's provisioning note |
| raw | unbuffered read, 1 MiB requests | passed: first 1 MiB of each of 15987 requests compared against what was written |  |
| raw | unbuffered read, 8 MiB requests | passed: first 1 MiB of each of 3748 requests compared against what was written |  |
| fsys | journal, buffered, window off: append(1 MiB) x N + sync_through | passed: all 1024 records replayed with JournalReader and compared byte for byte |  |
| fsys | journal, direct, window off: append(1 MiB) x N + sync_through | passed: all 1024 records replayed with JournalReader and compared byte for byte |  |

