# store-io &mdash; Platforms

> What store-io does on each operating system, and why: the system calls behind every durability primitive, how files are opened, locked, allocated and released, where the device evidence comes from, and what each backend cannot do yet. Windows, Linux and macOS have native backends; the deterministic simulator runs everywhere. Each section names the source it describes.

---

## Contents

1. [Backends at a glance](#1-backends-at-a-glance)
2. [Durability primitives per platform](#2-durability-primitives-per-platform)
3. [Windows](#3-windows)
4. [Linux](#4-linux)
5. [macOS](#5-macos)
6. [The simulator](#6-the-simulator)

---

## 1. Backends at a glance

| | Windows | Linux | macOS | Simulator |
|---|---|---|---|---|
| Crate | `store-io-win` | `store-io-posix` | `store-io-posix` (its `mac/` modules) | `store-io-sim` |
| Status | Native backend | Native backend (synchronous and io_uring tiers) | Native backend (synchronous) | Tests and conformance |
| Data I/O | Unbuffered overlapped `WriteFile` / `ReadFile`, reaped from a per-queue I/O completion port | `O_DIRECT` `pwritev2` / `preadv2`, or io_uring | `pwrite` / `pread` (`F_NOCACHE` as a hint), one system call per operation | In-memory model of one device |
| Barrier | `NtFlushBuffersFileEx` data-sync on NTFS, `FlushFileBuffers` elsewhere | `fdatasync` | `F_FULLFSYNC` | Flush of the writes completed before it |
| Ownership lock | `LockFileEx` on a sentinel byte | Open-file-description lock | Open-file-description lock (macOS 14 and later), `flock` before | Per-file flag |

The `store-io` crate's `NativePlatform` is the Windows backend on Windows and `store-io-posix`'s `PosixPlatform` on Linux and macOS. On other targets the engine and the simulator build, but there is no native backend. Both native crates compile to empty shells on the operating systems they do not serve.

The engine above every backend is the same: it issues only block-aligned transfers from aligned pooled buffers, decides the durability class from the evidence a backend reports ([Durability](./DURABILITY.md)), and never retries a failed write or flush.

Source: [`src/lib.rs`](../src/lib.rs), [`crates/store-io-platform/src/lib.rs`](../crates/store-io-platform/src/lib.rs)

---

## 2. Durability primitives per platform

The engine asks a backend for eight things. The table lists what each backend does for each, and what it guarantees.

| Primitive | Used for | Windows | Linux | macOS | Guarantee |
|---|---|---|---|---|---|
| **Data flush** (`FlushData`) | Every barrier on a flush-required, unverified or unsafe store; metadata commits on those stores | NTFS: `NtFlushBuffersFileEx(FLUSH_FLAGS_FILE_DATA_SYNC_ONLY)`. Other file systems: `FlushFileBuffers`. Chosen once when the file is opened. | `fdatasync` | `F_FULLFSYNC` (`fsync` on a volume that refused it: an overridden Unsafe store) | The file's written data, the metadata needed to read it back, and a device cache flush. Covers the writes that completed before it was submitted. |
| **Durable write** (`dsync`) | Every write on a power-safe store | The write, then the same data flush on the same handle, before the completion is reported. Never `FILE_FLAG_WRITE_THROUGH`. | Files: `pwritev2(RWF_DSYNC)`. Block devices: a plain `pwritev2`, then `fdatasync`. | `pwrite`, then `F_FULLFSYNC` (no class is power-safe on macOS, so the engine does not ask for it in practice) | The write is durable when it completes. |
| **Full flush** (`flush_all`) | Creating a store, the start of every writable open, provisioning, directory-mode files | `FlushFileBuffers` | `fsync` | `F_FULLFSYNC` | All data and metadata of the file, including its length, and a device cache flush. |
| **Directory flush** (`sync_dir`) | Creating a store, directory-mode replace | `FlushFileBuffers` on a directory handle opened with add-file access | `fsync` on the directory descriptor | `F_FULLFSYNC` on the directory descriptor | The directory's entries (create, rename, delete) are durable. An unsupported directory flush is an error, never success. |
| **Atomic replace** (`rename_replace`) | Directory-mode replace | `SetFileInformationByHandle(FileRenameInfoEx, REPLACE_IF_EXISTS \| POSIX_SEMANTICS)` on the open temporary file | `renameat` within the directory descriptor | `renameat` within the directory descriptor (no replace: `renameatx_np(RENAME_EXCL)`) | The target names the old or the new file, never neither. Durable only after the directory flush. |
| **Ownership lock** (`lock_exclusive`) | Every writable open of a store | `LockFileEx(EXCLUSIVE \| FAIL_IMMEDIATELY)` on one byte at offset 2^63 &minus; 2 | `fcntl(F_OFD_SETLK, F_WRLCK)` over the whole file, on the I/O descriptor's open file description (`flock(LOCK_EX \| LOCK_NB)` before Linux 3.15) | `fcntl(F_OFD_SETLK, F_WRLCK)` over the whole file (macOS 14 and later); `flock(LOCK_EX \| LOCK_NB)` before | One owner at a time; released when the owner's handle (or process) goes away. |
| **Reader lock** (`lock_shared`) | Every read-only open of a store | `LockFileEx(FAIL_IMMEDIATELY)` (shared) on the same byte | `fcntl(F_OFD_SETLK, F_RDLCK)` over the whole file (`flock(LOCK_SH \| LOCK_NB)` before Linux 3.15) | `fcntl(F_OFD_SETLK, F_RDLCK)` over the whole file (macOS 14 and later); `flock(LOCK_SH \| LOCK_NB)` before | Any number of readers, on read-only handles; readers and the owner exclude each other. |
| **Coarse clock** (`monotonic_ms`) | `oldest_outstanding` queries only, never the I/O path | `GetTickCount64` (10-16 ms resolution) | `clock_gettime(CLOCK_MONOTONIC_COARSE)` (one kernel tick, 1-10 ms) | `clock_gettime(CLOCK_UPTIME_RAW_APPROX)` (a value cached at context switch; stops in sleep) | Monotonic milliseconds, read from memory the kernel shares (on Linux through the vDSO where the architecture has one). |

How the engine combines them: on a power-safe store every write is a durable write and a barrier issues nothing; on every other class writes are plain and a barrier is one data flush shared between concurrent writers ([Durability](./DURABILITY.md), sections 7&ndash;8). No backend asks for FUA (forced unit access) writes directly: the Linux kernel may use FUA inside `RWF_DSYNC` as it chooses, the Windows backend never relies on write-through, whose FUA path is not certified, and macOS has no write-through primitive that flushes the drive.

Source: [`crates/store-io-win/src/queue.rs`](../crates/store-io-win/src/queue.rs), [`crates/store-io-win/src/file.rs`](../crates/store-io-win/src/file.rs), [`crates/store-io-win/src/dir.rs`](../crates/store-io-win/src/dir.rs), [`crates/store-io-win/src/lock.rs`](../crates/store-io-win/src/lock.rs), [`crates/store-io-posix/src/queue.rs`](../crates/store-io-posix/src/queue.rs), [`crates/store-io-posix/src/lock.rs`](../crates/store-io-posix/src/lock.rs), [`crates/store-io-posix/src/lib.rs`](../crates/store-io-posix/src/lib.rs), [`crates/store-io-posix/src/mac/`](../crates/store-io-posix/src/mac/)

---

## 3. Windows

### Files and directories

- **Data files** are opened `FILE_FLAG_NO_BUFFERING | FILE_FLAG_OVERLAPPED | FILE_FLAG_OPEN_REPARSE_POINT`, sharing read, write and delete with every other opener. Exclusivity comes from the ownership lock, never from share modes. A read-write handle has `GENERIC_READ | GENERIC_WRITE | DELETE`; a read-only handle only `GENERIC_READ`. A name that turns out to be a symlink, junction or directory is refused. New files are created with `CREATE_NEW`, so an existing file is never overwritten.
- **Directories** are opened with `FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT` and add-file access, which `FlushFileBuffers` needs; a reparse point is refused. Creating a directory creates one level and then flushes its parent, opened (following reparse points) with only `FILE_ADD_SUBDIRECTORY`, the right the create needed. The parent is flushed also when the directory already exists (an earlier attempt may have died before the flush), unless it cannot be opened for that, is write-protected, or cannot flush a directory (SMB, WSL shares). A directory the call made is removed again if its flush fails.
- **Delete** opens the file with delete access and sets `FileDispositionInfoEx(DELETE | POSIX_SEMANTICS)`, so the name goes at once and open handles keep working.
- **A killed owner's lock waits for its writes.** The lock is on the store's own handle and a queue writes through handles of its own (below), so releasing the one does not by itself wait for the other. Observed, not documented by Microsoft: a killed process's lock is not free until its in-flight writes have completed or been cancelled. Tested (`tests/kill.rs`, run once in CI): a child holds the lock with 512 MiB of unbuffered writes in flight (in odd rounds all issued by threads that have since exited) and is killed; the new owner's writes, made the moment it holds the lock, are never overwritten by the dead owner's. On the dev box (Windows 11 build 26200, NTFS, a consumer NVMe drive), the same writes made at the kill without waiting for the lock were overwritten in 7 of 10 rounds at 512 MiB in flight, so the test can see a late write, and the median wait for the lock grew with what was in flight: 0.4 ms for 256 KiB, 1-5 ms for 32 MiB, 110-220 ms for 512 MiB (at worst 7.1 s).
- **Fallbacks are recorded, not silent.** Where the POSIX-semantics rename or delete is not supported, the backend falls back to the legacy information class (and, for delete, `DeleteFileW`) and records that on the directory handle (`used_legacy_rename`, `used_legacy_delete`).

### The I/O queue

- Every queue owns its own I/O completion port. A file object can be tied to only one port, so each queue opens its own handle to a file with `ReOpenFile` (same file, no path lookup, `NO_BUFFERING | OVERLAPPED`) on first use and associates that handle with its port. Any number of queues can therefore write the same file, and each reaps only its own completions. A queue keeps up to 64 such handles and evicts an idle one when it needs more.
- Each per-queue handle has `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS | FILE_SKIP_SET_EVENT_ON_HANDLE`: a `WriteFile` or `ReadFile` that succeeds at once is completed inline and queues no packet; `ERROR_IO_PENDING` means a packet will come. Any other immediate failure is reported as a failed *completion*, not as a refused submission, because the backend cannot prove such a request never reached the device. The engine treats it as durability unknown.
- Completions are reaped with `GetQueuedCompletionStatusEx`, waiting either `INFINITE` or not at all. No wait in the backend has a finite timeout.
- A data flush runs synchronously inside `submit`. The trailing flush of a durable write runs where its completion is observed.
- Dropping a queue with operations in flight cancels them (`CancelIoEx`) and waits for every completion packet before any buffer or handle is freed. If the port cannot be waited on, buffers and handles are leaked rather than freed under the kernel.
- **Idle.** An idle store makes no system calls and has no timers: none of its threads is switched in or runs a CPU cycle (checked in CI).

### Flushes

The data-flush primitive is chosen once, when a file is opened, from its file system and recorded on the handle: `NtFlushBuffersFileEx` with `FLUSH_FLAGS_FILE_DATA_SYNC_ONLY` on NTFS (data, the metadata needed to read it, and a device cache flush), and `FlushFileBuffers` on any other file system. A full flush is always `FlushFileBuffers`. Every flush goes through one synchronous (non-overlapped, unbuffered) handle per writable file, never through an overlapped I/O handle: on an overlapped handle a flush may return `STATUS_PENDING` and finish later, so a flush that returned would not prove anything. A pending status, should one ever appear, is an error. A durable write is the write followed by the data flush; durability at completion never depends on `FILE_FLAG_WRITE_THROUGH`. On a power-safe store this means every write also pays a flush call, which the device itself should complete without work.

### Space

- **Allocation** sets `FileAllocationInfo` and then `FileEndOfFileInfo` to the new length. It never calls `SetFileValidData`, which needs a privilege and would expose stale clusters; the engine fills new space with sequential direct writes, which moves the valid data length forward with them.
- **Release** trims the region's data area in place (`FSCTL_FILE_LEVEL_TRIM`): the clusters stay allocated to the container and the device may discard their contents. The engine never deallocates on Windows, because that needs the file marked sparse (`FSCTL_SET_SPARSE`) for good, and a sparse file may allocate on overwrite, which would cost the store its power-safe class. A trim the volume does not support is not an error: the space simply stays as it was, and later provisioning refills it before reuse. The backend still implements deallocation (`FSCTL_SET_ZERO_DATA` on a sparse file) for callers of the platform layer.
- **Range state** for provisioning checks: whether the range lies below the valid data length, from `FSCTL_QUERY_FILE_REGIONS` (NTFS answers only the cached-data query, and reports `Unknown` if it refuses or truncates the answer). Page-cache residency is reported as zero, because every handle the backend opens bypasses the cache. Unwritten and shared extents are `Unknown`: there is no unprivileged NTFS query for them.

### The probe

Everything runs unprivileged. The physical drive is opened with desired access 0, which `IOCTL_STORAGE_QUERY_PROPERTY` accepts for a standard user. Every answer is parsed as untrusted bytes with bounds checks; what cannot be read stays unknown and is listed as missing.

| Fact | Source |
|---|---|
| File system | `GetVolumeInformationByHandleW` (NTFS, ReFS, FAT/exFAT, other) |
| Direct I/O honoured, copy-on-write | Known for NTFS only (yes, and no) |
| Directory flush supported | One real `FlushFileBuffers` on the store directory |
| Compressed, encrypted, sparse, cloud placeholder | The file's attributes (`FileAttributeTagInfo`) |
| Logical sector, atomicity, alignment | `FileStorageInfo` (`FILE_STORAGE_INFO`) |
| Network share or RAM disk | The drive type of the volume's mount point |
| Disk behind the volume | `IOCTL_STORAGE_GET_DEVICE_NUMBER`, else the first extent from `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` |
| Flush support, user power protection, cache type and state, FUA | `StorageDeviceWriteCacheProperty` |
| Logical and physical sector | `StorageAccessAlignmentProperty` |
| Bus, largest transfer | `StorageAdapterProperty`, `StorageDeviceProperty` |
| Model, firmware | `StorageDeviceProperty`, then NVMe Identify |
| NUMA node | `StorageDeviceNumaProperty` |
| NVMe: volatile cache present, AWUPF, multiple controllers | Identify Controller (CNS 01h); AWUPF dropped when namespace 1 is not the disk's (below) |
| NVMe: atomic fields, LBA size, preferred write granularity | Identify Namespace (CNS 00h) for namespace 1, used only when its size (NSZE times its LBA size) is the disk's length (`IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`) |
| NVMe: volatile cache enabled | Get Features, Volatile Write Cache (FID 06h) |
| NVMe: backup failed | SMART / Health log (LID 02h), critical warning bit 4 |
| Hypervisor | The disk's model string and bus (a virtual-disk model or bus) |

How the evidence maps: the OS is taken to pass flushes when the class driver reports flush support and user power protection is off. "Turn off write-cache buffer flushing" (user power protection) or no flush support makes the device `Unsafe`. On NVMe, the device's own Identify data overrides the class driver's view of the cache. The hypervisor is judged from the disk, not the CPU: Windows runs under Hyper-V as the root partition whenever virtualisation-based security is on, so a CPU hypervisor bit would misreport most physical Windows 11 machines.

The write-cache property query makes the class driver send one device cache flush, so probing is not free of I/O.

### Known limitations

- **NVMe namespace 1 only.** Identify Namespace is asked for namespace 1 (the disk's handle does not report its namespace id), and the answer is used only when that namespace is exactly the disk's size. On a drive whose volume lives in another namespace of a different size, no atomic-write evidence is used (AWUPF included, so no untorn unit) and the LBA size comes from the class driver; two namespaces of one size cannot be told apart.
- **NVMe over fabrics.** No Windows bus type in the SDK names an NVMe-over-fabrics initiator; one that reports the NVMe bus type would be classed as a local NVMe drive.
- **No storage-stack walk.** The layers below the volume (BitLocker, other filter drivers) are not inspected, and `Stack` is always reported missing. A Storage Spaces disk is reported as a pool layer (`StoragePool`, `Unverified`); its pool's own disks are not probed.
- **No certified fast write path.** Write-through (FUA) writes are not used until a power-cut rig can certify or rule them out (1.x).
- **No crash replay on NTFS.** Windows has no write log like Linux's `dm-log-writes`, so a real NTFS cannot be rebuilt at every point a power cut could leave it. NTFS gets the conformance tests; its crash states are the simulator's enumeration (each format-2 change crashed at every step, the open's own writes, and a matrix of damage to the metadata: `crates/store-io-engine/tests/format2_crash.rs`, `format2_crash_open.rs`, `format2_damage.rs`). On a real ext4 and XFS a workload of the same kinds of change is replayed at every state the device log shows (`tests/crash.rs`: appends, page writes, slot commits, provisioning, growth within and beyond capacity, recycle, release, a repair; not an accept, a copy, a crashed create or close, which only the simulator enumerates).
- **Released space is not returned to the file system.** A release trims the region's data extents in place (`FSCTL_FILE_LEVEL_TRIM`, a hint the device may ignore). It never deallocates: that would make the container a sparse file for good, and a sparse file may allocate on overwrite, which costs the store its power-safe class. The space stays allocated in the container with undefined contents, and any later region it fits reuses it ([the format](./FORMAT.md), section 17).
- **Page cache, unwritten and shared extents are unknown** (`PageCache` always missing).
- **Direct I/O and copy-on-write are known only on NTFS.** ReFS is `Unverified` (`FsUnqualified`); FAT and exFAT are `Unsafe`.
- **Paths.** Files are opened by full path under the store directory. Reparse points are refused for the store directory itself and the file, not for the directories above it.
- **Legacy fallbacks** for rename and delete are recorded on the directory handle but not shown in the device report.

Source: [`crates/store-io-win/src/`](../crates/store-io-win/src/) (`lib.rs`, `file.rs`, `dir.rs`, `queue.rs`, `lock.rs`, `alloc.rs`, `sys.rs`, `probe/`), [`crates/store-io-win/tests/ntfs.rs`](../crates/store-io-win/tests/ntfs.rs), [`crates/store-io-win/tests/kill.rs`](../crates/store-io-win/tests/kill.rs), [`dev/TODO.md`](../dev/TODO.md)

---

## 4. Linux

### Files and directories

- **Data files** are opened `O_RDWR | O_DIRECT | O_CLOEXEC | O_NOFOLLOW` (read-only: `O_RDONLY | O_DIRECT | O_CLOEXEC | O_NOFOLLOW`) *relative to the store directory's descriptor*: with `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` on Linux 5.6 and later, and `openat` otherwise. The name is always a single validated path component, so nothing can escape the directory or follow a symlink. Anything that resolves to neither a regular file nor a block device is refused.
- **New files** are created `O_CREAT | O_EXCL` with mode 0600; **directories** with mode 0700, followed by an `fsync` of the parent (the directory's `..`; where the caller may not read the parent, an `fsync` of the directory itself, which commits the transaction holding its entry, and `syncfs` of its file system), also when the directory already exists (an earlier attempt may have died before the sync), unless that file system cannot sync a directory at all. A directory the call made is removed again if that fails, when it is still the one made. One level is created.
- **Directories** are held as real `O_RDONLY | O_DIRECTORY` descriptors (never `O_PATH`), because `fsync` needs an open file. Rename and unlink are `renameat` and `unlinkat` relative to that descriptor.
- An open interrupted by `EINTR`, or by `openat2`'s `EAGAIN` when a rename races the confinement check, is retried up to 64 times and counted. Data calls are never retried.

### The I/O queue

Linux has two tiers, chosen by `StoreOptions::linux_tier` and named in `StoreReport::io` with the reason:

- **`Auto`** (the default) uses io_uring when the kernel accepts both of store-io's ring flavours, so Linux 6.1 or later. The probe's lasting answers are kept for the process; a refusal for want of resources is probed again next time. Otherwise it uses the synchronous tier: on a kernel without the needed setup flags, with `kernel.io_uring_disabled` set, under a seccomp or container policy that refuses `io_uring_setup`, or when the process is short of descriptors or locked memory. Under `Auto`, a queue whose ring is refused later is synchronous too, and `Store::io_path()` counts it (`StoreReport::io` is as of create or open). This covers a lack of descriptors or memory (`EMFILE`, `ENFILE`, `ENOMEM`, `EAGAIN`) and a filter installed since the probe (`EPERM`, `EACCES`, `ENOSYS`). Any other refusal fails the call that needed the queue.
- **`Uring`** asks for io_uring and fails the store's creation where the kernel refuses its rings. A ring refused later (a `Submitter`'s, a parallel lane's) is an error for what needed it (a parallel lane's operations then run one at a time), and `Store::io_path()` counts it.
- **`Sync`** uses no io_uring: one synchronous system call per operation, as in 0.3 (calls with several buffers run on the submit pool's threads).

**io_uring tier.** store-io binds io_uring itself (no third-party crate):

- **Setup.**
  - Every ring starts disabled. It is restricted to reads, writes, NOP and cancel, then enabled. Nothing that always runs on the kernel's worker threads (`FSYNC`, `FALLOCATE`, `OPENAT`, ...) can be submitted to it.
  - A `Submitter`'s ring belongs to its thread: `SINGLE_ISSUER | DEFER_TASKRUN` (Linux 6.1 and later), and its completions run only when that thread asks.
  - Queues that keep several operations in flight get rings: a `Submitter`'s, and the blocking API's parallel lanes (shared rings, `SUBMIT_ALL`, Linux 5.18 and later), which any thread may borrow for one call. A blocking call that runs one operation at a time and waits for it does that operation inline with one system call, on this tier too: a ring round trip for a lone operation costs more than the call itself.
  - Each ring holds two descriptors, the ring and an eventfd for its waker.
- **Never on io-wq.** Every read and write carries `RWF_NOWAIT`, so nothing ever runs on io-wq. When the kernel would block (`EAGAIN`), cannot honour NOWAIT for the file (`EOPNOTSUPP`), or transfers short, the rest is finished by one ordinary `pwritev2` or `preadv2` call on the thread that collects the completion. These fallbacks are counted (`store_io::io_uring_fallbacks()`); they are never failures.
- **Lost completions.** A completion the queue does not expect, or one the kernel could not keep (`EBADR`, the overflow counter), ends the queue. Everything in flight fails, and the engine treats a write among them as durability unknown and poisons the store. Their buffers are never freed, since the kernel may still write to them.
- **Durable writes.** A durable write on a power-safe store is a `RWF_DSYNC` write, which on a ring is a FUA write. A lone durable write runs inline, as on the synchronous tier: on a raw block device, a plain write followed by one `fdatasync` (see Flushes below). A data flush is the synchronous `fdatasync`, completed inside `submit`.
- **Drop.** Dropping a queue waits for every operation it holds.
- **Idle.** An idle store makes no system calls and has no timers (checked in CI).

**Synchronous tier.** `submit` performs the operation with one system call (`pwritev2`, `preadv2` or `fdatasync`) and stores its completion in a fixed ring sized by the queue depth: no thread hop, no timer, no allocation, and `wait` never waits. Queues asked to run operations concurrently (a `Submitter`'s, and the blocking API's parallel lanes for multi-buffer calls) hand them to a small worker pool (`StoreOptions::t3_pool_threads`) instead.

A short transfer the queue hands back is never retried by the engine (it is durability unknown, and the store stops); on a ring, a NOWAIT transfer cut short is first finished by the queue itself, as above. `EINTR` from a data call is returned as the result.

**Misaligned direct writes.** The backend does not check alignment; the kernel does. Older kernels refuse a misaligned `O_DIRECT` write with `EINVAL`. Newer kernels may instead complete it through the page cache, write it back and drop those pages, as a CI runner on kernel 6.17 does. The engine only ever issues block-aligned transfers from aligned buffers, so neither path is taken in normal use; the backend's tests accept both answers and check that the bytes land exactly where they were aimed.

### Flushes and durable writes

A data flush is `fdatasync`; a full flush is `fsync`. A durable write on a file is `pwritev2` with `RWF_DSYNC`. On a raw block device the kernel's synchronous `RWF_DSYNC` path issues both a FUA write and a cache flush, so there the backend writes plainly and follows a complete write with one `fdatasync`.

### Ownership lock

The lock is `fcntl(F_OFD_SETLK)` with a write lock over the whole file, taken on a `dup` of the very descriptor used for I/O, so it belongs to that open file description. It is released only when the last reference to the description goes, including references the kernel holds for in-flight I/O. A classic POSIX `F_SETLK` is never used, because any `close()` of any descriptor for the inode would drop it. Before Linux 3.15 (no OFD locks) the backend falls back to `flock(LOCK_EX | LOCK_NB)`, which has the same ownership. A write lock needs a writable descriptor. A read-only store open therefore takes a read lock (`F_RDLCK`, or `LOCK_SH`) on its read-only descriptor instead: readers share it, and it conflicts with the owner's write lock.

### Space

- **Allocation** is `fallocate` with mode 0 from the current size to the new size: real blocks, allocated as unwritten extents that read as zeros (never another file's data), and the size grows with them. It never truncates and never uses `FALLOC_FL_KEEP_SIZE`. The engine then converts the extents to written ones with direct writes.
- **Release** is `fallocate(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE)`. Trim in place is not available for Linux files and reports `EOPNOTSUPP`.
- **Range state** for provisioning checks: `FS_IOC_FIEMAP` (with `FIEMAP_FLAG_SYNC`, a fixed buffer of 32 extents per call, at most 2^20 calls) reports any unwritten, delayed, unknown-location or missing extent as *unwritten*, and any shared (reflinked) extent as *shared*; both are `Unknown` where the file system has no extent map. `cachestat(2)` (Linux 6.5) reports pages of the range in the page cache, which would mean a buffered reader has the file open; it is `Unknown` before 6.5.

### The probe

The probe is unprivileged, never writes a file, and parses every sysfs, procfs and device byte as hostile input.

| Fact | Source |
|---|---|
| Kernel version, hypervisor hints | `uname` release |
| Direct-I/O memory and offset alignment; direct I/O unsupported (offset alignment 0) | `statx` `STATX_DIOALIGN` (6.1) |
| Atomic write unit | `statx` `STATX_WRITE_ATOMIC` (6.11) |
| Compressed or encrypted file | `statx` attributes; `FS_IOC_GETFLAGS` |
| Data journaling on the file (`+j`) | `FS_IOC_GETFLAGS` |
| File system kind | `fstatfs` magic, refined by the mount's type in `/proc/self/mountinfo` |
| The mount | `/proc/self/mountinfo`, by `statx` mount id, else by device number and the longest matching mount point |
| Mount options | `mountinfo` per-mount and superblock options; for ext4 also `/proc/fs/ext4/<dev>/options`, which prints effective defaults such as `barrier` |
| Storage stack | A walk of `/sys/dev/block/<major>:<minor>` through partitions, device-mapper (crypt, VDO, LVM linear, thin, snapshot, cache and writecache, multipath; classified by name and uuid), MD (level, consistency policy, write-behind backlog), loop and bcache, down to the first leaf disk |
| Cache mode, FUA, block sizes, minimum I/O, largest transfer, NUMA node, bus | The leaf's `queue/` attributes and sysfs path; for NVMe, every controller's `transport` (the controller itself, or each controller of a multipath subsystem); for SCSI, the host's driver (`/sys/class/scsi_host/hostN/proc_name`) |
| NVMe cache presence and state, atomic fields, model, firmware, health | NVMe admin commands through `/dev/<namespace>` or `/dev/<controller>`, whichever opens: Identify Controller, Identify Namespace (the namespace id from sysfs), Get Features (volatile write cache), SMART / Health log |
| Hypervisor | DMI vendor and product, `/sys/hypervisor/type`, the CPU `hypervisor` flag, vmbus and virtio devices, a WSL kernel release |
| Page-cache query available | A `cachestat` call |

How the evidence maps: the OS is taken to pass flushes when the leaf's `queue/write_cache` reads `write back`. Copy-on-write is known from the file system kind (btrfs, ZFS and bcachefs yes; ext4, XFS, F2FS, FAT, tmpfs, ntfs3 no). The full-flush and directory-flush primitives are assumed for local file systems rather than tested.

### Known limitations

- **io_uring needs Linux 6.1 or later.** Both ring flavours must be accepted (a `Submitter`'s needs 6.1, the shared ones 5.18), so on older kernels every queue is synchronous under `Auto`, and `Uring` is refused.
- **A direct descriptor** (`StoreOptions::register_file`, on by default: reads 1.03x to 1.08x, of an unregistered ring, G3d in CI; writes take the unregistered path): a `Submitter`'s ring registers a read-only twin of the container as a direct descriptor while the ring is still disabled, and only reads of that file use it. The twin is a second open file description (opened `O_RDONLY|O_DIRECT` through `/proc/thread-self/fd`, checked to be the same inode, and only once a ring is made). Why: the store's lock lives on the store's own description and is released at its last reference. A write in flight holds a reference to the description it was issued on, so writes stay on the store's own: a dead owner's write keeps the store locked until it lands, and no successor writes before it. A ring's file table holding the store's own description would instead keep the store locked after its process died until the ring's teardown finished; the twin holds no lock, and cannot write. It does keep the file open: after a killed process, until the kernel's ring teardown has run (milliseconds), the container's file system or block device can still be busy (`umount` returning `EBUSY`), and an unlinked container's space comes back then. A refusal (no `/proc`, a failed open of the twin) is counted in `Store::io_path()`, never an error.
- **No registered buffers.** A read into an io_uring registered buffer (`READ_FIXED`) never completes on at least Linux 6.5.0 and 6.6.0 (seen fine on 6.1.0 and from 6.6.15 and 6.7; the versions between were not tested; CI VMs, 2026-10-19), and the process holding it can then not exit. Nothing a ring can probe tells those kernels apart (distribution kernels carry backported io_uring under older version numbers), so store-io never registers buffers, and its rings refuse the fixed-buffer opcodes.
- **Trim in place is unsupported** on files (`EOPNOTSUPP`).
- **Device facts need the device node.** The volatile-cache presence, its enabled state, the atomic-write fields and the health log come only from NVMe admin commands, through a device node an unprivileged user usually cannot open. Without them a device can be at most `FlushRequired`; power-safe needs readable Identify and SMART data, or a certificate. SATA and SAS cache presence is not read at all.
- **One leaf.** Only the first leaf device of a stack is inspected; the other members of a mirror or array are not.
- **Anonymous devices.** btrfs, overlay, network and FUSE mounts have no block device to walk from, so the stack and device facts are missing.
- **An NVMe controller without a `transport` attribute** (old kernels) leaves the bus unknown and the namespace's Identify and SMART data unused, so such a drive is at most `FlushRequired`.
- **Device-mapper targets** are inferred from names and uuids (reading the target table needs root); an unrecognised one marks the stack uncertain. An MD parity array whose consistency policy cannot be read is treated as having a write hole.
- **Virtual machines.** Any detected hypervisor makes the device `Unverified`, so cloud and CI virtual machines need an attestation or an override. GitHub's Ubuntu runners also mount their root file system `nobarrier`, which is `Unsafe`.
- **Kernel features by version:** `openat2` 5.6, OFD locks 3.15, `statx` direct-I/O alignment 6.1, `cachestat` 6.5, `statx` atomic-write fields 6.11. Older kernels lose the corresponding evidence or confinement, as described above. No kernel-specific bug rules exist yet.
- **Sharing a flush costs a wake.** A writer whose barrier joins another's flush sleeps until that flush finishes and is then woken by the leader, usually through an interrupt to another processor (on a virtual machine, one that goes through the hypervisor). Measured on CI's virtual machines over a memory-backed device (null_blk, 2026-10-29; the gates' G15 and `harness/examples/g15probe.rs`):
  - Where one writer's whole durable write took less than one wake (the Xeon runners: 7-8 µs against 9-13 µs), eight writers made only 1.2-1.3x the durable writes of one.
  - On every runner CPU, eight writers sharing flushes made 0.62-0.64x the durable writes of eight writers each flushing its own (`pwritev2` then `fdatasync`).
  - Neither leading a flush of its own on a free flush slot (an experiment, not shipped) nor running flushes side by side (`flushes_in_flight` 2 or 4) closed that gap.
  - Where a flush is real work, sharing wins: on WSL's virtual disk (the same probe, the same day), eight sharing writers made 6,860 durable writes a second, against 956 for eight writers each flushing its own.
- **Not yet measured** (planned, once the hardware in the [later list](../dev/TODO.md) exists): whether the lock is released only after a crashed owner's in-flight writes, across kernels; FUA writes against write plus flush; flush sharing between concurrent writers on bare metal.
- **Paths.** Only the container's name is confined; the store directory itself is opened by path, following symlinks.

Source: [`crates/store-io-posix/src/`](../crates/store-io-posix/src/) (`lib.rs`, `file.rs`, `dir.rs`, `queue.rs`, `lock.rs`, `alloc.rs`, `sys.rs`, `probe/`), [`crates/store-io-posix/tests/linux.rs`](../crates/store-io-posix/tests/linux.rs), [`dev/ROADMAP.md`](../dev/ROADMAP.md), [`dev/TODO.md`](../dev/TODO.md)

---

## 5. macOS

The macOS backend is `store-io-posix`'s `mac/` modules (D-133, D-134): the POSIX calls it shares with the Linux backend (`posix.rs`, the queue's executor, the submit pool), and its own files, directories, locks, space, flush state and probe. Nothing needs privilege.

### Files and directories

- **Data files** are opened `O_RDWR | O_CLOEXEC | O_NOFOLLOW` (read-only: `O_RDONLY | O_CLOEXEC | O_NOFOLLOW`) with `openat` relative to the store directory's descriptor; the name is a single validated path component. Anything but a regular file is refused. `F_NOCACHE` and `F_RDAHEAD 0` are set on each: a hint only (requests aligned to the kernel page, 16 KiB on Apple silicon, bypass the cache), never part of a durability argument. macOS has no direct I/O.
- **New files** are created `O_CREAT | O_EXCL`, mode 0600; **directories** mode 0700, then the parent (the directory's `..`) is flushed on its own volume, also when the directory already exists (an earlier attempt may have died before the flush). Where the caller may not open the parent, the directory itself is flushed, which commits the volume's transaction (APFS) or journal (HFS+) holding its entry. A directory the call made is removed again if its open or flush fails, when it is still the one made (same device and inode). For an existing directory, a read-only volume or one that cannot flush a directory (and has not accepted `F_FULLFSYNC`) is not an error.
- **Rename** is `renameat`; a rename that must not replace is `renameatx_np(RENAME_EXCL)`.

### The I/O queue

One tier, synchronous: every data operation is one system call (`pwrite`, `pread`, or the flush). A queue for one operation at a time performs it inline in `submit`; a parallel queue (a `Submitter`'s, the blocking API's parallel lanes) runs on the submit pool's worker threads, started on first use and parked while idle. POSIX AIO is not used (small limits, and its `aio_fsync` never reaches the drive). One call transfers at most `INT_MAX` bytes; a longer buffer transfers short. `StoreReport::io` names the path. An idle store makes no system calls and has no timers: none of its threads runs on a CPU, and it moves no byte to or from storage (checked in CI).

### Flushes and the flush check

Every flush is `F_FULLFSYNC`: macOS has no data-only drive flush, and `fsync` never reaches the drive's cache. A `PosixPlatform` (with its clones) keeps one flush state for each volume (by device number and mount source), shared by every directory and file opened through it. The simple API makes a new platform for each create or open, so each one starts from "not asked". The state has three values:

- **Not asked.** Every writable create or open checks, unless the volume has refused: one `F_FULLFSYNC` of the container, then one of its directory (a few milliseconds). The first answer is recorded.
- **Accepted.** Every flush is `F_FULLFSYNC`. A later refusal (another vnode, a race, a later open's check) is a failed flush, never a downgrade, because the store's class rests on the acceptance.
- **Refused** (`ENOTSUP`, `EOPNOTSUPP`, `ENOSYS`, `ENOTTY`, `EINVAL`), recorded only from "not asked". The store is `Unsafe` (`FullFlushUnsupported`) and refused unless overridden; an overridden store's flushes are all `fsync` from then on, directory flushes included, so it never meets the refusal again. Every receipt is labelled with the override.

A read-only open makes no flush check. Its report shows its platform's state for the volume: Yes, No, or Unknown when no writer on the same platform has asked. Through the simple API it is Unknown (`FullFlushUnsupported`, so Unverified); a read-only open is never refused for its class.

### Ownership lock

`fcntl(F_OFD_SETLK)` over the whole file on a `dup` of the I/O descriptor (macOS 14 and later); before macOS 14, where OFD locks are private API, `flock(LOCK_NB)`, which has the same ownership. The kind is chosen by OS release (`kern.osproductversion`; an unreadable release takes OFD), never by probing the call, and the two are never mixed on one file. Writers take the exclusive form, read-only opens the shared one. The kernel releases a killed owner's lock (tested).

### Space

- **Allocation** is `F_PREALLOCATE` (`F_ALLOCATEALL`, contiguous first, then not) of the growth past the blocks the file holds, then `ftruncate` to the new size: real blocks, read as zeros. It never truncates.
- **Allocating inside a file** is not available (`F_PREALLOCATE` only extends): `ENOTSUP`. The engine writes a capped region's slack, and the rest of a growth's extent, instead. An empty range is a no-op and a range past the end `EINVAL`, as the contract asks.
- **Release** is `F_PUNCHHOLE`; the range must be aligned to the file system's block. Trim in place is unsupported (`EOPNOTSUPP`).
- **Range state** reads nothing: unwritten and shared are `Unknown` (no unprivileged extent map), and page-cache residency is not reported (writes not aligned to the kernel page, 16 KiB on Apple silicon, stay cached by design, so it would decide nothing).

### The probe

| Fact | Source |
|---|---|
| File system, local, removable mount | `fstatfs`: `f_fstypename`, `MNT_LOCAL`, `MNT_REMOVABLE`; `f_mntfromname` names the disk |
| Release; hypervisor | `kern.osproductversion`; `kern.hv_vmm_present` |
| Machine architecture | `hw.optional.arm64` (decides the allowlist below, whatever the build: Rosetta) |
| Device: internal, protocol, model, removable and ejectable media, registry path | DiskArbitration's description of the BSD disk (a `/dev/disk…` mount source only) |
| Thunderbolt tunnel; RAID or multi-store container | IOKit from the volume's own media entry: `IOPCITunnelled` searched over every ancestor; every entry up to the root must have one parent. The media's `BSD Name` must read back first, so a refused read never passes for an absent property |
| Full and directory flush | The flush check (above) |

**Classes.** Never power-safe on macOS (no power-loss-protection claim from Apple; `F_FULLFSYNC` is best effort; APFS is copy-on-write). **FlushRequired** for APFS on an internal device whose `F_FULLFSYNC` was accepted, and nothing else. **Unverified** for HFS+ and any other local file system (unqualified), any device not known to be internal (`DeviceNotInternal`), under a hypervisor, and a read-only open whose flush state is unknown. **Unsafe** where `F_FULLFSYNC` is refused, and on FAT, exFAT, FUSE and network volumes.

**Internal** is an allowlist. Every one of these must hold:

- DiskArbitration calls the device internal.
- Its media is known to be neither removable nor ejectable, and the mount is not removable.
- Its protocol is `Apple Fabric`. On an Intel Mac, `PCI-Express` and `SATA` count as well.
- It is neither a disk image (a virtual-disk layer is reported) nor a virtual machine's disk.
- IOKit shows its media on a single line of parents, with nothing on that line tunnelled over Thunderbolt.
- Its registry path names no Thunderbolt or USB controller.

A device that fails a rule is `No`; one where a fact cannot be read is `Unknown`. Both are Unverified.

### Known limitations

- **Not yet seen on Apple's own storage stack.** CI's macOS runners are virtual machines. Their APFS volume's description has no protocol, so the runner is Unverified (and under a hypervisor anyway). Whether a real internal APFS volume reads as internal (Apple silicon, Intel T2) is checked on real hardware next. If not, the cost is availability: the store is Unverified, and an attestation opens it.
- **Apple silicon's Thunderbolt tunnels** are not known to carry `IOPCITunnelled`, so on Apple silicon only `Apple Fabric` counts as internal. A Mac Pro's PCIe card is Unverified there.
- **A multi-store APFS container** built by hand with an internal main store and an external secondary store is caught only if APFS shows the container under every store. If APFS shows it under the main store alone, the store reads as internal and is classed flush-required: the unsafe direction, in a deliberate and unusual setup. AppleRAID and CoreStorage sets are caught.
- **Intel edge cases IOKit cannot tell apart**, classed flush-required if they occur: hot-plug SATA or PCIe that is not Thunderbolt (ExpressCard eSATA, a Mac Pro's eSATA card) whose driver reports it internal and fixed, and a Thunderbolt enclosure reporting itself internal behind a sandbox that filters IOKit property reads by name.
- **Internal is not a correct flush.** A replaced or third-party internal SSD passes the allowlist; its `F_FULLFSYNC` behaviour is its own.
- **No direct I/O, no in-file allocation, no extent map, no trim in place.** See above.
- **Cost.** A writable create or open makes two `F_FULLFSYNC`s for the flush check, and creating a directory one more for its parent.
- An open interrupted by `EINTR` is retried up to 64 times and counted.

Mutation testing of the macOS backend runs on CI's macOS runners (`mutate.py --only-platform`); the Linux shards skip it.

Source: [`crates/store-io-posix/src/mac/`](../crates/store-io-posix/src/mac/) (`platform.rs`, `file.rs`, `dir.rs`, `lock.rs`, `alloc.rs`, `probe.rs`, `sys.rs`), [`crates/store-io-posix/src/posix.rs`](../crates/store-io-posix/src/posix.rs), [`crates/store-io-core/src/decide.rs`](../crates/store-io-core/src/decide.rs)

---

## 6. The simulator

`store-io-sim` implements the same platform interface as the native backends, entirely in memory, so the whole engine runs on it deterministically: the same seed and the same calls give the same run, byte for byte, which a trace hash checks.

**What it models**

- One device, with a volatile write cache or power-safe. Every file has *visible* bytes (what reads return) and *media* bytes (what survives a crash), and a visible and a durable length.
- Operations take effect when they complete, not when they are submitted; with reordering on, completions arrive in a seeded random order.
- A completed write updates the visible bytes. On a power-safe device, or with `dsync` (modelled as FUA), it updates the media at completion; otherwise it waits in the volatile cache.
- A data flush persists exactly the cached writes that completed *before the flush was submitted*, and makes the file's length durable. A full flush persists every cached write.
- Directory entries (create, rename, delete) are visible at once and durable only after a directory sync. A created directory is durable at once.
- Direct-I/O rules: a transfer whose offset or length is not a multiple of the logical block, or whose buffer is not 4096-aligned, completes with `EINVAL` and never reaches the device.
- A **crash** keeps every cached write, none, a chosen subset (a bit mask over up to 64 cached writes), a seeded random choice that may also tear a write at a logical-block boundary, or each cached write's exact fate (kept, lost, or torn after so many blocks, for any number of writes: what a crash enumeration gives); then it truncates every file to its durable length (past what reached the media, unwritten), restores every directory to its durable entries, releases every lock and invalidates every handle.
- **Crash points**: a step probe runs before every step that can change what a crash leaves (each operation a queue accepts, and each create, rename, unlink, directory sync, allocation, length change, release and full flush). It can cut the power there: that step and every later one fail with `EIO`, operations in flight complete with `EIO` unapplied, until the crash. A journaling file system's metadata commit (every name and file length durable, the data as cached) can be applied before a crash; a process can die with the machine up (its locks released, its handles dead, its cache kept for the next flush).
- **Faults**, targeted ("the third flush fails") or at a per-million rate from the seed: write or flush errors; a failed flush that keeps the cache, or drops it so the next flush succeeds with nothing to write (the Linux `fsync` failure behaviour); flushes that lie; short writes and reads; lost writes; misdirected writes; a failed directory sync; unreadable ranges; running out of space; and pages in the OS page cache.
- The probe returns evidence derived from the device kind (a simulated NVMe device on a qualified file system), or whatever evidence the test supplies.

**What it does not model**: more than one device; tears smaller than a logical block; timing and latency; file-system behaviour beyond file lengths and directory entries. It shows that the engine's logic is right against a precise device model; whether real devices follow that model is what the real-device tests and the planned power-cut rig are for ([Durability](./DURABILITY.md), section 13).

Source: [`crates/store-io-sim/src/world.rs`](../crates/store-io-sim/src/world.rs), [`crates/store-io-sim/src/fault.rs`](../crates/store-io-sim/src/fault.rs), [`crates/store-io-sim/src/platform.rs`](../crates/store-io-sim/src/platform.rs), [`crates/store-io-sim/tests/model.rs`](../crates/store-io-sim/tests/model.rs)

---

<div align="center"><sub>Apache-2.0 OR MIT &middot; &copy; 2026 James Gober</sub></div>
