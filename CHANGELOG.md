<h1 align="center">
    <img width="90px" height="auto" src="https://raw.githubusercontent.com/jamesgober/jamesgober/main/media/icons/hexagon-3.svg" alt="Triple Hexagon">
    <br><b>CHANGELOG</b>
</h1>
<p>
  All notable changes to <code>store-io</code> will be documented in this file. The format is based on <a href="https://keepachangelog.com/en/1.1.0/">Keep a Changelog</a>,
  and this project adheres to <a href="https://semver.org/spec/v2.0.0.html/">Semantic Versioning</a>.
</p>

---

## [Unreleased]

### Added

- Engine: **`RegionOptions`** (`size`, `max_size`, `validate`) and
  `Store::provision_{append,page}_region_with`,
  `Reservation::provision_{append,page}_region_with` (v0.5 E4 C10; the
  existing calls are `RegionOptions::default().size(bytes)`); re-exported
  as `store_io::RegionOptions`. A region with `max_size` (container format
  2, still behind the hidden `StoreOptions::container_format`; format 1
  returns `Unsupported(FormatVersion)`) is one extent of its cap (at least
  its size), flagged capped in its entry and header and charged whole to
  its tag (and a reservation): it grows to the cap in one run with no new
  space, and past it is `NoSpace(RegionLimit)`. Only the header blocks and
  the data are filled and verified; the rest of the cap is allocated
  without being written where the system can (Linux), and written with
  the region's pattern on Windows (NTFS zeroes everything below a write
  past a file's valid data, so unwritten slack would cost the next write
  after it) and macOS (which cannot allocate inside a file), there at the
  cost of writing the whole cap at provisioning (each growth within it
  writes its range again, as everywhere). The same rule now covers
  the rest of the extent a growth beyond capacity adds. A cap below the
  size is `NotWritten(InvalidOptions)` before any I/O.
- `store-io-core`: `NotWrittenCause::InvalidOptions`.
- Simulator: `SimConfig::valid_data_model` (NTFS's valid data length: the
  bytes a write starting past a file's valid data would have zeroed first
  are counted in `World::gap_fill_bytes`; `range_state` reports a range
  past it not valid; a crash or a shrink cuts it back);
  `World::is_unwritten`; a world that completes 500,000 writes, or holds
  262,144 unflushed in a volatile cache, panics (a write that never stops
  fails its test instead of exhausting memory).
- Engine: **growth beyond capacity** in format 2 (v0.5 E4 C9, still
  behind the hidden `StoreOptions::container_format`): `grow` past the
  capacity adds one extent of at least 1 MiB and an eighth of the
  capacity, placed after the region's last extent when that space is free
  (one run: I/O there is never split), else by first fit, else past
  everything allocated. Under the region lock and the metadata lock the
  new range is filled with the region's pattern, flushed and verified (the
  rest of the extent allocated, unwritten; on Windows the whole extent is
  written); the region header is written listing the extent, then one
  table page write commits the new `data_size`, `capacity`,
  `extent_count`, span and `footprint_seq`. A crash before the commit
  leaves the old size, the header possibly one extent ahead (that extent
  reported in `extra_extents`; free, unless the region's page is stale);
  the next growth first rewrites such a header with the committed extents
  only, so a copy listing an extent the region does not have is only ever
  one extent ahead of the entry, which the open ignores. Refused before
  any write: a
  capped region `NoSpace(RegionLimit)`, a header listing as many extents
  as it can hold `NoSpace(Extents)`, the tag's cap `NoSpace(Caller)`;
  running out of space during the growth is `NoSpace(Growth)`; after any
  refusal the space accounting is as it was (the container may have
  grown). The region map gains runs while I/O runs on it (append-only
  atomics in chunks that double, so its memory follows its runs; loom
  models of readers racing growths). An append region keeps the completion
  ring sized when it was opened, provisioned or recycled: grown past that
  size, it allows at most the old ring's blocks in flight until it is
  reopened or recycled.
- Engine: **growth within capacity** in format 2 (v0.5 E4 C8, still
  behind the hidden `StoreOptions::container_format`):
  `AppendRegion::grow`, `PageRegion::grow` (to at least `new_size`,
  rounded up to the block; returns the new size; at most the current one
  writes nothing) and `capacity()`. Under the region lock the new range is
  filled with the region's pattern (keyed by region offset, across runs),
  flushed and verified; one table page write commits the new `data_size`
  (`footprint_seq` unchanged); then the region map's length and the
  append frontier's limit are raised. A crash before the commit leaves the
  old size. While a table page is stale or lost it is `NotWritten(Frozen)`
  (stricter than the spec, which freezes only growth beyond the
  capacity); format 1 returns `Unsupported`. Space accounting follows
  (logical bytes; the capacity was already counted).
- `store-io-platform`: `Platform::allocate_range(file, offset, len)`:
  allocates real blocks inside the file, never changing its size or a
  written byte (blocks it allocates read as zeros until written); a
  range past the end, or a read-only handle, is refused. Linux:
  `fallocate` with `KEEP_SIZE` (`EOPNOTSUPP` where the file system cannot
  allocate inside a file); Windows: nothing to do in a plain NTFS file,
  a sparse, compressed or integrity-stream one refused as unsupported (a
  write there may still need space). A copy-on-write file system may
  still allocate when the range is written. For format 2's capped regions
  (v0.5 E4 C6). A conformance scenario runs on every backend.
- Simulator: `range_state` now reports unwritten what no write has
  covered: a hole a `Deallocate` release punched (a trim in place
  punches none), a file's growth, and anything past its end (it used to
  report everything written); `FaultPlan::fail_allocate_range`,
  `World::range_allocations`.
- `store-io-format`: `footprint::{Run, runs}`, a region's extents in data
  order coalesced into runs, extents adjacent in the file (spec §11). The
  engine's region map is built from them (v0.5 E4 C2): a region in several
  runs maps each range to its run, refuses one that crosses a run in every
  build, and its reads and scans stop each device read at a run's end; a
  region in one run (every region until format 2's growth) pays one
  comparison more. The `meta2_decode` fuzz target checks the runs of every
  header it accepts.
- Simulator: `FaultPlan::fail_release` (the n-th `release_range` fails)
  and `World::releases`.
- Engine: **regions in container format 2** (v0.5 E2b/E3, still behind
  the hidden `StoreOptions::container_format`):
  - The open checks every region of every page that is not Lost (spec §9
    steps 3-6): its entry, its header pair, and its footprint
    (`store_io_format::footprint::check_region`: the header agrees with
    the entry; the committed extents sum to the capacity, lie inside the
    span and past the table area, overlap neither each other nor the
    header blocks). Usable regions are then settled: of two that overlap
    or share a name the one with the lower `footprint_seq` becomes
    unavailable (both on a tie); an unavailable region never demotes, but
    a usable one its own extents overlap with a newer sequence is
    possibly superseded (read-only). All of this happens before the open
    writes anything. Regions on a stale page, possibly superseded, or with
    an unknown read-only-compatible header feature open read-only; a
    region of several extents opens mapped by its runs (extents adjacent
    in the file coalesced, spec §11): reads, scans and writes cross runs
    in data order, each device operation inside one run. A blocking
    write's buffer across a run's end is written in place, a piece at a
    time, no buffer taken for a piece; a `Submitter`'s operation across
    runs keeps its row, one piece in flight at a time, the caller's
    buffer rotated between pieces and handed back with its bytes in
    order. One ticket and one completion either way, never `untorn`; a
    piece that fails, comes back short or is refused ends the operation:
    a write is `DurabilityUnknown` (poisoned), a read comes back with its
    error, or short (trimmed to the bytes read, in order). A 512 B
    append region opens the store read-only until §13.
  - Free space is the complement of every footprint, unavailable span and
    (on a stale page) extra extent; `SpaceReport::released` in format 2
    is that free space less what reservations hold.
  - Provisioning: first fit over the free space, else the tail (large
    data aligned as in format 1); fill, flush, verify; the header; then
    one page write carrying the entry, whose `footprint_seq` is that
    write's own `meta_seq`. Reservations work in format 2.
  - Release: the region drained, its lock (the header mutex) before the
    metadata lock; the page written without the entry (the commit;
    `next_seq` never reused), then the header Released, the data
    deallocated, free space recomputed.
  - Recycle: one header write under the region lock, the region
    generation one more, exactly the entry's extents (an extra one
    dropped), fill, key and flags kept. Slot commit: the object after the
    header's 64-byte fixed part (`object_len` bytes, capacity B - 192),
    the fixed part kept; a slot reads its object back from it.
  - The open reads the region header pairs many at a time (up to the
    queue depth in flight, as buffers allow), a pair whose slot B follows
    its slot A (every region provisioning makes) in one read of both, and
    from 16 regions (entries that passed the entry checks) on a queue of
    its own that keeps several reads in flight (a blocking
    call's lane runs one at a time on Linux, D-104). A failed read of both
    slots is read again slot by slot. An open of 65,536 regions
    (`harness/examples/open_scale.rs`): Windows 9 s to 1.0 s, near the
    disk's 4 KiB random-read rate (fio, 64 in flight); Linux (WSL,
    io_uring) 2.5-3.3 s, the virtual disk's rate (fio, 8 KiB, 64 in
    flight, NOWAIT: 24k a second), and 4.4-5.2 s on the synchronous tier
    (whose submit pool such an open now starts, as the first parallel read
    would). New: `store_io_buf::BufPool::class_len`;
    `store_io_sim::World::queues`.
  - New: `NotWrittenCause::Frozen` (allocation refused while a page is
    stale or lost); `Unavailable::{EntryInvalid, ExtentsInvalid,
    Overlaps, NameTaken}`; `MetadataReport::{invalid_entries,
    extra_extents, superseded, shares_reserved, stale_page_agreement,
    lost_header_writes, truncated}` (`lost_header_writes`: a region header
    slot that verifies with a newer generation than the version served, a
    recycle or slot commit possibly lost, spec §8); `store_io_format::meta2::RawEntry::{name,
    name_and_kind}`; `RawEntry::check` bounds the capacity by the span.
  - Tests against media from an independent writer
    (`tests/format2_util/craft.rs`) and judged by an independent audit of
    the raw media (`tests/format2_util/audit.rs`): power failing at every
    write of a provisioning and of a release, every write and flush
    failing in turn, a torn page write healing.
- `store-io-core`: `errno::is_lock_held` tells a lock another holder has
  from a file system that cannot lock.
- `store-io-format`: **container format 2, first step: the slot header.**
  `SlotFields` and `Expect` name the format (`FORMAT_1` or `FORMAT_2`); a
  format-2 slot carries `meta_seq`, the store-wide metadata sequence number,
  at byte 96 (at least 1 for the volume record and table pages, 0 for
  region headers), and its header is exactly 128 bytes. A reader refuses a
  slot of any format but the container's. `header_facts` reports what a
  failed slot's header still shows (its stored header CRC, and for a slot
  whose header passes every check but the payload CRC, its generation and
  `meta_seq`): what the format-2 open uses to tell a torn write from
  damage. The engine still writes format 1; format 2 is specified ahead of
  the code. Tests, a fuzz target extended to both formats, and five
  mutation entries.
- `store-io-format`: **format-2 payload codecs** (`meta2`): the 64-byte
  volume record (`VolumeMeta2`: the table's page count and entries per
  page, `clean_close`, must-understand `flags`); table pages
  (`encode_page`, `decode_page`) with 96-byte entries (`RawEntry`, checked
  into `Entry2` against `EntryLimits`, so a bad entry makes only its region
  unavailable and its clamped span stays reserved); region headers
  (`encode_region_header`, `decode_region_header`) with an extent list or a
  slot object. Payload lengths are exact. Tests for every rule and every
  truncation, a fuzz target (`meta2_decode`, canonical round trips), and 21
  mutation entries.
- `store-io-format`: **the format-2 tear judgement** (`judge`): given both
  slots of the volume record and every table page (status and header
  facts), it decides each pair's standing (Confirmed, Unconfirmed, Lost,
  Refused for a newer format, which fails the open),
  which failed copy is the one metadata write a crash can leave torn (to be
  healed), and whether the open may write. A failed copy is that write only
  if it is the next generation chained to the winner, its `meta_seq` is one
  above every other numbered slot (or its header is unreadable), it is the
  only such copy, and the store was not closed cleanly; a page's torn
  write is healed only when nothing else is damaged, the volume record's
  whenever it is the one candidate; an unreadable copy is always damage; a
  damaged or lost volume record disables page healing (and, with a page
  candidate, writing). Pure and allocation-free. 29 tests on slots written
  by a small store model, a fuzz target (`judge`) that checks heal,
  refusal, writability and every pair's standing against a brute-force
  reference written from the specification, and 25 mutation entries.
- Engine: **format-2 stores can be created and opened**, behind hidden
  options until format 2 is complete (not yet the default, and holding no
  regions yet: provisioning and reserving in one fail
  `Unsupported(FormatVersion)` until the regions arrive). Create makes
  every table page durable before it writes the volume record. An open
  reads every page (a slot whose read failed is read once more; a slot
  that read is never read again), applies the tear judgement, rewrites the
  one torn metadata write a crash can leave (at that write's own
  `meta_seq`, or the next one when its header is torn too), and reports
  what it found in `StoreReport::metadata` (`MetadataReport`: whether the
  last writer closed cleanly, stale and lost pages, a damaged volume
  record, what was healed (`Healed`), whether allocation is frozen, why a
  writable open was made read-only, and how many reads were retried). The
  format is read only from a header whose CRC verifies. A page or volume
  record of a newer format (a longer volume record included) fails the
  open; one carrying a read-only-compatible feature this version does not
  know opens read-only; a page that fails the page checks is lost, and
  then no page's torn write is healed. A writable store records a clean
  close when its last handle drops (not during a panic, and not when
  poisoned). Create takes over the remains of a crashed format-2 create
  (generation-1 table pages, empty or torn, and generation-1 volume
  records only when their payload is torn; past format 1's size bound, at
  least one of these) and nothing else: the file grows past format 1's
  bound only once page 0 is durable, so a crash that leaves a larger file
  always leaves one. `StoreReport::format` names the container format
  (also in its `Display`). 39 simulator tests, among them a power failure
  inside a create at each of its writes, and 41 mutation entries.
- Engine: **flushes in flight** (`StoreOptions::flushes_in_flight`, 1 to
  4, default 1). A store can run several flushes at once, for drives that
  execute flushes in parallel (NVMe). Each runs on a handle and queue of
  its own: the container reopened from the store's own handle
  (`Platform::reopen`, new; never by name, so a rename cannot put another
  file under it), without the ownership lock, so each reports its own
  failures. A receipt still needs every flush up to its own to have
  succeeded: a flush finishing before an earlier one satisfies nothing
  until the earlier one has succeeded, and any failure poisons the store.
  Metadata writes lead their flush only on the main handle's slot, and a
  full flush of the main handle holds that slot. Platforms run one unless
  a flush through any handle persists the whole file and failures are
  reported per handle (`Platform::flush_errors_per_handle`, new: Linux
  4.16 and later, and the simulator); the number run is in
  `StoreReport::flushes_in_flight`: 1 also for a read-only store (a
  format-2 store found read-only at open included: its extra flush queues
  are closed and it opens no flush handles) and for a device that needs no
  flush, and fewer than asked when a handle cannot be made (its queue is
  closed too). `DomainStats` counts the most that overlapped
  (`peak_in_flight`) and those that finished early (`out_of_order`). The
  stuck-operation report treats every flush lane as a flush. The harness's
  G15 measures eight writers at 1, 2 and 4 (and the number each run
  actually used). The default stays 1: 2 and 4 gained nothing on CI's
  memory-backed device (43,329, 42,816 and 42,959 durable writes a second)
  and lost on a WSL virtual disk (1,314, 974 and 595); there the
  eight-writer runs at 1 (median 1,374) made 3.3 times the durable writes
  of fio's eight writers flushing after every write (421), and on CI 0.68
  times (43,329 against 63,899), where a flush is cheap (fio flushing
  after every write kept 46% of its writes, against 1% on WSL). Tests:
  domain tests for out-of-order completion, a failure behind a success,
  poison during a flush, the issue window and slot 0 for the main handle;
  three new loom models; simulator tests (the wiring and end to end: every
  acknowledged append survives a power failure, with and without a failed
  flush, at 1 and 4 in flight, with flushes shown to overlap and reorder);
  two tests that metadata and full flushes of the main handle wait for its
  slot; tests that a queue left without a handle is closed and that a
  format-2 store found read-only opens no flush handle and closes its
  extra queues; a test on the real file system. Data leaders take the main
  handle's slot last, so metadata operations are not starved. At the
  default of one flush, provisioning's full flush now holds the slot, so a
  data barrier that needs a new flush waits for it instead of running
  beside it (two flushes on one handle could split one failure report). 14
  new mutation entries; 6 repointed.
- `store-io-sim`: `FaultPlan::fail_reopen` (the n-th reopen of a
  writable file makes no handle) and `World::reopens()`.

### Changed

- `store-io-core`: **`ReserveTag` is `#[non_exhaustive]`** and gains
  `RegionLimit`, `Extents` and `Growth` (the reasons a growth beyond
  capacity runs out). Breaking: a `match` on it outside the crate needs a
  wildcard arm.
- Engine: **an append region's completion ring is allocated at its first
  completed append** (or a `resume_at` that moves its tail), not when the
  region is opened, provisioned or recycled: a store of many append
  regions (65,536 in format 2) pays for none until each is used (up to
  8 MiB a region; at the default window, 32 KiB for one of 16 MiB or more
  at 4 KiB blocks). The ring is sized from the region's capacity (v0.5 E4
  C7).
- Engine: **a finished flush wakes only the barriers it covered** (D-125).
  Every finished flush used to wake every waiting barrier, on the leader's
  path, including those needing a later flush; on a virtual machine each
  wake is an interrupt to another processor, and eight blocking writers
  spent 18-20% of their CPU waking and 3.5 context switches per durable
  write. Barriers now sleep by the flush they need: the leader wakes one
  barrier per need it covered, each passes the wake on, and one barrier
  needing the next flush is woken to lead it. Measured with eight writers
  on CI (a diagnostic build, before the shared fallback below was added):
  79k to 92k durable appends a second on null_blk, 43k to 48k on a loop
  device, voluntary context switches 3.5 to 0.1-0.6 per write. A barrier
  whose bucket holds another need, or that may lead on fewer slots than
  the domain has (a metadata write), sleeps on a shared condvar that every
  finished flush wakes. Who to wake is decided under the slot lock and the
  wakes are sent after it is released, so a woken barrier finds the lock
  free instead of blocking on it at once (measured on null_blk: eight
  writers 155k to 168k, sixteen 173k to 213k, voluntary context switches
  1.56 to 1.28 per write, on another runner and run). Receipts, poison and
  the join rule are unchanged. Tests: six wake tests (a covered barrier
  passes the wake on, a jump past several flushes wakes each need, the
  next flush's barrier leads, a barrier sharing a bucket with another need
  is still woken, the next flush is led on the freed slot by a barrier
  that can use it, poison wakes every bucket); the domain tests build with
  two buckets, so the shared fallback runs often; a loom model of three
  barriers of one need (the pass-on); 8 new mutation entries, run on the
  domain's wake tests.
- Engine: **waiting barriers sleep without the slot lock and a finished
  flush wakes each covered need with one call** (D-126). A barrier still
  registers under the lock, then sleeps on a word of its own bed (a
  per-need bucket; the designate, the one barrier that will lead the next
  flush; or a shared word), without the lock: `futex` on Linux,
  `WaitOnAddress` on Windows, a mutex and condvar elsewhere (new
  `store_io_buf::WaitWord`). A leader deregisters and bumps every bed it
  wakes under the lock and wakes each with one call after releasing it;
  a covered barrier returns on reading `done`, never re-taking the lock,
  where before each woken barrier re-took it to pass the wake to the next.
  Measured on CI's loop-device runners at 8, 16 and 64 writers: within
  2-3% of D-125 (that device is not limited by the wake chain); see D-126
  for where the remaining time goes. Receipts, poison and the join rule
  are unchanged. Tests: a stray wake leaves a barrier asleep and
  registered once; the designate leads once for its bucket; a jump past
  more needs than buckets wakes each; a bucket collision on three slots;
  the window's designate; loom models of tickets taken while flushes run,
  a full flush beside the designate, narrow and wide barriers on two
  slots, and outside poison against sleepers; `WaitWord` unit tests and a
  loom model. The held flushes in the domain tests now outlast every
  wait of the test, so a stranded barrier always fails (a mutant of
  D-125's routing survived at random because a held flush released
  itself just in time). 15 mutation entries replace D-125's.
- **Breaking** (`Directory`): a replace takes the directory's `.sio-lock`
  file (created on first use and kept) and fails `Locked`, not waiting,
  while another handle or process replaces there; the caller retries. A
  file system that cannot lock gives `Io(Lock)`. Every writer must be on
  this version for the protection to hold. Object names that could
  resolve to another file are refused in `replace`, `remove` and `read`
  alike: longer than 247 bytes; ending in `.` or a space (`x.`); shaped
  like a Windows short name (`data~1`, `v~1.0`); Windows device names with
  or without an extension (`aux.json`, `con.cfg`); and the directory's
  own `.sio-lock` and `*.sio-tmp`, compared as a case-insensitive file
  system compares names (case folded, ignorable code points dropped).
  Objects written under such names by 0.4 can no longer be read or
  removed through the API. `Error::Locked`'s message now covers
  directories too.
- A store's open and create (the reclaim of an abandoned container
  included) now report a file system that cannot lock as `Io(Lock)`, not
  `Locked` or `AlreadyExists` (another holder still gives those).
- **Breaking:** the flush domain's `barrier` takes the slot and the
  flush's number (`FnMut(usize, u64)`); `barrier_main` leads only on
  slot 0 and `exclusive_main` holds it. `DomainStats` gains
  `peak_in_flight` and `out_of_order`, `StoreReport` gains
  `flushes_in_flight`, and `StoreOptions` gains `flushes_in_flight`.

- **Breaking:** `StoreOptions` gains two hidden fields (the container
  format and a format-2 store's region capacity) while format 2 is
  incomplete; code that builds it without `..StoreOptions::default()`
  must add them.
- **Breaking:** `RegionId` is 64-bit (`from_raw(u64)`, `get() -> u64`),
  for container format 2's ids (`page << 32 | seq`), which this release
  does not create yet. Format 1 ids are
  unchanged in value. Inside the engine every region-to-file offset now
  comes from one place (the region's map of extents, one extent today),
  ready for regions that grow across extents.
- **Breaking** (`store-io-format`): `slot::FORMAT_MAJOR` is gone (now
  `FORMAT_1` and `FORMAT_2`); `SlotFields` gains `format_major` and
  `meta_seq`, `Expect` gains `format_major` and `numbered`, and
  `EncodeError` gains `Format`.
- **Breaking:** `StoreOptions::append_ring` (blocks) is now
  `StoreOptions::append_window` (bytes, default 16 MiB, the same reach as
  before at 4 KiB blocks). In blocks, the reach shrank with the block size:
  at 512 B blocks the old default held only 2 MiB ahead. Renamed rather
  than reinterpreted, so old settings fail to compile instead of silently
  meaning something else. The window is rounded down to a power of two
  blocks, so it never exceeds what was asked; a region smaller than the
  window gets a ring of its own size rounded up, so it never pays for a
  large ring and never limits its own appends.
- `store-io-buf`: **the buffer pool hands out the most recently returned
  buffer first.** Each size class's free list is now a lock-free stack
  (`IndexStack`, new and public) in place of a first-in-first-out queue,
  so the next operation reuses a buffer that is still in the CPU's caches.
  First-in-first-out rotated through every buffer of a class: 1 MiB appends
  kept 8 deep at memory speed ran 7-10% slower that way (64 buffers
  rotating against 9, measured on CI). It also keeps fewer buffers
  resident: only about as many as are in use at once are ever touched.
  Allocation-free and blocking-free as before; `IndexQueue` stays public.
  A class now holds at most 16,777,215 buffers
  (`IndexStack::MAX_CAPACITY`; the head word keeps a 40-bit tag against
  ABA), and a larger count is a configuration error.
- Tests: the loom models of `store-io-buf` no longer yield in their retry
  loops. Loom never offers a thread a value it read before yielding, so a
  stale read on a retry was never tried: without a preemption bound, a
  missing release on the stack's push survived. Mutation entries can now
  run under loom (`dev/tools/mutate.py`, bounded as the CI loom job is);
  five new ones cover the stack's tag and memory orders and the queue's
  publish. `store-io-buf` changes now trigger the mutation workflow.
- Harness: G2 (the barrier alone against the raw flush alone, p50 and
  p99) is measured in 20 alternated rounds and judged as G1 is, instead of
  comparing two windows measured one after the other (a virtual machine's
  flush drifts between windows by more than the gate).
- Roadmap: 1.0 is the stable API and container format, proven on the
  hardware available; power-cut certification and the power-safe
  reference measurements move to 1.x (they add evidence without changing
  the API or the format).
- CI: every job of the CI workflow has a time limit (20 to 60 minutes,
  well above its usual run; the mutation workflow already had one), so a
  hung test fails its job instead of running for hours.
- CI: a failed `apt-get update` (one package source refusing, as
  packages.microsoft.com once did) no longer stops the packages from
  installing from the lists it did fetch; a failed install still fails the
  step. The io-wq gate now says when fio is missing or its control run
  failed, instead of reporting that it cannot see io-wq threads.

### Fixed

- Engine: a `Submitter` operation split at a run's end now checks, in
  release builds too, that every piece moves it on (a split that did not,
  only through a bug, re-issued writes for ever).
- Engine: an operation split at a run's end (a `Submitter` read or
  write, a buffered write, `read_at`, a scan) that reached past the data's
  end, which only a bounds-check bug could cause, went on as empty pieces
  for ever; every split now refuses an empty piece, in release builds too
  (`RegionMap::run_left`).
- Engine: **a release stopped deallocating at the first extent that
  failed.** The rest of the region's data stayed allocated (on Windows,
  where a failed trim is no error, silently). Every extent is now tried;
  off Windows the first failure is still returned, after the last extent.
  (Every region has one extent until format 2's growth.)
- Engine: **a panic between a `Submitter` append's reservation and its
  submission left a hole that a later barrier waited on.** Nothing
  poisoned the store until the submitter was dropped, so a durable append
  past the hole on another thread waited until then. The reservation is
  now held under a guard that poisons and wakes every waiter at once.
- Engine: **a `Submitter`'s durable writes could each get their own flush.**
  Completions collected together asked for their flushes one by one, so a
  flush thread quick enough to start between two asks flushed for each in
  turn (64 flushes for 64 writes in one macOS test run). Every completion of
  a pass is now processed before any flush is asked for, so one flush can
  cover them all. With a 1 ms pause after each ask, the old order took 64
  flushes for 64 writes and the new one 7 or 8.
- `store-io-core`: the out-of-space and media-error checks matched Linux
  error numbers on every system. On macOS, 61 and 74 (ECONNREFUSED,
  EPROGUNAVAIL) would have counted as media errors, EBADMSG (94) would
  not, and EDQUOT is 69, not 122. Each system's numbers are now matched
  against its own; on systems other than Linux and macOS only the numbers
  every Unix shares count (no change on Linux or Windows).
- Engine: **`AppendRegion::resume_at` could mark unflushed bytes
  durable.** Called after an append that was not yet durable, it raised
  the durable point past that append, so a later sync of it returned a
  receipt without flushing, and a power failure lost the bytes. It now
  moves the durable point only when everything appended in this open is
  already durable (otherwise the next flush covers both), and it is
  refused on a poisoned store. The same held for a forced region's
  flushed prefix on a power-safe store (D-102): its sync could claim
  `data_synced` without a flush.
- Engine: **two lost wakeups in the append frontier.** A writer waiting
  for room in a full ring took its snapshot of the completed prefix after
  the refusal, so when every append in flight finished in between it
  waited for a prefix nothing would reach (the last writer of a burst
  could hang forever). And a completion read the waiter count with a
  plain load after publishing the prefix, which may miss a waiter that in
  turn misses the prefix (store buffering), losing the last wake of a
  waiting append or sync. Both fixed and modelled under loom. A waiter
  woken after the store was poisoned now never reserves. And an append
  racing a `resume_at` (between its moves of the tail and of the
  prefix) could be left stranded, or a writer waiting for room left
  unwoken: `resume_at` now advances and wakes as a completion does. A
  `resume_at` to the current tail could also move the completed prefix
  back below an append that had just completed there, hanging every later
  sync; it now moves the prefix only forward from the old tail.
- Engine: **directory replaces from two handles could clobber each
  other.** Every replace of an object used the same temporary name, and
  nothing kept two handles (or processes) on one directory apart: one
  could rename the other's half-written temporary file over the object,
  report success, and leave it empty after a crash. A replace now holds an
  exclusive lock on the directory's `.sio-lock` file; another handle's
  replace fails `Locked` with nothing written. A handle poisoned while a
  replace ran (by a failed `sync`, say) no longer commits it when the
  poison is seen at the commit point, and is checked again once the
  handle's own change lock is held.
- Engine: directory object names that could resolve to another file were
  accepted: the directory's own (`.sio-lock`, `*.sio-tmp`, in any case,
  Unicode case folding included, and with ignorable code points such as
  U+200C inside, which HFS+ and Linux case-folding directories drop),
  names that fold that way to `.`, `..` or nothing,
  their Windows short names
  (`SIO-LO~1`), names ending in `.` or a space (`x.` read `x`; `...` is
  the directory), Windows device names (`NUL`, `COM1`, which poisoned the
  handle at the rename), and names longer than 247 bytes (readable but
  never replaceable). `replace("x")` could delete an object named
  `x.sio-tmp`, and a replace of `SIO-LO~1` could replace the lock file,
  splitting the writers. All are now refused (`NotWritten(InvalidName)`)
  in `replace`, `remove` and `read`, alike on every platform.
- `store-io-buf`: a pool with more than 65,536 size classes recorded a
  buffer's class in 16 bits, so a buffer of the last class returned to
  class 0's free list and a held slot could be handed out twice (unsound
  from safe code; no store or directory builds such a pool). Refused now.
- `store-io`: concurrent `probe` calls on one directory shared one
  temporary file name; each call now uses its own.

---

## [0.4.1] - 2026-10-20

A patch release for one defect, present since 0.3.0: a region release
running at the same time as a provisioning on another thread could destroy
the new region's data. The on-disk format and the public API are
unchanged. See `docs/release/v0.4.1.md`.

### Fixed

- `store-io-engine`: **a release could destroy a region provisioned during
  it.** `release` marked the region released in the table, then let go of
  the store's metadata lock before writing the region's Released header and
  deallocating its data area. A provisioning on another thread in that
  window could reuse the extent (a released extent of exactly the same
  length is reused) and commit a new region there; the release's late
  writes then landed in it: the deallocation through its data, including
  bytes it had already given durable receipts for, and the header write
  over one of its header slots (which could leave the new region
  unavailable at the next open). Present since 0.3.0; needs a release and
  a provisioning of the same extent length on another thread, reaching
  placement between the release's table update and its deallocation. The release now holds the lock until its
  space is deallocated. Tests: the lock is held while the space is
  deallocated (deterministic, through a simulator hook); a region placed in
  just-released space keeps its durable bytes across a reopen. Mutation
  entry killed. Found in the v0.5 design review.
- Harness: G6 stamps only the ends of each block inside its timed loop (a
  full stamp was a memory pass per MiB that fio never makes, which a fast
  runner exposed), and W3's store-io regions are sized for 100,000 commits
  a second (a fast device's warm-up exhausted them).

---

## [0.4.0] - 2026-10-19

io_uring on Linux, through store-io's own binding, and the engine layer: a
`Submitter` keeps many reads and writes in flight from one thread with the
same integrity rules as the blocking calls, which are now built on it. A
0.3.1 store opens with 0.4.0 (the on-disk format is unchanged; one check is
stricter: the volume record's reserved bytes must be zero). See
`docs/release/v0.4.0.md`.

### Added

- `store-io-posix`: **io_uring on Linux**, through store-io's own binding
  (no third-party io_uring crate). Reads and writes complete
  asynchronously, many in flight from one thread; nothing ever runs on the
  kernel's worker threads (what cannot be done without blocking is finished
  by one ordinary system call, and counted). Engine submitters get rings
  only their thread may use; the blocking API's parallel lanes get shared
  rings, and its single operations run inline (one system call), which is
  faster for an operation the caller waits for at once.
- `StoreOptions::linux_tier` (`Auto`, `Sync`, `Uring`): `Auto`, the
  default, uses io_uring where the kernel accepts store-io's rings, else
  the synchronous tier. `StoreReport::io` names the interface and why it
  was chosen. Other systems ignore the option.
- `store_io::io_uring_fallbacks()` (Linux; also `store_io_posix::`): operations the io_uring tier
  finished with a blocking call (never a failure; a measure of how often
  the asynchronous path was not taken).
- `StoreOptions::register_file` (Linux io_uring, D-078): a `Submitter`'s
  ring registers a read-only twin of the store's container (a second open
  file description) as a direct descriptor, and reads use it. Writes stay
  on the store's own description, so a dead owner's write in flight keeps
  the store locked until it lands (lock fencing), while the ring's own
  reference never holds the lock. Best effort;
  `Store::io_path()` reports what each ring got. On by default (G3d, CI on
  a memory-backed device: reads 1.03x to 1.08x of an unregistered ring;
  writes take the unregistered path). `Platform::queue_registered` is the
  backend hook; `store_io_posix::Registration` what a queue registered.
  Buffers are never registered, and every ring refuses the fixed-buffer
  opcodes (D-107): a read into a registered buffer never completes on at
  least Linux 6.5.0 and 6.6.0 (found by the CI kernel matrix), and no
  probe tells the kernels apart.
- `Store::io_path()`: the kernel interface in use now, with every queue made
  since create or open counted (`StoreReport::io` is the snapshot at open).
- `Store::open_readonly_with` (explicit options for a read-only open);
  `Op::Queue` for a failed queue setup (it was reported as `Open`).
- CI checks of the io_uring tier, each failing closed when it cannot run:
  - **Crash replay.** `dm-log-writes` records every write, flush and FUA
    of a workload on ext4 and XFS. The log is replayed to every prefix, and
    to subsets of the writes cached before each flush (all of them for runs
    of up to six writes, sampled beyond), and store-io must recover a
    consistent store with every receipted record intact.
  - **Kernel matrix.** The test suites run in VMs on Linux 5.15, 6.1 and
    6.6.
  - **No kernel worker threads.** A gate samples io-wq workers under load
    (`dev/tools/iowq-gate.sh`), with a fio control that must create them.
  - **Idle.** For 60 s an idle store makes no system calls, no I/O and no
    timer wakeups, on both tiers (`tests/idle.rs`).
  - **Hot path.** A submitter's steady-state loop makes no futex call and
    no timed wait, checked with `strace` (`tests/hotpath.rs`,
    `dev/tools/hotpath-check.py`).
- Harness: `gates` runs the ROADMAP's performance gate table: the
  workloads, fio controls, CPU efficiency, allocation counts and QoS under
  a background writer. Each row is judged PASS, NOT RESOLVABLE (inside the
  machine's own run-to-run spread), FAIL, REPORTED (the table asks for a
  report) or ELSEWHERE (another check measures it). Every store-io,
  raw and fsys measurement reads back a sample of what it wrote and
  compares it; a measurement that failed, or whose check did not pass,
  never feeds a verdict. CPU efficiency counts each side's whole process
  (fio's own figure leaves out the completion thread of its Windows
  engine). G1 alternates store-io and the raw primitive in short rounds
  and judges the worse of the median per-round difference and the pooled
  one (a virtual machine's flush latency drifts by more than the gate
  between long windows). A CI job runs the gates on a memory-backed device
  and fails on any FAIL.
- `store-io-engine`: the engine-layer **`Submitter`** (D-065, D-080, D-098),
  from `Store::submitter(SubmitterConfig)`. One thread keeps many appends,
  page writes and reads in flight without blocking, from the store's pooled
  buffers (`take_buf`) with no copy; `reap` and `wait` hand back
  `Completion`s, each with its tag, its buffer and `Done::Written(Written)`
  (the `WriteTicket`) or `Done::Read`. The completion types are in
  `store_io_engine::completion`; the facade exports `store_io::Submitter`
  and a `store_io::submit` module (`SubmitterConfig`, `MAX_DEPTH`,
  `Completion`, `Completions`, `Done`, `Written`, `Rejected`, `WriteMode`,
  `IoBuf`, `Wake`, and `Waker`, the native `SubmitterWaker`).
  - **Same rules as the blocking calls, per operation.** An append's
    position is final at submission; its ticket is minted only when its
    completion shows every byte written, and only then does the region's
    completed prefix move past it. A write that fails or transfers short
    after it was issued is `DurabilityUnknown` and poisons the store. Each
    operation holds its region open until its completion is processed, so
    recycle and release wait for it, and an operation on a recycled
    generation is refused (`StaleGeneration`). Another store's region
    (`ForeignPosition`) or buffer (`ForeignBuffer`) is refused before
    anything is reserved.
  - **Never blocks on submission.** A full submitter (`depth` operations
    submitted and not yet taken back, at most 4096) refuses with `Rejected`
    (`QueueFull`), buffer returned, nothing reserved. Nothing allocates
    after creation; the oldest operation is tracked in amortised O(1). On the
    Linux synchronous tier the operations run on the submit pool (D-096);
    on Windows on IOCP.
  - **Drop waits** for every operation in flight, untimed, before releasing
    anything the device may still touch; `Store::oldest_outstanding` now
    also reports a submitter's oldest unprocessed operation, for a watchdog.
  - A thread must process its own submitter's operations on a region before
    it recycles or releases that region, or syncs or appends far past them
    (documented on `Submitter`, `recycle` and `release`). Buffers come from
    the store's pool, shared with its blocking calls.
  - **Durable writes and barriers (D-099).** `append` and `write` now take
    a `WriteMode`: with `Durable`, the completion comes back only once the
    write is durable, with a receipt (`Written::receipt`).
    - **Receipt kinds.** A durable append whose region was complete up to
      its end when it completed gets a prefix receipt (new
      `DurableReceipt::is_prefix`: every byte before `durable_through` is
      durable, as from `sync_through`, and the region's `durable_through`
      advances); otherwise, and for `barrier(&ticket)` and page writes, the
      receipt covers the write's own range. `durable_through`'s docs now
      describe both kinds.
    - **Barriers.** `barrier(&ticket)` completes with `Done::Synced` (a
      receipt for that write), `barrier_all` with `Done::Flushed` (every
      write completed in the store before it is durable). They take a place
      in the submitter's depth; `barrier` is refused `ForeignPosition`,
      `StaleGeneration`, `NotReady` (released region), `QueueFull` or
      `ReadOnly`, `barrier_all` `QueueFull` or `ReadOnly`.
    - **The flush thread.** On a flush-required store, `store-io-flush`
      (one per store, started by the first request, parked with no timer
      when idle) leads or joins the same shared flushes as the blocking
      barriers, and wakes a submitter once per batch, only if it is
      blocked. A barrier already covered by a finished flush completes at
      once. Read-only stores start no thread; power-safe ones only
      when a region forces data sync (D-102). None of these
      calls blocks, unless the OS refused the thread: the flush then runs on
      the calling thread.
    - **Failures.** A receipt is only ever handed back after a flush issued
      after the write completed has succeeded. A failed flush poisons; the
      writes it was for complete `DurabilityUnknown` (with the write's
      region and range) or `Poisoned`, except one an earlier flush already
      covered, which keeps its receipt. On a poisoned store, power-safe or
      not, durable writes and barriers complete `Poisoned`.
    - **Lifecycle.** A durable write keeps its region open until its flush:
      recycle and release wait for it (reap or wait until its completion is
      handed back). Dropping a submitter waits for its pending flushes.
    - **Waker.** `Submitter::waker` returns a `SubmitterWaker` (also
      `store_io::submit::Waker`), which ends a wait for I/O or for a flush;
      `wait` returns early only on that waker.
  - **Large single operations (D-101).** `StoreOptions::max_write`
    (default 4 MiB, rounded up to a power of two, at most 1 GiB) and
    `large_buffers` (default 8): when `max_write` is above `max_io`, the
    pool holds that many buffers of `max_write` bytes, used by submitters
    only, and a submitter writes or reads up to `max_write` from one of them
    as one OS operation (`Submitter::max_buf`). The defaults add 32 MiB to
    each store's pool (Windows charges the whole pool to the commit limit at
    creation; a buffer becomes resident when first used). The blocking calls
    keep their `max_io` pieces and never take the large buffers.
    `StoreOptions` gains two public fields (struct literals without
    `..StoreOptions::default()` must add them). `max_io` is now also capped
    at 1 GiB.
  - **Forced data sync per region (D-102, G7).**
    `set_force_data_sync` and `force_data_sync` on `AppendRegion`,
    `PageRegion` and `Slot`: every barrier on that region issues the OS data
    sync, even on a power-safe store, where barriers are otherwise free:
    batches, `sync_through`, durable writes, slot commits, submitter durable
    writes and barriers on it, and `Submitter::barrier_all` while any region
    forces it. A device trusted as power-safe that still reports a volatile
    write cache has that cache flushed (a device with none has nothing to
    flush; on Windows power-safe writes are flushed already, so forcing adds
    one). A runtime option shared by every handle and generation of the
    region, never stored: set it again after each open. New
    `DurableReceipt::data_synced`: whether a data sync issued after the
    covered writes completed has succeeded (always on flush-required stores;
    on power-safe stores for forced regions; false means not guaranteed). A
    forced region's receipts carry a real flush number, so they cover only
    writes completed before it. `AppendFrontier::synced` and `publish_synced`: the
    contiguous prefix a data sync made durable.
  - Vectored writes (gathering separately filled buffers into one
    operation) are deferred (D-101).
- `store-io-buf`: `BufPool::owns`, whether a buffer came from that pool.
- `store-io-core`: `NotWrittenCause::ForeignBuffer`.
- `store-io-engine`: `Store::unavailable_regions()`, `UnavailableRegion` and
  `Unavailable`; `StoreReport::unavailable` (also in its `Display`).
- `store-io-core`: `CorruptionKind::Truncated` (the container is shorter than
  its metadata says).
- `store-io-sim`: `FaultPlan::fail_read`, which fails the n-th read with a
  chosen raw code.
- `store-io-platform`: `Platform::lock_shared`, the reader form of the ownership lock, implemented by every backend. Outside implementers of `Platform` must add it.
- `store-io-conformance`: a check that readers share the lock on read-only handles and exclude the owner.
- `store-io-sim`: `FaultPlan::fail_wait` (the n-th queue wait fails with a
  chosen raw code, leaving its operations in flight) and `World::waits`.
- `store-io-sim`: `FaultPlan::read_only_media` (every read-write open fails with EROFS) and `World::faults`. The simulator's exclusive lock now refuses a read-only handle with EBADF, as Linux does.
- `store-io-sim`: `FaultPlan::dir_sync_unsupported` (every directory flush
  fails as unsupported) and `FaultPlan::fail_set_len` (the n-th `set_len`
  fails).
- The observer (feature `observe`, off by default; D-094). `Store::observe()`
  returns exact event counters and sampled per-stage latency histograms
  (log2 buckets with nearest-rank quantiles). Off, every hook compiles away
  (0 ns). On, per durable write: about 9 ns on one thread, 10 to 12 ns on
  four, 15 ns with all 16 cores busy (Windows; Linux under WSL2 about 8, 9.5
  to 10.5, 14.4 to 15.8) [measured, harness `observe-cost`, median of 3 runs,
  on a busy machine]. Counts are exact. Each observer has one block per CPU
  (rounded up to a power of two, 16 to 256); that many live threads own one
  each and update it without locked instructions, and further threads
  spread over as many shared blocks with atomic adds (still exact). No
  allocation
  after creation, no timer, no thread, no dependency. The facade and the
  engine pass the feature through to `store-io-core`, and the facade
  re-exports `store_io::observe`.
- `store-io-engine`: `Store::oldest_outstanding()` and `Outstanding` (D-084,
  D-095): the age of the oldest operation still in flight on the store's
  shared queues, and whether it is a device flush, for a watchdog the caller
  owns. The I/O path pays only a busy mark per queue, on a cache line of its
  own (plain stores by the queue's holder: when it is taken, at each
  completion, when it is released); the query reads the clock, so the age
  counts from the first query that saw the operation and restarts at each
  completion (a long job that keeps progressing never looks hung). Covers the
  shared queues and the flush lane.
- `store-io-platform`: `Platform::monotonic_ms`, a coarse monotonic clock
  read only by queries (`GetTickCount64` on Windows,
  `CLOCK_MONOTONIC_COARSE` on Linux; the simulator's clock moves only when a
  test advances it). Outside implementers of `Platform` must add it.
- `store-io-engine`: `Store::poisoned()` reports the failure that poisoned
  the store, if any.
- `store-io-platform`: `QueueConfig::parallel`, a hint that the caller
  submits several independent operations before it waits, and
  `QueueConfig::pool_threads`, the size of a backend's shared worker pool if
  that queue starts it (D-096); `Queue::workers` (default `None`). Code that
  builds a `QueueConfig` with a struct literal must add the fields
  (`..QueueConfig::default()`).
- `store-io-posix`: the submit pool (D-051, D-096). A parallel queue runs its
  operations concurrently on worker threads shared by every store on the
  platform: started by the first parallel queue (default one per CPU, at most
  32; never more than 256), parked with no timer while idle, and stopped when
  the platform and its last queue are gone. Nothing is allocated per
  operation. Operations complete in any order; a pooled queue never waits for
  more than is in flight. Its drop waits for every operation still running:
  when it returns none can land later and every buffer is back in its pool.
  Each job holds the file's descriptor (`PosixFile` now keeps it in an
  `Arc`), so the descriptor stays open, and its number unused, until the
  job's system call returns. Workers block every signal except those the
  kernel forces on the thread that caused them (faults, traps, seccomp's
  `SIGSYS`), so a process's handlers for any other signal never run on
  them. No signal the calling thread blocked is ever unblocked, even
  briefly, and its mask is restored exactly after the spawn. A worker panic
  aborts the process rather than leave a queue waiting forever. Do not use a
  pooled queue in a child after `fork()`. `PosixPlatform::pool_threads` and
  `Queue::workers` report the size. Queues without the hint are unchanged:
  every operation stays on the calling thread.
- `store-io-engine`: page batches of more than one write, and reads and
  scans larger than one buffer, run on parallel lanes (D-096): on the Linux
  synchronous tier through the submit pool, elsewhere on the backend's own
  asynchronous queues. The lanes are created by the first such call. A
  one-write batch, every single write (of any size), a read or scan of one
  buffer, barriers and slot commits stay on the caller's thread. When every
  parallel lane is busy, a call takes a shared lane instead of waiting for
  a parallel one. If the pool cannot start, the store uses its shared lanes
  from then on. New `StoreOptions::t3_pool_threads` (0 = one per CPU, at
  most 32) and `Store::submit_pool_threads()`. `StoreOptions` gains a field,
  so code that builds one with a struct literal must add it.
  `oldest_outstanding` covers the parallel lanes. The scan docs now state
  that a visitor calling into the same store can wait forever with few
  queues and several such scans; collect, then act after the scan. Measured
  on Linux (WSL2 ext4 on a virtual disk, harness W4, N random 4 KiB pages +
  one barrier per commit, median of 3 runs alternating with the previous
  build): 1.45x the serial path at N = 16, 2.7x at 64, 4.7x at 256 (37,698
  pages/s against 8,030; commit p50 6.7 ms against 31 ms); N = 1 and lone
  durable writes (W1) unchanged, both equal to the raw `pwritev2` +
  `fdatasync` floor [measured].
- Harness: W5 measures sequential appends and raw writes of 2 and 8 MiB per
  call as well as 1 MiB.
- `store-io-platform`: queue wakers (D-083). `Queue::waker()` returns a
  `Send + Sync + Clone` handle (trait `Wake`) another thread uses to end the
  queue's current or next `wait`, which may then return having added
  nothing; a wake never creates, loses or repeats a completion.
  `QueueConfig::owner` (`Owner::Shared`, the default, or `Owner::Thread`)
  says who uses a queue. Outside implementers of `Queue` must add the
  `Waker` type and `waker()`; code that builds a `QueueConfig` with a struct
  literal must add `owner` (`..QueueConfig::default()`).
  - Linux (`PosixWaker`): a pooled queue's wait ends on the wake; an inline
    queue never blocks, so its waker does nothing.
  - Windows (`WinWaker`): a completion packet with no `OVERLAPPED` posted to
    the queue's port, at most one at a time, which the queue never reports
    as a completion; every wait spends any wake packet still posted before
    it returns, so idle wakes do not pile up, and a wake ends at most one
    wait, the current or the next. The port lives as long as any waker.
  - Simulator (`SimWaker`): a flag the next wait sees.
  - Tests: on every backend a wake never creates, loses or repeats a
    completion (conformance, inline and parallel queues). Loom shows a wake
    ends a blocked wait on the Linux pool's inbox; on NTFS, a thousand idle
    wakes leave no stale packet and a wake is never counted as a
    completion.
- `store-io-platform`: `Queue::wait` states that it never waits for more than
  is in flight (every backend already behaved so).
- `store-io-engine`: a store's queues are dropped before its file and
  ownership lock, so no queued operation can outlive the lock.
- Conformance: a parallel queue completes every operation once with its
  buffer, makes durable writes and flushes, refuses when full, and returns
  every buffer when dropped with operations in flight (on every backend).
- Tests: a mutation-list entry may name the packages whose tests must kill
  it; entries for Linux-only packages run in the Linux CI and are reported as
  skipped elsewhere. Five entries guard the pooled queue's drop and depth,
  the workers' signal mask (blocked, with the fault signals open) and the
  restored caller's mask; one more guards that each parallel lane owns its
  busy mark, and ten that every queue user starts on an empty queue and
  that an abandoned operation is never reported as done nor as a media
  verdict, two that an abandoned provisioning write never makes a region
  ready or reads as "out of space". The list has 89 mutations, all
  caught.
- Tests: a large append whose pieces complete out of order (with buffers
  reused for later pieces) lands every byte; a piece failing at any point
  poisons and returns every buffer.
- Tests: loom models the submit pool's job queue and inbox handoffs, with
  stray wakeups, and the smallest (one-worker) pool.
- Harness: `observe-cost` measures what the observer adds per durable write.
- `store-io-engine`: `DomainStats::already_durable`, which counts the receipts
  `AppendRegion::sync_through` returned without a barrier because the ticket
  was already durable (SIO-TYP-02: a barrier is never a silent no-op).
  `DomainStats` gains a field, so code that builds one with a struct literal
  must add it.
- Tests: on NTFS, ext4 and XFS, releasing a region leaves the regions on
  either side, the slots and the table intact, before and after a reopen; on
  Windows the container keeps its length and does not become sparse.
- Fuzz targets (`fuzz/`, cargo-fuzz) with a seed corpus of valid on-disk
  structures, run for 60 s each in CI (job "Fuzz smoke"). Each target checks
  a property, not only the absence of panics:
  - `slot_validate`: validation is deterministic, and an accepted slot is
    exactly what the encoder writes;
  - `pair_torn_update`: after an update torn at any set of 512-byte sectors,
    the A/B pair decides for the old or the new version, never anything else;
  - `meta_decode`: the volume record and region header round-trip;
  - `table_decode`: an accepted table drops no entry, obeys the id, name,
    alignment and overlap rules, and round-trips.
  - `probe_linux` and `probe_windows`: the device probe's parsers (sysfs and
    procfs text, `mountinfo`, `uevent`, NVMe pages; Windows storage
    descriptors, protocol-specific answers, volume structures) on hostile
    input. Both run on Linux: the Windows parsers are pure and now compile on
    every platform, their few SDK constants pinned to `windows-sys` by
    compile-time assertions on Windows.
- Tests: the mutation check now treats any build failure as an invalid
  mutation. Eight listed mutations had never compiled (a `deny(warnings)`
  lint) and were being counted as caught, and three no longer matched the
  code. All are rewritten so they build. Two then survived, exposing real
  test gaps, now closed: a batch append that fails mid-record must leave no
  partial bytes, and reservations, space reports and provisioning must take
  their locks in one order (a concurrency test with a deadline). The list
  then had 67 mutations, all caught.
- `store-io-sim`: `FaultPlan::read_only_media` also refuses creating files
  and directories, renames and unlinks. A targeted `fail_read` takes
  precedence over a bad range. New `World::misaligned_reads`.
- `store-io-format`: the volume record's trailing reserved bytes must be zero
  (every version so far wrote zeros there), so no future field can be read
  and then dropped on rewrite.
- `store-io-platform`: at most one lock (exclusive or shared) per handle,
  documented.
- Conformance: the shared-lock check releases readers one at a time and
  requires the owner to stay excluded while any remains.
- CI: the fuzz job uses a fixed nightly and keeps crash inputs as an
  artifact; the mutation check also runs on pull requests.
- CI: `cargo vet` (job "Security"). Audits are imported from Mozilla, Google,
  the Bytecode Alliance, Embark, ISRG and Zcash; `windows-sys` is trusted
  through its publisher; `libc` and `loom`'s test-only tree are exempted,
  each listed in `supply-chain/config.toml`.
- CI: `deny.toml` is now a crate allowlist. Any crate not listed (today
  `libc`, `windows-sys` and `windows-link` at run time, and `loom`'s tree for
  model checking) fails `cargo deny check`.
- `store-io-core`: `errno::is_unsupported`, which tells "the object cannot do
  this" from "it failed while doing it".
- CI: a mutation check (`dev/tools/mutate.py`, workflow "Mutation"). Every
  listed mutation of a flush, barrier, poison, directory-sync or accounting
  site must be killed by the engine and simulator tests; a survivor, or a
  mutation that does not compile, fails the run.

### Changed

- `store-io-engine`: the blocking calls are built on the engine layer
  (PLANNING R3.3): every operation of a blocking call lives in a row of the
  same op table a `Submitter` uses, matched to its completion by row and
  generation, and every write and read verdict (ticket, append reservation,
  poison, read classification) comes from one place for both.
  - An append whose write the platform refused after its space was reserved
    now poisons the store once, so `Store::poisoned` reports the OS code
    (before, its first cause had none).
  - `Store::oldest_outstanding` reports a queue as busy while an operation is
    in flight on it (before: while it was held), from before the operation is
    submitted (a queue that performs it inside `submit` included); the age
    restarts when the queue's oldest operation completes, so one stuck
    behind newer completions keeps aging.
  - A flush leader wakes waiting followers only when one is registered: a
    lone durable write no longer pays a futex system call per barrier.
  - The observer's `Reads` and `ReadBytes` count reads that completed
    successfully and the bytes they returned (before: some paths counted
    every read whose buffer came back, with the requested length).
- **Linux stores use io_uring by default** (`linux_tier: Auto`). Each
  io_uring queue (a store's parallel lanes, created on first use, and each
  submitter) holds two file descriptors, the ring and an eventfd; a queue
  whose ring the kernel cannot set up for want of resources is synchronous
  instead, and the report counts it. `linux_tier: Sync` uses no io_uring
  (one synchronous system call per operation, as in 0.3; calls with several
  buffers run on the submit pool's threads).
- `StoreReport` is `#[non_exhaustive]`; `StoreOptions` gains `max_write`,
  `large_buffers`, `linux_tier` and `register_file`, and `StoreReport`
  gains `io` and `unavailable`.
- `dev/ROADMAP.md`: the v0.4 section describes the approved scope (the engine
  layer, the io_uring backend with no kernel worker threads, the synchronous
  tier's worker pool, runtime tier selection, IoRing evaluated and not
  adopted) and its exit criteria.

### Fixed

- `store-io-engine`: a flush that panicked left its leader's place taken,
  so every later barrier on the store waited forever. The leader now
  poisons the store (the flush's outcome is unknown) and gives leadership
  back, so later barriers fail at once with `Poisoned`. Test with a
  panicking flush.
- `store-io-win`: every data flush allocated (the status block of
  `NtFlushBuffersFileEx`), so a durable write did too. Each file of a queue
  now keeps one status block and reuses it: a warm blocking durable write
  allocates nothing on Windows either (`tests/allocs.rs`, Linux and
  Windows; gate G8a).
- `store-io-engine`: `release` waited for the region's operations in flight
  while holding the store's metadata lock, so an operation whose holder
  needed that lock before finishing (a scan visitor provisioning a region;
  now also a `Submitter` thread provisioning before it reaps) deadlocked
  with it. It now waits first and takes the lock after. Test with a
  submitter; found in review.
- `store-io-engine`: **operations abandoned on a queue could affect the next
  operation on it.** An operation is abandoned when the wait for it fails (a
  Windows IOCP port can fail a wait; the Linux synchronous queues never do)
  or when a scan's visitor panics with reads in flight (any platform).
  - **Before.** The queue went back to the store with those operations still
    in flight. The next user could take their completions for its own: a
    read could return another read's bytes, a large read could deliver wrong
    ranges, a batch could be acknowledged before its own writes completed,
    and a queue full of abandoned operations refused everything forever.
  - **Now.** Every user of a queue first collects and discards what an
    earlier user abandoned (a completion means the device is done with its
    buffer), so it always starts on an empty queue; on a clean queue that
    is one length check each time a call takes the queue. A queue that
    cannot even be cleared fails the call: with `Io` before any space is
    reserved, or fail-stop inside a metadata update. Tests reproduce each case on the simulator with
    out-of-order completions; each failed before the fix.
- `store-io-engine`: a write refused by a full queue that did not return its
  buffer (a backend breaking its contract) fails the call; before, it was
  skipped and the call could still report every write done.
- `store-io-engine`: a failed wait is a failure of the queue, never a verdict
  on the media.
  - **Before.** Its error code went through the media-error path. A single
    read was reported as `Corruption(MediaError)` and rescanned; at open, a
    slot whose read was abandoned was weighed as unreadable, so the open
    could use the older region table, and a later provisioning overwrote the
    newer one, losing a region. On the shipped backends a wait error has
    never carried a media-class code, so this was seen on the simulator.
  - **Now.** An abandoned operation is its own outcome: a read fails with
    `Io` and its raw code, a write or flush is `DurabilityUnknown` and
    poisons (no receipt), and `create` never reclaims a container it could
    not read. Clearing an earlier caller's abandoned operations happens
    before a call reserves space or submits anything; if it fails, the call
    fails with `Io` and nothing of it happens.
- `store-io-engine`: open no longer drops a region silently when it cannot use it.
  - **Which regions.** A ready table entry whose container is too short, whose header has no valid copy, or whose header holds impossible values, names another region or kind, or says released.
  - **Before.** Such a region was skipped, and a lookup of its name returned `NotFound`, the same answer as for a name that never existed. A caller could take that for a fresh start while the data was still on disk.
  - **Now.** Such regions are reported with the reason, and a lookup returns `Corruption` naming the region.
- `store-io-engine`: a failed region read is reported accurately.
  - **Before.** Every failure was reported as `Corruption(MediaError)` and the raw code was dropped, so an invalid argument, an exhausted resource or a bad handle looked like a failing disk.
  - **Now.** A media error names the exact unreadable range, found by the scan's bisection. Any other failure is `Io` with its raw code and the byte range.
- `store-io-engine`: a read-only open needs no write rights, and readers share the store.
  - **Before.** `open_readonly` opened the container read-write to take the exclusive ownership lock. It failed on a read-only mount, a write-protected file or a backup copy, which is where a recovery tool most needs it, and two readers excluded each other.
  - **Now.** It opens one read-only handle and takes a shared lock on it. Any number of readers can open a store at once; readers and the writer still exclude each other.
- `store-io-engine`: **growing the container could bring a truncated region
  back as healthy.**
  - **Before.** A region cut off by a short container was reported at open,
    but provisioning or reserving afterwards grew the container over its
    missing tail, zero-filled. The next open found the region whole and
    ready, its lost bytes read as zeros; an append region silently ended
    early.
  - **Now.** While such a region exists, growth is refused with
    `Corruption(Truncated)` naming it, and it stays reported.
- `store-io-engine`: **a transient read error at open could roll back
  committed metadata.**
  - **Before.** Any failed slot read (a bad handle, an exhausted resource,
    an invalid request) counted as an unreadable slot. On the region table,
    one such error made the open choose the older table copy, so a region
    provisioned since then was missing (`NotFound`) with no error. On a
    region header, a healthy region was marked unavailable for the session.
  - **Now.** A slot is unreadable only on a media error, a short read or a
    read past the end of the file. Any other failure fails the open with
    `Io` and its code.
- `store-io-engine`: **opening a store could report a healthy store as
  corrupt.** A transient read error (not a media error) while looking for the
  volume record made open skip that location; on a store that had only been
  created, the second copy is still blank, so open returned
  `Corruption(HeaderCrc)`. A caller that recreates a store on corruption
  would have destroyed it. Such an error now fails the open with `Io`.
- `store-io-engine`: when open finds no valid copy of a metadata pair (the
  volume record, the region table) and a read met a media error, it reports
  `Corruption(MediaError)`, not `Corruption(HeaderCrc)`: the error may be
  transient, and "corrupt" invites a caller to recreate the store. This
  covers the search's probe reads and the full slot reads.
- `store-io-engine`: a volume record from a newer format (a payload longer
  than 48 bytes, or a non-zero `features` field) is refused with
  `Unsupported(FormatVersion)` instead of being reported as corruption or
  accepted and rewritten without its extra bytes.
- `store-io-engine`: the bisection of a failed read halves in whole blocks
  while a piece spans several, so every read of a block or more stays
  block-aligned. Before, an odd number of blocks could be split mid-block,
  which a device with large sectors refuses, aborting the scan.
- `store-io-engine`: splitting a failed read below the store block now
  happens only where the probe reported the direct-I/O alignment. A device
  that refuses smaller reads (for example dm-crypt with 4 KiB sectors over a
  512e disk, on a kernel without `STATX_DIOALIGN`) made the scan abort with
  `Io(EINVAL)`; a refused sub-block read is now reported unreadable and the
  scan continues.
- `store-io-engine`: when the rescan that locates a media error fails (a
  short buffer pool, a full queue), the media error is still reported over
  the whole read, not replaced by that failure. The failed read's buffer is
  released before the rescan.
- `store-io-engine`: the flush domain's statistics counters sit on a cache
  line of their own, so counting no longer evicts the line every barrier
  reads.
- `store-io-core`: `errno::is_unsupported` matches each operating system's
  own errno numbers (Linux and macOS number them differently) and is
  documented as valid for flush and sync results only. New
  `errno::is_end_of_file`.
- `store-io-posix`: a malformed `uevent` device name (one ending in `/`) gave
  the probe an empty device name, so it looked up the wrong ext4 options file
  and skipped its fallback to the mount source. The mount options were then
  reported missing (a more cautious class, never a wrong one). Such a name is
  now no answer, and the fallback runs. Found by the new probe fuzz target.
- `store-io-engine`: a media error names the exact failing sector.
  - **Before.** Bisection stopped at the 4 KiB store block, so one bad 512-byte sector cost the caller seven readable ones. A read larger than one buffer named only the whole requested range.
  - **Now.** Scans and reads bisect down to the device's logical block (or the direct-I/O alignment, if larger) where the probe reports it, and every read names the exact unreadable range.
- `store-io-engine`: `Directory` refuses a change it could never make durable before writing anything.
  - **Before.** On a file system that cannot flush directories, `replace` wrote the temporary file and renamed it over the target, then failed on the directory flush. The target had already changed, with no durability.
  - **Now.** `Directory::open` tries a directory flush. If it is unsupported, every `replace` and `remove` returns `Unsupported(DirectoryFlush)`; if it failed otherwise, `Io(SyncDir)`. In both cases nothing is written.
- `store-io-engine`: a failed `set_len` during a `Directory` replace is fail-stop. It returns `DurabilityUnknown` and poisons the handle, as a failed write does; before, the handle stayed usable. The target keeps its old contents.
- `store-io-engine`: the test that a flush leader never waits for a shared queue is now deterministic. It holds the only queue and requires the flush to finish anyway; the old test depended on timing and could pass without the flush lane.

---

## [0.3.1] - 2026-10-12

A patch release for five defects found in 0.3.0 while recording its design
changes. Two could lose acknowledged data and one could hang a store. Upgrade
from 0.3.0; the on-disk format and the public API are unchanged.

### Fixed

- `store-io-engine`: **`Store::create` could truncate a real store.**
  - **The defect.** `create` reused an existing `store.sio` as an unfinished create, emptying it, when three conditions held:
    - the file was unlocked;
    - it was no larger than 256 KiB;
    - its volume record could not be found.
  - **How data could be lost.** The volume record could also be missing for a store that:
    - hit a transient read error;
    - had both volume copies damaged;
    - was written by a newer format;
    - was opened while the buffer pool was empty.
  - Small stores holding slots and small regions fit in 256 KiB, so a caller that answered a failed `open` with `create` could lose every acknowledged byte.
  - **The fix.** `create` now reuses the file only after reading every block in full and finding no slot header whose header CRC verifies. A store, damaged or not, is never touched (`AlreadyExists`). A create that crashed before its metadata was durable is still taken over.
- `store-io-engine`: **release and recycle could lose acknowledged writes, and a slot could read back its previous version after a commit.**
  - **The defect.** After writing a metadata slot durably, store-io read the pair back from the device. That read could fail (for example with an empty buffer pool) or see the new copy unreadable. It was then reported as "not written", or the older copy was kept as current:
    - a failed release or recycle left the region open for writes although the release or recycle was already durable;
    - a slot's `read()` could return the version before a commit that had just returned its receipt.
  - **The fix.** The new state of the pair is now derived from the exact bytes written, validated in memory. Nothing is read back after a durable metadata write.
- `store-io-engine`: **slot commits and barriers could deadlock.**
  - **The defect.** A slot commit, header rewrite, table update or provisioning step kept its I/O queue while waiting for a flush. The thread leading that flush borrowed a queue from the same set, so with every queue held this way the store hung until `Store::poison()`.
  - **The fix.** The flush leader now uses a queue of its own, which no other thread uses, so it never waits for one.
- `store-io-engine`: **a provisioning fill write that failed after submission did not poison the store.**
  - **The defect.** A short write returned `DurabilityUnknown`, and an I/O error returned `Io`, while the store went on accepting writes.
  - **The fix.** Both now poison the store (fail-stop) and return `DurabilityUnknown`. Running out of space is still `NoSpace` and poisons nothing.
- `store-io-engine`: **`AppendRegion::resume_at` on a read-only store could overstate durability.**
  - **The defect.** It moved `durable_through()` with no open-time device flush behind it.
  - **The fix.** It is now refused there with `NotWritten(ReadOnly)`; appends are refused on read-only stores anyway.
- `docs/FORMAT.md` said a file of at most 256 KiB "cannot hold region data". It can, and the create rule above no longer relies on that.

### Changed

- `store-io-engine`: every store opens one more platform queue than
  `StoreOptions::queues`, used only by the thread leading a device flush.
- `store-io-engine`: one device read fewer per metadata commit (slot commit,
  header rewrite, table update, provisioning, writable open).

---

## [0.3.0] - 2026-10-09

The first usable release. Stores, regions, batches, slots, scans,
recycle and release, reservations and directory mode, through a simple API
on Windows (IOCP) and Linux (direct I/O), with durability classes decided
from device evidence, unforgeable receipts and fail-stop poisoning. Verified
on a deterministic simulator with crash enumeration and fault injection,
loom and Miri, a conformance suite on NTFS, ext4 and XFS, a real-kernel
device-error run, and a performance harness against the raw primitive and
fsys.

### Added

- `store-io-format` crate (workspace member): CRC-32C with SSE4.2 and
  AArch64 hardware paths on three interleaved streams, recombined with
  compile-time shift tables, plus a slicing-by-8 fallback; verified against
  the standard check value, the RFC 3720 vectors and a bit-at-a-time
  reference at every tail length and alignment.
- `dev/ROADMAP.md`: architecture phase closed (file map, scenario review,
  requirement traceability, independent critique resolved).
- `dev/TODO.md`: Windows ownership-lock kill test.
- `store-io-format`: bounds-checked little-endian codec; the A/B slot
  header (encode and validate in a fixed order: blank, magic, header CRC,
  version, flags, identity, location, payload CRC); slot-pair winner logic
  (confirmed, unconfirmed, fork, refused, lost; generation-gap and chain-break
  anomalies); volume, region-table and region-header payload codecs with
  overlap, duplicate, alignment and id checks; the keyed fill pattern v1.
  Tests include a slot overwrite torn at every byte boundary (never accepted
  as a mixed version) and every single-bit flip (never accepted).
- `store-io-core` crate: identities (`VolumeId`, `RegionId`, `Generation`);
  durability classes, receipt labels and the reason / missing-evidence
  vocabularies; the error model (`NotWritten` and `DurabilityUnknown` never
  conflated, raw OS codes kept, no payload bytes, at most 64 bytes); errno
  classification by stage; the device evidence model; the pure class
  decision (unsafe and unverified rules, power-safe only on device evidence
  or an exact-model certificate, attestation and labelled override); the
  corrected untorn-unit rule; checked alignment helpers.
- `store-io-buf` crate: page-aligned arena (anonymous `mmap` with
  `MADV_DONTFORK` on Linux, `VirtualAlloc` on Windows, the global allocator
  under Miri); power-of-two size classes with lock-free bounded MPMC free
  lists; owned `IoBuf` that returns to its class on drop and never prints its
  contents; sensitive pools that wipe on return and lock their arena in RAM
  and out of core dumps where the OS allows; `wipe` with volatile writes;
  `CachePadded`.
- `store-io-platform` crate: the completion-shaped I/O boundary. `Platform`
  (metadata operations and queue factory) and `Queue` (`submit`, `reap`,
  `wait`, never with a timeout); `IoOp` owns its buffer and the buffer returns
  in the `Completion` or `Rejected`; validated single-component `FileName`;
  a fixed-capacity `CompletionBuf` that never grows on the hot path.
- `store-io-sim` crate: deterministic simulated platform. One device with a
  volatile write cache; a flush persists exactly the writes that completed
  before it was submitted; FUA and power-safe writes are durable at
  completion; directory entries need a directory sync; operations take
  effect at completion and can complete out of order; crashes keep any
  subset (or a seeded random tearing) of cached writes, invalidate handles
  and release locks; injectable faults (failed writes and flushes, flush
  failure that drops the cache, lying flushes, short, lost and misdirected
  writes, latent sector errors, out of space, page-cache pollution); trace
  hash for same-seed replay checks.
- `store-io-engine` crate: the engine over any platform.
  - Flush domain with the join rule and leader-inline flushing: a lone
    durable write costs one flush; concurrent barriers share flushes with no
    timer or window; a flush is issued only after the barrier's ticket, so it
    covers every write that completed before it; a failed flush poisons the
    device forever and is never retried.
  - Lock-free append frontier: atomic tail reservation, out-of-order
    completion, contiguous completed and durable prefixes; ring-slot
    accesses are acquire-release read-modify-writes so no completion is
    stranded under any memory model (found by loom).
  - `Store`: `create` (exclusive, probed, refused on unsafe or unverified
    devices unless attested or overridden, parent directory synced), `open`
    (ownership lock, drain a dead owner's writes, metadata from A/B slots,
    every ready region verified against its header, owner generation bumped),
    `open_readonly` (no write rights, modifies nothing), provisioning with
    direct-I/O fill, full flush, verification and table flip, never-reused
    region ids, the device report, explicit `poison`.
  - `AppendRegion` (store-chosen positions, blocks never rewritten,
    `NotPositioned` after a reopen until `resume_at`), `PageRegion` (checked
    positions, foreign and stale positions rejected), `Slot` (old or new after
    any crash), unforgeable `RegionPos`, `WriteTicket` and `DurableReceipt`.
  - End-to-end simulator tests: receipts survive every crash subset, fsyncgate
    stays poisoned, slots never mix, power-safe barriers never flush, read-only
    opens change nothing.
- `store-io-win` crate: the Windows backend. Unbuffered overlapped I/O
  reaped from a per-queue I/O completion port (`GetQueuedCompletionStatusEx`,
  waits are infinite or zero, never timed); every queue reopens its own handle
  to a file, so any number of queues share a file and each reaps only its own
  completions; synchronous completions skip the port. A durable write is the
  write followed by the data flush (`NtFlushBuffersFileEx` data-sync on NTFS,
  `FlushFileBuffers` elsewhere), never write-through alone. Rename and delete
  by handle with POSIX semantics; an ownership lock on a sentinel byte;
  allocation with valid-data checks; the probe reads the volume, file system,
  storage stack and, on NVMe, the identify data, write-cache feature and
  health log, all parsed as untrusted bytes. Dropping a queue with I/O in
  flight cancels it and waits for every completion before any buffer is
  released.
- `store-io-posix` crate: the Linux synchronous backend (tier T3).
  `O_DIRECT` handles opened relative to their directory with `openat2`
  (no symlinks, no escaping the directory), or `openat` with `O_NOFOLLOW` on
  older kernels; `pwritev2` / `preadv2`, with `RWF_DSYNC` for a durable write on a
  file and write then `fdatasync` on a raw block device; open-file-description
  locks; `fallocate` with size checks; unwritten and shared extents from
  FIEMAP and resident pages from `cachestat`; the probe reads `statx`
  (direct-I/O and atomic-write limits), the file system and its mount
  options, the block stack from sysfs (device-mapper, MD, loop, virtual
  disks, hypervisor detection) and, where permitted, NVMe identify data.
- `store-io-engine`: the batch layer. `AppendBatch` packs records back to
  back into pooled aligned buffers as they are added (one copy, or none with
  `append_with`, which lends a zeroed slice to encode in place) and commits
  with one reservation, the fewest device writes kept in flight together, and
  one barrier: 100 records of 64 bytes are one write and one flush.
  `PageBatch` checks writes as they are added, refuses overlaps, and submits
  them together before one barrier. Both report exact positions.
- `store-io-engine`: a region with at least 1 MiB of data starts its data
  area on a 1 MiB boundary, so large transfers never straddle the units below
  the file system. On ext4 over a virtual disk, direct 1 MiB writes 48 KiB off
  a boundary measured 35% slower; the harness's sequential writes there went
  from 0.58x to 0.81x of raw. A reservation is charged only the extent: the
  gap comes from growing the file, or the region is placed unaligned when the
  disk is full, so a reservation's guarantee holds.
- `store-io-engine`: reads larger than one pooled buffer keep eight reads
  in flight (the scan pipeline) instead of one at a time; the harness had
  measured them at 33% of raw bandwidth with 8 MiB requests.
- `store-io-engine`: appends and page writes of any length; data larger
  than the largest pooled buffer is written in pieces kept in flight together,
  reusing at most eight buffers.
- `store-io-engine`: appends wait for the append ring to drain instead of
  failing with `TooManyInFlight` (`AppendFrontier::reserve_wait`, woken by
  completions, never by a timer).
- `store-io-engine`: a reservation guard poisons the device domain if an
  append's reserved range is abandoned unwritten, including by a panic.
- `store-io-engine`: raw ordered scans (`AppendRegion::scan`,
  `PageRegion::scan`, `ScanItem`, `ScanSummary`). Eight large direct reads in
  flight, delivered strictly in order to a visitor that decides where valid
  data ends. A scan never stops at bad data: a failing read is bisected to
  whole blocks on an idle queue, unreadable ranges are reported (adjacent ones
  merged) and skipped, and short reads report their missing bytes. Scans work
  on read-only and poisoned stores and change nothing.
- `store-io-engine`: recycle and release, run only when the caller asks.
  `recycle` waits for every operation in flight on the region, writes the
  next generation into the region header, makes it durable, and returns the
  new generation's handle; it does no other I/O. `release` marks the region
  released in the region table, then in its header, then deallocates its
  space, which later provisioning refills before reuse. The table goes first
  so an interrupted release never leaks space. Handles, tickets and positions
  of an old generation or a released region are refused
  (`StaleGeneration`, `NotReady`); `generation()` on region handles.
- `store-io` (facade): the simple API on the native backend, with no
  generics in user code. `Store::create`, `Store::open`, `Store::open_with`
  and `Store::open_readonly` take a path; the store derefs to the engine for
  regions, slots, batches, scans, recycle, release, reservations and the
  report. `AppendRegion`, `PageRegion`, `Slot`, `AppendBatch`, `PageBatch`
  and `Reservation` name the native types; `Directory::open` opens a
  directory-mode handle; `probe` reports the evidence and class of a
  directory's device. Every error and evidence type is re-exported.
  `NativePlatform` is the Windows backend on Windows and the Linux backend on
  Linux.
- `store-io-conformance` crate: the suite every platform backend must pass,
  on any file system. Fifteen scenarios: the platform contract (exclusive
  create and not-found mapping, sizes, many operations in flight completing
  once each with their buffers, durable writes and flushes, queue limits,
  read-only handles, rename, unlink and directory sync, the ownership lock,
  range state after a full write, release, the probe listing what it could
  not read) and store behaviour on the platform (round trip through a
  reopen, a read-only open that changes nothing, ownership, directory
  replace). It passes on the simulator, NTFS and ext4.
- CI: a dm-error job puts the file system on a device-mapper target that
  is switched mid-run to fail every I/O; no write may succeed after the
  first failure, and every acknowledged record must survive healing.
- CI: conformance and end-to-end tests on loop-mounted ext4 and XFS images
  with barriers on (`STORE_IO_TEST_DIR`).
- `store-io-posix`: `PosixPlatform` is `Clone` (clones share the retry
  counters).
- `store-io-engine`: compile-fail suite (rustdoc `compile_fail` tests, no
  third-party harness): an integer never type-checks as a position, a
  position or receipt cannot be built by hand, a ticket is not a position and
  a receipt is not a ticket, a ticket cannot be copied, and dropping a ticket
  unused is flagged. Each case has a compiling twin.
- CI: a Miri job checks the unsafe buffer code (arena, pool, lock-free
  queue, secure wipe) for undefined behaviour; the heavy concurrency tests
  run fewer iterations under Miri.
- CI: a loom job model-checks the buffer pool's free lists, the append
  frontier, the flush domain and the region gate.
- `harness/`: the performance harness (a standalone, unpublished workspace)
  and its first results on the development machine (Windows NTFS on a
  consumer NVMe; WSL2 ext4). Against the raw write-and-flush primitive and
  fsys 1.1.3: a lone durable 4 KiB append runs at 1.06x raw and 1.29x fsys
  (43x fsys's default configuration); 64 concurrent durable writers at 23x
  raw and 1.76x fsys; `AppendBatch` at 1.0-2.6x raw and 1.2-4x fsys;
  sequential 1 MiB writes at 1.06x raw and 3.5x fsys. Results name the
  device, class, file system, OS and store options.
- `docs/GUIDE.md`: a walk through store-io from the first durable write to
  recovery (receipts, regions, batches, many writers, slots and directory
  files, scans and `resume_at`, the device report, every error and what to
  do, reservations, recycle and release, tuning).
- `docs/FORMAT.md`: the on-disk format of a store, precise enough for an
  independent reader (byte layouts, CRC coverage, validation order, winner
  rules, fill pattern, crash consistency of every change).
- `docs/PLATFORMS.md`: what store-io does on Windows and Linux, the
  durability primitive behind each operation, and known limits.
- `docs/DURABILITY.md`: classes, evidence, every reason and missing item,
  trust and labels, the flush domain and join rule, `covers()`, fail-stop.
- `README.md`: status, quick start and the documentation index.
- `docs/API.md`: every public item, grouped by purpose (replaces the 0.1
  placeholder).
- `store-io`: re-exports `RegionKind`, `DomainStats`, `CertId`, `Reason`,
  `Missing` and their sets, and `DurableOpen`.
- `store-io` binary (feature `cli`): `store-io probe <dir>` and
  `store-io info <store>` (read-only).
- End-to-end tests of the simple API on the machine's real file system
  (NTFS, ext4); where a device is refused they use the labelled override and
  check that every receipt carries it.
- `store-io-engine`: directory mode. `Directory::replace` swaps a whole
  file atomically and durably (temporary file in the same directory, exact
  length, full flush, rename over the target, directory flush); after any
  crash the file holds exactly the old or the new bytes. A failure from the
  rename on, or a failed write or flush of the new bytes, is
  `DurabilityUnknown` and poisons the handle; an unsupported directory flush
  is never success. `read` (direct I/O), `remove` (durable) and `sync`. The
  device is probed with the first file and refused per the caller's `Trust`.
- `store-io-platform`: `Platform::set_len` (exact file size; `ftruncate` on
  Linux, end-of-file information on Windows).
- `store-io-core`: `errno::is_already_exists`, judged per numbering.
- `store-io-core`: `errno::is_not_found`, judged per numbering (errno,
  Win32, NTSTATUS); the engine no longer treats errno 3 as "not found".
- `store-io-sim`: `FaultPlan::fail_dir_sync` and `World::dir_syncs()`.
- `store-io-engine`: space reservations and accounting. `Store::reserve`
  checks the tag's hard cap (`Store::set_cap`) and grows the container so that
  regions provisioned from the `Reservation` can never run out of space; it
  is the only place a full file system or quota surfaces, as
  `NoSpace { tag }`, and nothing is poisoned. Provisioning without a
  reservation grows the container past every outstanding reservation, so it
  never takes space promised to another tag. `Store::space` reports the
  container, ready and released regions, reservations, and logical and
  physical usage per tag; `Store::extent_of` gives a region's footprint.
- `store-io-format`: region table entries record the caller's
  space-accounting tag (4 of the 16 reserved bytes), so per-tag usage is
  derived again on every open.
- `store-io-engine`: per-region admission gate. Every data operation holds a
  pass for its I/O; recycle and release retire the region and wait for the
  last pass, so no write of an old generation lands after its successor
  exists. Read-modify-write ordering, proven with loom.
- `store-io-engine`: `Debug` for `Store`, the region handles and batches.
- `store-io-core`: `errno::is_media_error` tells device read failures apart
  from bad requests (errno, Win32 and NTSTATUS codes).
- `store-io-sim`: `World::reads()`, `World::file_id()` and
  `FaultPlan::short_read`.
- `store-io-sim`: `World::writes()` for arming write faults; data transfers
  must meet direct-I/O alignment (offset and length in logical blocks, buffer
  4096-aligned) or complete with `EINVAL`, as on real devices.
- `store-io-core`: `NotWrittenCause::Overlap`.
- `store-io-core`: `NotWrittenCause::NotPositioned`.
- `store-io-core`: `OsError` implements `std::error::Error`, so raw
  platform results work with `?` in callers returning boxed errors.
- `store-io-sim`: `World::flushes()` for arming flush faults.
- `store-io-buf`: loom-aware spin hint in the free-list retry loops.
- Workspace-wide package metadata and the REPS lint set as `[workspace.lints]`.
- CI packages every crate in dependency order (`cargo package --workspace`).
- `dev/ROADMAP.md`: v0.2 and v0.3 exit criteria marked with what is verified and what remains.
- `dev/DIRECTIVES.md`: the simple-API rule (simple, batch and engine layers;
  `docs/GUIDE.md` tutorial) and the rule that a lone durable write, concurrent
  writers and caller batches are all fast and all gated.
- `dev/ROADMAP.md`: performance gates for the lone-writer, concurrent-writer
  and caller-batch patterns; the phase 1 API sketch is layered.
- `dev/TODO.md`: the later list (hardware, measurements, settings changes,
  in-house replacements for `thiserror` and `zeroize`, design gaps).

### Changed

- `store-io-engine`: receipts are exact. A ticket records the first device
  flush that can cover its write and a receipt the flush its barrier waited
  for; `DurableReceipt::covers` holds exactly when the write completed before
  that flush (or, for append regions, lies in the durable prefix). Tickets and
  receipts from different opens of a store never match.
- `store-io-core`: `Op`, `NotWrittenCause`, `CorruptionKind` and `Capability`
  are `#[non_exhaustive]`.
- `dev/ROADMAP.md`: research phase progress. Thirteen research tracks, a
  critic pass and the synthesis are complete; requirement amendments await
  approval; measurements on bare-metal Linux, a power-loss-protected NVMe and
  macOS are outstanding.
- Hardware-bound measurements move to `dev/TODO.md` (recorded in the roadmap
  per the anti-deferral rule); they gate the certified 1.0, not usable 0.x
  releases.
- `dev/ROADMAP.md`: research phase closed; build order changed so the first
  usable release (v0.3) covers Windows and the Linux synchronous tier, with
  io_uring, macOS, multi-device and passthrough after it and certification at
  1.0.
- `dev/ROADMAP.md`: Linux CI conformance runs on loop-mounted ext4 and XFS
  images with barriers on; the probe found that GitHub's Ubuntu runners mount
  their root filesystem `nobarrier`, which store-io classes as unsafe.

### Fixed

- `store-io-win`: data flushes went through overlapped handles. On such a
  handle `NtFlushBuffersFileEx` may return `STATUS_PENDING`, which was
  counted as success (a barrier could report durability before the flush
  finished), and the kernel would then write its status block into a stack
  frame that had already returned; `FlushFileBuffers` would wait forever on
  a queue handle that never signals. Every flush now goes through one
  synchronous handle per writable file, and a pending status is an error
  with its status block kept alive. It never triggered in 800 attempts on
  the development machine (found by the benchmark harness).
- `store-io-win`: `allocate` to a smaller size truncated the file, unlike the
  other backends; it now never shrinks, as the platform contract says (found
  by the conformance suite).
- `store-io-engine`: with keyed fill, the first chunk of every region (about
  1 MiB) was written as zeros instead of the pattern; the pattern now starts
  at the first byte of the data area.
- `store-io-engine`: provisioning counted only ready entries against the
  region table's capacity and sized the table encoding for the whole slot,
  so a table near full failed in the middle of provisioning and poisoned the
  store. Released entries now count, and a full table is refused with
  `NoSpace(Table)` before any I/O.
- `store-io-engine`: a metadata write that failed before reaching the
  device poisoned the store; only failures after submission poison now.
- `store-io-engine`: a region skipped at open (its header failed to verify)
  left its name free, and reusing the name made the table undecodable; names
  are now checked against every ready table entry.
- `store-io-engine`: a slot pair carrying a read-only-compatible feature
  this version does not know is refused (`Unsupported(FormatVersion)`)
  instead of overwritten.
- `store-io-engine`: release deallocated the region's header together with
  its data, undoing the Released record; only the data area is released now.
  On Windows release trims in place instead of making the container sparse,
  which had cost the store its power-safe class for good.
- `store-io-engine`: a crash during `create` left a container that would not
  open and blocked every later `create`; `create` now takes over a container
  that never finished (unlocked, no larger than the metadata area, no volume
  record) and refuses anything else.
- `store-io-engine`: a flush refused without an OS error was reported with
  an invented error code; it now carries none.
- `store-io-sim`: a read wholly past the end of a file panicked instead of
  transferring nothing.
- Doc comments that promised more than the code does (`UntrustedBus`,
  `UserPowerProtection`, the table's entry limit, fuzzing of the format
  crate).
- `store-io-engine`: concurrent durable writers no longer queue behind one
  another's flushes. A barrier's followers held an I/O queue for the whole
  flush wait, and a caller finding every queue busy blocked on one fixed
  queue; now only the flush leader borrows a queue, for the flush alone, and
  a caller waits for whichever queue is returned first (found by the
  benchmark harness with 64 writers).
- `store-io-engine`: an append or page write larger than the largest pooled
  buffer failed with `PoolExhausted`; a page read did not check the
  position's volume.
- Lints that only fire on some toolchains and platforms: the free-list
  sequence comparison is a `match` (clippy 1.85), Linux `madvise` calls
  carry their safety comments in dedicated helpers, and the simulator's
  crash path takes the cache with `mem::take`.

---

## [0.1.0] - 2026-10-08

The project scaffold. Reserves the crate name; no public API.

### Added

- Crate manifest (edition 2024, MSRV 1.85, `Apache-2.0 OR MIT`), with the
  facade crate as the workspace root so backend, simulator, conformance and
  harness crates can join as members.
- `src/lib.rs` with the REPS crate-root lint set and the crate overview.
- `README.md`, `CHANGELOG.md`, `REPS.md`, `LICENSE-APACHE`, `LICENSE-MIT`.
- `dev/DIRECTIVES.md` (engineering standards, definition of done, release
  ceremony) and `dev/ROADMAP.md` (research, architecture and build phases
  with exit gates and per-device-class performance gates).
- CI: format, clippy, test and doc on Linux, macOS and Windows, on stable and
  1.85; a package dry run; `cargo audit` and `cargo deny`.
- `rust-toolchain.toml`, `clippy.toml`, `rustfmt.toml`, `deny.toml`,
  `.gitattributes`, `.github/FUNDING.yml`.
- `docs/API.md` and `docs/release/v0.1.0.md`.

[Unreleased]: https://github.com/jamesgober/store-io/compare/v0.4.1...HEAD
[0.4.1]: https://github.com/jamesgober/store-io/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/jamesgober/store-io/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/jamesgober/store-io/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/jamesgober/store-io/compare/v0.1.0...v0.3.0
[0.1.0]: https://github.com/jamesgober/store-io/releases/tag/v0.1.0
