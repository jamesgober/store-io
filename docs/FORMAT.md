# store-io &mdash; Format

> The on-disk format of a store's container file, `store.sio`, byte by byte: the layout, the A/B slot every piece of metadata is written with, the metadata records, the keyed fill pattern, every validation rule, how a reader decides which copy of each record is current, and why each multi-step change survives a crash. It is written so that an independent reader can be built from it. Every statement is taken from the code, and each section names its source.
>
> **Status: pre-1.0.** store-io 0.5 creates **container format 2** (Part I). It still opens a container of **format 1**, the format store-io 0.1 to 0.4 write, but for reading only (Part II); `copy_store` (`store-io copy`) copies one into a new format-2 store. Either format may change incompatibly before the first stable release, and a container written by one 0.x release is not promised to open in another.

---

## The two formats

| | Format 1 | Format 2 |
|---|---|---|
| Written by | store-io 0.1 to 0.4 | store-io 0.5 |
| Opened by 0.5 | read-only (a writable open fails `Unsupported(FormatVersion)` before anything is written) | read and write |
| Slot header `format_major` (offset 14) | `1` | `2` |
| Slot header bytes `[96, 104)` | reserved, zero | `meta_seq` |
| Volume record | 48 bytes | 64 bytes |
| Region table | one A/B pair of T bytes, 64-byte entries | `P` A/B page pairs of T bytes each, 96-byte entries |
| Region ids | a store-wide counter, `u32` | `page << 32 \| seq` |
| Region header | 48 bytes; one extent per region | 64 bytes, then a list of extents or a slot object |
| Choosing the current copy | the pair rule for every pair | the pair rule for region headers; a store-wide judgement for the volume record and the table pages |

**Telling them apart.** Both formats keep the volume record in the A/B pair at offsets `0` and `B`, and every slot of both begins with the same magic. A reader finds a slot header at offset `0` or `B` whose header CRC verifies (section 2) and reads its `format_major` field, the `u16` at byte 14: `2` is format 2, and the container is read as Part I describes; any other value is read as format 1 (Part II), so a value other than `1` makes the volume pair Refused and the open fails `Unsupported(FormatVersion)`. Within one container every slot carries the same `format_major`: a reader of a format-2 container rejects a slot of any other format as `UnsupportedVersion` (section 5).

---

## Contents

**Part I: Container format 2**

1. [Conventions](#1-conventions)
2. [Finding the volume record and the format](#2-finding-the-volume-record-and-the-format)
3. [Container layout and geometry](#3-container-layout-and-geometry)
4. [The slot](#4-the-slot)
5. [Validating a slot](#5-validating-a-slot)
6. [The pair rule](#6-the-pair-rule)
7. [The volume record (object 0)](#7-the-volume-record-object-0)
8. [Table pages and entries (object 2^61 | p)](#8-table-pages-and-entries-object-261--p)
9. [Region headers and slot objects (object 2^62 | id)](#9-region-headers-and-slot-objects-object-262--id)
10. [Region data, capacity and growth](#10-region-data-capacity-and-growth)
11. [Names](#11-names)
12. [The keyed fill pattern](#12-the-keyed-fill-pattern)
13. [Validating a region](#13-validating-a-region)
14. [Judging the volume record and the table pages](#14-judging-the-volume-record-and-the-table-pages)
15. [Settling regions against each other](#15-settling-regions-against-each-other)
16. [Reading a container](#16-reading-a-container)
17. [Space: placement and reuse](#17-space-placement-and-reuse)
18. [Versioning and feature flags](#18-versioning-and-feature-flags)
19. [Crash consistency of each change](#19-crash-consistency-of-each-change)
20. [Damage and the ways out](#20-damage-and-the-ways-out)
21. [What format 2 does not check](#21-what-format-2-does-not-check)

**Part II: Format 1 (read-only from 0.5)**

- [F1. Conventions](#f1-conventions) &middot; [F2. Container layout](#f2-container-layout) &middot; [F3. The slot](#f3-the-slot) &middot; [F4. Validating a slot](#f4-validating-a-slot) &middot; [F5. Choosing the winner of a pair](#f5-choosing-the-winner-of-a-pair) &middot; [F6. The volume record](#f6-the-volume-record-object-0) &middot; [F7. The region table](#f7-the-region-table-object-1) &middot; [F8. Region headers and slot objects](#f8-region-headers-and-slot-objects) &middot; [F9. The keyed fill pattern](#f9-the-keyed-fill-pattern) &middot; [F10. Reading a container](#f10-reading-a-container) &middot; [F11. Region placement and space reuse](#f11-region-placement-and-space-reuse) &middot; [F12. Versioning and feature flags](#f12-versioning-and-feature-flags) &middot; [F13. Crash consistency of each change](#f13-crash-consistency-of-each-change) &middot; [F14. What store-io does not check](#f14-what-store-io-does-not-check)

---

# Part I: Container format 2

## 1. Conventions

- **Byte order.** Every integer is little-endian. `u8`, `u16`, `u32` and `u64` are unsigned.
- **Offsets.** Offsets in the layout tables are relative to the start of the structure being described. Offsets stored *in* the format (`slot_offset`, `table_offset`, `header_a`, `header_b`, `span_start`, `span_end`, an extent's offset) are absolute byte offsets in the container file.
- **Ranges** are half-open: `[a, b)` holds `a` and not `b`. Two ranges that touch (one ends where the other starts) do not overlap.
- **Reserved bytes** are written as zero, and every reserved field of format 2 is checked to be zero (each section says what a non-zero value does).
- **Checksum.** CRC-32C (Castagnoli): reflected polynomial `0x82F63B78` (normal form `0x1EDC6F41`), initial value `0xFFFFFFFF`, final XOR `0xFFFFFFFF`, reflected input and output. The CRC of the ASCII bytes `123456789` is `0xE3069283`, and the CRC of empty input is `0`. A stored CRC is a `u32`, little-endian.
- **B** is the container's block size, **T** its table slot size, **P** the number of table pages and **E** the number of entries a page holds (section 3). B and T are powers of two.
- **Durably** means: on a power-safe device, a write with durability at completion; on every other class, a plain write followed by a device flush ([Durability](./DURABILITY.md)). A **full flush** is a flush of the whole file, data and metadata: `fsync` on Linux, `FlushFileBuffers` on Windows, `F_FULLFSYNC` on macOS ([Platforms](./PLATFORMS.md)).

Source: [`crates/store-io-format/src/codec.rs`](../crates/store-io-format/src/codec.rs), [`crates/store-io-format/src/crc32c.rs`](../crates/store-io-format/src/crc32c.rs)

---

## 2. Finding the volume record and the format

B is not known until the volume record is read, and the record's slot B lies at offset `B`. A reader therefore tries every legal block size, from `log2` 12 to 16, and at each size the two places the record may be, offset `0` and offset `2^log2`, in this order: `(12, 0)`, `(12, 4096)`, `(13, 0)`, `(13, 8192)`, and so on to `(16, 65536)`. For each candidate:

1. Read 4096 bytes at the candidate offset. Move on to the next candidate unless the read returned all 4096 bytes (a media error, a short read and the end of the file move on; any other I/O error fails the open).
2. Move on unless bytes `[0, 8)` are the magic `"SIOSLOT\0"` and byte 78 (`log2_slot_size`) equals the `log2` being tried.
3. Move on unless the header CRC verifies: CRC-32C over bytes `[0, max(header_len, 128))` with bytes `[8, 12)` read as zero equals the `u32` at offset 8 (`header_len` is the `u16` at offset 12). A `header_len` larger than the 4096 bytes read is not judged here, unless the header claims format 2 (bytes 14&ndash;15 equal `2`), which then moves on.
4. Take the volume id from bytes `[32, 48)` (unverified so far) and `format_major` from bytes `[14, 16)`.
5. If `format_major` is `2`, read the pair at `0` and `2^log2` as format-2 slots of size `2^log2`, object 0, that volume id, numbered (section 5), and choose its winner by the pair rule (section 6):
   - **Refused**: the open fails `Unsupported(FormatVersion)`.
   - **Empty**, **Lost** or **Fork**: move on.
   - **Confirmed** or **Unconfirmed**: decode the winner's payload as a format-2 volume record (section 7). A payload longer than 64 bytes, or a non-zero `features` or `flags`, fails the open with `Unsupported(FormatVersion)`; any other decoding failure fails it with `Corruption(Metadata)`. If the record's `log2_block_size` is not the `log2` being tried, move on. Otherwise the record is found: B, T and P come from it.
6. Any other `format_major`: the candidate is read as format 1 (section F10).

If no candidate succeeds, the open fails with `Corruption(MediaError)` when some read on the way failed with a media error, and `Corruption(HeaderCrc)` otherwise.

The record found here is only the first step: the volume pair is judged again, with every table page, in section 14, and a slot whose first read failed is read once more (section 16).

Source: [`crates/store-io-engine/src/open.rs`](../crates/store-io-engine/src/open.rs) (`find_volume`, `header_verifies`, `winner_payload`), [`crates/store-io-engine/src/v2.rs`](../crates/store-io-engine/src/v2.rs) (`decode_volume`)

---

## 3. Container layout and geometry

A store is a directory that holds one container file, `store.sio`. Everything store-io keeps for that store is inside this file.

### Geometry

The geometry is fixed when the store is created and recorded in the volume record (section 7). Later opens read it from there and ignore the size options.

| Quantity | Value |
|---|---|
| Block size B | The largest of 4096, the device's logical block, physical block and preferred write size, the direct-I/O memory and offset alignments, and the caller's requested `block_size`, rounded up to a power of two and clamped to 4 KiB&ndash;64 KiB (`log2` 12&ndash;16). |
| Table slot size T | The caller's `log2_table_slot` (default 14, so 16 KiB), clamped to between `log2(B)` and 16. |
| Entries per page E | `floor((T - 128 - 24) / 96)`: 41, 83, 169, 339 and 681 at T = 4, 8, 16, 32 and 64 KiB. |
| Table pages P | `ceil(min(region_capacity, 4,194,304) / E)`, clamped to `1 <= P <= 2^30 - 1`. `region_capacity` defaults to 65,536, so `P = 388` at T = 16 KiB. |
| Region capacity | `P * E` entries (65,572 by default). |

### Layout

| Offset | Size | Contents |
|---|---|---|
| `0` | B | Volume record, slot A |
| `B` | B | Volume record, slot B |
| `2B + 2pT` | T | Table page `p`, slot A (`0 <= p < P`) |
| `2B + 2pT + T` | T | Table page `p`, slot B |
| `2B + 2PT` | to end of file | Region header blocks and data extents, each starting on a multiple of B |

`2B + 2PT` is the **table end**. Nothing of a region lies before it.

### Regions in the file

A region owns:

- two **header blocks** of B bytes each, at the offsets its table entry names (`header_a`, `header_b`), holding its header pair (section 9);
- for an append or page region, one or more **extents**, each a B-aligned `(offset, length)` range listed in its header, in data order (section 10). A slot region has no extents: its object lives in its header pair.

A region's **footprint** is its two header blocks and its committed extents (the first `extent_count` extents its header lists, where `extent_count` is the entry's). Every overlap, free-space and placement rule uses footprints. Its **span** `[span_start, span_end)`, recorded in its entry, runs from the lowest byte of its footprint to one past the highest; it may cover other regions' bytes between its extents.

store-io 0.5 places a new region's header blocks next to each other, `header_b = header_a + B`, followed by its first extent at `header_a + 2B`. A reader must not assume that: it finds every region through its entry and its header, and the rules of sections 8 and 13 are the only constraints on placement.

### Object identities

Every slot carries the 64-bit identity of the object its pair stores:

| Object | Id |
|---|---|
| Volume record | `0` |
| Table page `p` | `2^61 \| p` (`0x2000_0000_0000_0000 \| p`) |
| Header of region `id` | `2^62 \| id` (`0x4000_0000_0000_0000 \| id`) |

A region id is `p << 32 | seq`, where `p` is the page holding its entry and `seq` the entry's sequence number in that page (section 8). Since `p < 2^30`, every id is below `2^62` and the three kinds of object id never collide.

### Volume identity

Each container has a random 16-byte volume id, chosen at creation. Every slot in the file carries it, so a slot copied from another container is rejected as foreign.

### File length

A new container is exactly `2B + 2PT` bytes long. The file grows when regions are provisioned or grown and when space is reserved, and it never shrinks. Bytes outside every footprint have no defined contents.

### Example

With B = 4096, T = 16384 and P = 388 (the defaults on a 4 KiB device): the volume slots are at 0 and 4096; page `p`'s slots at `8192 + 32768p` and `24576 + 32768p`; the table end, and the file length of a new store, is 12,722,176 (12.1 MiB of table). A first region of 64 KiB has its header blocks at 12,722,176 and 12,726,272 and its data from 12,730,368. A first region of 1 MiB or more has its data area aligned to 1 MiB (section 17): header blocks at 13,623,296 and 13,627,392, data from 13,631,488.

Source: [`crates/store-io-format/src/meta2.rs`](../crates/store-io-format/src/meta2.rs) (`entries_per_page`, `MAX_TABLE_PAGES`, `VolumeMeta2::table_end`), [`crates/store-io-engine/src/layout.rs`](../crates/store-io-engine/src/layout.rs) (`choose_log2_block`, `Layout`), [`crates/store-io-engine/src/v2.rs`](../crates/store-io-engine/src/v2.rs) (`layout_for`, `MAX_REGION_CAPACITY`, `page_object`), [`crates/store-io-engine/src/provision2.rs`](../crates/store-io-engine/src/provision2.rs)

---

## 4. The slot

Every piece of metadata is stored as a **pair** of slots, A and B, in two separate places. A slot is a power-of-two block holding a 128-byte header and then a payload: the volume record and region headers use slots of B bytes, table pages slots of T bytes. An update always writes the slot that does *not* hold the current version, as one whole-slot write, and then makes it durable, so a crash can only damage the slot being written.

### Header (128 bytes)

| Offset | Size | Type | Field | Meaning |
|---|---|---|---|---|
| 0 | 8 | bytes | `magic` | `"SIOSLOT\0"` (`53 49 4F 53 4C 4F 54 00`) |
| 8 | 4 | u32 | `header_crc32c` | CRC-32C of bytes `[0, header_len)`, computed with this field read as zero |
| 12 | 2 | u16 | `header_len` | Exactly `128` in format 2 |
| 14 | 2 | u16 | `format_major` | `2` |
| 16 | 4 | u32 | `incompat_flags` | `0`. Any unknown bit: the slot is refused. |
| 20 | 4 | u32 | `ro_compat_flags` | `0`. Any unknown bit: readable, but must not be rewritten. |
| 24 | 4 | u32 | `compat_flags` | `0`. Unknown bits are ignored. |
| 28 | 4 | u32 | `payload_crc32c` | CRC-32C of the payload bytes |
| 32 | 16 | bytes | `volume_uuid` | The container's volume id |
| 48 | 8 | u64 | `object_id` | The object this pair stores (section 3) |
| 56 | 8 | u64 | `generation` | At least 1. Each committed update writes the winner's generation + 1. |
| 64 | 8 | u64 | `slot_offset` | Absolute file offset this slot was written to |
| 72 | 4 | u32 | `payload_len` | Payload length in bytes, at most `slot size - 128` |
| 76 | 1 | u8 | `slot_index` | `0` = A, `1` = B |
| 77 | 1 | u8 | `slot_count` | `2` |
| 78 | 1 | u8 | `log2_slot_size` | Slot size as a power of two: `log2(B)` or `log2(T)` |
| 79 | 1 | &mdash; | reserved | `0` |
| 80 | 4 | u32 | `prev_header_crc32c` | `header_crc32c` of the version this one replaced; `0` for generation 1 |
| 84 | 4 | &mdash; | reserved | `0` |
| 88 | 8 | u64 | `owner_generation` | The volume record's owner generation of the writer (section 7) |
| 96 | 8 | u64 | `meta_seq` | The volume record and table pages: the store-wide metadata sequence number of this write, at least 1 (section 14). Region headers: `0`. |
| 104 | 24 | &mdash; | reserved | `0` |

The payload starts at offset 128 and is `payload_len` bytes long. Every byte after the payload, up to the end of the slot, is written as zero.

The volume record and the table pages are **numbered objects**: their slots carry a `meta_seq`. Region headers are not.

### CRC coverage

| Bytes | Covered by |
|---|---|
| `[0, 8)` and `[12, 128)` | `header_crc32c` (bytes `[8, 12)` count as four zero bytes) |
| `[128, 128 + payload_len)` | `payload_crc32c` |
| `[128 + payload_len, slot end)` | Nothing. Written as zero, never read. |

Region data carries no store-io checksums: the caller's records must carry their own.

### Writing a slot

The writer fills the whole slot with zeros, copies in the payload, writes the header with `header_crc32c` as zero, computes the header CRC over `[0, 128)`, and stores it at offset 8. Generation 0 is never written. store-io always writes `incompat_flags`, `ro_compat_flags` and `compat_flags` as zero. The next write of a pair is planned from its current winner (section 6):

| Winner | Slot the next update writes | Its generation | Its `prev_header_crc32c` |
|---|---|---|---|
| none yet (both slots blank) | A | 1 | 0 |
| in slot *i* | the other slot | winner's generation + 1 (an overflow refuses the write) | winner's `header_crc32c` |
| Fork, Refused, Lost | none: the pair must not be written | &mdash; | &mdash; |

A winner with an unknown `ro_compat_flags` bit is never overwritten: the write fails `Unsupported(FormatVersion)`. Each slot written also records the writer's owner generation and, for a numbered object, the write's `meta_seq` (section 14).

Source: [`crates/store-io-format/src/slot.rs`](../crates/store-io-format/src/slot.rs) (`encode`, `write_header`), [`crates/store-io-engine/src/slots.rs`](../crates/store-io-engine/src/slots.rs) (`plan_next`, `write_slot`)

---

## 5. Validating a slot

A reader knows where it read the slot from and what it expects there: the volume id, the object id, the absolute offset, the slot index, the slot size, the format (`2`) and whether the object is numbered. Checks run in this order, and the first failure decides the status:

| # | Check | Status if it fails |
|---|---|---|
| 0 | The read succeeded and returned the whole slot (checked by the caller: a media error, a short read or the end of the file; any other read error fails the operation) | `ReadError` |
| 1 | The expected `log2_slot_size` is 9&ndash;16 and the buffer is exactly that size | `BadLength` |
| 2 | The first 128 bytes are not all zero | `Blank` (never written) |
| 3 | `magic` is `"SIOSLOT\0"` | `BadMagic` |
| 4 | `128 <= header_len <= slot size` | `BadHeaderCrc` |
| 5 | `header_crc32c` matches (over `[0, header_len)`) | `BadHeaderCrc` |
| 6 | `format_major` is the expected format, `2` | `UnsupportedVersion { major }` |
| 7 | `incompat_flags` has no unknown bit (none is known) | `Incompatible { flags }` |
| 8 | `generation != 0`; `slot_count == 2`; `slot_index <= 1`; the reserved bytes at 79, `[84, 88)` and `[104, 128)` are zero; `meta_seq >= 1` for a numbered object and `meta_seq == 0` otherwise; `header_len == 128` | `Malformed` |
| 9 | `volume_uuid` is the expected volume | `Foreign` |
| 10 | `object_id` is the expected object | `WrongObject` |
| 11 | `slot_offset`, `slot_index` and `log2_slot_size` match where the slot was read from | `Misplaced` (a misdirected write) |
| 12 | `payload_len <= slot size - header_len` | `Malformed` |
| 13 | `payload_crc32c` matches | `BadPayloadCrc` |
| &mdash; | Every check passed | `Valid` |

An out-of-range `header_len` reports as `BadHeaderCrc`, not `Malformed`, and the `payload_len` bound is checked only after the identity checks. A failed read is retried once (section 16); a slot whose second read fails too is `ReadError`.

A valid slot whose `ro_compat_flags` has an unknown bit is still `Valid`: readable, never rewritten (section 18).

### What a header shows

The judgement of section 14 also reads what a slot's header shows when the slot is not valid:

- for any slot of the right length that is not blank and has the magic: its **stored** `header_crc32c`, whether or not it verifies;
- for a slot that passes checks 1&ndash;12 (it is `Valid` or `BadPayloadCrc`): its `generation`, `meta_seq` and `prev_header_crc32c`.

A slot of the volume record or a table page that passes checks 1&ndash;12 is a **numbered slot**: its `meta_seq` counts in section 14. A foreign, misplaced or malformed slot's number counts for nothing.

Source: [`crates/store-io-format/src/slot.rs`](../crates/store-io-format/src/slot.rs) (`validate`, `parse_header`, `parse`, `header_facts`), [`crates/store-io-engine/src/slots.rs`](../crates/store-io-engine/src/slots.rs) (`read_one`, `read_pairs`, `reread_failed`)

---

## 6. The pair rule

Both slots of a pair are read and validated, and a current version is chosen from the two statuses. Choosing never writes either slot. In format 2 this rule decides region header pairs, and it is how the volume record is first found (section 2); the volume record and the table pages are then judged by the store-wide rules of section 14.

Rows are tried in order; "explained" means `Blank`, `BadHeaderCrc` or `BadPayloadCrc`, the states an ordinary crash during the latest write leaves behind.

| Slot A | Slot B | Winner |
|---|---|---|
| `UnsupportedVersion` or `Incompatible` | anything | **Refused** |
| anything | `UnsupportedVersion` or `Incompatible` | **Refused** |
| `Blank` | `Blank` | **Empty** (never written) |
| `Valid`, generation *g* | `Valid`, generation *g* | **Fork** (no winner) |
| `Valid`, generation *a* | `Valid`, generation *b*, *a* &ne; *b* | **Confirmed**, the higher generation |
| `Valid` | explained | **Confirmed** A |
| explained | `Valid` | **Confirmed** B |
| `Valid` | any other status | **Unconfirmed** A |
| any other status | `Valid` | **Unconfirmed** B |
| anything else | | **Lost** |

"Any other status" is `ReadError`, `BadLength`, `BadMagic`, `Malformed`, `Foreign`, `WrongObject` or `Misplaced`. An **Unconfirmed** winner is the newest version that can still be read; for a region header it is used exactly like a Confirmed one.

When both slots are valid with different generations, a **generation gap** (not consecutive) and a **chain break** (consecutive, but the newer slot's `prev_header_crc32c` is not the older slot's `header_crc32c`) are computed. Neither changes the winner, and format 2 does not act on either.

Source: [`crates/store-io-format/src/pair.rs`](../crates/store-io-format/src/pair.rs) (`decide`)

---

## 7. The volume record (object 0)

The volume record is the payload of the pair at offsets `0` and `B`, with slot size B. Its payload is exactly 64 bytes:

| Offset | Size | Type | Field | Meaning |
|---|---|---|---|---|
| 0 | 1 | u8 | `log2_block_size` | `log2(B)`, 12&ndash;16 |
| 1 | 1 | u8 | `log2_table_slot_size` | `log2(T)`, from `log2_block_size` to 16 |
| 2 | 2 | u16 | `device_index` | `0` (single-device store); not checked |
| 4 | 4 | &mdash; | reserved | must be `0` |
| 8 | 8 | u64 | `owner_generation` | `1` at creation; one more on every writable open |
| 16 | 8 | u64 | `table_offset` | Offset of page 0's slot A; must be `2B` |
| 24 | 8 | u64 | `created_unix_s` | Creation time, seconds since the Unix epoch; informative only |
| 32 | 8 | u64 | `features` | must be `0` (no feature is defined) |
| 40 | 8 | &mdash; | reserved | must be `0` |
| 48 | 4 | u32 | `table_pages` | P, `1 <= P < 2^30` |
| 52 | 2 | u16 | `entries_per_page` | E; must equal `floor((T - 152) / 96)` |
| 54 | 1 | u8 | `clean_close` | `1` when the last writer closed cleanly, else `0` |
| 55 | 1 | &mdash; | reserved | must be `0` |
| 56 | 4 | u32 | `flags` | Must-understand volume flags; must be `0` (none is defined) |
| 60 | 4 | &mdash; | reserved | must be `0` |

Bytes `[0, 48)` are format 1's volume record (section F6), with `table_offset` fixed at `2B`.

### Decoding

Checks, in order; the first failure is the error:

1. The payload is exactly 64 bytes. A longer payload is a newer layout: the open fails `Unsupported(FormatVersion)`. A shorter one fails it with `Corruption(Metadata)`.
2. `features` (bytes `[32, 40)`) is zero, then `flags` is zero. Either non-zero is a newer format: `Unsupported(FormatVersion)`. These are checked before any other field, so a newer record is never mistaken for damage.
3. Format 1's checks of bytes `[0, 48)`: the reserved `u32` at 4 and the bytes `[40, 48)` are zero, `log2_block_size` is 12&ndash;16, `log2_table_slot_size` is between `log2_block_size` and 16, `table_offset` is a multiple of B.
4. The reserved byte at 55 and the reserved `u32` at 60 are zero.
5. `table_offset == 2B`.
6. `1 <= table_pages < 2^30`.
7. `entries_per_page` equals E for this T.
8. `clean_close` is `0` or `1`.

A failure in steps 3&ndash;8 fails the open with `Corruption(Metadata)`. The open also requires the table area to lie within the file, `2B + 2PT <=` the file's length, and fails with `Corruption(Metadata)` otherwise.

### Fields that change

The record is written by create (`owner_generation = 1`, `clean_close = 0`), by every writable open (`owner_generation` one higher than the record it found, `clean_close = 0`) and by a clean close (`clean_close = 1`, the open's `owner_generation`); no other write changes it. Every slot a writer writes records the writer's owner generation in its header. `owner_generation` is forensic: it tells a reader which open wrote what, and judges nothing; the ownership lock is the fence.

Source: [`crates/store-io-format/src/meta2.rs`](../crates/store-io-format/src/meta2.rs) (`VolumeMeta2`), [`crates/store-io-format/src/meta.rs`](../crates/store-io-format/src/meta.rs) (`VolumeMeta`), [`crates/store-io-engine/src/v2.rs`](../crates/store-io-engine/src/v2.rs) (`decode_volume`, `open_metadata`, `write_volume`)

---

## 8. Table pages and entries (object 2^61 | p)

The region table is P table pages. Page `p` is the payload of the pair at `2B + 2pT` and `2B + 2pT + T`, with slot size T and object id `2^61 | p`. The table is the only authority on which regions exist: a region header that no entry names does not exist.

### Page header (24 bytes)

| Offset | Size | Type | Field | Meaning |
|---|---|---|---|---|
| 0 | 4 | u32 | `page_index` | `p`; must match the object id |
| 4 | 4 | u32 | `next_seq` | The next sequence number this page gives out. Every entry's `seq` is below it. It never decreases. |
| 8 | 2 | u16 | `count` | Number of entries that follow, at most E |
| 10 | 14 | &mdash; | reserved | must be `0` |

Entries follow at offset `24 + 96 * i`, in any order. The payload is exactly `24 + 96 * count` bytes. A new container's pages are each the 24-byte header with `next_seq = 0` and `count = 0`.

### Entry (96 bytes)

| Offset | Size | Type | Field | Meaning |
|---|---|---|---|---|
| 0 | 4 | u32 | `seq` | Sequence number in the page; the region id is `p << 32 \| seq` |
| 4 | 1 | u8 | `kind` | `1` append, `2` page, `3` slot |
| 5 | 1 | u8 | `state` | `1` Ready. A released region has no entry. |
| 6 | 1 | u8 | `flags` | Must-understand flags (below) |
| 7 | 1 | u8 | `compat_flags` | May-ignore flags: none is defined; kept byte for byte |
| 8 | 24 | bytes | `name` | The region name, padded with zeros (section 11) |
| 32 | 4 | u32 | `tag` | The caller's space-accounting tag; `0` when provisioned without a reservation |
| 36 | 2 | u16 | `extent_count` | Committed extents: `0` for a slot region, at least `1` otherwise |
| 38 | 2 | &mdash; | reserved | must be `0` |
| 40 | 8 | u64 | `header_a` | Offset of the region header's slot A |
| 48 | 8 | u64 | `header_b` | Offset of the region header's slot B |
| 56 | 8 | u64 | `data_size` | Usable length in bytes, at most `capacity` |
| 64 | 8 | u64 | `capacity` | Reserved length: the sum of the first `extent_count` extents the header lists |
| 72 | 8 | u64 | `span_start` | The lowest offset of the footprint |
| 80 | 8 | u64 | `span_end` | One past the highest |
| 88 | 8 | u64 | `footprint_seq` | The `meta_seq` of the page write that last changed this region's footprint: its provisioning, or its latest growth beyond capacity |

| `kind` | Meaning |
|---|---|
| `1` | Append region: store-io chooses positions from a tail; blocks are never rewritten. |
| `2` | Page region: the caller chooses positions and overwrites in place. |
| `3` | Slot region: a small object kept in the region's own header pair (section 9). |

| `flags` bit | Name | Meaning |
|---|---|---|
| 0 (`0x01`) | unwritten | Provisioned with `unwritten_ok`. Defined and accepted by readers; store-io 0.5 never sets it. |
| 1 (`0x02`) | capped | Growth is limited to `capacity` (`max_size`, section 10). |

### Page checks

A page whose winning copy fails any of these is **Lost** (section 14): its regions are unknown. Checked in this order:

1. The payload holds the 24-byte header and its reserved bytes are zero.
2. `page_index == p`.
3. `count <= E` and `count <= next_seq`.
4. The payload is exactly `24 + 96 * count` bytes.
5. For each entry in stored order: `seq < next_seq`; the reserved bytes `[38, 40)` are zero; no later entry has the same `seq` (`Duplicate`).

### Entry checks

An entry that fails any of these makes **its region unavailable**; the page stands. The checks need the container's B, the table end, the file's length, and the `meta_seq` of the page copy holding the entry (section 14). Checked in this order; the first failure decides:

1. `flags` has no bit other than `0x01` and `0x02` (checked first, so an entry from a newer release is recognised as such).
2. `kind` is `1`, `2` or `3`.
3. `state` is `1`.
4. The name is valid (section 11).
5. `data_size <= capacity`.
6. `span_start < span_end <= file length`.
7. `capacity <= span_end - span_start`.
8. `span_start` and `span_end` are multiples of B.
9. For each of `header_a`, `header_b`: a multiple of B; `h + B` does not overflow; `h >= table end`, `h >= span_start` and `h + B <= span_end`.
10. `header_a != header_b`.
11. `footprint_seq` is at least 1 and at most the `meta_seq` of the page copy holding the entry.
12. A slot region has `extent_count == 0`, `capacity == 0` and `data_size == 0`; any other region has `extent_count >= 1` and `capacity > 0`.
13. `capacity` is a multiple of B; `data_size` is a multiple of B, or for an append region of 512 (the region's own data block, from its header, then decides: section 13).

**A region cut short.** An entry whose `span_end` is past the file's length is a region **cut short** by a shorter file, reported `ContainerShort` whatever else it fails. When that is its only failure (it passes every check with the file's length taken as unlimited), its header pair is still read and judged alone, so what it may have written is its own header blocks and extents, not its whole span (section 15); otherwise it is treated as an entry that does not decode, its whole span reserved.

**What an unavailable entry keeps reserved.** An entry that fails these checks keeps its span reserved, widened outwards to whole blocks and clamped to `[table end, file length)` (nothing when that is empty). A region cut short keeps `[span_start, min(span_end, file length))`. Both are allocated space (section 17). An entry whose name and `kind` decode is listed as an unavailable region and keeps its name reserved; one whose name decodes but whose `kind` is unknown keeps its name reserved and is listed by place (`(page, seq)`); one whose name does not decode is listed by place.

### Writer rules

- An update rewrites a page as a whole: the page header, then every entry. Entries the writer does not change are written back with every field as read, `compat_flags` and unknown `flags` bits included.
- `next_seq` never decreases. A release or repair removes an entry and leaves `next_seq` as it was, so a region id is never given out again by a committed write. A provisioning takes `seq` at or above `next_seq` and commits `next_seq = max(next_seq, seq + 1)`; a page whose next `seq` would be `u32::MAX` takes no new entry.
- A page whose winning copy holds an entry with `footprint_seq` above that copy's own `meta_seq` (only damage writes one) is written only to remove that entry (section 15).
- Only a page judged Confirmed is ever written, except by `accept_page_rollback` (section 20).

Source: [`crates/store-io-format/src/meta2.rs`](../crates/store-io-format/src/meta2.rs) (`PageHeader`, `RawEntry`, `RawEntry::check`, `reserved_span`, `decode_page`, `encode_page`), [`crates/store-io-engine/src/open2.rs`](../crates/store-io-engine/src/open2.rs) (`check_regions`), [`crates/store-io-engine/src/v2.rs`](../crates/store-io-engine/src/v2.rs) (`write_page`, `Page2::ahead`)

---

## 9. Region headers and slot objects (object 2^62 | id)

Each region's header pair lies at its entry's `header_a` (slot A) and `header_b` (slot B), with slot size B and object id `2^62 | id`. It is judged by the pair rule (section 6); a Confirmed or Unconfirmed winner is used.

### Header payload

A 64-byte fixed part, then either the extents (append and page regions) or the slot object (slot regions).

| Offset | Size | Type | Field | Meaning |
|---|---|---|---|---|
| 0 | 8 | u64 | `region_id` | `p << 32 \| seq`; must match the entry |
| 8 | 1 | u8 | `kind` | As the entry |
| 9 | 1 | u8 | `state` | `1` Ready, `2` Released (written by release; forensic) |
| 10 | 1 | u8 | `fill` | `0` zeros, `1` keyed pattern version 1 (section 12) |
| 11 | 1 | u8 | `flags` | Must-understand flags; must equal the entry's |
| 12 | 1 | u8 | `compat_flags` | May-ignore flags: none is defined; kept on every rewrite |
| 13 | 1 | u8 | `log2_block` | The region's data block: `log2(B)`, or `9` for an append region with 512-byte blocks |
| 14 | 2 | u16 | `extent_count` | Extents listed: the entry's `extent_count`, or one more after a crash inside a growth beyond capacity (never more for a slot region) |
| 16 | 8 | u64 | `region_generation` | The region's generation: `1` at provisioning (a region `copy_store` recreates keeps its source's); one more at each recycle |
| 24 | 16 | bytes | `fill_key` | Key of the keyed pattern; all zero when `fill = 0` |
| 40 | 4 | u32 | `object_len` | Slot regions: the object's length (`0` is an empty object). Others: `0`. |
| 44 | 20 | &mdash; | reserved | must be `0` |
| 64 | 16 each | (u64, u64) | extents | Append and page regions: `extent_count` extents, each `(offset, length)`, in data order |
| 64 | `object_len` | bytes | object | Slot regions: the slot object |

The payload is exactly `64 + 16 * extent_count` bytes for an append or page region and `64 + object_len` for a slot region.

| Container B | Most extents a header lists, `(B - 192) / 16` | Largest slot object, `B - 192` |
|---|---|---|
| 4 KiB | 244 | 3,904 bytes |
| 8 KiB | 500 | 8,000 bytes |
| 16 KiB | 1,012 | 16,192 bytes |
| 32 KiB | 2,036 | 32,576 bytes |
| 64 KiB | 4,084 | 65,344 bytes |

### Decoding

Checks, in order; any failure makes the header invalid:

1. `flags` has no bit other than `0x01` and `0x02` (first, so a header from a newer release is recognised as such).
2. `kind` is 1&ndash;3, `state` is 1 or 2, `fill` is 0 or 1.
3. The reserved bytes `[44, 64)` are zero.
4. `log2_block` equals `log2(B)`, or is `9` and the region is an append region.
5. `region_generation != 0`.
6. `fill = 0` implies an all-zero `fill_key`.
7. A slot region: `extent_count == 0`, and the payload after the fixed part is exactly `object_len` bytes and at most `B - 192`. Any other region: `object_len == 0`, `1 <= extent_count <= (B - 192) / 16`, and the payload after the fixed part is exactly `16 * extent_count` bytes.
8. Each extent: `length != 0`, `offset` and `length` are multiples of B (also in an append region with 512-byte blocks), and `offset + length` does not overflow (an overflow makes the header invalid, `HeaderInvalid`, not `ExtentsInvalid`).

The decoder can also check that each extent ends within the file. The open does not apply that rule: it decodes headers by themselves, so that the verdict never depends on the file's length (a later growth of the file would change it). The entry's span bounds every committed extent instead (section 13), and only an extent listed past the entry's count may lie past the end of the file.

### Generations of a region header

The header pair's slot generation counts every header write. The region's generation is the header's `region_generation` field, which only provisioning and recycle set.

| Write | Header | `region_generation` |
|---|---|---|
| Provisioning | slot A, slot generation 1, Ready; one extent (none for a slot region) | 1 (or the copied region's) |
| Growth beyond capacity | the entry's extents and the new one | unchanged |
| Recycle | Ready, the entry's extents only | + 1 |
| Slot commit | the same fixed part with the new `object_len`, then the object | unchanged |
| Release | Released, the entry's extents only | unchanged |

Every rewrite keeps `fill`, `fill_key`, `flags`, `compat_flags` and `log2_block` as they were, and lists only the entry's committed extents (dropping an extra one a header may list), except a growth beyond capacity's own write, which adds its extent.

### Slot objects

A slot region (`kind = 3`) has no extents and no data area. Its object lives in its header pair: slot generation 1 is the provisioning header, with no object; every commit writes the next generation with the fixed part and the object. The current object is therefore the `object_len` bytes after offset 64 of the winning header's payload, or none if the winning slot's generation is 1. A commit is refused unless the winning header's state is Ready. Slot regions are never recycled or released.

### Placement

The entry checks require each header slot to be a whole block inside the span, past the table end, the two apart. store-io 0.5 writes `header_b = header_a + B`, and reads such a pair with one read of both blocks; any other placement is read slot by slot.

Source: [`crates/store-io-format/src/meta2.rs`](../crates/store-io-format/src/meta2.rs) (`RegionHeader2`, `encode_region_header`, `decode_region_header`, `max_extents`, `max_object`), [`crates/store-io-engine/src/lifecycle2.rs`](../crates/store-io-engine/src/lifecycle2.rs) (`rewrite_header2`), [`crates/store-io-engine/src/slot_handle.rs`](../crates/store-io-engine/src/slot_handle.rs), [`crates/store-io-engine/src/open2.rs`](../crates/store-io-engine/src/open2.rs)

---

## 10. Region data, capacity and growth

### Data order

A region's data is the concatenation of its committed extents (the first `extent_count` extents its header lists), in the order listed. Region offset `x` lies in extent `k` where the cumulative lengths of the extents before `k` are at most `x` and including `k` exceed it, at file offset `extent_k.offset + (x - cumulative_before_k)`. Only `[0, data_size)` is addressable. Bytes in `[data_size, capacity)` have no defined contents.

Extents are never merged on disk. In memory, extents that follow each other in data order and in the file (one ends where the next begins) are coalesced into **runs**, and one device operation never crosses a run.

### Sizes at provisioning

| Region | `data_size` | `capacity` | Extents | `flags` |
|---|---|---|---|---|
| Append or page | the requested size rounded up to a multiple of B (at least B) | `data_size` | one: `(header_a + 2B, capacity)` | `0` |
| Append or page with `max_size` | as above | `max_size` rounded up to B, and at least `data_size` | one: `(header_a + 2B, capacity)` | capped (`0x02`) |
| Slot | `0` | `0` | none | `0` |

`capacity / B` must stay below `2^32 - 1`. The span is `[header_a, header_a + 2B + capacity)`. A region that `copy_store` recreates with a data size of 0 gets `data_size = 0` and `capacity = B`.

### Growth within capacity

Growth to a new size (rounded up to B) that does not pass `capacity` changes the entry's `data_size` only: the range `[data_size, new size)` is filled with the region's pattern (section 12), made durable and verified, then the page is written with the new `data_size`. `footprint_seq` does not change: the footprint did not move.

### Growth beyond capacity

A region that is not capped, and whose entry lists fewer extents than a header can hold, grows past its capacity by one new extent:

- **Length:** the largest of 1 MiB, `capacity / 8` and `new size - capacity`, rounded up to a multiple of B.
- **Place:** right after the region's last extent when that range is free, or when the last extent ends at the tail (the end of the last allocated byte: the file then grows), so the new extent continues the last run; else by first fit with its start aligned to 1 MiB; else past everything allocated; aligned to B instead of 1 MiB when the file cannot grow for the alignment (section 17).
- **Writes:** the bytes from `data_size` to the old capacity and the new extent's part up to the new size are filled, made durable and verified; the header is written listing the new extent after the committed ones; then the page is written with `data_size` = the new size, `capacity` + the extent's length, `extent_count` + 1, the span widened to cover the extent, and `footprint_seq` = this page write's own `meta_seq`.

A capped region refuses growth past `capacity` (`NoSpace(RegionLimit)`); a region whose entry already lists `(B - 192) / 16` extents refuses it too (`NoSpace(Extents)`). The new size, divided by B, must stay below `2^32 - 1`.

**What is written and what is only allocated.** Provisioning writes the two header blocks with zeros and the data area with the region's pattern: the whole capacity, except that a capped region's slack `[data_size, capacity)` is only allocated, unwritten, where the system can do so without a later cost. On Windows (where writing past a file's valid data length zeroes everything below it first) and macOS (which cannot allocate inside a file) the slack is written with the pattern too. A growth beyond capacity likewise allocates the rest of its new extent past the new size, or writes it on Windows and macOS. Every growth fills the range it makes addressable, even where provisioning wrote it before: nothing on disk records that those bytes are this region's.

Source: [`crates/store-io-engine/src/provision2.rs`](../crates/store-io-engine/src/provision2.rs) (`provision2`, `fill_and_commit2`), [`crates/store-io-engine/src/grow2.rs`](../crates/store-io-engine/src/grow2.rs), [`crates/store-io-engine/src/regionmap.rs`](../crates/store-io-engine/src/regionmap.rs), [`crates/store-io-format/src/footprint.rs`](../crates/store-io-format/src/footprint.rs) (`runs`)

---

## 11. Names

A name is 1 to 24 bytes of UTF-8 with no NUL byte, stored in the entry's 24-byte field and padded with zeros. When decoding, the name ends at the first zero byte; it must be at least one byte long, every byte after it must be zero, and the bytes before it must be valid UTF-8. A name that fails these rules fails the entry checks.

Names are unique among the regions an open serves. Where two entries share a name, section 15 decides which one is served, if either. A name is taken, for provisioning, by a usable region, by an unavailable one, by an entry of a kind this version does not know, and by a provisioning in flight; it is free again once its entry is removed by a release or a repair.

Source: [`crates/store-io-format/src/meta.rs`](../crates/store-io-format/src/meta.rs) (`RegionName`), [`crates/store-io-engine/src/provision2.rs`](../crates/store-io-engine/src/provision2.rs) (`name_taken2`)

---

## 12. The keyed fill pattern

On thin, virtual, deduplicating, compressing or zero-detecting storage, writing zeros may never really allocate space. Fill pattern 1 (`KeyedV1`), chosen for each new region by the `keyed_fill` option of the open that provisions it, makes every 8-byte word of a region's data unique and incompressible. Pattern 0 writes zeros.

Let the 16-byte `fill_key` (from the region header) be split into `k0` (bytes 0&ndash;7) and `k1` (bytes 8&ndash;15), each read as a little-endian `u64`, and let `GAMMA = 0x9E3779B97F4A7C15`. Word `j` of the region, the 8 bytes at **region offset** `8j` (data order, section 10, not the file offset), is stored little-endian as:

```text
mix(z) = z1 ^ (z1 >> 31)
         where z0 = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9
               z1 = (z0 ^ (z0 >> 27)) * 0x94D049BB133111EB

word(j) = mix(k0 + (j + 1) * GAMMA) ^ k1
```

All arithmetic wraps modulo 2^64. With an all-zero key, words 0 and 1 are `0xE220A8397B1DCDAF` and `0x6E789E6AA1B965F4`. The definition of version 1 never changes; a new pattern gets a new `fill` value. The key is random per region (16 random bytes), and a region `copy_store` recreates keeps its source's pattern and key.

The fill describes what provisioning and growth *write*; the format makes no promise about what a region's data holds afterwards. The two header blocks are always written with zeros at provisioning, whatever the pattern, so no older header survives at that place.

Source: [`crates/store-io-format/src/fill.rs`](../crates/store-io-format/src/fill.rs), [`crates/store-io-engine/src/provision.rs`](../crates/store-io-engine/src/provision.rs) (`fill`, `fill_range`)

---

## 13. Validating a region

Every entry on a page that is not Lost is checked, and the region is **usable** only if every rule below holds. Any other region is **unavailable**, with the reason in parentheses: it is not served, a lookup of its name returns `Corruption` naming it (never `NotFound`), its name stays reserved, and its span stays allocated.

1. The entry passes the entry checks (section 8) (`EntryInvalid`; any entry whose `span_end` is past the file's length: `ContainerShort`, its header still read when that is its only failure).
2. The header pair, read at `header_a` and `header_b` (section 9), has a Confirmed or Unconfirmed winner (`HeaderUnreadable`). A header pair Refused by the pair rule (a newer format) also counts as unreadable here: only the volume record and the table pages make an open fail on a newer format.
3. The winner's payload decodes as a region header (section 9), by itself, without the file's length (`HeaderInvalid`).
4. The header agrees with the entry:
   - `region_id` is the entry's id, `kind` the entry's `kind`, `flags` the entry's `flags` (`HeaderMismatch`);
   - `state` is Ready (`HeaderReleased`);
   - `extent_count` is the entry's or one more, and for a slot region exactly the entry's (`HeaderInvalid`);
   - `data_size` is a multiple of the header's data block, `2^log2_block` (`HeaderInvalid`).
5. The first `extent_count` extents (the entry's count) each lie inside the entry's span and at or past the table end, and sum to exactly `capacity` (`ExtentsInvalid`).
6. The footprint, the two header blocks and those extents, has no two ranges that overlap (`ExtentsInvalid`).
7. The region is not cut short (`ContainerShort`).

A header that lists one extent more than its entry is **one extent ahead**: a growth beyond capacity that crashed between its header write and its page write. The extra extent is not part of the region, and is reported. It is free space, unless the region's page is Unconfirmed, where it may be the newest growth: it is then counted allocated (clamped to the file and to whole blocks).

Regions that pass these rules are then settled against each other and against the unavailable ones (section 15), which may make some of them unavailable (`Overlaps`, `NameTaken`) or read-only.

### Other findings, reported

- **A possibly lost header write.** The winner's other slot passes checks 1&ndash;12 of section 5 (`BadPayloadCrc`) with a higher generation: a recycle, slot commit or growth's header may have been lost to a crash. The older version is served.
- For each Unconfirmed page, whether every region header on it agrees with its entry (no rule above failed, and no header is one extent ahead). Agreement does not prove nothing was lost: a growth within capacity changes only the entry; a lost provisioning leaves no entry; a lost release whose Released header was never written leaves a Ready header over space that may since have held another region's bytes.

### 512-byte append blocks

An append region whose header says `log2_block = 9` uses 512-byte data blocks: its `data_size` is a multiple of 512, while its extents and `capacity` stay multiples of B. store-io 0.5 reads such a region but does not write it: a store with a usable one opens read-only, with the reason.

Source: [`crates/store-io-format/src/meta2.rs`](../crates/store-io-format/src/meta2.rs) (`Entry2::check_header`), [`crates/store-io-format/src/footprint.rs`](../crates/store-io-format/src/footprint.rs) (`check_region`), [`crates/store-io-engine/src/open2.rs`](../crates/store-io-engine/src/open2.rs) (`check_regions`, `reason_of`), [`crates/store-io-engine/src/report.rs`](../crates/store-io-engine/src/report.rs) (`Unavailable`)

---

## 14. Judging the volume record and the table pages

A crash can tear at most one write of the volume record or of a table page. The judgement tells that one torn write apart from damage, using `meta_seq` and `clean_close`, and decides for each of these pairs whether its winner is the latest committed version.

### `meta_seq`

Every write of the volume record or a table page carries a `meta_seq` in its slot header, at least 1:

| Write | `meta_seq` |
|---|---|
| Create | page `p`'s first copy `p + 1`; the volume record's first copy `P + 1` |
| A table page (provisioning, growth, release, repair, an accepted rollback) | one above the highest `meta_seq` of every numbered slot of the volume record and of every page |
| The volume record (writable open, clean close) | one above the highest `meta_seq` of every numbered slot except the slot this write overwrites |
| A heal (below) | the torn copy's own `meta_seq` when it was readable, else one above every numbered slot except the torn slot |

So a new write's number is above every number a crash could leave readable elsewhere, a damaged copy's number is never reused, and a write that tears is recognisable as the newest. A write that would overflow `meta_seq` is refused (`Unsupported(FormatVersion)`).

These writes are serialised: a writer holds the store's metadata lock from the write through its flush, and each is durable before the next is issued, so at a crash at most one is in flight. A metadata write that fails after it was issued poisons the store, so nothing is written after a failed one.

### The rules

Both slots of the volume record and of every page are read (a failed read is retried once) and validated (section 5). Pair 0 is the volume record; pair `p + 1` is page `p`.

**1. Each pair on its own.**

- A slot `UnsupportedVersion` or `Incompatible`, in either slot: the pair is **Refused**, and the open fails `Unsupported(FormatVersion)`.
- No `Valid` slot, or two `Valid` slots of the same generation (a fork): the pair is **Lost**. Both slots are damage.
- Two `Valid` slots of different generations: the higher wins, **Confirmed**.
- One `Valid` slot, the **winner** with generation `g`, and one failed slot, judged by what it shows (section 5):

| The failed slot | It is |
|---|---|
| `Blank`, it is slot B, and `g == 1` | never written (a pair's slot B before its first update): nothing |
| `Blank` otherwise | **damage** (no history leaves that slot blank) |
| `BadPayloadCrc` with generation `g + 1` and `prev_header_crc32c` equal to the winner's `header_crc32c` | a **candidate** for the torn write, its `meta_seq` as recorded |
| `BadPayloadCrc` with generation below `g` | an older copy, damaged: reported only |
| `BadPayloadCrc` otherwise | **damage** |
| `BadHeaderCrc` whose stored `header_crc32c` equals the winner's `prev_header_crc32c`, with `g >= 2` | the winner's predecessor, damaged: reported only |
| `BadHeaderCrc` otherwise | a **candidate**, its `meta_seq` unknown |
| `ReadError` (after the retry), `BadLength`, `BadMagic`, `Malformed`, `Foreign`, `WrongObject`, `Misplaced` | **damage** |

A pair whose failed slot is nothing or an older copy is **Confirmed**. One whose failed slot is damage is **Unconfirmed**: its winner is used, but it may be missing later commits.

**2. Candidates.** For a candidate `c` with a readable `meta_seq` `m`, let `S(c)` be the highest `meta_seq` among the numbered slots of all these pairs other than `c` itself (counted with multiplicity: if another slot holds the same `m`, `S(c) = m`). `c` **qualifies** if its `meta_seq` is unknown or `m == S(c) + 1`. A candidate that does not qualify is damage, and its pair Unconfirmed.

**3. The store-wide rules.**

- **Clean close**: the volume pair is Confirmed, has no qualifying candidate, and its winner says `clean_close = 1`. Then nothing was in flight, and every page candidate is damage.
- Otherwise a qualifying candidate is **the in-flight write** only if it is the **only** qualifying candidate in the store and nothing is Refused, and, for a page's candidate, also no pair at all is Unconfirmed, Lost or Refused (damage elsewhere could hide the true highest number, and a heal could then undo a committed write). The volume record's candidate is exempt from that last rule: the record holds only `owner_generation` and `clean_close`, and the open rewrites it at once, so healing it undoes nothing.
- Every other candidate is damage, and its pair Unconfirmed.
- The in-flight write's pair stands **Confirmed**.

A page whose winner then fails the page checks (section 8) is Lost too. Since the judgement could not see that, a heal of another page is dropped then, and its torn copy counts as damage, its page Unconfirmed. The same happens to the heal of a page holding an entry whose `footprint_seq` is above the page's own `meta_seq` (section 15).

**4. Writing.** A writable open may write only if nothing is Refused, the volume record has a winner, and the volume record is not Unconfirmed while some page had a candidate (writing the volume record would remove the evidence). Otherwise it opens read-only. It also opens read-only when the winner of the volume record or of any page has an unknown `ro_compat_flags` bit.

### Healing

A writable open, after its region checks and before its volume-record write, rewrites the in-flight write's failed slot with its winner's payload byte for byte: slot generation `g + 1`, `prev_header_crc32c` the winner's header CRC, and the `meta_seq` given in the table above, durably. A healed page stands Confirmed. A power cut during the heal leaves the same kind of candidate, judged and healed again. A read-only open heals nothing.

### What cannot be told apart

After an unclean stop: (a) a single newest copy damaged in its header (`BadHeaderCrc`, not the predecessor) is taken for the in-flight write for as long as that copy exists, until its page is next written; (b) payload damage to the most recent durable metadata write (its header still verifies, so it reads as the write one above everything else) is taken for the in-flight write, until the next metadata write or clean close. In both cases that write is undone.

### Page standings and what they mean

| Standing | Its regions | The page |
|---|---|---|
| Confirmed | served (read-only when possibly accepted while another page is Unconfirmed or Lost, below; section 15 may make others read-only) | written by later changes |
| Unconfirmed (**stale**) | served **read-only**: no data write, growth, recycle, slot commit or release | never written, except by `accept_page_rollback` |
| Lost | unknown: none served | never written |

Any Unconfirmed or Lost page **freezes allocation**: provisioning, growth (within or beyond capacity), release and `repair_unavailable` are refused (`NotWritten(Frozen)`) until no page is Unconfirmed or Lost.

**Possibly accepted pages.** While any page is Unconfirmed or Lost, a Confirmed page whose two slots are both Valid with the same payload (the trace an accepted rollback, or a heal, leaves until the page's next write), or whose non-winning slot is neither Valid nor Blank (the trace may have decayed), has its regions served read-only too: its newest commit may have been given up by `accept_page_rollback`, and a region that commit released may have had its space given to a region in another stale page's lost commit.

**`next_seq` raised.** For every Unconfirmed page, and every Confirmed page whose two slots are both Valid with the same payload or whose non-winning slot is neither Valid nor Blank (whatever the other pages' standing: this, unlike the read-only rule above, holds in a healthy store too), the open counts the page's `next_seq` 4,096 higher in memory, and each open does so again while that trace lasts, and the page's next commit writes that value, so region ids a given-up commit may have handed out are not handed out again (unless more than 4,096 provisionings failed on that page between its last two commits).

Source: [`crates/store-io-format/src/judge.rs`](../crates/store-io-format/src/judge.rs) (`judge`, `opposite`), [`crates/store-io-engine/src/v2.rs`](../crates/store-io-engine/src/v2.rs) (`open_metadata`, `next_meta_seq`, `heal`, `write_page`, `write_volume`, `SEQ_MARGIN`), [`crates/store-io-engine/src/slots.rs`](../crates/store-io-engine/src/slots.rs) (`PairRead::possibly_accepted`)

---

## 15. Settling regions against each other

The regions that passed section 13's rules (on Confirmed and Unconfirmed pages alike) are compared with each other, and with the unavailable ones, all at once. `footprint_seq` decides: every change to a region's footprint precedes its release, a name is reused only after a release, and every footprint was placed by a free map that excluded every live footprint, so where two claims meet, the newer is the live one.

**The sequence each entry counts for.** A usable region, and an unavailable one whose entry passed the entry checks, count for their `footprint_seq`. An entry that failed the entry checks counts for its `footprint_seq` if that is between 1 and its page copy's `meta_seq`, and as the highest possible value (newest of all) if it is 0 or above that `meta_seq`.

**What an unavailable region may have written.** Its header blocks and every extent its header lists, when its header winner decodes by itself (section 9) and lists at least as many extents as its entry; otherwise its whole span. For an entry that failed the entry checks: its reserved span (section 8).

The rules, each judged for every usable region against every other at once (a region demoted by one rule still demotes others):

1. **Overlaps.** Where the footprints of two usable regions overlap, the one with the lower `footprint_seq` becomes unavailable (`Overlaps`); with equal values, both do.
2. **Names.** Where usable regions share a name, or a usable region shares a name with an unavailable entry whose name decodes, every usable region whose `footprint_seq` is below the highest value of that name becomes unavailable (`NameTaken`); if two or more claims share the highest value, every usable one of them does too. So an entry that fails its checks never lets an older one of its name be served.
3. **Possibly superseded.** Where what an unavailable region may have written overlaps a usable region's footprint and the unavailable region's sequence is strictly higher, the usable region is **possibly superseded**: served read-only, reported, never made writable by `accept_page_rollback`, and marked in a copy (it may read another region's bytes). An unavailable region never makes a usable one unavailable.
4. **Shares reserved.** Where a usable region's footprint overlaps the span an unavailable region keeps reserved (including the span of one demoted by rules 1 and 2), the overlap is reported; those bytes stay allocated.

A region demoted by rules 1 or 2 keeps its whole span allocated, and supersedes nothing.

**Keeping the verdicts stable.** Removing an entry (a release, or `repair_unavailable`) could lift a verdict at the next open. Before removing one, store-io judges rules 1&ndash;3 again over what would remain, and refuses the removal (`NotWritten(Overlap)`, naming the region) if a region it demoted would be served, or one it made possibly superseded would not be. A page holding an entry whose `footprint_seq` is above the page copy's own `meta_seq` is never rewritten keeping that entry (a later `meta_seq` would change how the next open counts it): provisioning skips that page, growth and release of its regions are refused, the open does not heal it, and the only write it takes is one removing that entry.

Source: [`crates/store-io-engine/src/footprint.rs`](../crates/store-io-engine/src/footprint.rs) (`settle`, `shares_reserved`), [`crates/store-io-engine/src/open2.rs`](../crates/store-io-engine/src/open2.rs) (`check_regions`), [`crates/store-io-engine/src/repair.rs`](../crates/store-io-engine/src/repair.rs) (`would_lift`), [`crates/store-io-engine/src/v2.rs`](../crates/store-io-engine/src/v2.rs) (`Page2::ahead`)

---

## 16. Reading a container

This is how store-io opens a format-2 container. An independent reader that follows it reads exactly what store-io reads; steps 1, 12 and 13 are what a writer does besides.

1. **(Writable) Take ownership and drain.** Take the exclusive ownership lock; probe the device and refuse an unsafe or unverified one unless trusted; then make the whole file durable (a full flush) **before reading any slot**. A read-only open takes the shared lock and flushes nothing.
2. **Find the volume record** (section 2). B, T and P come from it.
3. **Check the table area**: `2B + 2PT <=` the file's length, else `Corruption(Metadata)`.
4. **Re-read** any slot of the volume pair whose read failed, once, and judge the pair again. A pair with no winner now (Fork or Lost) fails the open (`Corruption(Fork)`, `Corruption(HeaderCrc)` or `Corruption(MediaError)`). After any re-read the winner's record is decoded again (section 7) and must give the same B, T and P, else `Corruption(Metadata)`.
5. **Read both slots of every page** (each failed read retried once).
6. **Judge** the volume record and every page (section 14). A Refused pair fails the open with `Unsupported(FormatVersion)`.
7. **Decode every page** that is not Lost: its winner's payload must pass the page checks (section 8), else the page is Lost. Raise `next_seq` in memory for an Unconfirmed or possibly accepted page (section 14). Drop a page's heal if a page failed here or the page to heal holds an entry past its own write (the volume record's heal is never dropped).
8. **Freeze** allocation if any page is Unconfirmed or Lost.
9. **Check every region** on every Confirmed and Unconfirmed page (section 13): entry checks, then each header pair (both slots, each failed read retried once), then the header rules.
10. **Settle** the regions (section 15).
11. **Free space** is the complement of everything allocated (section 17).
12. **Decide read-only.** The open is read-only if it was asked to be; if section 14 forbids writing; if a metadata winner has an unknown `ro_compat_flags` bit; or if a usable append region has 512-byte blocks. Everything above writes nothing, so this verdict comes before the first write.
13. **(Writable) Heal and take over.** Heal the in-flight write (section 14), then write the volume record's next version: `owner_generation` one higher, `clean_close = 0`, the next `meta_seq`, durably.

A region is served **read-only** when its page is Unconfirmed; when its page is possibly accepted while some page is Unconfirmed or Lost; when it is possibly superseded; or when its header winner has an unknown `ro_compat_flags` bit.

The open's report lists every candidate and damaged slot, every Unconfirmed or Lost page, every entry that cannot be listed as a region, every extra extent, every possibly superseded region and every region that shares reserved bytes, the stale pages' agreement, every possibly lost header write, every unavailable region with its reason, and whether the last writer closed cleanly.

Source: [`crates/store-io-engine/src/open.rs`](../crates/store-io-engine/src/open.rs) (`open_inner`), [`crates/store-io-engine/src/v2.rs`](../crates/store-io-engine/src/v2.rs) (`open_metadata`, `read_retried`, `read_all_retried`), [`crates/store-io-engine/src/open2.rs`](../crates/store-io-engine/src/open2.rs) (`check_regions`)

---

## 17. Space: placement and reuse

### Free space

Free space is the complement, within `[table end, file length)`, of everything allocated:

- every usable region's footprint;
- the span of every region demoted in section 15;
- the reserved span of every unavailable entry (section 8);
- every extra extent of a region on an Unconfirmed page (section 13);
- space marked by a provisioning or growth in flight.

It is always recomputed as a complement after a change, never by adding a range back, so space inside an unavailable span that another region's footprint covers stays allocated. The **tail** is the end of the last allocated byte (at least the table end).

### Placement

A new region's footprint is placed with its data area aligned: to 1 MiB when its capacity is at least 1 MiB, else to B.

1. **First fit**: the first free range, in offset order, holding the whole footprint (`2B + capacity`) at an offset `at` with `at + 2B` aligned.
2. Otherwise **past everything allocated**: the tail, moved up so `tail + 2B` is aligned.

The file then grows, if needed, to the end of the new footprint or the tail, whichever is further, plus every byte reserved and not yet provisioned. A region provisioned from a reservation whose growth finds no room for the alignment is placed aligned to B instead. Free space left before an aligned region stays free, and later placements use it.

A growth beyond capacity places its extent as section 10 says. The file is never grown while any entry's `span_end` is past its end (a region cut short, whatever else the entry fails): growth there would make zeros of the lost tail, and is refused (`Corruption(Truncated)`).

A released region's footprint becomes free space at once, and is reused by any later placement it fits, whatever its length. The file never shrinks.

### Accounting

A tag is charged a region's `capacity + 2B` physical bytes and its `data_size` logical bytes. An unavailable region whose entry passed the entry checks still counts, except one cut short.

Source: [`crates/store-io-engine/src/footprint.rs`](../crates/store-io-engine/src/footprint.rs) (`FreeMap`), [`crates/store-io-engine/src/provision2.rs`](../crates/store-io-engine/src/provision2.rs), [`crates/store-io-engine/src/grow2.rs`](../crates/store-io-engine/src/grow2.rs), [`crates/store-io-engine/src/space.rs`](../crates/store-io-engine/src/space.rs) (`grow_to`, `tail`), [`crates/store-io-engine/src/open2.rs`](../crates/store-io-engine/src/open2.rs)

---

## 18. Versioning and feature flags

| Mechanism | Where | Rule |
|---|---|---|
| `format_major` | Every slot, offset 14 | `2`. Fixes every layout in Part I. A change a format-2 reader must understand is a new `format_major` or a must-understand bit. A slot of another value in a format-2 container is `UnsupportedVersion`: the open fails `Unsupported(FormatVersion)` when it is in the volume record or a table page, and the region is unavailable (`HeaderUnreadable`) when it is in a region header. |
| `incompat_flags` | Every slot, offset 16 | No bit is defined. Any set bit refuses the pair, as above. |
| `ro_compat_flags` | Every slot, offset 20 | No bit is defined. An unknown bit in the winner of the volume record or a page opens the store read-only; in a region header's winner, that region read-only. Such a slot is never rewritten. |
| `compat_flags` | Every slot, offset 24 | No bit is defined. Unknown bits are ignored, and every slot store-io writes has `compat_flags = 0`. |
| `header_len` | Every slot, offset 12 | Exactly 128 in format 2; any other value that passes the header CRC is `Malformed`. |
| `features` | Volume record, offset 32 | Must be `0`: non-zero is `Unsupported(FormatVersion)`. |
| `flags` | Volume record, offset 56 | Must be `0`: non-zero is `Unsupported(FormatVersion)`. |
| Payload lengths | Every payload | Exact. The volume record's is 64 bytes, and a longer one is a newer layout (`Unsupported(FormatVersion)`). A page's follows from `count` (otherwise the page is Lost), a region header's from `extent_count` or `object_len` (otherwise the region is unavailable). An entry is 96 bytes; a different size is a new `format_major`. |
| `flags` | Entry, offset 6; region header, offset 11 | Must-understand. Known: `0x01` unwritten, `0x02` capped. An unknown bit makes the entry fail its checks, or the header fail to decode: that region is unavailable, never the store. An entry with an unknown bit is never changed by a writer. |
| `compat_flags` | Entry, offset 7; region header, offset 12 | May-ignore. No bit is defined; kept byte for byte on every rewrite. |
| `kind`, `state`, `fill` | Entries, region headers | Undefined values fail the entry checks or the header's decoding (that region unavailable). A new fill pattern gets a new `fill` value; version 1 never changes. |

Source: [`crates/store-io-format/src/slot.rs`](../crates/store-io-format/src/slot.rs) (`KNOWN_INCOMPAT`, `KNOWN_RO_COMPAT`, `SlotInfo::writable`), [`crates/store-io-format/src/meta2.rs`](../crates/store-io-format/src/meta2.rs) (`KNOWN_FLAGS`), [`crates/store-io-engine/src/v2.rs`](../crates/store-io-engine/src/v2.rs) (`decode_volume`, `open_metadata`), [`crates/store-io-engine/src/slots.rs`](../crates/store-io-engine/src/slots.rs) (`plan_next`)

---

## 19. Crash consistency of each change

Every metadata write below is an A/B slot update (section 4): the slot not holding the current version is written whole, as one I/O, and made durable before the next step. A crash leaves that slot old, new, or damaged (a torn write fails the header or payload CRC; an all-zero header reads as blank), and the current version untouched. Updates to one pair are serialised. A write that fails after it was submitted poisons the store (`DurabilityUnknown`), so nothing more is written until it is reopened. "Commit" marks the write that makes a change exist.

**Why a writable open flushes first.** A process can die after writing the free slot but before its flush. The write is then visible to reads but not yet durable. Without a flush, the next owner would read it as the winner, write its own update over the *older* slot, the only durable copy, and a power cut could then lose both. A writable open therefore makes everything the previous owner wrote durable before it reads any slot.

### Create

1. Create the directory if missing (and flush its parent), then create `store.sio` exclusively and take the lock (or take over a crashed create's file, below, truncating it to zero bytes).
2. Probe the device; refuse it unless trusted. Choose B, T and P.
3. Size the file to the end of page 0's slot A, `2B + T` (to the whole table, `2B + 2PT`, when P = 1), and write page 0's slot A: generation 1, `meta_seq` 1, an empty page.
4. If P > 1: full flush; size the file to `2B + 2PT`; write each other page's slot A, page `p` at `meta_seq` `p + 1`.
5. Full flush.
6. Write the volume record's slot A: generation 1, `meta_seq` `P + 1`, `owner_generation` 1, `clean_close` 0.
7. Full flush. The in-memory state of every pair is what was written; nothing is read back.
8. Flush the directory.

A failure in steps 2&ndash;7 removes the file again. A crash before step 7 completes may leave no valid volume record: such a file does not open (`Corruption(HeaderCrc)`), and the next `create` takes it over. Step 6's write may also be durable before the flush returns, and the file then opens as an empty store (a `create` does not take that one over: it needs a torn record). Growing the file past `2B + T` only once page 0 is durable means a crash never leaves a file longer than that without a page a later create recognises.

**Taking over a crashed create.** When `store.sio` already exists, a format-2 `create` reuses it only if all of these hold, and otherwise fails with `AlreadyExists` without touching it:

- no one holds its lock;
- it is no longer than `2B + 2PT` for the geometry this create would use (checked first against the largest such value over every block size, then, after the probe, against the chosen one): a crashed create never wrote past its table area, and a crashed create of a larger table is taken over only by a create asking for at least as much;
- every 4 KiB block of it reads back without error (the last, partial block as far as the file goes);
- no 4 KiB block starts with a slot header whose header CRC verifies, other than a crashed create's leftovers: a slot with `format_major` 2, generation 1, `header_len` 128, slot index A, a `slot_offset` equal to where it lies and its payload within the 4 KiB read, that is either the volume record (object 0) at offset 0 with a torn payload (its payload CRC does not match), or page `p`'s first copy (object `2^61 | p`) at `2B' + 2pT'` for some block size B' from 4 KiB to 64 KiB, with T' this create's table slot size clamped to between B' and 64 KiB, whose payload is torn or is an empty page. Leftovers of any volume id qualify (an earlier crashed create may have left another);
- when the file is longer than 4 &times; 64 KiB (format 1's bound), at least one such leftover was found: a larger file with nothing recognisable may be a damaged store of either format.

The file is then truncated to zero bytes; the truncation becomes durable with the create's first full flush.

### Writable open

1. Full flush (section 16, step 1).
2. Heal the in-flight write, if the judgement found one (section 14).
3. The volume record's next version: `owner_generation` + 1, `clean_close` 0.

A crash in step 2 or 3 leaves what the judgement reads at the next open: a torn heal is the same kind of candidate and is healed again, and a torn volume-record write is the volume record's candidate.

### Close

Once every handle is gone (so every other write has completed), a writable store that is not poisoned, and not unwinding a panic, writes the volume record's next version with `clean_close = 1`. A crash or failure before it is durable leaves the store judged as not closed cleanly, which is safe. After a clean close, the next open treats every page candidate as damage: nothing was in flight.

### Provision

1. Under the metadata lock: claim the name and a table slot `(p, seq)` (the first Confirmed page with room, `seq` at least `next_seq`, skipping a page that holds an entry past its own write); place the footprint (section 17) and grow the file; mark the footprint allocated; charge the tag or hold the reservation's bytes.
2. Without the lock: fill the whole footprint with direct writes, the two header blocks with zeros and the data with the region's pattern (a capped region's slack allocated instead, except on Windows and macOS). This blanks any older header at that place. A fill write that fails after submission poisons the store; running out of space is `NoSpace` and poisons nothing.
3. Full flush, then verify the filled range: the region is refused (`NotWritten(NotReady)`) if the platform reports any part of it unwritten or shared, any of its pages in the operating system's page cache, or (Windows) any part of it above the valid data length.
4. Write the header's slot A: slot generation 1, Ready, its extents.
5. Under the metadata lock: write the page as it is now with the new entry added, its `footprint_seq` this write's own `meta_seq`, `next_seq` past its `seq` (**commit**).

| Crash after | State after reopen |
|---|---|
| steps 1&ndash;4 | No entry. The footprint is free space; a header written in step 4 names an id no entry has, is never read, and the next provisioning there blanks it. The `seq` may be given out again, since `next_seq` was not committed. |
| step 5 | The region exists, filled and verified. |

A failure before the commit gives back the claim, the marked space and the tag's charge. Released bytes never reappear inside a new region: the fill covers the whole footprint and is durable before the commit.

### Growth within capacity

Under the region lock: (1) fill `[data_size, new size)`, full flush, verify; (2) the page with the new `data_size` (**commit**); then the new size is published. A crash before 2 leaves the old size; the filled bytes past it have no defined contents.

### Growth beyond capacity

Under the region lock: (1) under the metadata lock, choose and mark the new extent and charge the tag; (2) without it, fill (section 10), full flush, verify; (3) if the header is already one extent ahead (an earlier growth whose commit was lost), rewrite it with the committed extents only, then write the header listing the new extent; (4) under the metadata lock, the page with the new `data_size`, `capacity`, `extent_count`, span and `footprint_seq` (**commit**).

| Crash after | State after reopen |
|---|---|
| steps 1&ndash;2 | Nothing on disk names the extent: free space. |
| step 3 | The header is one extent ahead: the extra extent is reported and is free (allocated while the page is Unconfirmed). The region keeps its old size. |
| step 4 | Grown. |

Step 3's first rewrite keeps the two copies of the pair from both listing as many extents as the entry with one the region never had.

### Recycle

Stop admitting operations on the region and wait for every one in flight; then, under the region lock, write the header's next version: Ready, `region_generation` + 1, the entry's extents. No other byte is written: the data keeps the old generation's bytes. A crash leaves the old or the new `region_generation`, and the caller's records carry the generation to tell them apart.

### Slot commit

An A/B update of the region's header pair carrying the fixed part (with the new `object_len`) and the new object. After a crash, the object reads back as exactly the old one or exactly the new one.

### Release

Stop admitting operations on the region and wait for every one in flight; then, holding the region lock and the metadata lock throughout:

1. Refuse if removing the entry would lift a verdict (section 15).
2. Write the page without the entry, `next_seq` unchanged (**commit**). Free space is recomputed at once.
3. Write the header's next version with state Released (forensic).
4. Deallocate the region's data extents ([Platforms](./PLATFORMS.md)); the header blocks keep the Released header.

| Crash after | State after reopen |
|---|---|
| step 1 | The region is still there and intact. |
| steps 2&ndash;4 | The region is gone; its footprint is free space. |

The header goes second so that, should the page's commit ever be rolled back (its newest copy found damaged and the rollback accepted), the region is then unavailable (`HeaderReleased`), not served over space another region may have used. A header write refused before it was submitted leaves the data in place and the release committed; the residual (a rolled-back release whose Released header was never written) is stated in section 13.

### `repair_unavailable`

Not while allocation is frozen. Under the metadata lock: refuse if removing the entry would lift a verdict (section 15); write the page without the entry (**commit**); free space is the complement again: the entry's span is never added back, and other regions' bytes inside it stay allocated. The region's bytes are not touched. A crash leaves the entry or its absence.

### `accept_page_rollback`

For each named page that is Unconfirmed (every page is checked before any is written: a Lost page, a page the table does not have, or one holding an entry past its own write refuses the whole call):

1. Rewrite the page's winner byte for byte into its failed slot, even an unreadable one (writing usually remaps the sector): slot generation `g + 1`, the next `meta_seq`, durably.
2. Read the pair back (a failed read retried once): both slots must be Valid with the same payload. Otherwise, or if the read fails, the store is poisoned: the new copy is durable at a `meta_seq` the open's state cannot count, so only the next open knows the page.
3. The page is Confirmed; its extra extents are freed and free space recomputed. Whatever its lost commits held is given up.

Once no page is Unconfirmed or Lost, allocation unfreezes and the regions served read-only for their page become writable (not a possibly superseded one, nor one whose header has an unknown `ro_compat_flags` bit). A crash before step 1 completes leaves the page Unconfirmed; a torn rewrite is a candidate, healed to the same content when the store is otherwise sound, else the page stays Unconfirmed and the accept is run again.

### `copy_store`

The source is opened read-only (it never heals and never writes its volume record). The target is created as a new format-2 store, with the source's block size (refused if the target's device needs larger blocks) and room for at least as many regions as it copies, under the temporary name `store.sio.copying`; every region is recreated (name, kind, data size, cap, tag, `region_generation`, fill pattern and key; laid out anew, one extent each) and its data copied, made durable, the target closed cleanly, and only then renamed to `store.sio` by a rename that never replaces. A crash before the rename leaves the target name untouched; a leftover temporary file is removed by the next copy.

Source: [`crates/store-io-engine/src/create.rs`](../crates/store-io-engine/src/create.rs) (`create_named`, `reclaim_abandoned`, `holds_slot_header`, `crashed_create_leftover`), [`crates/store-io-engine/src/v2.rs`](../crates/store-io-engine/src/v2.rs) (`create_metadata`, `open_metadata`, `heal`, `accept_page`, `Drop for Inner`), [`crates/store-io-engine/src/provision2.rs`](../crates/store-io-engine/src/provision2.rs), [`crates/store-io-engine/src/provision.rs`](../crates/store-io-engine/src/provision.rs) (`fill`, `verify_ready`), [`crates/store-io-engine/src/grow2.rs`](../crates/store-io-engine/src/grow2.rs), [`crates/store-io-engine/src/lifecycle.rs`](../crates/store-io-engine/src/lifecycle.rs) (`recycle_region`), [`crates/store-io-engine/src/lifecycle2.rs`](../crates/store-io-engine/src/lifecycle2.rs), [`crates/store-io-engine/src/slot_handle.rs`](../crates/store-io-engine/src/slot_handle.rs), [`crates/store-io-engine/src/repair.rs`](../crates/store-io-engine/src/repair.rs), [`crates/store-io-engine/src/copy.rs`](../crates/store-io-engine/src/copy.rs)

---

## 20. Damage and the ways out

Apart from the failures of sections 2, 7 and 16 (no volume record found, a newer format, a table area past the end of the file, an I/O error), the open of a format-2 store succeeds: damage makes parts of it unavailable, read-only or frozen, never the whole store unreadable. Each kind of damage has a way out:

| Found | Effect | Way out |
|---|---|---|
| An unavailable region (section 13, 15) | not served; name and span reserved | `repair_unavailable(id)`: the entry is removed, its name freed, its space free where no other footprint lies. Refused while frozen, and when the removal would lift a verdict (repair a demoted region first). An entry of a kind this version does not know is repaired by its id `page << 32 \| seq`. |
| An Unconfirmed (stale) page | its regions read-only; allocation frozen | `accept_page_rollback(pages)`: the page's winner becomes its content, giving up whatever its lost commits held (a region a lost commit created is missing; one it released is back). Or `copy_store` with `accept_stale_pages`. |
| A Lost page | its regions unknown; allocation frozen | `copy_store` with `accept_stale_pages` only (accept refuses a Lost page). |
| The volume record Unconfirmed while a page had a candidate | the store opens read-only | `copy_store` with `accept_stale_pages` (a volume record that is not Confirmed demotes every page candidate, so that page is always stale). |
| A possibly superseded region | read-only | `copy_store` with `accept_stale_pages` (the copy marks it). |
| An entry past its page's own write, or entries no repair can remove (a tie; one superseding a live region; two past their page's write on one page) | its page never rewritten keeping it | `repair_unavailable(id)` when it is the only such entry on its page and supersedes nothing; otherwise `copy_store` with `accept_stale_pages`. |
| A region cut short | unavailable; growth of the file refused | `repair_unavailable(id)`. |

**What `copy_store` copies.** A slot object larger than a format-2 slot takes is refused before anything is created, with or without `accept_stale_pages`. Without it, so is a source with an unavailable region, an entry it cannot read, or a stale or lost page. With it: every usable region, by its recorded `data_size`; a usable region of an Unconfirmed page **in full**, `[0, capacity)`, plus the extra extent its header lists when that overlaps nothing any other bytes may belong to (the metadata, every other footprint and extra extent, every reserved span and what each unavailable entry may have written), never while a page is Lost, with its recorded `data_size` reported as the part known to be its data; unavailable regions and unreadable entries left out and reported; lost and stale pages reported; and every region that may hold another region's bytes (possibly superseded, or of a stale or possibly accepted page while another page is stale, lost or possibly accepted) copied and reported, since the copy's regions are writable.

Source: [`crates/store-io-engine/src/repair.rs`](../crates/store-io-engine/src/repair.rs) (`repair_unavailable`, `accept_page_rollback`), [`crates/store-io-engine/src/copy.rs`](../crates/store-io-engine/src/copy.rs) (`CopyOptions`, `CopyReport`)

---

## 21. What format 2 does not check

A reader that wants to be stricter than store-io can add these checks; store-io 0.5 does not make them.

- The bytes of a slot after its payload.
- The volume record's `device_index` and `created_unix_s`.
- `owner_generation` in slot headers and in the volume record: recorded, never used to accept or refuse anything.
- The generation-gap and chain-break anomalies of a pair (computed, not acted on).
- A region header's `fill`, `fill_key`, `region_generation` and `compat_flags` against anything: the entry holds none of them.
- That an extent listed past the entry's count lies within the file (section 9).
- The order of entries in a page, and the relation of `next_seq` to the entries of other pages.
- Region data, including `[data_size, capacity)`: the caller's records carry their own checksums.
- A region header's own rollback (a damaged newer header copy): reported, not refused (section 13).
- A lost release whose Released header was never written, brought back by a page rollback over space another region has used since (section 13).

---

# Part II: Format 1 (read-only from 0.5)

Format 1 is the format store-io 0.1 to 0.4 write. store-io 0.5 opens a format-1 container for reading only: `Store::open_readonly` reads it as these sections describe, and a writable open fails `Unsupported(FormatVersion)` after its flush-first step and before anything is written. `copy_store` copies a format-1 store into a new format-2 one. 0.5 creates a format-1 container only through a hidden option kept for tests and migration fixtures. The sections on writing (F10's writable open, F11, F13) describe what 0.1 to 0.4 do, and are kept so that a reader can interpret any format-1 container it finds.

## F1. Conventions

- **Byte order.** Every integer is little-endian. `u8`, `u16`, `u32` and `u64` are unsigned.
- **Offsets.** Offsets in the layout tables are relative to the start of the structure being described. Offsets stored *in* the format (`slot_offset`, `table_offset`, a region's `offset` and `data_offset`) are absolute byte offsets in the container file.
- **Reserved bytes** are written as zero. Section F14 lists which ones a reader checks.
- **Checksum.** CRC-32C (Castagnoli): reflected polynomial `0x82F63B78` (normal form `0x1EDC6F41`), initial value `0xFFFFFFFF`, final XOR `0xFFFFFFFF`, reflected input and output. The CRC of the ASCII bytes `123456789` is `0xE3069283`, and the CRC of empty input is `0`. A stored CRC is a `u32`, little-endian.
- **B** is the container's block size and **T** its region-table slot size. Both are powers of two (section F2).

Source: [`crates/store-io-format/src/codec.rs`](../crates/store-io-format/src/codec.rs), [`crates/store-io-format/src/crc32c.rs`](../crates/store-io-format/src/crc32c.rs)

---

## F2. Container layout

A store is a directory that holds one container file, `store.sio`. Everything store-io keeps for that store is inside this file.

### Geometry

The geometry is fixed when the store is created and recorded in the volume record (section F6). Later opens read it from there and ignore the size options.

| Quantity | Value |
|---|---|
| Block size B | The largest of 4096, the device's logical block, physical block and preferred write size, the direct-I/O memory and offset alignments, and the caller's requested `block_size`, rounded up to a power of two and clamped to 4 KiB&ndash;64 KiB (`log2` 12&ndash;16). |
| Table slot size T | The caller's `log2_table_slot` (default 14, so 16 KiB), clamped to between `log2(B)` and 16. |
| Region-table capacity | `floor((T - 128 - 16) / 64)` entries: 253 at 16 KiB, 1021 at 64 KiB. |

### Layout

| Offset | Size | Contents |
|---|---|---|
| `0` | B | Volume record, slot A |
| `B` | B | Volume record, slot B |
| `2B` | T | Region table, slot A |
| `2B + T` | T | Region table, slot B |
| `2B + 2T` | to end of file | Region extents, each starting on a multiple of B |

A **region extent** occupies `2B + data_size` bytes starting at its table entry's `offset`:

| Offset in extent | Size | Contents |
|---|---|---|
| `0` | B | Region header, slot A |
| `B` | B | Region header, slot B |
| `2B` | `data_size` | The data area (absent for slot regions, where `data_size` is 0) |

With the defaults (B = 4096, T = 16384): volume slots at 0 and 4096, table slots at 8192 and 24576, and the first region's header slots at 40960 and 45056, with its data area from 49152.

### Object identities

Every slot carries the 64-bit identity of the object its pair stores:

| Object | Id |
|---|---|
| Volume record | `0` |
| Region table | `1` |
| Header of region `r` | `2^32 + r` (that is, `0x1_0000_0000 | r`) |

### Volume identity

Each container has a random 16-byte volume id, chosen at creation. Every slot in the file carries it, so a slot copied from another container is rejected as foreign.

### File length

The file is at least `2B + 2T` bytes long. It grows when regions are provisioned and when space is reserved, and it never shrinks. Bytes past the end of the last extent, and the bytes of released extents, have no defined contents.

Source: [`crates/store-io-engine/src/layout.rs`](../crates/store-io-engine/src/layout.rs), [`crates/store-io-engine/src/create.rs`](../crates/store-io-engine/src/create.rs) (`build_new`), [`crates/store-io-engine/src/store.rs`](../crates/store-io-engine/src/store.rs) (`pair_volume`, `pair_table`, `pair_region`)

---

## F3. The slot

Every piece of metadata is stored as a **pair** of slots, A and B, in two separate places. A slot is a power-of-two block (512 bytes to 64 KiB at this layer; store-io itself uses B or T) holding a 128-byte header and then a payload. An update always writes the slot that does *not* hold the current version, as one whole-slot write, and then makes it durable, so a crash can only damage the slot being written.

### Header (128 bytes)

| Offset | Size | Type | Field | Meaning |
|---|---|---|---|---|
| 0 | 8 | bytes | `magic` | `"SIOSLOT\0"` (`53 49 4F 53 4C 4F 54 00`) |
| 8 | 4 | u32 | `header_crc32c` | CRC-32C of bytes `[0, header_len)`, computed with this field read as zero |
| 12 | 2 | u16 | `header_len` | 128 in format 1. Readers accept `128 <= header_len <= slot size`. |
| 14 | 2 | u16 | `format_major` | `1` |
| 16 | 4 | u32 | `incompat_flags` | `0`. Any unknown bit: the slot is refused. |
| 20 | 4 | u32 | `ro_compat_flags` | `0`. Any unknown bit: readable, but must not be rewritten. |
| 24 | 4 | u32 | `compat_flags` | `0`. Unknown bits are ignored. |
| 28 | 4 | u32 | `payload_crc32c` | CRC-32C of the payload bytes |
| 32 | 16 | bytes | `volume_uuid` | The container's volume id |
| 48 | 8 | u64 | `object_id` | The object this pair stores (section F2) |
| 56 | 8 | u64 | `generation` | At least 1. Each committed update writes the winner's generation + 1. |
| 64 | 8 | u64 | `slot_offset` | Absolute file offset this slot was written to |
| 72 | 4 | u32 | `payload_len` | Payload length in bytes, at most `slot size - header_len` |
| 76 | 1 | u8 | `slot_index` | `0` = A, `1` = B |
| 77 | 1 | u8 | `slot_count` | `2` |
| 78 | 1 | u8 | `log2_slot_size` | Slot size as a power of two (9&ndash;16) |
| 79 | 1 | &mdash; | reserved | `0` |
| 80 | 4 | u32 | `prev_header_crc32c` | `header_crc32c` of the version this one replaced; `0` for generation 1 |
| 84 | 4 | &mdash; | reserved | `0` |
| 88 | 8 | u64 | `owner_generation` | The volume record's owner generation when the slot was written (section F6) |
| 96 | 32 | &mdash; | reserved | `0` |

The payload starts at `header_len` and is `payload_len` bytes long. Every byte after the payload, up to the end of the slot, is written as zero.

### CRC coverage

| Bytes | Covered by |
|---|---|
| `[0, 8)` and `[12, header_len)` | `header_crc32c` (bytes `[8, 12)` count as four zero bytes) |
| `[header_len, header_len + payload_len)` | `payload_crc32c` |
| `[header_len + payload_len, slot end)` | Nothing. Written as zero, never read. |

Region data areas carry no store-io checksums: the caller's records must carry their own.

### Writing a slot

The writer fills the whole slot with zeros, copies in the payload, writes the header with `header_crc32c` as zero, computes the header CRC over `[0, 128)`, and stores it at offset 8. Generation 0 is never written. The new slot's `generation`, `prev_header_crc32c` and `slot_index` come from the pair's current state (section F5).

Source: [`crates/store-io-format/src/slot.rs`](../crates/store-io-format/src/slot.rs) (`encode`, `write_header`)

---

## F4. Validating a slot

A reader knows where it read the slot from and what it expects there: the volume id, the object id, the absolute offset, the slot index and the slot size. Checks run in this order, and the first failure decides the status:

| # | Check | Status if it fails |
|---|---|---|
| 0 | The read succeeded and returned the whole slot (checked by the caller) | `ReadError` |
| 1 | `log2_slot_size` expected is 9&ndash;16 and the buffer is exactly that size | `BadLength` |
| 2 | The first 128 bytes are not all zero | `Blank` (never written) |
| 3 | `magic` is `"SIOSLOT\0"` | `BadMagic` |
| 4 | `128 <= header_len <= slot size` | `BadHeaderCrc` |
| 5 | `header_crc32c` matches | `BadHeaderCrc` |
| 6 | `format_major == 1` | `UnsupportedVersion { major }` |
| 7 | `incompat_flags` has no unknown bit (none are known in format 1) | `Incompatible { flags }` |
| 8 | `generation != 0`, `slot_count == 2`, `slot_index <= 1`, and the reserved bytes at 79, `[84, 88)` and `[96, 128)` are zero | `Malformed` |
| 9 | `volume_uuid` is the expected volume | `Foreign` |
| 10 | `object_id` is the expected object | `WrongObject` |
| 11 | `slot_offset`, `slot_index` and `log2_slot_size` match where the slot was read from | `Misplaced` (a misdirected write) |
| 12 | `payload_len <= slot size - header_len` | `Malformed` |
| 13 | `payload_crc32c` matches | `BadPayloadCrc` |
| &mdash; | Every check passed | `Valid` |

Note that an out-of-range `header_len` reports as `BadHeaderCrc`, not `Malformed`, and that the `payload_len` bound is checked only after the identity checks.

A valid slot whose `ro_compat_flags` has an unknown bit is still `Valid`; such a slot is readable but must not be overwritten with a newer generation (see section F12 for what store-io does today).

Source: [`crates/store-io-format/src/slot.rs`](../crates/store-io-format/src/slot.rs) (`validate`, `parse_header`, `parse`), [`crates/store-io-engine/src/slots.rs`](../crates/store-io-engine/src/slots.rs) (`read_one`)

---

## F5. Choosing the winner of a pair

Both slots are read and validated, and the pair's current version is chosen from the two statuses. Choosing never writes either slot.

### Decision table

Rows are tried in order; "explained" means `Blank`, `BadHeaderCrc` or `BadPayloadCrc`, the states an ordinary crash during the latest write leaves behind.

| Slot A | Slot B | Winner |
|---|---|---|
| `UnsupportedVersion` or `Incompatible` | anything | **Refused** |
| anything | `UnsupportedVersion` or `Incompatible` | **Refused** |
| `Blank` | `Blank` | **Empty** (never written) |
| `Valid`, generation *g* | `Valid`, generation *g* | **Fork** (no winner) |
| `Valid`, generation *a* | `Valid`, generation *b*, *a* &ne; *b* | **Confirmed**, the higher generation |
| `Valid` | explained | **Confirmed** A |
| explained | `Valid` | **Confirmed** B |
| `Valid` | any other status | **Unconfirmed** A |
| any other status | `Valid` | **Unconfirmed** B |
| anything else | | **Lost** |

"Any other status" is `ReadError`, `BadLength`, `BadMagic`, `Malformed`, `Foreign`, `WrongObject` or `Misplaced`: something an ordinary history never puts in the other slot, so a newer version may have existed there and been destroyed. An **Unconfirmed** winner is the newest version that can still be read.

### Anomalies

When both slots are valid with different generations, two flags are also computed:

- **generation gap**: the generations are not consecutive (an update was lost, or a slot was restored from elsewhere);
- **chain break**: they are consecutive, but the newer slot's `prev_header_crc32c` is not the older slot's `header_crc32c`.

Neither flag changes the winner.

### The next write

| Winner | Slot the next update writes | Its generation | Its `prev_header_crc32c` |
|---|---|---|---|
| Empty | A | 1 | 0 |
| Confirmed or Unconfirmed in slot *i* | the other slot | winner's generation + 1 (an overflow refuses the write) | winner's `header_crc32c` |
| Fork, Refused, Lost | none: the pair must not be written | &mdash; | &mdash; |

The slot holding the current version is never overwritten.

### How store-io uses the winner

| Pair | Confirmed / Unconfirmed | Empty or Lost | Fork | Refused |
|---|---|---|---|---|
| Volume record | used | try the next block size (section F10) | try the next block size | open fails: `Unsupported(FormatVersion)` |
| Region table | used | `Corruption(HeaderCrc)` (`Corruption(MediaError)` when a read failed on the media) | `Corruption(Fork)` | `Unsupported(FormatVersion)` |
| Region header | used | region not presented | region not presented | region not presented |

store-io currently treats an Unconfirmed winner exactly like a Confirmed one, and does not act on or report the generation-gap and chain-break flags.

Source: [`crates/store-io-format/src/pair.rs`](../crates/store-io-format/src/pair.rs), [`crates/store-io-engine/src/slots.rs`](../crates/store-io-engine/src/slots.rs) (`plan_next`), [`crates/store-io-engine/src/open.rs`](../crates/store-io-engine/src/open.rs) (`winner_payload`, `find_volume`, `open_inner`)

---

## F6. The volume record (object 0)

The volume record is the payload of the pair at offsets `0` and `B`, with slot size B. Its payload is 48 bytes:

| Offset | Size | Type | Field | Meaning |
|---|---|---|---|---|
| 0 | 1 | u8 | `log2_block_size` | `log2(B)`, 12&ndash;16 |
| 1 | 1 | u8 | `log2_table_slot_size` | `log2(T)`, from `log2_block_size` to 16 |
| 2 | 2 | u16 | `device_index` | `0` (single-device store) |
| 4 | 4 | &mdash; | reserved | must be `0` |
| 8 | 8 | u64 | `owner_generation` | `1` at creation; one more on every writable open |
| 16 | 8 | u64 | `table_offset` | Offset of region-table slot A; always `2B` |
| 24 | 8 | u64 | `created_unix_s` | Creation time, seconds since the Unix epoch; informative only |
| 32 | 8 | u64 | `features` | must be `0` (no feature is defined) |
| 40 | 8 | &mdash; | reserved | must be `0` |

Decoding fails (`Length`) if the payload is shorter than 48 bytes; the codec ignores bytes beyond 48, and open refuses them (below). Field checks, in order: `features` is zero (checked first, so a newer record is recognised as newer), the reserved `u32` at 4 and the reserved bytes at `[40, 48)` are zero, `log2_block_size` is 12&ndash;16, `log2_table_slot_size` is between `log2_block_size` and 16, `table_offset` is a multiple of B. At open, a payload longer than 48 bytes or a non-zero `features` is a newer format: the open is refused with `Unsupported(FormatVersion)` and nothing is written. At open, a record whose `log2_block_size` differs from the slot size it was found at is skipped, and a `table_offset` other than `2B` fails the open (section F10).

`owner_generation` is forensic: every slot written records the writer's owner generation, so a reader can tell which open wrote what. It is not a fence; the ownership lock is.

Source: [`crates/store-io-format/src/meta.rs`](../crates/store-io-format/src/meta.rs) (`VolumeMeta`), [`crates/store-io-engine/src/create.rs`](../crates/store-io-engine/src/create.rs) (`build_new`), [`crates/store-io-engine/src/open.rs`](../crates/store-io-engine/src/open.rs) (`find_volume`, `open_inner`)

---

## F7. The region table (object 1)

The region table is the payload of the pair at `2B` and `2B + T`, with slot size T. It is the only authority on which regions exist: a region whose header exists but which has no table entry does not exist.

### Table header (16 bytes)

| Offset | Size | Type | Field | Meaning |
|---|---|---|---|---|
| 0 | 4 | u32 | `next_region_id` | The id the next region will get. Every entry's id is below it. |
| 4 | 4 | u32 | `count` | Number of entries that follow |
| 8 | 8 | &mdash; | reserved | must be `0` |

Entries follow at offset `16 + 64 * i`. The payload is `16 + 64 * count` bytes. A new container's table is the 16-byte header with `next_region_id = 0` and `count = 0`. Region ids start at 0, increase by one per provisioned region, and are never reused.

### Table entry (64 bytes)

| Offset | Size | Type | Field | Meaning |
|---|---|---|---|---|
| 0 | 4 | u32 | `region_id` | The region's id |
| 4 | 1 | u8 | `kind` | See below |
| 5 | 1 | u8 | `state` | See below |
| 6 | 1 | u8 | `fill` | Fill pattern used at provisioning (see below) |
| 7 | 1 | &mdash; | reserved | must be `0` |
| 8 | 24 | bytes | `name` | The region name, padded with zeros |
| 32 | 8 | u64 | `offset` | Absolute offset of the region's header slot A (the start of its extent) |
| 40 | 8 | u64 | `data_size` | Bytes of data area after the two header blocks; `0` for slot regions |
| 48 | 4 | u32 | `tag` | The caller's space-accounting tag; `0` when provisioned without a reservation |
| 52 | 12 | &mdash; | reserved | must be `0` |

| `kind` | Meaning |
|---|---|
| `1` | Append region: store-io chooses positions from a tail; blocks are never rewritten. |
| `2` | Page region: the caller chooses positions and overwrites in place. |
| `3` | Slot region: a small object kept in the region's own header pair (section F8). |

| `state` | Meaning |
|---|---|
| `1` | Ready |
| `2` | Released: the extent is free for reuse by a region of exactly the same extent length (section F11). |

| `fill` | Meaning |
|---|---|
| `0` | Zeros |
| `1` | Keyed pattern, version 1 (section F9) |

Any other `kind`, `state` or `fill` value is a decoding error.

### Names

A name is 1 to 24 bytes of UTF-8 with no NUL byte, stored in the 24-byte field and padded with zeros. When decoding, the name ends at the first zero byte; it must be at least one byte long, every byte after it must be zero, and the bytes before it must be valid UTF-8.

### Validation

A table decodes only if all of the following hold, checked in this order; the first failure is the error:

1. The payload holds the 16-byte header, the reserved bytes are zero, and `16 + 64 * count` bytes are present (`Length`, `Field("reserved")`). Trailing bytes are ignored.
2. For each entry in order:
   1. it decodes (valid `kind`, `state`, `fill`, name; zero reserved bytes);
   2. `region_id < next_region_id`;
   3. `offset` and `data_size` are multiples of B (`Unaligned`);
   4. a slot region has `data_size == 0`;
   5. `offset + 2B + data_size` does not overflow (`Overlap`);
   6. against every earlier entry: a different `region_id` and, if both are Ready, a different name (`Duplicate`); and no overlap of the two extents `[offset, offset + 2B + data_size)` (`Overlap`). Released entries take part in the id and overlap checks, but may share a name with any other entry.

A table that fails to decode makes the open fail with `Corruption(Metadata)`.

Source: [`crates/store-io-format/src/meta.rs`](../crates/store-io-format/src/meta.rs) (`encode_table`, `decode_table`, `decode_entry`, `RegionName`)

---

## F8. Region headers and slot objects

Each region's first two blocks are its header pair, with slot size B and object id `2^32 + region_id`. The header describes the region; its slot **generation is the region's generation**.

### Header payload (48 bytes, then a slot object for slot regions)

| Offset | Size | Type | Field | Meaning |
|---|---|---|---|---|
| 0 | 4 | u32 | `region_id` | Must match the table entry |
| 4 | 1 | u8 | `kind` | As in the table |
| 5 | 1 | u8 | `state` | `1` Ready or `2` Released |
| 6 | 1 | u8 | `fill` | As in the table |
| 7 | 1 | &mdash; | reserved | must be `0` |
| 8 | 8 | u64 | `data_offset` | Absolute offset of the data area: the entry's `offset + 2B` |
| 16 | 8 | u64 | `data_size` | As in the table |
| 24 | 16 | bytes | `fill_key` | Key of the keyed fill pattern; zero for `fill = 0` |
| 40 | 8 | &mdash; | reserved | must be `0` |
| 48 | n | bytes | object | Slot regions only, from generation 2 on: the caller's object |

Decoding needs at least 48 bytes and checks the `kind`, `state` and `fill` values and both reserved fields. Bytes past 48 are not part of the record.

### Generations of a region header

| Generation | Written by | Contents |
|---|---|---|
| 1 | Provisioning | State Ready, in slot A |
| *n* + 1 | Recycle | The same record with state Ready: the region's next generation |
| *n* + 1 | Release | The same record with state Released |
| *n* + 1 | Slot commit (slot regions) | The same 48-byte record followed by the caller's object |

### Slot objects

A slot region (`kind = 3`) has no data area. Its object lives in its header pair: generation 1 is the provisioning header with no object, and every commit writes the next generation with the 48-byte record followed by the object. The current object is therefore the bytes after offset 48 of the winning header's payload, or none if the winner's generation is 1. The largest object is `B - 128 - 48` bytes: 3920 bytes with 4 KiB blocks. Slot regions are never recycled or released.

Source: [`crates/store-io-format/src/meta.rs`](../crates/store-io-format/src/meta.rs) (`RegionMeta`), [`crates/store-io-engine/src/slot_handle.rs`](../crates/store-io-engine/src/slot_handle.rs) (`Slot`), [`crates/store-io-engine/src/lifecycle.rs`](../crates/store-io-engine/src/lifecycle.rs)

---

## F9. The keyed fill pattern

On thin, virtual, deduplicating, compressing or zero-detecting storage, writing zeros may never really allocate space. Fill pattern 1 (`KeyedV1`) makes every 8-byte word of a data area unique and incompressible.

Let the 16-byte `fill_key` be split into `k0` (bytes 0&ndash;7) and `k1` (bytes 8&ndash;15), each read as a little-endian `u64`, and let `GAMMA = 0x9E3779B97F4A7C15`. Word `j` of the data area, at byte offset `8j` from `data_offset`, is stored little-endian as:

```text
mix(z) = z1 ^ (z1 >> 31)
         where z0 = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9
               z1 = (z0 ^ (z0 >> 27)) * 0x94D049BB133111EB

word(j) = mix(k0 + (j + 1) * GAMMA) ^ k1
```

All arithmetic wraps modulo 2^64. With an all-zero key, words 0 and 1 are `0xE220A8397B1DCDAF` and `0x6E789E6AA1B965F4`. The definition of version 1 never changes; a new pattern gets a new `fill` value.

The fill pattern describes what provisioning *writes*; the format makes no promise about what a data area holds afterwards. Recycling keeps the old bytes, releasing makes them undefined, and the caller's own records decide where valid data ends.

The two header blocks of an extent are always written as zeros; the data area carries the pattern from its first byte, whatever chunk size the writer uses.

Source: [`crates/store-io-format/src/fill.rs`](../crates/store-io-format/src/fill.rs), [`crates/store-io-engine/src/provision.rs`](../crates/store-io-engine/src/provision.rs) (`provision`, `fill`)

---

## F10. Reading a container

This is how store-io opens a container. An independent reader that follows it reads exactly what store-io reads.

1. **Find the volume record.** B is not known yet. For each `log2` from 12 to 16, and for each candidate offset `0` and `2^log2`:
   1. read 4096 bytes there; skip unless the read is complete, bytes 0&ndash;7 are the magic, byte 78 equals `log2` and the slot header's CRC verifies;
   2. take the volume id from bytes 32&ndash;47 (unverified so far);
   3. read the pair at `0` and `2^log2` with slot size `2^log2`, object 0 and that volume id, and choose the winner (section F5);
   4. if the pair is Refused, stop: the container needs a newer store-io. If there is no Confirmed or Unconfirmed winner, try the next candidate;
   5. decode the winner's payload as a volume record; a decoding failure is `Corruption(Metadata)`;
   6. if `log2_block_size` is not `log2`, try the next candidate; if `table_offset` is not `2 * 2^log2`, the open fails with `Corruption(Metadata)`.

   If no candidate succeeds, the open fails with `Corruption(HeaderCrc)`, or `Corruption(MediaError)` when a read failed on the media.
2. **Read the region table** from the pair at `2B` and `2B + T` (slot size T, object 1), and decode and validate it (section F7).
3. **Read each Ready entry's header** from its pair at `offset` and `offset + B`. The region is presented only if all of these hold; otherwise it is reported as unavailable with its reason (see "Which regions an open uses"), and its table entry, its name and its space stay:
   - its extent ends within the file;
   - the pair has a Confirmed or Unconfirmed winner, and its payload decodes as a region header;
   - the header's `region_id` and `kind` match the entry, and its state is Ready.

   The region's data area starts at the entry's `offset + 2B` and is `data_size` bytes long; its generation is the winning header slot's generation.
4. Released entries are not read.

A **writable** open also does the following, in this order: it takes the ownership lock; refuses an unsafe or unverified device unless trusted; flushes the whole file (data and metadata) *before* reading any slot; reads as above; and finally writes the volume record's next generation with `owner_generation` one higher, durably. A **read-only** open writes nothing. (That is the writable open of 0.1 to 0.4. store-io 0.5 refuses a writable open of a format-1 container with `Unsupported(FormatVersion)` once step 1 has found the record, after the flush and before anything is written.)

Source: [`crates/store-io-engine/src/open.rs`](../crates/store-io-engine/src/open.rs) (`open_inner`, `find_volume`)

---

## F11. Region placement and space reuse

A region asked for `size` bytes gets `data_size = size` rounded up to a multiple of B (0 for a slot region), and an extent of `2B + data_size` bytes. The data area may hold at most `2^32 - 2` blocks.

Placement uses the region table only:

1. **Reuse.** The first Released entry, in table order, whose extent length equals the new extent length exactly, and whose extent overlaps no Ready entry, is reused: the new region goes at its `offset`, and the new entry *replaces* the released entry in the table at the same index.
2. **Append.** Otherwise the extent goes at the **tail**: the end of the furthest extent in the table (Ready or Released), or `2B + 2T` for an empty table. A region with at least 1 MiB of data is placed so that its data area starts on a 1 MiB boundary (its two header blocks just before it); the gap between the tail and the header stays unused. The new entry is appended to the table.

The file is then grown, if needed, to the end of the new extent plus every byte reserved and not yet provisioned. A region provisioned from a reservation is charged its extent only: its alignment gap comes from growing the file, and when that growth finds no space the region goes at the tail unaligned instead. Readers find every region by its table entry and must not assume either placement. Released extents of other lengths are neither reused nor merged, the tail never moves back, and the file never shrinks.

A Released entry stays in the table until a region of the same extent length reuses it, so it continues to count against the table's capacity.

Source: [`crates/store-io-engine/src/layout.rs`](../crates/store-io-engine/src/layout.rs) (`place`, `tail`), [`crates/store-io-engine/src/provision.rs`](../crates/store-io-engine/src/provision.rs) (`provision`), [`crates/store-io-engine/src/space.rs`](../crates/store-io-engine/src/space.rs)

---

## F12. Versioning and feature flags

| Mechanism | Where | Rule |
|---|---|---|
| `format_major` | Every slot, offset 14 | `1` in this document. 0.4 and earlier read and write only `1`: any other value refuses the pair (the open fails with `Unsupported(FormatVersion)` for the volume record and the region table, and the region is not presented for a region header). 0.5 writes `2`. In a format-1 container it refuses any value other than `1` the same way (a format-2 slot included), in a format-2 container any value other than `2` (section 5), and it opens a format-1 container for reading only (a writable open is `Unsupported(FormatVersion)`, before anything is written). |
| `incompat_flags` | Every slot, offset 16 | No bit is defined. Any set bit refuses the pair, as above. |
| `ro_compat_flags` | Every slot, offset 20 | No bit is defined. A slot with an unknown bit stays readable, and the pair must not be written by a version that does not know the bit. |
| `compat_flags` | Every slot, offset 24 | No bit is defined. Unknown bits are ignored. |
| `header_len` | Every slot, offset 12 | 128 in format 1. Readers accept a longer header up to the slot size, cover it with the header CRC, and start the payload after it, so later formats can add header fields. |
| `features` | Volume record, offset 32 | Must be `0`. A non-zero value fails decoding, and the open treats it as a newer format: `Unsupported(FormatVersion)` (section F6). |
| `kind`, `state`, `fill` | Table entries, region headers | Undefined values fail decoding. A new fill pattern gets a new `fill` value; version 1 never changes. |

store-io writes `incompat_flags`, `ro_compat_flags` and `compat_flags` as zero. Before writing a pair it checks the current winner's `ro_compat_flags`: a bit it does not know makes the write fail with `Unsupported(FormatVersion)`, so a pair written by a newer store-io is never overwritten by an older one.

Source: [`crates/store-io-format/src/slot.rs`](../crates/store-io-format/src/slot.rs) (`KNOWN_INCOMPAT`, `KNOWN_RO_COMPAT`, `SlotInfo::writable`), [`crates/store-io-format/src/meta.rs`](../crates/store-io-format/src/meta.rs), [`crates/store-io-engine/src/slots.rs`](../crates/store-io-engine/src/slots.rs) (`plan_next`)

---

## F13. Crash consistency of each change

"Durably" below means: on a power-safe device, a write with durability at completion; on every other class, a plain write followed by a device flush through the flush domain ([Durability](./DURABILITY.md)). A **full flush** is `fsync` on Linux and `FlushFileBuffers` on Windows ([Platforms](./PLATFORMS.md)).

### A/B slot update (every metadata change)

1. Choose the next slot from the current winner (section F5): never the slot holding the current version.
2. Write the whole slot as one I/O, then make it durable.

A crash before step 2 completes leaves the current version untouched in its slot. The slot being written is old (unchanged), new (complete), or damaged: torn writes fail the header or payload CRC, and an all-zero header reads as blank. The format's own tests tear a slot overwrite at every byte boundary and flip every single bit of a slot; none is ever accepted as a mixed or altered version. After the crash, the winner is either the old version (Confirmed, since a torn or blank slot is explained) or the new one. Updates to one pair are serialised, so two writers never race for the free slot; two valid slots with the same generation (a fork) are refused rather than guessed.

**Why a writable open flushes first.** A process can die after writing the free slot but before its flush. The write is then visible to reads but not yet durable. Without a flush, the next owner would read it as the winner, write its own update over the *older* slot, the only durable copy, and a power cut could then lose both. A writable open therefore makes everything the previous owner wrote durable before it reads any slot.

### Create

1. Create the directory if missing (and flush its parent), then create `store.sio` exclusively and take the lock.
2. Probe the device; refuse it unless trusted.
3. Grow the file to `2B + 2T`.
4. Write volume slot A (generation 1) and table slot A (generation 1, empty table).
5. Full flush. The in-memory state of both pairs is what was written; nothing is read back.
6. Flush the directory.

A failure in steps 2&ndash;5 removes the file again. A *crash* before step 6 completes may leave no file, or a file whose volume slots are blank or incomplete. Such a file does not open, but the next `create` takes it over. When `store.sio` already exists, `create` reuses it only if all of these hold:

- no one holds its lock;
- it is no longer than the largest metadata area (`4 × 64 KiB`);
- every 4 KiB block of it reads back in full, and no block starts with a slot header whose header CRC verifies (whatever the header's version, volume or object).

A create that crashed leaves only blank blocks or torn headers, so it passes this test, and the file is then truncated and created afresh. Anything else is treated as a store, and `create` fails with `AlreadyExists` without touching it. That includes a file that could not be read in full, a damaged store, and a store written by a newer format. Even a small file can hold a real store: with 4 KiB blocks the metadata area is about 40 KiB, and slots and small regions fit in the rest.

### Writable open

The only write is the volume record's next generation (owner generation + 1), an A/B update. A crash leaves the old or the new owner generation.

### Which regions an open uses

The region table decides which regions exist. A ready table entry is used only if both of these hold:

- the container is long enough to hold it;
- its header pair has a valid winner whose record names the same region id and kind, in the Ready state, with a valid generation.

Any other entry is reported as unavailable, with the reason, in `Store::unavailable_regions()` and the store report. A lookup of its name returns `Corruption` naming the region, never `NotFound`. The name stays reserved.

A header that no ready table entry names is never used: an interrupted release can leave one in released space, and the next provisioning there blanks it.

### Provision

1. Choose the placement (section F11) and grow the file if needed.
2. Fill the whole extent with direct writes: the two header blocks with zeros, the data area with zeros or the keyed pattern. This blanks any older header at that place. A fill write that fails or transfers fewer bytes after it was submitted poisons the store (`DurabilityUnknown`); running out of space is `NoSpace` and poisons nothing.
3. Full flush (data and the new file length).
4. Verify the extent. The region is refused (`NotWritten(NotReady)`) if the platform reports any part of it unwritten or shared, any of its pages in the operating system's page cache, or (Windows) any part of it above the valid data length. A fact the platform cannot read does not refuse the region.
5. Write header slot A, generation 1, state Ready, durably.
6. Write the region table's next generation with the new entry (appended, or replacing the reused released entry), durably.

The table flip in step 6 is the commit point: the region exists only once it is durable.

| Crash after | State after reopen |
|---|---|
| step 1&ndash;4 | No table entry. The bytes past the tail, or the released extent that was being reused, are not part of any region; the next provisioning places there again and refills it. |
| step 5 | Same, plus a header naming a region id that is in no table entry. It is never read (the table is the authority) and the next provisioning at that place overwrites it. The id is assigned again, since `next_region_id` was not committed. |
| step 6 | The region exists, filled and verified, with generation 1. |

Released bytes never reappear inside a new region, because the fill in step 2 covers the whole extent and is durable before the region is committed.

### Slot commit (slot regions)

An A/B update of the region's header pair, carrying the 48-byte header record and the new object. After a crash, `read()` returns exactly the old object or exactly the new one.

### Recycle

1. Stop admitting operations on the region and wait for every one in flight.
2. Write the header's next generation (state Ready) durably.

No other byte is written: the data area keeps the old generation's bytes. A crash leaves the old or the new generation in the header, and records written by the caller carry the generation to tell them apart.

### Release

1. Stop admitting operations on the region and wait for every one in flight.
2. Write the region table's next generation with the entry's state set to Released, durably.
3. Write the region header's next generation with state Released, durably.
4. Release the data area only: the header blocks keep the Released record. Linux punches a hole; Windows trims in place (`FSCTL_FILE_LEVEL_TRIM`), keeping the file's allocation and never making it sparse.

The table goes first because it is the only authority on open:

| Crash after | State after reopen |
|---|---|
| step 1 | The region is still Ready and intact. |
| step 2 or 3 | The entry is Released and the header (Ready or Released) is ignored. The space is not deallocated yet, but it is reusable: provisioning refills it before reuse. |
| step 4 | Released; the extent's contents are undefined. |

The opposite order could leave a Ready entry whose region can no longer be opened, leaking its space. If deallocation fails, the region is released anyway and the call reports the I/O error.

Source: [`crates/store-io-engine/src/create.rs`](../crates/store-io-engine/src/create.rs) (`create`, `build_new`, `reclaim_abandoned`), [`crates/store-io-engine/src/open.rs`](../crates/store-io-engine/src/open.rs) (`open_inner`), [`crates/store-io-engine/src/provision.rs`](../crates/store-io-engine/src/provision.rs) (`provision`, `fill`, `verify_ready`, `flip_table`), [`crates/store-io-engine/src/store.rs`](../crates/store-io-engine/src/store.rs) (`durable_slot`), [`crates/store-io-engine/src/lifecycle.rs`](../crates/store-io-engine/src/lifecycle.rs), [`crates/store-io-engine/src/slots.rs`](../crates/store-io-engine/src/slots.rs), [`crates/store-io-format/src/slot.rs`](../crates/store-io-format/src/slot.rs) (tests)

---

## F14. What store-io does not check

A reader that wants to be stricter than store-io can add these checks; store-io as it is now does not make them.

- The bytes of a slot after its payload, and header bytes in `[128, header_len)` (they are covered by the header CRC but not interpreted).
- The volume record's `device_index`.
- That table entries lie at or after `2B + 2T`, or within the file (an entry whose extent ends past the end of the file is skipped at open, not rejected).
- That a region header's `data_offset`, `data_size` and `fill` agree with its table entry; only `region_id`, `kind` and the Ready state are compared.
- `owner_generation` in slot headers: it is recorded, never used to accept or refuse a slot.
- The generation-gap and chain-break anomalies of a pair (computed, not acted on).

---

<div align="center"><sub>Apache-2.0 OR MIT &middot; &copy; 2026 James Gober</sub></div>
