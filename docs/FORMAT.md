# store-io &mdash; Format

> The on-disk format of a store's container file, `store.sio`, byte by byte: the layout, the A/B slot every piece of metadata is written with, the three metadata records, the keyed fill pattern, every validation rule, and why each multi-step change survives a crash. It is written so that an independent reader can be built from it. Every statement is taken from the code, and each section names its source.
>
> **Status: pre-1.0.** This is on-disk format 1 as store-io writes it today. The format may change incompatibly before the first stable release, and a container written by one 0.x release is not promised to open in another.

---

## Contents

1. [Conventions](#1-conventions)
2. [Container layout](#2-container-layout)
3. [The slot](#3-the-slot)
4. [Validating a slot](#4-validating-a-slot)
5. [Choosing the winner of a pair](#5-choosing-the-winner-of-a-pair)
6. [The volume record (object 0)](#6-the-volume-record-object-0)
7. [The region table (object 1)](#7-the-region-table-object-1)
8. [Region headers and slot objects](#8-region-headers-and-slot-objects)
9. [The keyed fill pattern](#9-the-keyed-fill-pattern)
10. [Reading a container](#10-reading-a-container)
11. [Region placement and space reuse](#11-region-placement-and-space-reuse)
12. [Versioning and feature flags](#12-versioning-and-feature-flags)
13. [Crash consistency of each change](#13-crash-consistency-of-each-change)
14. [What store-io does not check](#14-what-store-io-does-not-check)

---

## 1. Conventions

- **Byte order.** Every integer is little-endian. `u8`, `u16`, `u32` and `u64` are unsigned.
- **Offsets.** Offsets in the layout tables are relative to the start of the structure being described. Offsets stored *in* the format (`slot_offset`, `table_offset`, a region's `offset` and `data_offset`) are absolute byte offsets in the container file.
- **Reserved bytes** are written as zero. Section 14 lists which ones a reader checks.
- **Checksum.** CRC-32C (Castagnoli): reflected polynomial `0x82F63B78` (normal form `0x1EDC6F41`), initial value `0xFFFFFFFF`, final XOR `0xFFFFFFFF`, reflected input and output. The CRC of the ASCII bytes `123456789` is `0xE3069283`, and the CRC of empty input is `0`. A stored CRC is a `u32`, little-endian.
- **B** is the container's block size and **T** its region-table slot size. Both are powers of two (section 2).

Source: [`crates/store-io-format/src/codec.rs`](../crates/store-io-format/src/codec.rs), [`crates/store-io-format/src/crc32c.rs`](../crates/store-io-format/src/crc32c.rs)

---

## 2. Container layout

A store is a directory that holds one container file, `store.sio`. Everything store-io keeps for that store is inside this file.

### Geometry

The geometry is fixed when the store is created and recorded in the volume record (section 6). Later opens read it from there and ignore the size options.

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

Source: [`crates/store-io-engine/src/layout.rs`](../crates/store-io-engine/src/layout.rs), [`crates/store-io-engine/src/store.rs`](../crates/store-io-engine/src/store.rs) (`build_new`, `pair_volume`, `pair_table`, `pair_region`)

---

## 3. The slot

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
| 48 | 8 | u64 | `object_id` | The object this pair stores (section 2) |
| 56 | 8 | u64 | `generation` | At least 1. Each committed update writes the winner's generation + 1. |
| 64 | 8 | u64 | `slot_offset` | Absolute file offset this slot was written to |
| 72 | 4 | u32 | `payload_len` | Payload length in bytes, at most `slot size - header_len` |
| 76 | 1 | u8 | `slot_index` | `0` = A, `1` = B |
| 77 | 1 | u8 | `slot_count` | `2` |
| 78 | 1 | u8 | `log2_slot_size` | Slot size as a power of two (9&ndash;16) |
| 79 | 1 | &mdash; | reserved | `0` |
| 80 | 4 | u32 | `prev_header_crc32c` | `header_crc32c` of the version this one replaced; `0` for generation 1 |
| 84 | 4 | &mdash; | reserved | `0` |
| 88 | 8 | u64 | `owner_generation` | The volume record's owner generation when the slot was written (section 6) |
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

The writer fills the whole slot with zeros, copies in the payload, writes the header with `header_crc32c` as zero, computes the header CRC over `[0, 128)`, and stores it at offset 8. Generation 0 is never written. The new slot's `generation`, `prev_header_crc32c` and `slot_index` come from the pair's current state (section 5).

Source: [`crates/store-io-format/src/slot.rs`](../crates/store-io-format/src/slot.rs) (`encode`, `write_header`)

---

## 4. Validating a slot

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

A valid slot whose `ro_compat_flags` has an unknown bit is still `Valid`; such a slot is readable but must not be overwritten with a newer generation (see section 12 for what store-io does today).

Source: [`crates/store-io-format/src/slot.rs`](../crates/store-io-format/src/slot.rs) (`validate`, `parse`), [`crates/store-io-engine/src/slots.rs`](../crates/store-io-engine/src/slots.rs) (`read_one`)

---

## 5. Choosing the winner of a pair

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
| Volume record | used | try the next block size (section 10) | try the next block size | open fails: `Unsupported(FormatVersion)` |
| Region table | used | `Corruption(HeaderCrc)` | `Corruption(Fork)` | `Unsupported(FormatVersion)` |
| Region header | used | region not presented | region not presented | region not presented |

store-io currently treats an Unconfirmed winner exactly like a Confirmed one, and does not act on or report the generation-gap and chain-break flags.

Source: [`crates/store-io-format/src/pair.rs`](../crates/store-io-format/src/pair.rs), [`crates/store-io-engine/src/slots.rs`](../crates/store-io-engine/src/slots.rs) (`plan_next`), [`crates/store-io-engine/src/store.rs`](../crates/store-io-engine/src/store.rs) (`winner_payload`, `find_volume`, `open_inner`)

---

## 6. The volume record (object 0)

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
| 40 | 8 | &mdash; | reserved | `0` |

Decoding fails (`Length`) if the payload is shorter than 48 bytes; bytes beyond 48 are ignored. Field checks, in order: the reserved `u32` at 4 is zero, `log2_block_size` is 12&ndash;16, `log2_table_slot_size` is between `log2_block_size` and 16, `table_offset` is a multiple of B, `features` is zero. At open, a record whose `log2_block_size` differs from the slot size it was found at is skipped, and a `table_offset` other than `2B` fails the open (section 10).

`owner_generation` is forensic: every slot written records the writer's owner generation, so a reader can tell which open wrote what. It is not a fence; the ownership lock is.

Source: [`crates/store-io-format/src/meta.rs`](../crates/store-io-format/src/meta.rs) (`VolumeMeta`), [`crates/store-io-engine/src/store.rs`](../crates/store-io-engine/src/store.rs) (`build_new`, `find_volume`, `open_inner`)

---

## 7. The region table (object 1)

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
| `3` | Slot region: a small object kept in the region's own header pair (section 8). |

| `state` | Meaning |
|---|---|
| `1` | Ready |
| `2` | Released: the extent is free for reuse by a region of exactly the same extent length (section 11). |

| `fill` | Meaning |
|---|---|
| `0` | Zeros |
| `1` | Keyed pattern, version 1 (section 9) |

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

## 8. Region headers and slot objects

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

Source: [`crates/store-io-format/src/meta.rs`](../crates/store-io-format/src/meta.rs) (`RegionMeta`), [`crates/store-io-engine/src/region.rs`](../crates/store-io-engine/src/region.rs) (`Slot`), [`crates/store-io-engine/src/lifecycle.rs`](../crates/store-io-engine/src/lifecycle.rs)

---

## 9. The keyed fill pattern

On thin, virtual, deduplicating, compressing or zero-detecting storage, writing zeros may never really allocate space. Fill pattern 1 (`KeyedV1`) makes every 8-byte word of a data area unique and incompressible.

Let the 16-byte `fill_key` be split into `k0` (bytes 0&ndash;7) and `k1` (bytes 8&ndash;15), each read as a little-endian `u64`, and let `GAMMA = 0x9E3779B97F4A7C15`. Word `j` of the data area, at byte offset `8j` from `data_offset`, is stored little-endian as:

```text
mix(z) = z1 ^ (z1 >> 31)
         where z0 = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9
               z1 = (z0 ^ (z0 >> 27)) * 0x94D049BB133111EB

word(j) = mix(k0 + (j + 1) * GAMMA) ^ k1
```

All arithmetic wraps modulo 2^64. With an all-zero key, words 0 and 1 are `0xE220A8397B1DCDAF` and `0x6E789E6AA1B965F4`. The definition of version 1 never changes; a new pattern gets a new `fill` value.

The fill pattern describes what provisioning *writes*; the format makes no promise about what a data area holds afterwards. Recycling keeps the old bytes, releasing makes them undefined, and the caller's own records decide where valid data ends. The two header blocks are always filled with zeros, whatever the pattern.

The two header blocks of an extent are always written as zeros; the data area carries the pattern from its first byte, whatever chunk size the writer uses.

Source: [`crates/store-io-format/src/fill.rs`](../crates/store-io-format/src/fill.rs), [`crates/store-io-engine/src/store.rs`](../crates/store-io-engine/src/store.rs) (`provision`, `fill`)

---

## 10. Reading a container

This is how store-io opens a container. An independent reader that follows it reads exactly what store-io reads.

1. **Find the volume record.** B is not known yet. For each `log2` from 12 to 16, and for each candidate offset `0` and `2^log2`:
   1. read 4096 bytes there; skip unless the read is complete, bytes 0&ndash;7 are the magic and byte 78 equals `log2`;
   2. take the volume id from bytes 32&ndash;47 (unverified so far);
   3. read the pair at `0` and `2^log2` with slot size `2^log2`, object 0 and that volume id, and choose the winner (section 5);
   4. if the pair is Refused, stop: the container needs a newer store-io. If there is no Confirmed or Unconfirmed winner, try the next candidate;
   5. decode the winner's payload as a volume record; a decoding failure is `Corruption(Metadata)`;
   6. if `log2_block_size` is not `log2`, try the next candidate; if `table_offset` is not `2 * 2^log2`, the open fails with `Corruption(Metadata)`.

   If no candidate succeeds, the open fails with `Corruption(HeaderCrc)`.
2. **Read the region table** from the pair at `2B` and `2B + T` (slot size T, object 1), and decode and validate it (section 7).
3. **Read each Ready entry's header** from its pair at `offset` and `offset + B`. The region is presented only if all of these hold; otherwise it is silently left out, while its table entry, and so its space, stays:
   - its extent ends within the file;
   - the pair has a Confirmed or Unconfirmed winner, and its payload decodes as a region header;
   - the header's `region_id` and `kind` match the entry, and its state is Ready.

   The region's data area starts at the entry's `offset + 2B` and is `data_size` bytes long; its generation is the winning header slot's generation.
4. Released entries are not read.

A **writable** open also does the following, in this order: it takes the ownership lock; refuses an unsafe or unverified device unless trusted; flushes the whole file (data and metadata) *before* reading any slot; reads as above; and finally writes the volume record's next generation with `owner_generation` one higher, durably. A **read-only** open writes nothing.

Source: [`crates/store-io-engine/src/store.rs`](../crates/store-io-engine/src/store.rs) (`open_inner`, `find_volume`)

---

## 11. Region placement and space reuse

A region asked for `size` bytes gets `data_size = size` rounded up to a multiple of B (0 for a slot region), and an extent of `2B + data_size` bytes. The data area may hold at most `2^32 - 2` blocks.

Placement uses the region table only:

1. **Reuse.** The first Released entry, in table order, whose extent length equals the new extent length exactly, and whose extent overlaps no Ready entry, is reused: the new region goes at its `offset`, and the new entry *replaces* the released entry in the table at the same index.
2. **Append.** Otherwise the extent goes at the **tail**: the end of the furthest extent in the table (Ready or Released), or `2B + 2T` for an empty table. The new entry is appended to the table.

The file is then grown, if needed, to the end of the new extent plus every byte reserved and not yet provisioned. Released extents of other lengths are neither reused nor merged, the tail never moves back, and the file never shrinks.

A Released entry stays in the table until a region of the same extent length reuses it, so it continues to count against the table's capacity.

Source: [`crates/store-io-engine/src/layout.rs`](../crates/store-io-engine/src/layout.rs) (`place`, `tail`), [`crates/store-io-engine/src/store.rs`](../crates/store-io-engine/src/store.rs) (`provision`), [`crates/store-io-engine/src/space.rs`](../crates/store-io-engine/src/space.rs)

---

## 12. Versioning and feature flags

| Mechanism | Where | Rule |
|---|---|---|
| `format_major` | Every slot, offset 14 | Only `1` is read or written. Any other value refuses the pair: the open fails with `Unsupported(FormatVersion)` for the volume record and the region table, and the region is not presented for a region header. |
| `incompat_flags` | Every slot, offset 16 | No bit is defined. Any set bit refuses the pair, as above. |
| `ro_compat_flags` | Every slot, offset 20 | No bit is defined. A slot with an unknown bit stays readable, and the pair must not be written by a version that does not know the bit. |
| `compat_flags` | Every slot, offset 24 | No bit is defined. Unknown bits are ignored. |
| `header_len` | Every slot, offset 12 | 128 in format 1. Readers accept a longer header up to the slot size, cover it with the header CRC, and start the payload after it, so later formats can add header fields. |
| `features` | Volume record, offset 32 | Must be `0`. A non-zero value fails decoding, and the open reports `Corruption(Metadata)`, not a version refusal. |
| `kind`, `state`, `fill` | Table entries, region headers | Undefined values fail decoding. A new fill pattern gets a new `fill` value; version 1 never changes. |

store-io writes `incompat_flags`, `ro_compat_flags` and `compat_flags` as zero. Before writing a pair it checks the current winner's `ro_compat_flags`: a bit it does not know makes the write fail with `Unsupported(FormatVersion)`, so a pair written by a newer store-io is never overwritten by an older one.

Source: [`crates/store-io-format/src/slot.rs`](../crates/store-io-format/src/slot.rs) (`KNOWN_INCOMPAT`, `KNOWN_RO_COMPAT`, `SlotInfo::writable`), [`crates/store-io-format/src/meta.rs`](../crates/store-io-format/src/meta.rs), [`crates/store-io-engine/src/slots.rs`](../crates/store-io-engine/src/slots.rs) (`plan_next`)

---

## 13. Crash consistency of each change

"Durably" below means: on a power-safe device, a write with durability at completion; on every other class, a plain write followed by a device flush through the flush domain ([Durability](./DURABILITY.md)). A **full flush** is `fsync` on Linux and `FlushFileBuffers` on Windows ([Platforms](./PLATFORMS.md)).

### A/B slot update (every metadata change)

1. Choose the next slot from the current winner (section 5): never the slot holding the current version.
2. Write the whole slot as one I/O, then make it durable.

A crash before step 2 completes leaves the current version untouched in its slot. The slot being written is old (unchanged), new (complete), or damaged: torn writes fail the header or payload CRC, and an all-zero header reads as blank. The format's own tests tear a slot overwrite at every byte boundary and flip every single bit of a slot; none is ever accepted as a mixed or altered version. After the crash, the winner is either the old version (Confirmed, since a torn or blank slot is explained) or the new one. Updates to one pair are serialised, so two writers never race for the free slot; two valid slots with the same generation (a fork) are refused rather than guessed.

**Why a writable open flushes first.** A process can die after writing the free slot but before its flush. The write is then visible to reads but not yet durable. Without a flush, the next owner would read it as the winner, write its own update over the *older* slot, the only durable copy, and a power cut could then lose both. A writable open therefore makes everything the previous owner wrote durable before it reads any slot.

### Create

1. Create the directory if missing (and flush its parent), then create `store.sio` exclusively and take the lock.
2. Probe the device; refuse it unless trusted.
3. Grow the file to `2B + 2T`.
4. Write volume slot A (generation 1) and table slot A (generation 1, empty table).
5. Full flush; re-read both pairs.
6. Flush the directory.

A failure in steps 2&ndash;5 removes the file again. A *crash* before step 6 completes may leave no file, or a file whose volume slots are blank or incomplete. Such a file does not open, but the next `create` takes it over: when `store.sio` already exists, `create` reuses it only if no one holds its lock, it is no longer than the largest metadata area (`4 × 64 KiB`, so it cannot hold region data), and no volume record can be found in it; it is then truncated and created afresh. Any other existing file is a store, and `create` fails with `AlreadyExists` without touching it.

### Writable open

The only write is the volume record's next generation (owner generation + 1), an A/B update. A crash leaves the old or the new owner generation.

### Provision

1. Choose the placement (section 11) and grow the file if needed.
2. Fill the whole extent with direct writes: the two header blocks with zeros, the data area with zeros or the keyed pattern. This blanks any older header at that place.
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

Source: [`crates/store-io-engine/src/store.rs`](../crates/store-io-engine/src/store.rs) (`create`, `build_new`, `open_inner`, `provision`, `fill`, `verify_ready`, `flip_table`, `durable_slot`), [`crates/store-io-engine/src/lifecycle.rs`](../crates/store-io-engine/src/lifecycle.rs), [`crates/store-io-engine/src/slots.rs`](../crates/store-io-engine/src/slots.rs), [`crates/store-io-format/src/slot.rs`](../crates/store-io-format/src/slot.rs) (tests)

---

## 14. What store-io does not check

A reader that wants to be stricter than store-io can add these checks; store-io as it is now does not make them.

- The bytes of a slot after its payload, and header bytes in `[128, header_len)` (they are covered by the header CRC but not interpreted).
- The volume record's reserved bytes at `[40, 48)`, and its `device_index`.
- That table entries lie at or after `2B + 2T`, or within the file (an entry whose extent ends past the end of the file is skipped at open, not rejected).
- That a region header's `data_offset`, `data_size` and `fill` agree with its table entry; only `region_id`, `kind` and the Ready state are compared.
- `owner_generation` in slot headers: it is recorded, never used to accept or refuse a slot.
- The generation-gap and chain-break anomalies of a pair (computed, not acted on).

---

<div align="center"><sub>Apache-2.0 OR MIT &middot; &copy; 2026 James Gober</sub></div>
