# store-io &mdash; Durability

> What a store-io receipt means and how it is earned: the four durability classes and the evidence that decides them, every reason and every piece of missing evidence the report can show, trust inputs and receipt labels, how barriers share device flushes, what `covers()` checks, fail-stop poisoning, what a receipt does and does not prove, and how all of this is tested. Each section names the source it describes.

---

## Contents

1. [The promise](#1-the-promise)
2. [The four classes](#2-the-four-classes)
3. [How evidence decides the class](#3-how-evidence-decides-the-class)
4. [Reasons](#4-reasons)
5. [Missing evidence](#5-missing-evidence)
6. [Trust inputs and receipt labels](#6-trust-inputs-and-receipt-labels)
7. [How each class writes](#7-how-each-class-writes)
8. [The flush domain and the join rule](#8-the-flush-domain-and-the-join-rule)
9. [Tickets, receipts and `covers()`](#9-tickets-receipts-and-covers)
10. [Fail-stop poisoning](#10-fail-stop-poisoning)
11. [What a receipt proves, and what it does not](#11-what-a-receipt-proves-and-what-it-does-not)
12. [Untorn writes](#12-untorn-writes)
13. [How store-io is tested](#13-how-store-io-is-tested)

---

## 1. The promise

store-io makes one promise: a `DurableReceipt` exists only once the bytes it covers are durable on the device, under that device's fault model, at the class the receipt names. Everything below explains how a store decides that class, how it earns each receipt, and where the promise ends.

The class is decided once, when a store is created or opened, from **evidence**: facts the device, the operating system and the file system report about the path. Operating-system cache toggles, vendor tables and timings never promote a device.

Source: [`crates/store-io-engine/src/receipt.rs`](../crates/store-io-engine/src/receipt.rs), [`crates/store-io-core/src/decide.rs`](../crates/store-io-core/src/decide.rs)

---

## 2. The four classes

| Class | Report text | Meaning | Durable open |
|---|---|---|---|
| `PowerSafe` | `power-safe` | The device keeps its write cache through power loss: it reports no volatile write cache and its health shows the backup working, or an exact-model certificate says so. A completed durable write is durable; barriers need no flush. | Allowed |
| `FlushRequired` | `flush-required` | The device has a volatile cache and the operating system passes flushes to it. Data is durable once a flush issued after the write completed has itself completed. | Allowed |
| `Unverified` | `unverified` | The evidence could hide suppressed flushes. | Refused unless attested or overridden |
| `Unsafe` | `unsafe` | Something in the stack is known to drop or fake durability. | Refused unless overridden |

Every class except `PowerSafe` needs a device flush for a barrier, including a refused class that was overridden. A read-only open is never refused: reading is always allowed.

Source: [`crates/store-io-core/src/class.rs`](../crates/store-io-core/src/class.rs) (`DurabilityClass`), [`crates/store-io-engine/src/store.rs`](../crates/store-io-engine/src/store.rs) (`refuse`, `open_inner`)

---

## 3. How evidence decides the class

The decision is a pure function of the evidence and the caller's trust inputs (section 6), with no I/O, so the same report always gives the same class. The rules apply in this order:

1. **Unsafe**, if any unsafe reason applies (section 4): an unsafe file system (tmpfs, ramfs, FUSE, a network file system, overlayfs, FAT or exFAT, a pass-through file system such as 9p, drvfs or virtiofs, bcachefs), a RAM disk, direct I/O that is not honoured, data journaling, no write barriers, disabled syncs, compressed or software-encrypted files, flushes the OS reports unsupported or suppresses, Windows "turn off write-cache buffer flushing", parity RAID with a write hole, or a missing full-flush primitive on macOS. The unverified reasons that also apply are reported alongside.
2. **Unverified**, if any unverified reason applies: an unqualified file system, a weak error mode, write-through reported over a cache whose presence is unknown, a USB device, a write-behind mirror member, a filter driver, a loop device, or a hypervisor; on macOS, a device not known to be internal, or a full-flush primitive not checked. An attestation lifts this to `FlushRequired`.
3. Otherwise the stack is clean, and two facts can still block power-safe without making the stack unsafe: a layer that may allocate on overwrite (thin provisioning, deduplication, a virtual-disk file, a copy-on-write file system or a sparse file) and a write-back cache layer. Each adds its reason to the report; macOS also blocks power-safe.
4. **PowerSafe (certified)**, if the caller supplies a certificate, nothing blocks power-safe, and the device has not reported a failed backup.
5. If the device reports **no volatile write cache** and nothing blocks power-safe:
   - health readable and the backup working: **PowerSafe**;
   - health reports a failed backup: **FlushRequired** (reason `BackupFailed`);
   - health unreadable: **FlushRequired**, flagged in the report as a *power-safe candidate*.
6. **FlushRequired**, if the operating system is known to pass flushes to the device: on Windows, the class driver reports flush support; on Linux, the block queue's cache mode is `write back`; on macOS, the full-flush primitive is supported (on an internal device: step 2).
7. Otherwise **Unverified**, with reason `CacheStateUnknown`.

Missing evidence never raises the class: a fact that cannot be read stays unknown, and every rule needs a known fact to promote. A device that reports a volatile cache, under an OS that passes flushes, is therefore `FlushRequired` however much else is missing, and a device whose cache state cannot be read at all is `Unverified`.

Source: [`crates/store-io-core/src/decide.rs`](../crates/store-io-core/src/decide.rs) (`decide`, `unsafe_reasons`, `unverified_reasons`, `flushes_reach_device`), [`crates/store-io-core/src/evidence.rs`](../crates/store-io-core/src/evidence.rs)

---

## 4. Reasons

A reason says why a device is below power-safe, or why it was refused. The report and the `UnsafeDevice` and `Unverified` errors print the text in the second column.

| Reason | Text | Effect | Raised when |
|---|---|---|---|
| `FsUnsafe` | filesystem cannot provide durable direct I/O (tmpfs, FUSE, network, 9p, FAT, RAM disk) | Unsafe | The file system is tmpfs/ramfs, FUSE, network, overlay, FAT/exFAT, pass-through or bcachefs; or the device is a RAM disk; or direct I/O is known not to be honoured. |
| `FsUnqualified` | filesystem not yet qualified for durable writes | Unverified | ReFS, HFS+, btrfs, ZFS, F2FS, Linux ntfs3, or an unrecognised file system. Qualified today: NTFS, ext4 (including ext3 mounts), XFS, APFS, a raw block device. |
| `DataJournal` | mounted with data journaling | Unsafe | ext4 `data=journal`, or the file's `+j` attribute. |
| `NoBarrier` | mounted without write barriers | Unsafe | `nobarrier`, `barrier=0`/`none`/`off`, F2FS `fsync_mode=nobarrier`. |
| `SyncDisabled` | filesystem syncs disabled (for example ZFS sync=disabled) | Unsafe | `sync=disabled`, or an overlay mounted `volatile`. |
| `InodeFlags` | file has data journaling, compression or software encryption enabled | Unsafe | The file is compressed or encrypted (inode flags, `statx` attributes, mount-level compression on Linux; compressed or EFS-encrypted attributes on Windows). |
| `WeakErrorMode` | errors=continue or no journal: metadata errors may be ignored | Unverified | `errors=continue`, `journal_async_commit`, `noload`, `norecovery`, or an external journal (`journal_dev=`, `journal_path=`). |
| `FlushSuppressed` | operating system reports write-through over a device with a volatile cache | Unsafe | The OS cache mode is write-through while the device reports a volatile cache that is not known to be disabled. |
| `FlushUnsupported` | operating system reports that cache flushes are not supported | Unsafe | Windows only: the class driver reports no flush support. |
| `UserPowerProtection` | "turn off write-cache buffer flushing" is set and no power protection is attested | Unsafe | Windows only: the user-defined power protection setting is on. An attestation does not lift it; only an override does. |
| `ParityWriteHole` | parity RAID without a write journal or partial parity log | Unsafe | An MD RAID 4/5/6 array whose consistency policy is neither `journal` nor `ppl`, or cannot be read. |
| `WriteBehind` | mirror member acknowledges before writing (write-behind) | Unverified | An MD array whose write-intent bitmap has a non-zero write-behind backlog. |
| `CacheLayer` | a write-back cache layer sits in the stack | Blocks power-safe | An LVM cache or writecache volume, or bcache in write-back mode. |
| `AllocatingLayer` | a thin, copy-on-write or virtual-disk layer may allocate on overwrite | Blocks power-safe; Unverified for a loop device | An LVM thin pool or snapshot, VDO deduplication, a copy-on-write file system, or a sparse file (Windows); a loop device makes it Unverified. A virtual-disk-file layer is reported for a macOS disk image. |
| `Hypervisor` | running under a hypervisor without a certificate for this volume type | Unverified | A hypervisor is detected, or the disk is a virtual device. |
| `FilterDriver` | a filter driver or software encryption layer sits in the stack | Unverified | A filter layer in the stack. Defined, but no backend detects one yet. |
| `UntrustedBus` | device bus (USB) commonly drops or reorders flushes | Unverified | The device is attached over USB. (An unknown bus does not raise it.) |
| `RemoteTarget` | device kept by another system: over a network or fabric (NVMe over fabrics, iSCSI, Fibre Channel, NBD, RBD) or a user-space server | Unverified | What it acknowledges is the remote target's to keep, and no evidence of that target's storage reaches the probe. Linux: an NVMe namespace with a controller whose `transport` is not `pcie` (or Apple silicon's `apple-nvme`), every controller of a multipath subsystem counted; a SCSI disk under an iSCSI session or a Fibre Channel remote port, or on an SRP, iSCSI or Fibre Channel host driver; NBD, Ceph RBD, RNBD, DRBD, ATA-over-Ethernet, ublk, s390 DASD, PowerFlex and Portworx devices. Windows: the iSCSI and Fibre Channel bus types. An NVMe transport that cannot be read leaves the bus unknown instead, and the namespace's Identify and SMART data unused (so not power-safe from them). |
| `StoragePool` | a storage pool (Windows Storage Spaces) with its own resiliency and caching sits under the volume | Unverified | Windows: the disk's bus type is Storage Spaces. (It is not a hypervisor's disk, and the pool's own disks are not probed.) |
| `BackupFailed` | SMART reports the volatile-memory backup has failed | Demotes power-safe to flush-required | NVMe SMART critical warning bit 4 is set on a device that reports no volatile cache. |
| `CacheStateUnknown` | volatile write cache state could not be read | Unverified | Write-through reported over a cache of unknown presence, or no rule could show that flushes reach the device. |
| `FullFlushUnsupported` | the full-flush primitive (macOS F_FULLFSYNC) is not supported, or was not checked | Unsafe if known unsupported, Unverified if unknown | macOS only: `F_FULLFSYNC` refused on the store's file or directory; unknown on a read-only open, which makes no flush check. |
| `KnownBug` | known kernel or driver bug affecting this configuration | &mdash; | Reserved: no rule raises it yet. |
| `DeviceNotInternal` | device not known to be internal (an external drive, a disk image, or not identified) | Unverified | macOS only: the device, read through DiskArbitration, is not known to be built in. Internal means all of: DiskArbitration calls it internal; its protocol is `Apple Fabric` (on Apple silicon, decided by the machine, not the build: only that), or on an Intel Mac also `PCI-Express` or `SATA`; its media is known to be neither removable nor ejectable; the mount is not removable; it is neither a disk image nor a virtual machine's disk; IOKit shows its media on a single line of parents (no RAID set or multi-store container) hanging from no Thunderbolt-tunnelled PCIe link (`IOPCITunnelled`); and its registry path names no Thunderbolt or USB controller. Anything else is not known to be internal: an SD card, an external enclosure, a Mac Pro's PCIe card on Apple silicon, a device DiskArbitration does not describe or IOKit cannot read. |

Source: [`crates/store-io-core/src/class.rs`](../crates/store-io-core/src/class.rs) (`Reason`), [`crates/store-io-core/src/decide.rs`](../crates/store-io-core/src/decide.rs), [`crates/store-io-posix/src/probe/`](../crates/store-io-posix/src/probe/), [`crates/store-io-posix/src/mac/probe.rs`](../crates/store-io-posix/src/mac/probe.rs), [`crates/store-io-win/src/probe/`](../crates/store-io-win/src/probe/)

---

## 5. Missing evidence

Evidence a backend could not obtain is listed in the report and in the `Unverified` error. Missing evidence is never a reason by itself; it only means a rule that needed the fact could not promote the device.

| Missing | Text | Typically missing when |
|---|---|---|
| `Identify` | device identify data (needs a readable device node) | The device node cannot be opened (Linux without permission), the device is not NVMe, or no physical disk is found behind the volume. |
| `Smart` | SMART / health log (needs privilege on Linux) | As above; health is read only from the NVMe health log. |
| `CachePresence` | volatile write cache presence | Identify is missing and nothing else reports the cache type. |
| `CacheEnabled` | volatile write cache enabled state | The cache feature or property cannot be read. |
| `OsCacheMode` | operating-system cache mode | Linux: no readable `queue/write_cache`; Windows: no write-cache property. |
| `AtomicFields` | device atomic-write fields | The NVMe namespace data cannot be read. |
| `DioAlignment` | direct-I/O alignment | Linux: `statx` does not report it (before 6.1, or not supported by the file system); Windows: no storage information. |
| `Stack` | storage stack layers below the filesystem | Always on Windows (the stack below the volume is not walked); on Linux when the sysfs walk is incomplete or there is no block device to walk from (btrfs, overlay, network, FUSE). |
| `MountOptions` | filesystem mount options | The mount, or ext4's effective options, cannot be found. |
| `InodeFlags` | file attribute flags | The file system has no inode flags, or they cannot be read. |
| `PageCache` | page-cache residency of a range (Linux `cachestat`) | Always on Windows; on Linux when `cachestat` is unavailable (before 6.5). |

Source: [`crates/store-io-core/src/class.rs`](../crates/store-io-core/src/class.rs) (`Missing`), [`crates/store-io-posix/src/probe/mod.rs`](../crates/store-io-posix/src/probe/mod.rs), [`crates/store-io-win/src/probe/mod.rs`](../crates/store-io-win/src/probe/mod.rs)

---

## 6. Trust inputs and receipt labels

`StoreOptions::trust` carries three explicit inputs. Each one changes the label on every receipt the store issues, so the choice is never hidden.

| Input | Effect | Label |
|---|---|---|
| none | The class from evidence alone. | `Evidence` |
| `certificate: Some(id)` | Promotes to `PowerSafe` (rule 4 in section 3) when the stack is clean, nothing blocks power-safe, and the device has not reported a failed backup. It cannot lift an unsafe or unverified stack. | `Certified(id)` when it promoted; otherwise the label the other rules give |
| `attestation: Some(id)` | Lifts `Unverified` to `FlushRequired`. Has no effect on any other class. | `Attested(id)` when it lifted |
| `override_refusal: true` | Allows a durable open on an `Unverified` or `Unsafe` device. The class stays `Unverified` or `Unsafe`, and every barrier issues a device flush. | `Overridden(reasons)` |

A `CertId` is an opaque 64-bit identifier chosen by the caller. A certificate is meant to cover one exact device model, firmware, capacity and LBA format, backed by a power-cut rig log, but **store-io does not verify a certificate**: it records the id, applies it if the stack allows, and prints it. Supplying the right certificate for the right device is the caller's responsibility.

An `Overridden` receipt proves only that store-io issued the most conservative primitive it had: a plain write, and a device flush at every barrier. Use an override for development on virtual machines, CI runners and laptops, never for data you cannot lose.

The report prints the class, the label, whether the open was allowed or overridden, the power-safe-candidate note, every reason and every missing item.

Source: [`crates/store-io-core/src/decide.rs`](../crates/store-io-core/src/decide.rs) (`Trust`, `DurableOpen`, `refused`), [`crates/store-io-core/src/class.rs`](../crates/store-io-core/src/class.rs) (`ReceiptLabel`, `CertId`), [`crates/store-io-engine/src/store.rs`](../crates/store-io-engine/src/store.rs) (`StoreReport`)

---

## 7. How each class writes

The class fixes a store's write policy for the life of the open:

| Class | Data and metadata writes | Barrier (`sync_through`, batch commit, slot commit) |
|---|---|---|
| `PowerSafe` | Durable at completion: `RWF_DSYNC` on Linux files; on Windows and Linux block devices, the write followed by a data flush before completion is reported | No device flush. Returns at once (or `Poisoned`). |
| `FlushRequired` | Plain direct writes | One device data flush, shared through the flush domain (section 8) |
| `Unverified`, `Unsafe` (overridden) | Plain direct writes | One device data flush, shared through the flush domain |

Some steps use a **full flush** (data and metadata of the whole file) instead, outside the flush domain: creating a store, the start of every writable open, provisioning a region, and writing a directory-mode file. A failed full flush during provisioning poisons the store.

The [Platforms](./PLATFORMS.md) document lists the exact system calls behind each primitive.

Source: [`crates/store-io-engine/src/store.rs`](../crates/store-io-engine/src/store.rs) (`policy_of`, `durable_slot`, `flush_all_accounted`), [`crates/store-io-engine/src/io.rs`](../crates/store-io-engine/src/io.rs) (`barrier`)

---

## 8. The flush domain and the join rule

Each open store has one **flush domain**, shared by every region of that store. It decides when a barrier needs a new device flush and lets concurrent barriers share one, with no timer, no window and no waiting for company.

The domain keeps two counters: `issued`, the number of flushes started, and `done`, the highest `k` such that flushes `1..=k` all succeeded. At most one flush runs at a time.

**The join rule.** A barrier must cover a set of writes that have all completed. After the last of them completed, it reads `e = issued` and needs `done >= e + 1`. Flush `e + 1` is started only after `issued` becomes `e + 1`, which happened after that read, so it was started after every covered write had completed. A device flush makes durable every write that completed before the flush was submitted, so flush `e + 1`, or any later one, covers the barrier's writes.

**Leader-inline flushing.** A barrier whose need is not met, and that finds no flush running, becomes the leader: it increments `issued` before issuing the flush, runs the flush on its own thread, publishes `done`, and wakes the others. A barrier that arrives while a flush runs waits for it; if that flush started too early to cover it, it loops and may lead the next one. A lone writer therefore pays exactly one flush, and concurrent writers share flushes.

`Store::domain_stats()` reports `flushes` (flushes issued), `led` (barriers that issued their flush) and `joined` (barriers satisfied by a flush another barrier issued, or already satisfied). `already_durable` counts the receipts an append region's `sync_through` returned without any barrier because an earlier barrier already covered the ticket; such a receipt reports the real durable frontier, never just the ticket's end. Metadata writes on a flush-required store (region headers, slot commits, region-table updates) use the same domain and are counted too.

**Append regions** also keep a durable prefix. A barrier first waits until every append up to its ticket has completed, takes the contiguous completed prefix, then runs the domain barrier, and only after the flush succeeds publishes that prefix as durable. `durable_through()` never passes a write that has not completed, and an append whose reserved range is abandoned unwritten (an error or a panic between reserving and writing) poisons the domain, so no later append can be acknowledged past the hole.

Source: [`crates/store-io-engine/src/domain.rs`](../crates/store-io-engine/src/domain.rs), [`crates/store-io-engine/src/frontier.rs`](../crates/store-io-engine/src/frontier.rs), [`crates/store-io-engine/src/region.rs`](../crates/store-io-engine/src/region.rs) (`sync_through`), [`crates/store-io-engine/src/io.rs`](../crates/store-io-engine/src/io.rs) (`Reserved`)

---

## 9. Tickets, receipts and `covers()`

Positions (`RegionPos`), write tickets (`WriteTicket`) and receipts (`DurableReceipt`) have no public constructors. A ticket exists only for a write that has completed; a receipt exists only after a durable write or a successful barrier.

Each carries the volume, the region, the region generation, and the **epoch**: a number unique to one open store in this process, taken from a process-wide counter when the store is opened.

| Value | Records |
|---|---|
| `WriteTicket` | Its byte range, and its flush number: `issued + 1` read when the write's completion was observed, the first flush that can cover it. |
| `DurableReceipt` | Its class and label; `durable_through()` and `is_prefix()`; `untorn()`; `data_synced()` (a data sync issued after the covered writes completed succeeded); and the flush number it waited for. That number is `u64::MAX` on a power-safe store (every completed write is durable) unless the region forces data sync, and `0` for an append that was already inside the durable prefix (no barrier ran) and for a slot commit. |

`receipt.covers(&ticket)` is true exactly when **all** of these hold:

1. the receipt and the ticket come from the same open store (same epoch);
2. the same volume, the same region and the same region generation;
3. and either
   - the ticket's flush number is not later than the receipt's: the receipt's flush was started after the ticket's write completed; or
   - the receipt is an append-region receipt and the ticket's write ends at or before the receipt's `durable_through()`.

Consequences worth knowing:

- A page-region receipt covers every ticket of that region and generation whose write completed before the receipt's flush started, not only the ticket passed to `sync_through`.
- On a power-safe store a receipt covers every ticket of its region, generation and open, because each such write was durable when it completed; except for a region forcing data sync, whose receipts carry a real flush number and cover only writes completed before that flush (so `covers` and `data_synced` together mean the write was flushed).
- A slot-commit receipt covers no ticket: slots have no tickets, and the receipt stands for the commit alone.
- A reopened store never vouches for an earlier open's writes: tickets and receipts from different opens never match, and `sync_through` refuses a foreign ticket (`ForeignPosition`). A ticket of an older generation is refused with `StaleGeneration`.
- `durable_through()` is, for an append region, the offset before which every byte is durable; for a page region or a slot, the end of the covered write.

Source: [`crates/store-io-engine/src/receipt.rs`](../crates/store-io-engine/src/receipt.rs), [`crates/store-io-engine/src/io.rs`](../crates/store-io-engine/src/io.rs) (`ticket`, `receipt`, `barrier`), [`crates/store-io-engine/src/region.rs`](../crates/store-io-engine/src/region.rs), [`crates/store-io-engine/src/store.rs`](../crates/store-io-engine/src/store.rs) (`EPOCH`)

---

## 10. Fail-stop poisoning

On Linux, a failed `fsync` or `fdatasync` can drop the dirty data it failed to write and clear the error, so that the next flush "succeeds" with nothing left to write. Retrying a flush can therefore report success for data that is gone. store-io never retries a flush and never reports success after one failed: the first failure poisons the store's flush domain, permanently.

The domain is poisoned by:

| Cause | Error the failing call returns |
|---|---|
| A device flush that fails (the leader gets the failure) | `DurabilityUnknown` |
| A data or metadata write that fails, or transfers fewer bytes than asked, after it was submitted | `DurabilityUnknown` |
| An append's reserved range abandoned unwritten (an error, or a panic while the reservation is held) | `DurabilityUnknown`, or the original error |
| A provisioning fill write that fails, or transfers fewer bytes than asked, after it was submitted (running out of space is `NoSpace` instead and poisons nothing) | `DurabilityUnknown` |
| A full flush that fails while provisioning | `DurabilityUnknown` |
| A metadata write (region header, region table, slot) that fails after it was submitted, or whose flush fails | `DurabilityUnknown` |
| `Store::poison()`, for a caller's watchdog that decided the device is hung | &mdash; |

After that:

- every write, barrier, provisioning, reservation, recycle, release and slot commit fails with `Poisoned { first }`, carrying the first cause;
- barriers waiting for a flush are woken and fail with `Poisoned`;
- a barrier already satisfied before it saw the poison keeps its result, so receipts issued for flushes that completed before the failure stay valid;
- reads and scans keep working.

A poisoned store never recovers. Drop it, reopen, and recover with a scan: the writable open first flushes everything the device still holds, and whatever the scan then finds is durable. A `Directory` handle has its own poison with the same rules.

Failures *before* submission are different: `NotWritten` means the media is provably untouched, and `NoSpace` comes only from reservation and provisioning. Neither poisons the data paths.

Source: [`crates/store-io-engine/src/domain.rs`](../crates/store-io-engine/src/domain.rs), [`crates/store-io-engine/src/io.rs`](../crates/store-io-engine/src/io.rs) (`unknown`, `Reserved`), [`crates/store-io-engine/src/store.rs`](../crates/store-io-engine/src/store.rs) (`poison`, `durable_slot`, `flip_table`, `provision`), [`crates/store-io-engine/src/directory.rs`](../crates/store-io-engine/src/directory.rs)

---

## 11. What a receipt proves, and what it does not

A receipt **proves**:

- every byte it covers was made durable by the primitive its class requires: a completed durable write on a power-safe store, or a device flush started after the write completed and finished without error;
- at the class it names, for the reason its label gives;
- for the writes `covers()` accepts (section 9), and for an append region every byte before `durable_through()`.

A receipt **does not** prove:

- **More than the class.** `FlushRequired` relies on the device honouring flushes; `PowerSafe` relies on the device's power-loss protection as reported by its own evidence, or as asserted by the caller's certificate. An `Overridden` receipt proves only that the most conservative primitive was issued.
- **That the bytes are correct.** store-io does not frame or checksum caller data. Your records need their own length and checksum, and the region generation if you recycle.
- **That the bytes stay readable.** Later media errors are reported by reads and scans (`Corruption(MediaError)`, `ScanItem::Unreadable`), never hidden.
- **Anything about writes it does not cover.** A write that completed but was not yet covered by a receipt may be lost, or, if it overwrote a page in place, found torn after a crash. Writes of other regions, other generations and other opens are outside its scope, even when the same flush happened to make them durable.
- **That the write was untorn.** `untorn()` is `false` on every receipt today (section 12).
- **That the region still exists.** After a recycle or a release, an old receipt still describes what was made durable then, but the region's later state is the caller's to track.

Source: [`crates/store-io-engine/src/receipt.rs`](../crates/store-io-engine/src/receipt.rs), [`crates/store-io-core/src/class.rs`](../crates/store-io-core/src/class.rs), [`crates/store-io-engine/src/io.rs`](../crates/store-io-engine/src/io.rs) (`receipt`)

---

## 12. Untorn writes

*Untorn* is not *durable*: an untorn write is one the stack guarantees is never partially persisted after a power loss, and it still needs a durable completion or a barrier. store-io computes the largest untorn unit the evidence supports, and promises nothing when the evidence is incomplete:

| Path | Rule |
|---|---|
| Raw NVMe namespace (store-io's own access) | `base = AWUPF + 1` blocks. With NSABP set, `NAWUPF + 1` blocks (NAWUPF = 0 means "same as AWUPF"), with a boundary of `NABSPF + 1` blocks starting at block NABO (none when NABSPF = 0). A boundary that cannot hold the unit, or NABO beyond NABSPF, is a specification violation: `base` with no boundary is used and the violation is flagged. A namespace shared by several controllers without NSABP gets 1 block. The unit is rounded down to a power of two and capped at the maximum transfer. |
| Linux files and block devices | Only what the running kernel reports through `statx` (`STATX_WRITE_ATOMIC`), never the raw device fields. |
| Windows | The smaller of the file system's reported atomicity and the device's raw unit; nothing if either is unknown. |
| macOS, SATA, SAS, anything else | Nothing. |

No write path claims an untorn unit yet, so every receipt reports `untorn() == false`. Slots and the region table do not depend on untorn writes: their A/B layout tolerates any tear ([Format](./FORMAT.md), section 19; format 1: F13).

Source: [`crates/store-io-core/src/untorn.rs`](../crates/store-io-core/src/untorn.rs), [`crates/store-io-engine/src/io.rs`](../crates/store-io-engine/src/io.rs) (`receipt`)

---

## 13. How store-io is tested

### The deterministic simulator

`store-io-sim` is a platform backend that models one device and decides exactly what survives a crash. The same seed and the same calls give the same run, byte for byte, checked by a trace hash. It models:

- a volatile write cache, or a power-safe device; writes take effect when they complete, optionally in a seeded random order;
- flushes that persist exactly the writes completed before the flush was submitted, and the flushed file's length; full flushes; durable-at-completion writes;
- directory entries that become durable only after a directory sync;
- direct-I/O alignment: a misaligned transfer completes with `EINVAL`;
- crashes that lose every cached write, keep every one, keep any chosen subset (up to 64 writes, enumerable bit by bit), keep, drop or tear each at random from the seed, or give each cached write its exact fate (kept, lost, or torn after so many blocks); tears are at logical-block granularity; then truncate files to their durable length, restore directories, release locks and invalidate handles;
- crash points: a probe before every step that can change what a crash leaves (each queue operation, and each create, rename, unlink, directory sync, allocation, length change, release and full flush) that can cut the power there, every later step and completion failing with `EIO` until the crash; a journaling file system's metadata commit (names and lengths durable early); a process dying with the machine up (its cache kept for the next flush);
- injected faults: a chosen write or flush failing; a failed flush that keeps or drops the cache (the second models the Linux behaviour in section 10); flushes that lie; short writes and reads; lost and misdirected writes; a failed directory sync; unreadable ranges; out of space; pages in the OS page cache; and random I/O errors at a given rate.

The engine's tests run on it: receipted appends survive every crash subset; a slot reads back old or new after every crash mode; a crash during a recycle leaves the old or the new generation; a release interrupted after its table update still frees the space; a failed flush poisons the store and success is never reported again; a short transfer is never success; receipts cover exactly the writes completed before their flush; a reopened store never vouches for an earlier open; a power-safe store never flushes unless a region forces data sync; a read-only open changes nothing; a directory replace leaves the old or the new file at every crash point.

The format's own tests tear a slot overwrite at every byte boundary and flip every single bit of a slot; none is ever accepted as a mixed or altered version.

### Crash enumeration (format 2)

Every change format 2 makes to the media is crashed at every point and judged against the spec's table of crash outcomes (`crates/store-io-engine/tests/format2_crash.rs`, `format2_crash_open.rs`, `format2_damage.rs`; the enumerator is `tests/format2_util/enumerate.rs`):

- **The operations**: create (three table sizes) and the create that takes over a crashed one, provisioning of each kind (page, append, slot, capped, from a reservation) and with a keyed fill into the space a released region left (its data must read as its fill), two provisionings committed to one table page, growth within and beyond capacity (also from a header already one extent ahead), release (also of a region of two extents), recycle, slot commit, `repair_unavailable` (also of a region cut short), `accept_page_rollback` (also of two pages, and of a page whose region lists an extra extent), a clean close and `copy_store` (also from a format-1 source, and with `accept_stale_pages` from a stale one). Each is cut before every step it makes and once after it returns; at each point the writes still cached get every kept/lost combination (exhaustively up to ten, a structured sample past that) and each write is torn at its block boundaries (every one for writes of up to eight blocks, else the first, the middle and the last); where names and lengths matter (create, copy, provisioning, growth beyond capacity, release) each state is judged again with the file system's metadata made durable early.
- **What every state must show**: the operation's before or its after, never a third state, and every outcome the spec allows actually reached; once the operation has returned, only its after. The store's first open must agree with the open after power fails during that first open. Where the store is left writable and nothing is stale, the media must pass an independent audit: every page and region header decoded with the format crate alone, no two regions sharing a byte, free space the exact complement of what is allocated, the tags' charges adding up, a clean close recorded wherever a writable store closed. The open's own verdict (stale and lost pages, the volume record stale, the heal) must equal the format crate's judgement of the media, made without the engine.
- **The open's own writes** (the flush of a dead owner's cached writes, the heal of a torn metadata write, the replacement of a damaged volume copy, the volume record that marks the store open) are enumerated the same way from eleven starting states, among them three dead owners (a process gone with its writes cached) and a store opened read only that must write nothing.
- **Damage**: a table of 52 cases: each kind of damage the spec's judgement tells apart (torn, damaged in its header, garbage, blank, unreadable, an incompatible flag, a fork, a broken chain, a skipped or an older generation, misplaced, another page's or another store's copy, a copy holding another page, a newer format) on the slots where the spec distinguishes it, eight pairs across pairs, under three histories (after a clean close, a stop, and a crash straight after a page write): the open must reach exactly the spec's verdict, and every way out (the open itself, `accept_page_rollback`, `copy_store` with `accept_stale_pages`) must end in a store that opens writable and audits clean, or be refused as documented.

Together these judge about 12,500 crash states on every test run (most of them from creates of the larger table, whose crash points are sampled).

### Concurrency models

The flush domain, the append frontier, the per-region admission gate and the buffer pool's free lists have [loom](https://crates.io/crates/loom) models, built only with `RUSTFLAGS="--cfg loom"` and run by a dedicated CI job (with at most three preemptions per execution). loom explores the interleavings of the modelled threads: two barriers racing, one of them failing; two appenders completing in either order; an operation racing a recycle.

### Compile-time guarantees

Rustdoc `compile_fail` tests show that an integer cannot be passed as a position, that positions and receipts cannot be built by hand, that a ticket is not a position and a receipt is not a ticket, and that a ticket cannot be copied, each next to a compiling twin.

### Real devices

The Windows backend has integration tests on NTFS and the Linux backend on the machine's own file system, and the simple API runs end to end on both; CI also runs the conformance and end-to-end tests on loop-mounted ext4 and XFS images with barriers on. Linux tests read the actual mount options and accept kernel-dependent answers, because CI runners differ from development machines (GitHub's Ubuntu runners mount their root `nobarrier`, which store-io classes as `Unsafe`).

On Linux, CI replays a workload on a real ext4 and XFS through a `dm-log-writes` device (`tests/crash.rs`): appends, batches, page writes, slot commits, a submitter, provisioning, growth within and beyond capacity, recycle, release, and a repair after the container is cut short. The device is rebuilt at every logged write, flush and mark, each state mounted (its journal recovered) and opened, and every acknowledgement passed by then must hold; subsets of the writes a cache could still lose are checked too. A `dm-error` job checks that the store stops at the first failed write or flush, and that every record it acknowledged reads back once the device is healed. NTFS has no such write log, so on Windows the simulator's enumeration stands for the crash states.

### Mutation testing

`dev/tools/mutate.py` holds a list of deliberate defects in the integrity code (each flush, barrier, poison, directory sync, generation bump, and each rule of the open's judgement), each paired with the tests that must catch it. A guard held in layers (an early refusal backed by a later one before any I/O, so removing either alone changes nothing) has a joint entry that removes every layer at once; a check no outside path can reach (behind a store-wide refusal that has its own entry), an invariant, or an exactness rule that loses nothing when broken is listed in the file with why it has no entry. The Mutation workflow runs the whole list (in four shards) on every push to `main` or `dev` that touches the crates, and a defect that survives, or one that no longer applies, fails it.

### Planned

These are on the [roadmap](../dev/ROADMAP.md) and the [later list](../dev/TODO.md):

- **A Windows kill test**: kill a process with writes in flight and prove the ownership lock is not released until they have landed.
- **Power-cut certification** (1.x, after the 1.0 API and format freeze): a rig that cuts the test machine's power while a second machine logs acknowledgements (a whole-machine cut leaves the drive the power supply's hold-up time, often 10-20 ms, which can hide a drive that acknowledges flushes it has not done; cutting the drive's own power is the stronger test, and is the follow-up); at least 500 cycles per device class and 3,000 per certified model and firmware, with the rig logs published with the release. Until then, no store-io receipt has been checked against a real power cut.
- **Fuzz campaigns** (1.0).

Source: [`crates/store-io-sim/`](../crates/store-io-sim/), [`crates/store-io-engine/tests/`](../crates/store-io-engine/tests/), [`crates/store-io-format/src/slot.rs`](../crates/store-io-format/src/slot.rs) (tests), [`crates/store-io-engine/src/domain.rs`](../crates/store-io-engine/src/domain.rs), [`crates/store-io-engine/src/frontier.rs`](../crates/store-io-engine/src/frontier.rs), [`crates/store-io-engine/src/gate.rs`](../crates/store-io-engine/src/gate.rs), [`crates/store-io-buf/src/queue.rs`](../crates/store-io-buf/src/queue.rs), [`crates/store-io-buf/src/stack.rs`](../crates/store-io-buf/src/stack.rs), [`crates/store-io-engine/src/receipt.rs`](../crates/store-io-engine/src/receipt.rs) (compile-fail tests), [`.github/workflows/ci.yml`](../.github/workflows/ci.yml), [`tests/crash.rs`](../tests/crash.rs), [`dev/tools/mutate.py`](../dev/tools/mutate.py), [`.github/workflows/mutation.yml`](../.github/workflows/mutation.yml), [`dev/ROADMAP.md`](../dev/ROADMAP.md), [`dev/TODO.md`](../dev/TODO.md)

---

<div align="center"><sub>Apache-2.0 OR MIT &middot; &copy; 2026 James Gober</sub></div>
