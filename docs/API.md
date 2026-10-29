# store-io &mdash; API Reference

> Every public item of the `store-io` crate, grouped by what it is for. The [guide](./GUIDE.md) shows how they fit together; [docs.rs](https://docs.rs/store-io) has the full rustdoc, which this file mirrors.

**Status: 0.x.** `0.x` releases make no compatibility promise; the SemVer promise starts at `1.0`.

---

## Contents

1. [Opening stores](#1-opening-stores)
2. [Append regions](#2-append-regions)
3. [Page regions](#3-page-regions)
4. [Batches](#4-batches)
5. [Slots](#5-slots)
6. [Directories](#6-directories)
7. [Positions, tickets and receipts](#7-positions-tickets-and-receipts)
8. [Scans](#8-scans)
9. [Space](#9-space)
10. [Device evidence and trust](#10-device-evidence-and-trust)
11. [Errors](#11-errors)
12. [The engine and platforms](#12-the-engine-and-platforms)
13. [The `store-io` tool](#13-the-store-io-tool)

Every call blocks until its I/O is done. Handles (`Store`, regions, slots) are cheap to clone and safe to share between threads.

---

## 1. Opening stores

### `Store`

| Item | Description |
|---|---|
| `Store::create(path, StoreOptions) -> Result<Store>` | Creates a store in `path` (a directory of its own; created if needed). Probes the device and refuses unsafe or unverified devices unless the options' `trust` says otherwise. |
| `Store::open(path) -> Result<Store>` | Opens an existing store for writing with default options. Takes the ownership lock, flushes whatever a dead owner left in the device cache, and verifies every region against its header. A store of container format 1 (made before 0.5) is refused (`Unsupported(FormatVersion)`) before anything is written: open it read-only, or copy it to format 2 with `copy_store`. |
| `Store::open_with(path, StoreOptions) -> Result<Store>` | As `open`, with explicit options. |
| `Store::open_readonly(path) -> Result<Store>` | Opens for reading and recovery with one handle that has no write rights, so it works on a read-only mount or a write-protected copy; nothing on disk changes. Takes the shared form of the ownership lock: other readers may open the store at the same time, and no writer runs meanwhile. |
| `store.append_region(name)` / `page_region(name)` / `slot(name)` | Looks up an existing region or slot. Never creates one. A name the region table holds but open could not use returns `Corruption` naming the region (`Truncated`, `HeaderCrc` or `Metadata`), never `NotFound`. |
| `store.unavailable_regions() -> Vec<UnavailableRegion>` | Regions the region table names that open could not use, each with its name, id, kind and reason (`Unavailable`): the container is too short, neither header copy is valid, the header holds impossible values, belongs to another region, or says released. Empty for a healthy store. Their names stay reserved. Live: a repair removes its entry. |
| `store.frozen() -> bool` | Allocation frozen: a table page is stale or lost (format 2). No provisioning, growth beyond capacity, release or repair until no page is (`accept_page_rollback`), or copy the store (`copy_store` with `accept_stale_pages`). |
| `store.repair_unavailable(id) -> Result<()>` | Releases an unavailable region's table entry (format 2, not frozen; an entry of an unknown kind too, by id). Its page is rewritten without it; its space becomes free where no other region uses it, its name reusable; its bytes are not touched. `NotWritten(Overlap)` (naming the region) for an entry another verdict of the open rests on; `NotWritten(Frozen)` while its page cannot be rewritten (not confirmed, or holding an entry newer than its own last write). See [GUIDE §15](GUIDE.md#15-a-damaged-region-table-the-three-ways-out). |
| `store.accept_page_rollback(pages) -> Result<()>` | Accepts stale table pages as their older copies (format 2): each rewritten over its damaged copy and read back, its newest commit given up. Every page is checked first (a lost page: `NotWritten(NotReady)`; a page holding an entry newer than its last write: `NotWritten(Frozen)`). Regions become writable (not a possibly superseded one, or one with a header feature this version may not write) and allocation unfreezes once no page is stale or lost. A failure after the rewrite poisons the store (the next open decides the page). |
| `store.repairs() -> Vec<Repair>` | What this handle's repairs and accepts did (`EntryReleased`, `InvalidEntryReleased`, `PageAccepted`), for your own log; store-io prints nothing. `store.report()` stays what the open found. |
| `store.provision_append_region(name, bytes)` | Creates an append region with at least `bytes` of data area, writing and verifying every block first. |
| `store.provision_page_region(name, bytes)` | Creates a page region likewise. |
| `store.provision_slot(name)` | Creates a small-object slot. |
| `store.regions() -> Vec<(String, RegionKind)>` | Names and kinds of the ready regions. |
| `store.report() -> &StoreReport` | The evidence, class decision, block size, table capacity, volume id, owner generation and unavailable regions; for format 2, `metadata` (`MetadataReport`): clean close, stale and lost pages, the volume record stale, the heal, the freeze, why read-only, retried reads, and `damaged_slots`, every candidate and damaged slot of the volume record and the pages (`DamagedSlot`: the pair, slot A or B, and `SlotDamage::Older`, `InFlight` or `Damage`). `Display` prints a readable report. |
| `store.space() -> SpaceReport` | See [Space](#9-space). |
| `store.reserve(bytes, tag)`, `set_cap(tag, cap)`, `extent_of(bytes)` | See [Space](#9-space). |
| `store.observe() -> Snapshot` | Event counters (writes, bytes, reads, barriers, flushes, split writes, refused writes, media errors) and sampled latency histograms per stage (write, read, barrier, flush, slot commit, provisioning). Writes, reads and barriers are the data path (metadata writes are not counted); media errors are counted once per unreadable range reported. Recorded only with the `observe` feature, which is off by default and costs nothing when off; on, 18.5 ns per durable write on one thread and about 20.7 ns with every core busy [measured, harness `observe-cost`]. All zeros without it. The types are in `store_io::observe`. |
| `store.oldest_outstanding() -> Option<Outstanding>` | The oldest operation still in flight on the store's queues: its age and whether it is a device flush. For a watchdog the caller owns (store-io keeps no timer): poll it, and call `store.poison()` when the age passes your deadline. The age counts from the first call that saw the operation, and restarts when a queue's oldest operation completes, so it is a lower bound within one poll interval, and a long but progressing job (a large provisioning) never looks hung. Covered: the store's shared queues, its parallel queues and its flush lane. Not covered: whole-file flushes during provisioning, directory syncs, and `Directory`. The I/O path pays a busy mark per queue, on a cache line of its own (about 1-2 ns per operation and one plain store per completion); the call reads a coarse clock once. |
| `store.submit_pool_threads() -> Option<usize>` | Worker threads in the submit pool that runs this store's multi-write page batches and large reads concurrently (Linux synchronous tier), once this store's first such call has started its parallel queues; `None` before that, if they could not be started, and on backends that need no pool. |
| `store.poisoned() -> Option<FirstCause>` | The failure that poisoned the store, if any: every later write and barrier fails with `Poisoned`. A cheap check for a health probe. |
| `store.domain_stats() -> DomainStats` | Flushes issued, barriers that joined another's flush, barriers that led one, and receipts returned without a barrier because the target was already durable (`already_durable`). |
| `store.poison()` | Poisons the device's barrier domain on purpose: every later write and barrier fails. |
| `store.volume() -> VolumeId` | The store's identity. |

Names are 1 to 24 bytes with no NUL. `Store` derefs to `engine::Store<NativePlatform>`, so everything there is available.

### `StoreOptions`

| Field | Default | Meaning |
|---|---|---|
| `trust: Trust` | none | Certificate, attestation and override inputs to the class decision. |
| `block_size: Option<u32>` | device's | Minimum block size; the device may require more. |
| `log2_table_slot: u8` | 14 | Region-table slot size as a power of two (a 16 KiB table page holds 169 regions; the table holds 65,536 by default). |
| `max_io: usize` | 1 MiB | Largest single I/O of the blocking calls, and the batch buffer size. |
| `max_write: usize` | 4 MiB | Largest single submitter operation: one buffer, one OS write or read (rounded up to a power of two, at most 1 GiB). |
| `large_buffers: u32` | 8 | Pool buffers of `max_write` bytes (0: the submitter's largest operation is `max_io`; at most 16,777,215). |
| `buffers_per_class: u32` | 64 | Pooled buffers per size class (at most 16,777,215). |
| `queues: usize` | CPU count (0) | I/O queues shared by blocking calls. |
| `queue_depth: u32` | 64 | Operations in flight per queue. |
| `t3_pool_threads: u32` | CPU count, at most 32 (0) | Worker threads of the submit pool that runs a page batch's writes and a large read's pieces concurrently on the Linux synchronous tier. Shared by every store on one platform and sized by the first of them that uses it, at most 256; each store opened through the simple API has a platform, and so a pool, of its own. Other backends ignore it. |
| `append_window: u64` | 16 MiB | Bytes an append may start ahead of the oldest incomplete one on its region (in blocks, rounded down to a power of two, or the region rounded up when it is smaller; 2 to 2^20 of them; 8 bytes of memory per block per append region). |
| `keyed_fill: bool` | `false` | Fill new regions with a keyed incompressible pattern instead of zeros. |
| `register_file: bool` | `true` | Linux io_uring: register a read-only twin of the container with each `Submitter`'s ring as a direct descriptor, used by reads (D-078, D-107). Best effort; what registered is in `Store::io_path()`. Buffers are never registered (D-107). |
| `linux_tier: LinuxTier` | `Auto` | Linux only. `Auto`: io_uring where the kernel accepts store-io's rings, else the synchronous tier. `Uring`: io_uring or the store is not created. `Sync`: one blocking system call per operation. The choice and its reason are `StoreReport::io` ([PLATFORMS](PLATFORMS.md#the-io-queue)). |

---

## 2. Append regions

`AppendRegion`: store-io picks each append's position; a written block is never written again.

| Item | Description |
|---|---|
| `append(&[u8]) -> Result<WriteTicket>` | Appends at least one byte, starting on a block boundary, zero-padded to the next. Returns once the write has completed; not yet durable. Any size: large data is written in pieces kept in flight together. |
| `sync_through(&WriteTicket) -> Result<DurableReceipt>` | Makes everything appended up to and including the ticket durable. |
| `append_durable(&[u8]) -> Result<(RegionPos, DurableReceipt)>` | `append` then `sync_through`. |
| `batch() -> AppendBatch` | A batch of appends; see [Batches](#4-batches). |
| `read(offset, &mut [u8]) -> Result<usize>` | Reads raw bytes at a block-aligned offset (any length). Copies from store-io's own aligned buffers (direct I/O needs aligned memory, and a read whose wait fails stays in flight, so it never targets your memory); `scan` lends those buffers, and a `Submitter`'s `read_append` reads into one you take with `take_buf`: no copy. |
| `scan(from, visit) -> Result<ScanSummary>` | Reads the region in order; see [Scans](#8-scans). |
| `resume_at(offset) -> Result<u64>` | After a reopen, positions the next append at `offset` rounded up to a block. Appends are refused (`NotPositioned`) until called. Never moves backwards. Refused on a read-only store (`NotWritten(ReadOnly)`), which flushes nothing at open. |
| `recycle() -> Result<AppendRegion>` | Starts the next generation with its append position at 0; returns its handle. |
| `release(self) -> Result<()>` | Gives the region's space back. |
| `generation() -> Generation` | The generation this handle writes. |
| `tail()`, `durable_through()`, `len()`, `is_empty()`, `block_size()` | Next append position, contiguous durable prefix, data-area size, block size. |
| `set_force_data_sync(bool)`, `force_data_sync()` | Every barrier on the region issues a data sync, even on a power-safe store (also on page regions and slots). Runtime only: set it again after each open. |

---

## 3. Page regions

`PageRegion`: blocks you address yourself, overwritten in place.

| Item | Description |
|---|---|
| `pos(offset) -> Result<RegionPos>` | A checked position: block-aligned and inside the region. |
| `write(RegionPos, &[u8]) -> Result<WriteTicket>` | Writes whole blocks (any number). |
| `sync_through(&WriteTicket) -> Result<DurableReceipt>` | Makes the ticket's write durable, with every other write to the store that completed before the call. |
| `write_durable(RegionPos, &[u8]) -> Result<DurableReceipt>` | `write` then `sync_through`. |
| `batch() -> PageBatch` | A batch of page writes. |
| `read(RegionPos, &mut [u8]) -> Result<usize>` | Reads any length at a position. Copies, as an append region's `read` does; `scan` and a `Submitter`'s `read_page` do not. |
| `scan`, `recycle`, `release`, `generation`, `block_size`, `len`, `is_empty` | As for append regions. |

---

## 4. Batches

### `AppendBatch`

Records packed back to back in the device's own buffers; one reservation, the fewest writes, one barrier.

| Item | Description |
|---|---|
| `append(&[u8]) -> Result<u64>` | Adds a record (copied once); returns its offset within the batch. |
| `append_with(len, impl FnOnce(&mut [u8])) -> Result<u64>` | Adds a record encoded in place in a zeroed slice, with no copy. At most `max_in_place()` bytes. |
| `commit() -> Result<(RegionPos, DurableReceipt)>` | Writes the batch and makes it durable. A record's region offset is the returned position's offset plus its batch offset. |
| `write() -> Result<(RegionPos, WriteTicket)>` | Writes without the barrier. |
| `len()`, `is_empty()`, `bytes()`, `clear()`, `max_in_place()` | Records and bytes staged; drop them; the in-place limit. |

A batch holds at most what the buffer pool can lend (`buffers_per_class × max_io`); an append beyond that fails with `PoolExhausted` and leaves the batch unchanged.

### `PageBatch`

| Item | Description |
|---|---|
| `write(RegionPos, &[u8]) -> Result<()>` | Adds a write of whole blocks (checked now, copied once). |
| `write_with(RegionPos, len, impl FnOnce(&mut [u8])) -> Result<()>` | Adds a write encoded in place. |
| `commit() -> Result<DurableReceipt>` | Refuses overlapping writes (`NotWritten(Overlap)`) before any I/O, then submits every write together and issues one barrier. |
| `len()`, `is_empty()`, `clear()` | Writes staged; drop them. |

---

## 5. Slots

`Slot`: a small object that reads back as exactly the old or exactly the new version after any crash.

| Item | Description |
|---|---|
| `commit(&[u8]) -> Result<DurableReceipt>` | Commits a new version: one block write, one barrier. |
| `read() -> Option<Vec<u8>>` | The current version, or `None` if never committed. |
| `capacity() -> usize` | The largest payload: the block size less 192 bytes of headers (3,904 bytes at 4 KiB; a format-1 slot, less 176). |

---

## 6. Directories

`Directory`: small objects as ordinary files, each replaced atomically and durably.

| Item | Description |
|---|---|
| `Directory::open(path, create) -> Result<Directory>` | Opens (or creates) a directory with default trust, and tries a directory flush. If that fails, `replace` and `remove` are refused before any write (`Unsupported(DirectoryFlush)` or `Io(SyncDir)`); `read` still works. |
| `Directory::open_with(path, create, Trust)` | With explicit trust inputs. |
| `replace(name, &[u8]) -> Result<()>` | Temporary file, exact length, flush, rename over the target, directory flush. The file holds the old or the new bytes after any crash. One replace at a time in a directory, across handles and processes (the `.sio-lock` file): another fails `Locked` with nothing written; a file system that cannot lock gives `Io(Lock)`. Names that could resolve to another file are refused (`NotWritten(InvalidName)`, also in `read` and `remove`): longer than 247 bytes, ending in `.` or a space, shaped like a Windows short name, a Windows device name, or the directory's own `.sio-lock` or `*.sio-tmp` (compared as a case-insensitive file system compares names: case folded, ignorable code points dropped). |
| `read(name) -> Result<Vec<u8>>` | Reads a whole file with direct I/O. |
| `remove(name) -> Result<()>` | Unlinks, then flushes the directory. |
| `sync() -> Result<()>` | Flushes the directory. |
| `class()`, `decision()` | The device's class, once a file has been opened through the handle. |

A failure from the rename on (or of the new bytes' write or flush) returns `DurabilityUnknown` and poisons the handle.

---

## 7. Positions, tickets and receipts

None of these can be constructed outside store-io.

| Type | Methods |
|---|---|
| `RegionPos` | `offset()`, `region()`, `generation()` |
| `WriteTicket` | `pos()`, `end()` |
| `DurableReceipt` | `class()`, `label()`, `durable_through()`, `is_prefix()`, `covers(&WriteTicket)`, `data_synced()`, `untorn()`, `volume()`, `region()`, `generation()` |

`covers(&ticket)` is exact: a receipt proves a ticket's write durable when the write completed before the receipt's flush was issued, or (append regions) lies within the durable prefix. Receipts and tickets from different opens of a store never match.

---

## 8. Scans

`scan(from, visit)` on append and page regions calls `visit` with each `ScanItem` in region order:

| `ScanItem` | Meaning |
|---|---|
| `Data { offset, bytes }` | Bytes read at region offset `offset`. |
| `Unreadable { start, end }` | The device could not read these whole sectors (the device's logical blocks where the probe knows them, else store blocks); the scan continues after them. |

`visit` returns `ControlFlow::Continue(())` or `ControlFlow::Break(())`. `ScanSummary` reports `end` (one past the last byte delivered), `unreadable` (bytes reported unreadable) and `stopped`. Scans use direct I/O with eight large reads in flight, and work on read-only and poisoned stores.

---

## 9. Space

| Item | Description |
|---|---|
| `store.reserve(bytes, tag) -> Result<Reservation>` | Checks the tag's cap and grows the container so provisioning from the reservation cannot run out of space. The only `NoSpace` site; never poisons. |
| `reservation.provision_append_region(name, bytes)`, `provision_page_region`, `provision_slot` | Provisions from the reservation. |
| `reservation.tag()`, `remaining()` | The tag; bytes still reserved. The rest returns when dropped. |
| `store.set_cap(tag, Option<u64>)` | A tag's hard cap on its regions plus reservations (in memory). |
| `store.extent_of(bytes) -> Option<u64>` | A region's footprint: data rounded to a block, plus two header blocks. |
| `store.space() -> SpaceReport` | `container`, `regions`, `released`, `reserved`, and `tags: Vec<TagSpace>` (`tag`, `logical`, `physical`, `reserved`, `cap`). |

---

## 10. Device evidence and trust

| Item | Description |
|---|---|
| `probe(dir) -> Result<Probe>` | The `Evidence` and `ClassDecision` for the device under an existing directory (creates and removes a small temporary file). |
| `copy_store(from, to, CopyOptions) -> Result<CopyReport>` | Copies a store (format 1 or 2) into a new format-2 store in `to`: the source opened read-only, every region recreated with its attributes and data, built under `COPYING` and renamed into place only when whole and durable (`to`'s `COPY_LOCK` held throughout). `CopyOptions::store` holds the target's `StoreOptions` (trust among them). `CopyOptions::accept_stale_pages` copies a source with a stale or lost table page or unusable entries instead of refusing it: a stale page's regions in full. `CopyReport` has `regions`, `bytes`, `source_format`, and, for such a copy, `omitted`, `lost_pages`, `stale_pages`, `verified_up_to` and `superseded` (what the copy cannot vouch for: [GUIDE §15](GUIDE.md#15-a-damaged-region-table-the-three-ways-out)). |
| `DurabilityClass` | `PowerSafe`, `FlushRequired`, `Unverified`, `Unsafe`. |
| `ReceiptLabel` | `Evidence`, `Certified(id)`, `Attested(id)`, `Overridden(reasons)`. |
| `ClassDecision` | `class`, `label`, `durable_open: DurableOpen` (`Allowed`, `Overridden`, `RefusedUnsafe`, `RefusedUnverified`), `reasons: ReasonSet`, `missing: MissingSet`, `power_safe_candidate`. |
| `Trust` | `certificate`, `attestation` (`Option<CertId>`), `override_refusal: bool`. |
| `Evidence` | Device facts (model, firmware, bus, write cache, FUA, power protection, block sizes, atomic-write fields), file-system facts (kind, mount options, direct-I/O alignment, flush support), the storage stack, hypervisor, privilege, kernel, and what was missing. |

---

## 11. Errors

`Error` (with `Result<T>`) is one enum with a plain-English `Display`:

| Variant | Meaning |
|---|---|
| `NotWritten { cause: NotWrittenCause, ctx }` | Refused before reaching the device; media untouched. Causes include `Misaligned`, `OutOfBounds`, `ForeignPosition`, `StaleGeneration`, `PoolExhausted`, `NotReady`, `RegionFull`, `Empty`, `TooLarge`, `InvalidName`, `ReadOnly`, `QueueFull`, `NotPositioned`, `Overlap`. |
| `DurabilityUnknown { op, raw, ctx }` | Failed after submission; the domain is now poisoned. |
| `Poisoned { first }` | An earlier failure poisoned the domain. |
| `NoSpace { tag, ctx }` | A reservation or provisioning ran out of space or quota; nothing poisoned. |
| `Corruption { kind, ctx }` | `MediaError`, `HeaderCrc`, `IdentityMismatch`, `Metadata`, `Fork`, `Truncated` (the container is shorter than its metadata says). |
| `UnsafeDevice { reasons }`, `Unverified { missing, reasons }` | The device was refused. |
| `Unsupported { what }` | A capability the platform lacks; or (`FormatVersion`) a newer on-disk format, or a format-1 store opened for writing (copy it with `copy_store`). |
| `Fenced { stale }`, `Locked` | Ownership: a newer owner exists (never returned today: ownership is the OS lock with the acquire barrier, which no newer owner passes while a handle holds the store, D-049; reserved until the 1.0 API freeze); another holder has the lock (another handle, in this or another process, holds the store, or another handle or process is replacing in a directory). |
| `NotFound { what }`, `AlreadyExists { what }` | Store, region or slot lookups. `NotFound` means no such name exists in the store at all. |
| `Io { op, raw, ctx }` | A setup or metadata failure; durable data unaffected. |

`ErrorContext` names the region and byte range where known; `OsError` keeps the raw errno, Win32 or NTSTATUS code. `Error` is at most 64 bytes.

---

## 12. The engine and platforms

- `store_io::engine` is the generic engine (`engine::Store<P>` and friends) over any `Platform`, including the deterministic simulator used by store-io's own tests.
- `NativePlatform` is `store_io_win::WinPlatform` on Windows and `store_io_posix::PosixPlatform` on Linux and macOS. On macOS the store's class needs an internal device (DiskArbitration and IOKit; `DeviceNotInternal` otherwise), every flush is `F_FULLFSYNC`, and nothing is power-safe ([Platforms](./PLATFORMS.md), section 5). On other targets only the shared types compile.
- The engine layer: `Store::submitter(SubmitterConfig)` returns a `Submitter`, which keeps many operations in flight from one thread with owned buffers and completion tags ([GUIDE section 13](GUIDE.md#13-many-operations-from-one-thread-the-submitter)). Its types are in `store_io::submit`:
  - `SubmitterConfig`, `MAX_DEPTH`;
  - `WriteMode` (`Plain`, `Durable`);
  - `Completion` (tag, buffer, outcome), `Completions`, `Done` (`Written`, `Read`, `Synced`, `Flushed`), `Written` (ticket and receipt), `Rejected` (error and buffer);
  - `IoBuf`, `Wake`, `Waker`.
- Methods:
  - Submission: `take_buf`, `max_buf`, `append`, `write`, `read_append`, `read_page`.
  - Barriers: `barrier(&ticket, tag)`, `barrier_all(tag)`.
  - Completions: `reap`, `wait(min)`, `in_flight`, `waker`.
- `LinuxTier`, `IoPath` and `IoInterface` (`IoUring`, `Synchronous`, `Iocp`, `Simulator`, `Other`) describe the kernel interface a store uses; `store_io::io_uring_fallbacks()` (Linux) counts operations the io_uring tier finished with a blocking call.

---

## 13. The `store-io` tool

Built with the `cli` feature (`cargo install store-io --features cli`):

```text
store-io probe <dir>    evidence and durability class of <dir>'s device
store-io info <store>   a store's report, regions and space (read-only)
store-io copy [--override-refusal] [--accept-stale-pages] <old> <new>
                        copy the store in <old> into a new format-2 store in <new>
                        (--accept-stale-pages: a damaged table copied as far as it
                        can be; exit status 3 when it lists what it could not vouch for)
```

---

<div align="center"><sub>Apache-2.0 OR MIT &middot; &copy; 2026 James Gober</sub></div>
