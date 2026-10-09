# store-io performance harness: windows / t-force-tm8fpz004t (2026-10-09)

## Run

| item | value |
|---|---|
| date (UTC) | 2026-10-09T09:10:21Z |
| command | page-batch |
| store-io commit | a51493bda14df48e5fe64575dfdf88192cdde941 (crates/ modified) |
| OS | Microsoft Windows [Version 10.0.26200.9457] |
| kernel / build | NT build 10.0.26200.9457 |
| CPU | AMD64 Family 26 Model 68 Stepping 0, AuthenticAMD |
| logical CPUs | 32 |
| filesystem | Ntfs (volume C:) |
| directory under test | C:\Users\james\AppData\Local\Temp\store-io-harness-102764 |
| backend | store-io-win (WinPlatform: overlapped I/O on IOCP, NO_BUFFERING) |
| durability class | flush-required (receipt label evidence, durable open Allowed, block size 4096 B) |
| time per point | 5.0 s measured after 0.5 s warm-up (time-boxed; a point may end early at region capacity, noted where it does) |
| fsys | fsys 1.1.3 (crates.io), Handle method Sync, durability primitive fsync |
| raw primitives | WriteFile on a FILE_FLAG_NO_BUFFERING handle + NtFlushBuffersFileEx(FLUSH_FLAGS_FILE_DATA_SYNC_ONLY); ReadFile on a FILE_FLAG_NO_BUFFERING handle (one call per request) |
| harness build | release, lto=fat, codegen-units=1 (same as store-io's release profile) |
| total wall time | 92.4 s |

## Device report (`Store::report()`)

```text
volume           0b6c99fb-ce44-1a00-637d-b42172422a01
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
| page-batch | 1 random 4 KiB pages + one barrier | pages/s (higher is better) | 2647.8 | 2776.6 | 1640.1 | 0.954 | -4.6% | 1.614 | +61.4% | PageBatch::commit vs raw QD-N writes + flush vs fsys write_at x N + sync |
| page-batch | 1 random 4 KiB pages + one barrier | commit p99 µs (lower is better) | 641.4 | 592.8 | 920.9 | 1.082 | -8.2% | 0.696 | +30.4% | PageBatch::commit vs raw QD-N writes + flush vs fsys write_at x N + sync |
| page-batch | 16 random 4 KiB pages + one barrier | pages/s (higher is better) | 31946.8 | 40886.3 | 2878.3 | 0.781 | -21.9% | 11.099 | +1009.9% | PageBatch::commit vs raw QD-N writes + flush vs fsys write_at x N + sync |
| page-batch | 16 random 4 KiB pages + one barrier | commit p99 µs (lower is better) | 1066.1 | 701.4 | 101547.8 | 1.520 | -52.0% | 0.010 | +99.0% | PageBatch::commit vs raw QD-N writes + flush vs fsys write_at x N + sync |
| page-batch | 64 random 4 KiB pages + one barrier | pages/s (higher is better) | 76400.5 | 51712.0 | 3390.9 | 1.477 | +47.7% | 22.531 | +2153.1% | PageBatch::commit vs raw QD-N writes + flush vs fsys write_at x N + sync |
| page-batch | 64 random 4 KiB pages + one barrier | commit p99 µs (lower is better) | 1694.6 | 1917.6 | 36443.9 | 0.884 | +11.6% | 0.046 | +95.4% | PageBatch::commit vs raw QD-N writes + flush vs fsys write_at x N + sync |
| page-batch | 256 random 4 KiB pages + one barrier | pages/s (higher is better) | 53992.0 | 91534.7 | 3188.7 | 0.590 | -41.0% | 16.932 | +1593.2% | PageBatch::commit vs raw QD-N writes + flush vs fsys write_at x N + sync |
| page-batch | 256 random 4 KiB pages + one barrier | commit p99 µs (lower is better) | 13853.2 | 4809.3 | n/a | 2.881 | -188.1% | n/a | n/a | PageBatch::commit vs raw QD-N writes + flush vs fsys write_at x N + sync |

## Anomalies, errors and integrity failures

- diagnostic: NtFlushBuffersFileEx(DATA_SYNC_ONLY) on a FILE_FLAG_OVERLAPPED | NO_BUFFERING handle (the kind store-io-win flushes through) after a dirty 4 KiB write never returned STATUS_PENDING: 0 of 400 idle and 0 of 400 while a second thread wrote the same file [measured]. The latent hazard in store-io-win's flush wrapper (STATUS_PENDING counted as success, IO_STATUS_BLOCK dropped) did not trigger on this machine; that is not proof it cannot

## W4: page batch (N random 4 KiB pages, one barrier) (`page-batch`)

Wall time: 92.0 s.

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
| 1 | store-io | PageBatch: N x write + commit | 2648 | 2648 | 336.6 | 641.4 | 5359.5 | 1.00 | 1.00 |
| 1 | raw | QD1: N writes one at a time + one flush | 2757 | 2757 | 325.2 | 538.0 | 5328.1 | 1.00 | n/a |
| 1 | raw | QD-N: N overlapped writes in flight + one flush | 2777 | 2777 | 322.4 | 592.8 | 5303.2 | 1.00 | n/a |
| 1 | fsys | Handle::write_at x N + Handle::sync | 1640 | 1640 | 573.1 | 920.9 | 5598.2 | 1.00 | n/a |
| 16 | store-io | PageBatch: N x write + commit | 1997 | 31947 | 415.9 | 1066.1 | 1384.1 | 16.00 | 1.00 |
| 16 | raw | QD1: N writes one at a time + one flush | 1499 | 23982 | 603.7 | 1306.3 | 2302.7 | 16.00 | n/a |
| 16 | raw | QD-N: N overlapped writes in flight + one flush | 2555 | 40886 | 354.2 | 701.4 | 899.7 | 16.00 | n/a |
| 16 | fsys | Handle::write_at x N + Handle::sync | 180 | 2878 | 3847.8 | 101547.8 | n/a | 16.00 | n/a |
| 64 | store-io | PageBatch: N x write + commit | 1194 | 76400 | 770.7 | 1694.6 | 4593.1 | 64.00 | 1.00 |
| 64 | raw | QD1: N writes one at a time + one flush | 447 | 28623 | 2024.2 | 5870.9 | 15150.8 | 64.00 | n/a |
| 64 | raw | QD-N: N overlapped writes in flight + one flush | 808 | 51712 | 1227.0 | 1917.6 | 8293.2 | 64.00 | n/a |
| 64 | fsys | Handle::write_at x N + Handle::sync | 53 | 3391 | 18432.9 | 36443.9 | n/a | 64.00 | n/a |
| 256 | store-io | PageBatch: N x write + commit | 211 | 53992 | 4328.5 | 13853.2 | 24926.3 | 256.00 | 1.00 |
| 256 | raw | QD1: N writes one at a time + one flush | 57 | 14695 | 12268.7 | 130516.6 | n/a | 256.00 | n/a |
| 256 | raw | QD-N: N overlapped writes in flight + one flush | 358 | 91535 | 2539.6 | 4809.3 | 6653.0 | 256.00 | n/a |
| 256 | fsys | Handle::write_at x N + Handle::sync | 12 | 3189 | 41148.3 | n/a | n/a | 256.00 | n/a |

Integrity checks and notes per measurement:

| system | variant | integrity | notes |
|---|---|---|---|
| store-io | PageBatch: N x write + commit | passed: 1025 of 13063 written pages read back (latest write each) and compared byte for byte |  |
| raw | QD1: N writes one at a time + one flush | passed: 1025 of 13509 written slots read back unbuffered and compared byte for byte |  |
| raw | QD-N: N overlapped writes in flight + one flush | passed: 1025 of 13627 written slots read back unbuffered and compared byte for byte | NtFlushBuffersFileEx on this overlapped handle returned STATUS_PENDING 0 times in 15289 flushes |
| fsys | Handle::write_at x N + Handle::sync | passed: 1025 of 8437 written slots read back unbuffered and compared byte for byte |  |
| store-io | PageBatch: N x write + commit | passed: 1025 of 60628 written pages read back (latest write each) and compared byte for byte |  |
| raw | QD1: N writes one at a time + one flush | passed: 1025 of 55981 written slots read back unbuffered and compared byte for byte |  |
| raw | QD-N: N overlapped writes in flight + one flush | passed: 1025 of 63330 written slots read back unbuffered and compared byte for byte | NtFlushBuffersFileEx on this overlapped handle returned STATUS_PENDING 0 times in 13859 flushes |
| fsys | Handle::write_at x N + Handle::sync | passed: 1025 of 14587 written slots read back unbuffered and compared byte for byte |  |
| store-io | PageBatch: N x write + commit | passed: 1025 of 65336 written pages read back (latest write each) and compared byte for byte |  |
| raw | QD1: N writes one at a time + one flush | passed: 1025 of 59431 written slots read back unbuffered and compared byte for byte |  |
| raw | QD-N: N overlapped writes in flight + one flush | passed: 1025 of 64577 written slots read back unbuffered and compared byte for byte | NtFlushBuffersFileEx on this overlapped handle returned STATUS_PENDING 0 times in 4314 flushes |
| fsys | Handle::write_at x N + Handle::sync | passed: 1025 of 16052 written slots read back unbuffered and compared byte for byte |  |
| store-io | PageBatch: N x write + commit | passed: 1025 of 64454 written pages read back (latest write each) and compared byte for byte |  |
| raw | QD1: N writes one at a time + one flush | passed: 1025 of 45893 written slots read back unbuffered and compared byte for byte |  |
| raw | QD-N: N overlapped writes in flight + one flush | passed: 1025 of 65496 written slots read back unbuffered and compared byte for byte | NtFlushBuffersFileEx on this overlapped handle returned STATUS_PENDING 0 times in 1849 flushes |
| fsys | Handle::write_at x N + Handle::sync | passed: 1025 of 19429 written slots read back unbuffered and compared byte for byte |  |

