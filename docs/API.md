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
| `Store::open(path) -> Result<Store>` | Opens an existing store for writing with default options. Takes the ownership lock, flushes whatever a dead owner left in the device cache, and verifies every region against its header. |
| `Store::open_with(path, StoreOptions) -> Result<Store>` | As `open`, with explicit options. |
| `Store::open_readonly(path) -> Result<Store>` | Opens for reading and recovery: all I/O through a handle without write rights; nothing on disk changes. Takes the ownership lock, so no writer runs meanwhile. |
| `store.append_region(name)` / `page_region(name)` / `slot(name)` | Looks up an existing region or slot. Never creates one. |
| `store.provision_append_region(name, bytes)` | Creates an append region with at least `bytes` of data area, writing and verifying every block first. |
| `store.provision_page_region(name, bytes)` | Creates a page region likewise. |
| `store.provision_slot(name)` | Creates a small-object slot. |
| `store.regions() -> Vec<(String, RegionKind)>` | Names and kinds of the ready regions. |
| `store.report() -> &StoreReport` | The evidence, class decision, block size, table capacity, volume id and owner generation. `Display` prints a readable report. |
| `store.space() -> SpaceReport` | See [Space](#9-space). |
| `store.reserve(bytes, tag)`, `set_cap(tag, cap)`, `extent_of(bytes)` | See [Space](#9-space). |
| `store.domain_stats() -> DomainStats` | Flushes issued, barriers that joined another's flush, barriers that led one. |
| `store.poison()` | Poisons the device's barrier domain on purpose: every later write and barrier fails. |
| `store.volume() -> VolumeId` | The store's identity. |

Names are 1 to 24 bytes with no NUL. `Store` derefs to `engine::Store<NativePlatform>`, so everything there is available.

### `StoreOptions`

| Field | Default | Meaning |
|---|---|---|
| `trust: Trust` | none | Certificate, attestation and override inputs to the class decision. |
| `block_size: Option<u32>` | device's | Minimum block size; the device may require more. |
| `log2_table_slot: u8` | 14 | Region-table slot size as a power of two (16 KiB holds 253 regions). |
| `max_io: usize` | 1 MiB | Largest single I/O, and the batch buffer size. |
| `buffers_per_class: u32` | 64 | Pooled buffers per size class. |
| `queues: usize` | CPU count (0) | I/O queues shared by blocking calls. |
| `queue_depth: u32` | 64 | Operations in flight per queue. |
| `append_ring: usize` | 4096 | Blocks an append may start ahead of the oldest incomplete one. |
| `keyed_fill: bool` | `false` | Fill new regions with a keyed incompressible pattern instead of zeros. |

---

## 2. Append regions

`AppendRegion`: store-io picks each append's position; a written block is never written again.

| Item | Description |
|---|---|
| `append(&[u8]) -> Result<WriteTicket>` | Appends at least one byte, starting on a block boundary, zero-padded to the next. Returns once the write has completed; not yet durable. Any size: large data is written in pieces kept in flight together. |
| `sync_through(&WriteTicket) -> Result<DurableReceipt>` | Makes everything appended up to and including the ticket durable. |
| `append_durable(&[u8]) -> Result<(RegionPos, DurableReceipt)>` | `append` then `sync_through`. |
| `batch() -> AppendBatch` | A batch of appends; see [Batches](#4-batches). |
| `read(offset, &mut [u8]) -> Result<usize>` | Reads raw bytes at a block-aligned offset (any length). |
| `scan(from, visit) -> Result<ScanSummary>` | Reads the region in order; see [Scans](#8-scans). |
| `resume_at(offset) -> Result<u64>` | After a reopen, positions the next append at `offset` rounded up to a block. Appends are refused (`NotPositioned`) until called. Never moves backwards. |
| `recycle() -> Result<AppendRegion>` | Starts the next generation with its append position at 0; returns its handle. |
| `release(self) -> Result<()>` | Gives the region's space back. |
| `generation() -> Generation` | The generation this handle writes. |
| `tail()`, `durable_through()`, `len()`, `is_empty()`, `block_size()` | Next append position, contiguous durable prefix, data-area size, block size. |

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
| `read(RegionPos, &mut [u8]) -> Result<usize>` | Reads any length at a position. |
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
| `capacity() -> usize` | The largest payload: the block size less 176 bytes of headers. |

---

## 6. Directories

`Directory`: small objects as ordinary files, each replaced atomically and durably.

| Item | Description |
|---|---|
| `Directory::open(path, create) -> Result<Directory>` | Opens (or creates) a directory with default trust. |
| `Directory::open_with(path, create, Trust)` | With explicit trust inputs. |
| `replace(name, &[u8]) -> Result<()>` | Temporary file, exact length, flush, rename over the target, directory flush. The file holds the old or the new bytes after any crash. |
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
| `DurableReceipt` | `class()`, `label()`, `durable_through()`, `covers(&WriteTicket)`, `untorn()`, `volume()`, `region()`, `generation()` |

`covers(&ticket)` is exact: a receipt proves a ticket's write durable when the write completed before the receipt's flush was issued, or (append regions) lies within the durable prefix. Receipts and tickets from different opens of a store never match.

---

## 8. Scans

`scan(from, visit)` on append and page regions calls `visit` with each `ScanItem` in region order:

| `ScanItem` | Meaning |
|---|---|
| `Data { offset, bytes }` | Bytes read at region offset `offset`. |
| `Unreadable { start, end }` | The device could not read these whole blocks; the scan continues after them. |

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
| `Corruption { kind, ctx }` | `MediaError`, `HeaderCrc`, `IdentityMismatch`, `Metadata`, `Fork`. |
| `UnsafeDevice { reasons }`, `Unverified { missing, reasons }` | The device was refused. |
| `Unsupported { what }` | A capability the platform lacks. |
| `Fenced { stale }`, `Locked` | Ownership: a newer owner exists; another process holds the store. |
| `NotFound { what }`, `AlreadyExists { what }` | Store, region or slot lookups. |
| `Io { op, raw, ctx }` | A setup or metadata failure; durable data unaffected. |

`ErrorContext` names the region and byte range where known; `OsError` keeps the raw errno, Win32 or NTSTATUS code. `Error` is at most 64 bytes.

---

## 12. The engine and platforms

- `store_io::engine` is the generic engine (`engine::Store<P>` and friends) over any `Platform`, including the deterministic simulator used by store-io's own tests.
- `NativePlatform` is `store_io_win::WinPlatform` on Windows and `store_io_posix::PosixPlatform` on Linux. macOS is planned for 0.5; on other targets only the shared types compile.
- The engine layer (a per-thread submitter with owned buffers and completion tags) is planned for 0.4.

---

## 13. The `store-io` tool

Built with the `cli` feature (`cargo install store-io --features cli`):

```text
store-io probe <dir>    evidence and durability class of <dir>'s device
store-io info <store>   a store's report, regions and space (read-only)
```

---

<div align="center"><sub>Apache-2.0 OR MIT &middot; &copy; 2026 James Gober</sub></div>
