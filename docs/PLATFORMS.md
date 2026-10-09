# store-io &mdash; Platforms

> What store-io does on each operating system, and why: the system calls behind every durability primitive, how files are opened, locked, allocated and released, where the device evidence comes from, and what each backend cannot do yet. Windows and Linux have native backends; macOS is planned for 0.5; the deterministic simulator runs everywhere. Each section names the source it describes.

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
| Crate | `store-io-win` | `store-io-posix` | &mdash; | `store-io-sim` |
| Status | Native backend | Native backend (synchronous) | Planned for 0.5 | Tests and conformance |
| Data I/O | Unbuffered overlapped `WriteFile` / `ReadFile`, reaped from a per-queue I/O completion port | `O_DIRECT` `pwritev2` / `preadv2`, one system call per operation in the caller's thread | &mdash; | In-memory model of one device |
| Barrier | `NtFlushBuffersFileEx` data-sync on NTFS, `FlushFileBuffers` elsewhere | `fdatasync` | &mdash; | Flush of the writes completed before it |
| Ownership lock | `LockFileEx` on a sentinel byte | Open-file-description lock | &mdash; | Per-file flag |

The `store-io` crate's `NativePlatform` is the Windows backend on Windows and the Linux backend on Linux. On other targets the engine and the simulator build, but there is no native backend. Both native crates compile to empty shells on the other operating systems.

The engine above every backend is the same: it issues only block-aligned transfers from aligned pooled buffers, decides the durability class from the evidence a backend reports ([Durability](./DURABILITY.md)), and never retries a failed write or flush.

Source: [`src/lib.rs`](../src/lib.rs), [`crates/store-io-platform/src/lib.rs`](../crates/store-io-platform/src/lib.rs)

---

## 2. Durability primitives per platform

The engine asks a backend for six things. The table lists what each backend does for each, and what it guarantees.

| Primitive | Used for | Windows | Linux | Guarantee |
|---|---|---|---|---|
| **Data flush** (`FlushData`) | Every barrier on a flush-required, unverified or unsafe store; metadata commits on those stores | NTFS: `NtFlushBuffersFileEx(FLUSH_FLAGS_FILE_DATA_SYNC_ONLY)`. Other file systems: `FlushFileBuffers`. Chosen once when the file is opened. | `fdatasync` | The file's written data, the metadata needed to read it back, and a device cache flush. Covers the writes that completed before it was submitted. |
| **Durable write** (`dsync`) | Every write on a power-safe store | The write, then the same data flush on the same handle, before the completion is reported. Never `FILE_FLAG_WRITE_THROUGH`. | Files: `pwritev2(RWF_DSYNC)`. Block devices: a plain `pwritev2`, then `fdatasync`. | The write is durable when it completes. |
| **Full flush** (`flush_all`) | Creating a store, the start of every writable open, provisioning, directory-mode files | `FlushFileBuffers` | `fsync` | All data and metadata of the file, including its length, and a device cache flush. |
| **Directory flush** (`sync_dir`) | Creating a store, directory-mode replace | `FlushFileBuffers` on a directory handle opened with add-file access | `fsync` on the directory descriptor | The directory's entries (create, rename, delete) are durable. An unsupported directory flush is an error, never success. |
| **Atomic replace** (`rename_replace`) | Directory-mode replace | `SetFileInformationByHandle(FileRenameInfoEx, REPLACE_IF_EXISTS \| POSIX_SEMANTICS)` on the open temporary file | `renameat` within the directory descriptor | The target names the old or the new file, never neither. Durable only after the directory flush. |
| **Ownership lock** (`lock_exclusive`) | Every open of a store | `LockFileEx(EXCLUSIVE \| FAIL_IMMEDIATELY)` on one byte at offset 2^63 &minus; 2 | `fcntl(F_OFD_SETLK, F_WRLCK)` over the whole file, on the I/O descriptor's open file description (`flock(LOCK_EX \| LOCK_NB)` before Linux 3.15) | One owner at a time; released when the owner's handle (or process) goes away. |

How the engine combines them: on a power-safe store every write is a durable write and a barrier issues nothing; on every other class writes are plain and a barrier is one data flush shared between concurrent writers ([Durability](./DURABILITY.md), sections 7&ndash;8). Neither backend asks for FUA (forced unit access) writes directly: the Linux kernel may use FUA inside `RWF_DSYNC` as it chooses, and the Windows backend never relies on write-through, whose FUA path is not certified.

Source: [`crates/store-io-win/src/queue.rs`](../crates/store-io-win/src/queue.rs), [`crates/store-io-win/src/file.rs`](../crates/store-io-win/src/file.rs), [`crates/store-io-win/src/dir.rs`](../crates/store-io-win/src/dir.rs), [`crates/store-io-win/src/lock.rs`](../crates/store-io-win/src/lock.rs), [`crates/store-io-posix/src/queue.rs`](../crates/store-io-posix/src/queue.rs), [`crates/store-io-posix/src/lock.rs`](../crates/store-io-posix/src/lock.rs), [`crates/store-io-posix/src/lib.rs`](../crates/store-io-posix/src/lib.rs)

---

## 3. Windows

### Files and directories

- **Data files** are opened `FILE_FLAG_NO_BUFFERING | FILE_FLAG_OVERLAPPED | FILE_FLAG_OPEN_REPARSE_POINT`, sharing read, write and delete with every other opener. Exclusivity comes from the ownership lock, never from share modes. A read-write handle has `GENERIC_READ | GENERIC_WRITE | DELETE`; a read-only handle only `GENERIC_READ`. A name that turns out to be a symlink, junction or directory is refused. New files are created with `CREATE_NEW`, so an existing file is never overwritten.
- **Directories** are opened with `FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT` and add-file access, which `FlushFileBuffers` needs; a reparse point is refused. Creating a directory creates one level and then flushes its parent.
- **Delete** opens the file with delete access and sets `FileDispositionInfoEx(DELETE | POSIX_SEMANTICS)`, so the name goes at once and open handles keep working.
- **Fallbacks are recorded, not silent.** Where the POSIX-semantics rename or delete is not supported, the backend falls back to the legacy information class (and, for delete, `DeleteFileW`) and records that on the directory handle (`used_legacy_rename`, `used_legacy_delete`).

### The I/O queue

- Every queue owns its own I/O completion port. A file object can be tied to only one port, so each queue opens its own handle to a file with `ReOpenFile` (same file, no path lookup, `NO_BUFFERING | OVERLAPPED`) on first use and associates that handle with its port. Any number of queues can therefore write the same file, and each reaps only its own completions. A queue keeps up to 64 such handles and evicts an idle one when it needs more.
- Each per-queue handle has `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS | FILE_SKIP_SET_EVENT_ON_HANDLE`: a `WriteFile` or `ReadFile` that succeeds at once is completed inline and queues no packet; `ERROR_IO_PENDING` means a packet will come. Any other immediate failure is reported as a failed *completion*, not as a refused submission, because the backend cannot prove such a request never reached the device. The engine treats it as durability unknown.
- Completions are reaped with `GetQueuedCompletionStatusEx`, waiting either `INFINITE` or not at all. No wait in the backend has a finite timeout.
- A data flush runs synchronously inside `submit`. The trailing flush of a durable write runs where its completion is observed.
- Dropping a queue with operations in flight cancels them (`CancelIoEx`) and waits for every completion packet before any buffer or handle is freed. If the port cannot be waited on, buffers and handles are leaked rather than freed under the kernel.

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
| NVMe: volatile cache present, AWUPF, multiple controllers | Identify Controller (CNS 01h) |
| NVMe: atomic fields, LBA size, preferred write granularity | Identify Namespace (CNS 00h) for namespace 1 |
| NVMe: volatile cache enabled | Get Features, Volatile Write Cache (FID 06h) |
| NVMe: backup failed | SMART / Health log (LID 02h), critical warning bit 4 |
| Hypervisor | The disk's model string and bus (a virtual-disk model or bus) |

How the evidence maps: the OS is taken to pass flushes when the class driver reports flush support and user power protection is off. "Turn off write-cache buffer flushing" (user power protection) or no flush support makes the device `Unsafe`. On NVMe, the device's own Identify data overrides the class driver's view of the cache. The hypervisor is judged from the disk, not the CPU: Windows runs under Hyper-V as the root partition whenever virtualisation-based security is on, so a CPU hypervisor bit would misreport most physical Windows 11 machines.

The write-cache property query makes the class driver send one device cache flush, so probing is not free of I/O.

### Known limitations

- **NVMe namespace 1 only.** Identify Namespace is always asked for namespace 1. On a drive whose volume lives in another namespace, the atomic-write fields, LBA size and write granularity come from the wrong namespace.
- **No storage-stack walk.** The layers below the volume (BitLocker, Storage Spaces, other filter drivers) are not inspected, and `Stack` is always reported missing. A Storage Spaces disk reports a virtual bus and is therefore classed `Unverified` with the reason `Hypervisor`.
- **Fencing after a killed process is unverified.** Whether the ownership lock can be released before a killed owner's in-flight writes have landed has not been tested; a kill test is planned ([later list](../dev/TODO.md)). The device report does not show this yet.
- **No certified fast write path.** Write-through (FUA) writes are not used until a power-cut rig can certify or rule them out (1.0).
- **Released space is not returned to the file system.** It stays in the container and is reused by later regions of the same size (see [the format](./FORMAT.md)).
- **Page cache, unwritten and shared extents are unknown** (`PageCache` always missing).
- **Direct I/O and copy-on-write are known only on NTFS.** ReFS is `Unverified` (`FsUnqualified`); FAT and exFAT are `Unsafe`.
- **Paths.** Files are opened by full path under the store directory. Reparse points are refused for the store directory itself and the file, not for the directories above it.
- **Legacy fallbacks** for rename and delete are recorded on the directory handle but not shown in the device report.

Source: [`crates/store-io-win/src/`](../crates/store-io-win/src/) (`lib.rs`, `file.rs`, `dir.rs`, `queue.rs`, `lock.rs`, `alloc.rs`, `sys.rs`, `probe/`), [`crates/store-io-win/tests/ntfs.rs`](../crates/store-io-win/tests/ntfs.rs), [`dev/TODO.md`](../dev/TODO.md)

---

## 4. Linux

### Files and directories

- **Data files** are opened `O_RDWR | O_DIRECT | O_CLOEXEC | O_NOFOLLOW` (read-only: `O_RDONLY | O_DIRECT | O_CLOEXEC | O_NOFOLLOW`) *relative to the store directory's descriptor*: with `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` on Linux 5.6 and later, and `openat` otherwise. The name is always a single validated path component, so nothing can escape the directory or follow a symlink. Anything that resolves to neither a regular file nor a block device is refused.
- **New files** are created `O_CREAT | O_EXCL` with mode 0600; **directories** with mode 0700, followed by an `fsync` of the parent. One level is created.
- **Directories** are held as real `O_RDONLY | O_DIRECTORY` descriptors (never `O_PATH`), because `fsync` needs an open file. Rename and unlink are `renameat` and `unlinkat` relative to that descriptor.
- An open interrupted by `EINTR`, or by `openat2`'s `EAGAIN` when a rename races the confinement check, is retried up to 64 times and counted. Data calls are never retried.

### The I/O queue

The Linux backend is synchronous: `submit` performs the operation with one system call (`pwritev2`, `preadv2` or `fdatasync`) and stores its completion in a fixed ring sized by the queue depth, so there is no thread hop, no timer and no allocation. `wait` never waits, since everything completed inside `submit`. A short transfer is returned as such and never retried (the engine treats it as durability unknown); `EINTR` from a data call is returned as the result.

**Misaligned direct writes.** The backend does not check alignment; the kernel does. Older kernels refuse a misaligned `O_DIRECT` write with `EINVAL`. Newer kernels may instead complete it through the page cache, write it back and drop those pages, as a CI runner on kernel 6.17 does. The engine only ever issues block-aligned transfers from aligned buffers, so neither path is taken in normal use; the backend's tests accept both answers and check that the bytes land exactly where they were aimed.

### Flushes and durable writes

A data flush is `fdatasync`; a full flush is `fsync`. A durable write on a file is `pwritev2` with `RWF_DSYNC`. On a raw block device the kernel's synchronous `RWF_DSYNC` path issues both a FUA write and a cache flush, so there the backend writes plainly and follows a complete write with one `fdatasync`.

### Ownership lock

The lock is `fcntl(F_OFD_SETLK)` with a write lock over the whole file, taken on a `dup` of the very descriptor used for I/O, so it belongs to that open file description. It is released only when the last reference to the description goes, including references the kernel holds for in-flight I/O. A classic POSIX `F_SETLK` is never used, because any `close()` of any descriptor for the inode would drop it. Before Linux 3.15 (no OFD locks) the backend falls back to `flock(LOCK_EX | LOCK_NB)`, which has the same ownership. A write lock needs a writable descriptor, so even a read-only store open takes the lock through a read-write descriptor.

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
| Cache mode, FUA, block sizes, minimum I/O, largest transfer, NUMA node, bus | The leaf's `queue/` attributes and sysfs path |
| NVMe cache presence and state, atomic fields, model, firmware, health | NVMe admin commands through `/dev/<namespace>` or `/dev/<controller>`, whichever opens: Identify Controller, Identify Namespace (the namespace id from sysfs), Get Features (volatile write cache), SMART / Health log |
| Hypervisor | DMI vendor and product, `/sys/hypervisor/type`, the CPU `hypervisor` flag, vmbus and virtio devices, a WSL kernel release |
| Page-cache query available | A `cachestat` call |

How the evidence maps: the OS is taken to pass flushes when the leaf's `queue/write_cache` reads `write back`. Copy-on-write is known from the file system kind (btrfs, ZFS and bcachefs yes; ext4, XFS, F2FS, FAT, tmpfs, ntfs3 no). The full-flush and directory-flush primitives are assumed for local file systems rather than tested.

### Known limitations

- **Synchronous only.** Each operation is one blocking system call in the caller's thread; queue depth only bounds completions. The io_uring backend is planned for 0.4.
- **Trim in place is unsupported** on files (`EOPNOTSUPP`).
- **Device facts need the device node.** The volatile-cache presence, its enabled state, the atomic-write fields and the health log come only from NVMe admin commands, through a device node an unprivileged user usually cannot open. Without them a device can be at most `FlushRequired`; power-safe needs readable Identify and SMART data, or a certificate. SATA and SAS cache presence is not read at all.
- **One leaf.** Only the first leaf device of a stack is inspected; the other members of a mirror or array are not.
- **Anonymous devices.** btrfs, overlay, network and FUSE mounts have no block device to walk from, so the stack and device facts are missing.
- **NVMe over fabrics** is reported as an NVMe bus like a local drive; the transport is read but not used.
- **Device-mapper targets** are inferred from names and uuids (reading the target table needs root); an unrecognised one marks the stack uncertain. An MD parity array whose consistency policy cannot be read is treated as having a write hole.
- **Virtual machines.** Any detected hypervisor makes the device `Unverified`, so cloud and CI virtual machines need an attestation or an override. GitHub's Ubuntu runners also mount their root file system `nobarrier`, which is `Unsafe`.
- **Kernel features by version:** `openat2` 5.6, OFD locks 3.15, `statx` direct-I/O alignment 6.1, `cachestat` 6.5, `statx` atomic-write fields 6.11. Older kernels lose the corresponding evidence or confinement, as described above. No kernel-specific bug rules exist yet.
- **Not yet measured** (planned, once the hardware in the [later list](../dev/TODO.md) exists): whether the lock is released only after a crashed owner's in-flight writes, across kernels; FUA writes against write plus flush; flush sharing between concurrent writers.
- **Paths.** Only the container's name is confined; the store directory itself is opened by path, following symlinks.

Source: [`crates/store-io-posix/src/`](../crates/store-io-posix/src/) (`lib.rs`, `file.rs`, `dir.rs`, `queue.rs`, `lock.rs`, `alloc.rs`, `sys.rs`, `probe/`), [`crates/store-io-posix/tests/linux.rs`](../crates/store-io-posix/tests/linux.rs), [`dev/ROADMAP.md`](../dev/ROADMAP.md), [`dev/TODO.md`](../dev/TODO.md)

---

## 5. macOS

There is no macOS backend yet. The engine and the simulator build and run there, and the CI matrix compiles and tests the workspace on macOS, but `NativePlatform` does not exist on macOS and a store cannot be opened on a real file.

The rules for macOS are already part of the class decision: a store on macOS is never classed power-safe; a missing full-flush primitive (`F_FULLFSYNC`) is `Unsafe`, and an unknown one `Unverified`; flushes are taken to reach the device only when the full-flush primitive is supported.

Planned for 0.5 ([roadmap](../dev/ROADMAP.md)): an `F_FULLFSYNC` backend, conformance on APFS, a container byte-identical across operating systems, and zero idle wakeups. macOS flush costs and directory-flush support will be measured on real hardware first ([later list](../dev/TODO.md)).

Source: [`src/lib.rs`](../src/lib.rs), [`crates/store-io-core/src/decide.rs`](../crates/store-io-core/src/decide.rs), [`dev/ROADMAP.md`](../dev/ROADMAP.md)

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
- A **crash** keeps every cached write, none, a chosen subset (a bit mask over up to 64 cached writes, so every subset can be enumerated), or a seeded random choice that may also tear a write at a logical-block boundary; then it truncates every file to its durable length, restores every directory to its durable entries, releases every lock and invalidates every handle.
- **Faults**, targeted ("the third flush fails") or at a per-million rate from the seed: write or flush errors; a failed flush that keeps the cache, or drops it so the next flush succeeds with nothing to write (the Linux `fsync` failure behaviour); flushes that lie; short writes and reads; lost writes; misdirected writes; a failed directory sync; unreadable ranges; running out of space; and pages in the OS page cache.
- The probe returns evidence derived from the device kind (a simulated NVMe device on a qualified file system), or whatever evidence the test supplies.

**What it does not model**: more than one device; tears smaller than a logical block; timing and latency; file-system behaviour beyond file lengths and directory entries. It shows that the engine's logic is right against a precise device model; whether real devices follow that model is what the real-device tests and the planned power-cut rig are for ([Durability](./DURABILITY.md), section 13).

Source: [`crates/store-io-sim/src/world.rs`](../crates/store-io-sim/src/world.rs), [`crates/store-io-sim/src/fault.rs`](../crates/store-io-sim/src/fault.rs), [`crates/store-io-sim/src/platform.rs`](../crates/store-io-sim/src/platform.rs), [`crates/store-io-sim/tests/model.rs`](../crates/store-io-sim/tests/model.rs)

---

<div align="center"><sub>Apache-2.0 OR MIT &middot; &copy; 2026 James Gober</sub></div>
