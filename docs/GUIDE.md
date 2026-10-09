# store-io &mdash; Guide

> A walk through store-io from the first durable write to recovery after a crash. Every example uses the simple API in the `store-io` crate; the [API reference](./API.md) lists every item, and [docs.rs](https://docs.rs/store-io) has the rustdoc.

---

## Contents

1. [What a receipt proves](#1-what-a-receipt-proves)
2. [Your first durable append](#2-your-first-durable-append)
3. [Append regions and page regions](#3-append-regions-and-page-regions)
4. [Durable now, or batched](#4-durable-now-or-batched)
5. [Many writers](#5-many-writers)
6. [Small objects: slots and directory files](#6-small-objects-slots-and-directory-files)
7. [Recovery: read-only open, scan, resume](#7-recovery-read-only-open-scan-resume)
8. [The device report](#8-the-device-report)
9. [Errors, and what to do about each](#9-errors-and-what-to-do-about-each)
10. [Space: reservations and caps](#10-space-reservations-and-caps)
11. [Recycling and releasing regions](#11-recycling-and-releasing-regions)
12. [Tuning](#12-tuning)

---

## 1. What a receipt proves

store-io's one promise is the **receipt**. A `DurableReceipt` exists only once the bytes it covers are durable on the device, under that device's fault model, at the durability class it names:

| Class | Meaning | How a write becomes durable |
|---|---|---|
| `PowerSafe` | The device keeps its write cache through power loss, shown by its own evidence or a certificate for the exact model and firmware. | At completion. A barrier issues no flush. |
| `FlushRequired` | The device has a volatile write cache and honours flushes. | A device flush, shared between concurrent writers. |
| `Unverified` | The evidence is incomplete (a virtual disk, an unreadable stack). | Refused unless you attest or override. |
| `Unsafe` | The stack is known to drop or fake durability (`nobarrier`, tmpfs, network or FUSE file systems). | Refused unless you override. |

A receipt also carries a **label** saying why the class is trusted: `Evidence`, `Certified`, `Attested` or `Overridden`. An overridden receipt proves only that store-io issued the most conservative primitive it had; treat it accordingly.

Receipts cannot be forged. Positions (`RegionPos`), write tickets (`WriteTicket`) and receipts have no public constructors, so a sequence number can never stand in for a position, and a barrier can never be skipped by accident. `receipt.covers(&ticket)` tells you exactly whether a receipt proves a given write durable.

---

## 2. Your first durable append

```rust
use store_io::{Store, StoreOptions};

// First run: create. The device is probed, and unsafe or unverified devices
// are refused with an error that says why.
let store = Store::create("data/orders", StoreOptions::default())?;

// Regions are provisioned once, with their full size up front: provisioning
// writes and verifies every block, so later writes never allocate.
let wal = store.provision_append_region("wal", 256 << 20)?;

// Durable when this returns.
let (pos, receipt) = wal.append_durable(b"order 1001: 3 widgets")?;
assert!(receipt.durable_through() > pos.offset());
```

Every later run opens instead of creating; opening never creates anything:

```rust
let store = Store::open("data/orders")?;
let wal = store.append_region("wal")?;
```

A store is one container file (`store.sio`) in a directory of its own. Only one process can open a store for writing at a time; a second writable open fails with `Error::Locked`.

---

## 3. Append regions and page regions

**Append regions** are for logs. store-io chooses each append's position from an atomic tail, so appends from many threads never overlap, and a written block is never written again, so a torn later write can never damage an earlier receipt.

- `append(&data)` accepts any length of at least one byte. The append starts on a block boundary and is zero-padded to the next one; the padding is not your data.
- Data larger than one pooled buffer (1 MiB by default) is written in pieces, several in flight together.

**Page regions** are for blocks you address yourself and overwrite in place:

```rust
let pages = store.provision_page_region("pages", 1 << 30)?;
let bs = pages.block_size() as usize;          // 4096 on most devices
let at = pages.pos(8 * bs as u64)?;            // checked: aligned and in bounds
pages.write_durable(at, &page_bytes)?;         // whole blocks only
pages.read(at, &mut buf)?;
```

A position belongs to one region and one generation of it: passing it to another region, or after a recycle, fails before any I/O.

---

## 4. Durable now, or batched

There are three ways to make writes durable. Each costs exactly what it says:

```rust
// One durable write: one write, one flush (no flush on a power-safe device).
let (pos, receipt) = wal.append_durable(&record)?;

// Several writes, one barrier: N writes, one flush.
let t1 = wal.append(&a)?;
let t2 = wal.append(&b)?;
let receipt = wal.sync_through(&t2)?;    // append regions are a prefix: covers t1 too

// A batch: records packed back to back, the fewest writes, one flush.
let mut batch = wal.batch();
let a_at = batch.append(&a)?;            // offset of `a` within the batch
let b_at = batch.append_with(64, |buf| encode_into(buf))?;  // encode in place, no copy
let (start, receipt) = batch.commit()?;
let a_pos = start.offset() + a_at;       // where `a` landed in the region
```

A batch is the cheapest way to write many small records: 100 records of 64 bytes are one 8 KiB write and one flush. `append_with` hands you a zeroed slice inside the very buffer the device will read, so encoding there costs no copy. A batch keeps its buffer list between commits and makes no allocation after the first.

Page batches work the same way for page regions: `pages.batch()`, then `write` or `write_with` per page, then one `commit`. Writes in one batch must not overlap; the batch checks this before any I/O.

Neither layer has a timer or a "group-commit window": nothing waits for company. A lone durable write never pays for a batch it is not in.

---

## 5. Many writers

Handles are cheap to clone and safe to share: give each thread a clone of `wal`. When several threads ask for durability at once, they **share flushes**:

- The first barrier to find no flush running issues one and becomes the leader.
- Every barrier that arrives while that flush runs waits for it if the flush covers its writes, or for the next one if it does not.
- A flush covers exactly the writes that completed before it was issued, so no receipt ever claims a write the flush could not have covered.

Durable throughput therefore rises with the number of writers, with no timer and no tuning. `store.domain_stats()` shows how many flushes were issued and how many barriers joined one.

---

## 6. Small objects: slots and directory files

A **slot** holds a small object (a manifest, a superblock) inside the store. After any crash it reads back as exactly the old version or exactly the new one:

```rust
let manifest = store.provision_slot("manifest")?;   // payload up to manifest.capacity() bytes
manifest.commit(&encoded)?;                           // one block write, one flush
let current: Option<Vec<u8>> = manifest.read();
```

A **directory** holds small objects as ordinary files, for formats other tools must read (a `CURRENT` pointer, a configuration):

```rust
use store_io::Directory;

let meta = Directory::open("data/meta", true)?;
meta.replace("CURRENT", b"MANIFEST-000042")?;
let current = meta.read("CURRENT")?;
```

`replace` writes a temporary file in the same directory, sets its exact length, flushes it, renames it over the target and flushes the directory. It reports success only after all of that. If anything fails from the rename on, the directory may name either version, so the call returns `DurabilityUnknown` and the handle refuses further changes. An unsupported directory flush is an error, never a success.

---

## 7. Recovery: read-only open, scan, resume

store-io never decides what you keep after a crash; it gives you the bytes and gets out of the way.

**Inspect without changing anything.** A read-only open takes no write rights and modifies nothing on disk:

```rust
let store = Store::open_readonly("data/orders")?;
```

**Scan to find where your valid data ends.** A scan hands you every byte of a region in order, keeping several large direct reads in flight. It never stops at bad data: a range the device cannot read is narrowed down to whole blocks, reported as `Unreadable`, and skipped.

```rust
use std::ops::ControlFlow;
use store_io::ScanItem;

let wal = store.append_region("wal")?;
let mut end_of_valid = 0;
wal.scan(0, |item| match item {
    ScanItem::Data { offset, bytes } => {
        match my_log.parse(offset, bytes) {     // your framing, your checksums
            Parsed::More(end) => { end_of_valid = end; ControlFlow::Continue(()) }
            Parsed::End => ControlFlow::Break(()),
        }
    }
    ScanItem::Unreadable { start, end } => {
        my_log.note_hole(start, end);           // your policy: stop, or skip
        ControlFlow::Continue(())
    }
})?;
```

**Resume appending after the valid data.** After a writable reopen, an append region refuses appends until you say where the valid data ends (`NotPositioned`); store-io never guesses:

```rust
let store = Store::open("data/orders")?;
let wal = store.append_region("wal")?;
wal.resume_at(end_of_valid)?;      // the next append starts at the next block boundary
```

**What your records should carry.** store-io does not frame your bytes, so your records should carry their own length and checksum, and the region generation if you recycle regions (section 11). Two things to know when walking a log:

- Each `append` ends with zero padding up to the next block boundary.
- The unwritten part of a region reads as zeros, or as a keyed incompressible pattern if `StoreOptions::keyed_fill` is set.

A writable open first flushes everything a previous, dead owner left in the device cache, so whatever you find afterwards is durable.

---

## 8. The device report

```rust
println!("{}", store.report());
```

The report shows the evidence store-io read (device model and firmware, bus, write cache, FUA, power-loss protection, file system, mount options, storage stack, hypervisor), the class decided from it, the reasons, and what could not be read. The command-line tool shows the same for any directory:

```text
cargo install store-io --features cli
store-io probe /var/lib/mydb
store-io info  /var/lib/mydb/orders
```

When a device is refused, you have three ways forward, each recorded in every receipt's label:

- **Certificate** (`Trust::certificate`): a certificate for this exact device model, firmware, capacity and LBA format, backed by a power-cut rig log. It may promote the device to `PowerSafe`.
- **Attestation** (`Trust::attestation`): your operator vouches for this stack, lifting `Unverified` to `FlushRequired`.
- **Override** (`Trust::override_refusal`): proceed anyway on an unverified or unsafe device. Receipts are labelled `Overridden` and prove only that the most conservative primitive was issued.

```rust
use store_io::{StoreOptions, Trust};

let opts = StoreOptions {
    trust: Trust { override_refusal: true, ..Trust::default() },
    ..StoreOptions::default()
};
```

Use an override for development on virtual machines, CI runners and laptops, never for data you cannot lose.

---

## 9. Errors, and what to do about each

Every error carries the operation, the region and byte range where known, and the raw OS code.

| Error | What happened | What to do |
|---|---|---|
| `NotWritten { cause }` | Refused before reaching the device; the media is untouched. | Fix the cause (alignment, bounds, size, name, read-only, `NotPositioned`) and retry. |
| `DurabilityUnknown` | A write or flush failed after submission; the bytes may or may not be durable. The device's barrier domain is now poisoned. | Stop writing. Drop the store, reopen, and recover with a scan. |
| `Poisoned` | An earlier failure poisoned the domain; nothing will be written. Reads still work. | Same as above. |
| `NoSpace { tag }` | A reservation or provisioning ran out of space or quota. Nothing was written or poisoned. | Free space, raise the cap, or release regions. |
| `Corruption` | store-io's own metadata or the media is damaged. | Recover from a replica or backup; scan what remains. |
| `UnsafeDevice`, `Unverified` | The device was refused at open. | Use another device, or attest or override (section 8). |
| `Locked` | Another process holds the store. | Wait for it, or stop it. |
| `NotFound`, `AlreadyExists` | A store, region or slot lookup. | Create, or open. |
| `Unsupported` | The platform cannot do this. | See the platform notes. |
| `Io` | A setup or metadata call failed; durable data is unaffected. | Check the raw code. |

A failed flush is never retried, and success is never reported after one. This is deliberate: on Linux a failed `fsync` can drop the dirty data and then let the next `fsync` "succeed" with nothing left to write.

---

## 10. Space: reservations and caps

Provisioning writes every block of a region up front, so writes into a ready region never allocate and never fail for lack of space. To make provisioning itself safe from a filling disk, reserve first:

```rust
let mut r = store.reserve(1 << 30, TENANT_42)?;    // checks the tag's cap; grows the container now
let wal = r.provision_append_region("t42-wal", 256 << 20)?;  // cannot run out of space
```

- `reserve` is the only place a full file system or quota surfaces, as `NoSpace { tag }`; nothing is ever poisoned by it.
- Whatever is left of a reservation returns when it is dropped.
- `store.set_cap(tag, Some(bytes))` limits a tag's regions and reservations together; tags are opaque numbers you choose (a tenant, a class).
- `store.space()` reports the container, ready and released regions, outstanding reservations, and usage per tag. Tags are recorded with each region, so usage is right after a reopen.

---

## 11. Recycling and releasing regions

These run only when you call them; store-io never reclaims space on its own.

- **Recycle** keeps a region's blocks and starts a new generation: `let wal = wal.recycle()?;`.
  - It waits for every operation in flight on the region, writes the next generation into the region's header, and makes it durable. Nothing else is written, so the old bytes are still on disk.
  - Put `wal.generation()` into your records to tell old from new after a crash.
  - Every handle, ticket and position of the old generation is refused from then on.
- **Release** gives a region's space back: `pages.release()?;`.
  - The region table marks it released first, then its header, then the space is deallocated.
  - A later region of the same size reuses the space after refilling it, so released bytes never come back.

---

## 12. Tuning

`StoreOptions` holds every choice, and every durability-affecting one appears in the report:

| Option | Default | Effect |
|---|---|---|
| `block_size` | device's | Minimum block size; the device may require more. |
| `max_io` | 1 MiB | Largest single I/O, and the batch buffer size. |
| `buffers_per_class` | 64 | Pooled buffers per size class; bounds how much a batch can stage. |
| `queues` | CPU count | I/O queues shared by blocking calls. |
| `queue_depth` | 64 | Operations in flight per queue. |
| `append_ring` | 4096 | Blocks an append may run ahead of the oldest incomplete one. |
| `keyed_fill` | off | Fill new regions with an incompressible pattern instead of zeros (thin, deduplicating or compressing storage). |
| `log2_table_slot` | 14 | Region-table size: 16 KiB holds 253 regions. |

The engine layer, a per-thread submitter with owned buffers and completion tags for full asynchronous control, arrives in 0.4.

---

<div align="center"><sub>Apache-2.0 OR MIT &middot; &copy; 2026 James Gober</sub></div>
