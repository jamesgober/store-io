//! Where a region's data lies in the container.
//!
//! Every engine site that turns a region offset into a file offset goes
//! through [`RegionMap::locate`], so a region whose data is split across
//! extents (format 2) changes this module, not the I/O paths. A region's
//! extents, in data order, are coalesced into runs (adjacent in the file,
//! spec §11); one device operation never crosses a run. A region in one run
//! (every format-1 region, and a format-2 region that never grew elsewhere)
//! pays one load more than its bounds check.
//!
//! A growth adds to the map while I/O runs, under the region lock (the one
//! writer): an extent that continues the last run raises the capacity; any
//! other gets a new run, its slot written before the run count is published
//! and the count before the capacity; the length is published last, after
//! the growth's commit. Runs are never removed or moved. A reader loads the
//! length first, so every run it can reach below that length is visible to
//! it; the count and the capacity it then loads may be newer, which only
//! adds runs past the length.

use std::sync::OnceLock;

use store_io_format::footprint::runs;
use store_io_format::meta2::{Extent, max_extents};

use crate::sync::{AtomicU64, Ordering};

/// A run after the first: its region offset and its file offset.
#[derive(Debug)]
struct Slot {
    start: AtomicU64,
    file: AtomicU64,
}

/// Slot chunks: chunk `c` holds `2^c` runs after the first, made when the
/// first of them arrives, so a map holds at most twice the slots its runs
/// need (never the room for every run a header may list); the chunk array
/// itself is made with the first run after the first.
const CHUNKS: usize = 12;
const _: () = assert!(max_extents(16) - 1 < (1 << CHUNKS));

type Chunks = [OnceLock<Box<[Slot]>>; CHUNKS];

/// `n` empty slots.
fn slots(n: usize) -> Box<[Slot]> {
    (0..n)
        .map(|_| Slot {
            start: AtomicU64::new(0),
            file: AtomicU64::new(0),
        })
        .collect()
}

/// The chunk of run `j` after the first, and its index there.
fn place(j: usize) -> (usize, usize) {
    let c = (usize::BITS - 1 - (j + 1).leading_zeros()) as usize;
    (c, j + 1 - (1 << c))
}

/// A region's data placement: its runs, in data order, and its length.
#[derive(Debug)]
pub(crate) struct RegionMap {
    /// The file offset of region offset 0 (the first run's).
    first: u64,
    /// The data's length (`data_size`): only `[0, len)` is addressable. It
    /// only grows, published by a growth after its commit (Release), so a
    /// range a reader checked against an earlier length stays inside.
    len: AtomicU64,
    /// The runs' total length (the capacity): `len` at most; only grows.
    capacity: AtomicU64,
    /// Runs after the first published (`count`, Release after their slots).
    count: AtomicU64,
    /// Their slots, in chunks (nothing for a region in one run).
    slots: OnceLock<Box<Chunks>>,
    /// The most runs after the first (from the header's extent limit).
    room: usize,
}

impl Clone for RegionMap {
    /// A snapshot (taken under the region lock, as a recycle does).
    fn clone(&self) -> Self {
        let copy = Self::new(self.first, self.len(), self.capacity(), self.room);
        copy.fill_slots(&self.more());
        copy
    }
}

impl RegionMap {
    /// A map of one run whose runs after it may number `room`.
    fn new(first: u64, len: u64, capacity: u64, room: usize) -> Self {
        let map = Self {
            first,
            len: AtomicU64::new(len),
            capacity: AtomicU64::new(capacity),
            count: AtomicU64::new(0),
            slots: OnceLock::new(),
            room,
        };
        // Loom does not model the cells: every chunk the room may need
        // exists before the map is shared.
        #[cfg(loom)]
        if room > 0 {
            for (c, chunk) in map.chunks_made().iter().enumerate() {
                if (1 << c) - 1 < room {
                    let _set = chunk.set(slots(1 << c));
                }
            }
        }
        map
    }

    /// One extent: `len` bytes at file offset `base`, all of them data.
    pub(crate) fn single(base: u64, len: u64) -> Self {
        Self::new(base, len, len, 0)
    }

    /// `extents` (a region's committed extents in data order, checked by
    /// the open or the provisioning), of which the first `len` bytes are
    /// data.
    pub(crate) fn from_extents(extents: &[Extent], len: u64) -> Self {
        let mut all = runs(extents.iter().copied());
        // The decoder and the open's checks refuse data past the capacity;
        // one getting through would map past the last extent, so it stops
        // here in every build (once a region, at open).
        let Some(head) = all.next() else {
            assert_eq!(len, 0, "data without extents");
            return Self::single(0, 0);
        };
        let more: Vec<(u64, u64)> = all.map(|r| (r.start, r.file)).collect();
        let capacity = extents.iter().map(|e| e.1).sum();
        assert!(len <= capacity, "data past the capacity");
        let map = Self::new(head.file, len, capacity, more.len());
        map.fill_slots(&more);
        map
    }

    /// Room for `room` runs after the first (the most a growth may add up
    /// to): at construction, before the map is shared. Nothing is made for
    /// it until a run arrives.
    pub(crate) fn with_room(self, room: usize) -> Self {
        if room <= self.room {
            return self;
        }
        let map = Self::new(self.first, self.len(), self.capacity(), room);
        map.fill_slots(&self.more());
        map
    }

    /// Writes `more` (runs after the first) into slots and publishes them.
    fn fill_slots(&self, more: &[(u64, u64)]) {
        for (j, &(start, file)) in more.iter().enumerate() {
            let s = self.slot_made(j);
            s.start.store(start, Ordering::Relaxed);
            s.file.store(file, Ordering::Relaxed);
        }
        self.count.store(more.len() as u64, Ordering::Release);
    }

    /// The chunk array, made with the first run after the first (the
    /// writer only).
    fn chunks_made(&self) -> &Chunks {
        self.slots
            .get_or_init(|| Box::new(core::array::from_fn(|_| OnceLock::new())))
    }

    /// Run `j` after the first's slot, its chunk made if this is the
    /// chunk's first run (the writer only).
    fn slot_made(&self, j: usize) -> &Slot {
        let (c, i) = place(j);
        &self.chunks_made()[c].get_or_init(|| slots(1 << c))[i]
    }

    /// Run `j` after the first (published: below the count the caller
    /// loaded): its region offset and file offset.
    fn run(&self, j: usize) -> (u64, u64) {
        let (c, i) = place(j);
        let s = self.slots.get().and_then(|chunks| chunks[c].get());
        // A published run's chunk is made before the count that publishes
        // it: one missing is a bug that would map to the wrong bytes.
        assert!(s.is_some(), "a published run without its slot");
        s.map_or((0, 0), |s| {
            (
                s[i].start.load(Ordering::Relaxed),
                s[i].file.load(Ordering::Relaxed),
            )
        })
    }

    /// The data's length (`data_size`).
    #[inline]
    pub(crate) fn len(&self) -> u64 {
        self.len.load(Ordering::Acquire)
    }

    /// Publishes a growth's new length, after its commit (never more than
    /// the capacity; never shrinks).
    pub(crate) fn publish_len(&self, len: u64) {
        assert!(len <= self.capacity(), "data past the capacity");
        let _was = self.len.fetch_max(len, Ordering::Release);
    }

    /// Adds `n` bytes at file offset `file` past the capacity (a growth
    /// beyond it; the caller holds the region lock, so this is the only
    /// writer). Continuing the last run it raises the capacity; otherwise
    /// it is a new run. The length is not changed (see [`Self::publish_len`]).
    pub(crate) fn extend(&self, file: u64, n: u64) {
        let count = self.count.load(Ordering::Acquire) as usize;
        let cap = self.capacity.load(Ordering::Acquire);
        let (start, at) = match count {
            0 => (0, self.first),
            _ => self.run(count - 1),
        };
        if at.checked_add(cap - start) != Some(file) {
            assert!(count < self.room, "more runs than the region may have");
            let s = self.slot_made(count);
            s.start.store(cap, Ordering::Relaxed);
            s.file.store(file, Ordering::Relaxed);
            self.count.store(count as u64 + 1, Ordering::Release);
        }
        self.capacity.store(cap + n, Ordering::Release);
    }

    /// The runs' pieces of `[from, to)` (region offsets, at most the
    /// capacity): `(file offset, region offset, length)`, in data order.
    pub(crate) fn pieces(&self, from: u64, to: u64) -> Vec<(u64, u64, u64)> {
        let mut out = Vec::new();
        let mut start = 0u64;
        for (file, n) in self.extents() {
            let (s, e) = (start.max(from), (start + n).min(to));
            if s < e {
                out.push((file + (s - start), s, e - s));
            }
            start += n;
        }
        out
    }

    /// The runs' total length.
    pub(crate) fn capacity(&self) -> u64 {
        self.capacity.load(Ordering::Acquire)
    }

    /// The runs after the first, as published: `(region offset, file
    /// offset)`, ascending.
    fn more(&self) -> Vec<(u64, u64)> {
        let count = self.count.load(Ordering::Acquire) as usize;
        (0..count).map(|j| self.run(j)).collect()
    }

    /// The run holding region offset `pos` among the first `count` runs
    /// after the first: its region offset, file offset and end.
    fn run_of(&self, pos: u64, count: usize) -> (u64, u64, u64) {
        // The first run after the first starting past `pos`.
        let (mut lo, mut hi) = (0, count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.run(mid).0 <= pos {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let (start, file) = match lo {
            0 => (0, self.first),
            k => self.run(k - 1),
        };
        let end = if lo < count {
            self.run(lo).0
        } else {
            self.capacity()
        };
        (start, file, end)
    }

    /// The file offset of `[pos, pos + len)`, which the caller has already
    /// bounded by the region's size, and which lies in one run (one device
    /// operation never crosses runs: see [`Self::run_end`]). A range outside
    /// the map or across a run is a bug that would write or read the wrong
    /// bytes: checked in release builds too.
    #[inline]
    pub(crate) fn locate(&self, pos: u64, len: u64) -> u64 {
        let data = self.len();
        assert!(
            pos.checked_add(len).is_some_and(|end| end <= data),
            "region range {pos}+{len} outside the map ({data} bytes)"
        );
        let count = self.count.load(Ordering::Acquire) as usize;
        if count == 0 {
            return self.first + pos;
        }
        let (start, file, end) = self.run_of(pos, count);
        assert!(
            pos + len <= end,
            "region range {pos}+{len} crosses the run ending at {end}"
        );
        file + (pos - start)
    }

    /// Where the run holding region offset `pos` ends (a region offset),
    /// at most the data's length: an operation from `pos` stays in one run
    /// up to here.
    #[inline]
    pub(crate) fn run_end(&self, pos: u64) -> u64 {
        let data = self.len();
        let count = self.count.load(Ordering::Acquire) as usize;
        if count == 0 {
            return data;
        }
        self.run_of(pos, count).2.min(data)
    }

    /// The bytes from region offset `pos` to the end of its run (at most
    /// the data's length): never none. An operation at or past the data's
    /// end is a caller's bug that would otherwise go on as empty pieces for
    /// ever (a write's or a read's split): checked in release builds too.
    #[inline]
    pub(crate) fn run_left(&self, pos: u64) -> u64 {
        let end = self.run_end(pos);
        assert!(end > pos, "region offset {pos} at or past the data's end");
        end - pos
    }

    /// The runs, `(file offset, length)`, in data order, over the whole
    /// capacity (as published when called).
    pub(crate) fn extents(&self) -> impl Iterator<Item = (u64, u64)> + use<> {
        let more = self.more();
        let cap = self.capacity();
        let starts: Vec<(u64, u64)> = core::iter::once((0, self.first))
            .chain(more.iter().copied())
            .collect();
        let ends: Vec<u64> = more
            .iter()
            .map(|r| r.0)
            .chain(core::iter::once(cap))
            .collect();
        starts
            .into_iter()
            .zip(ends)
            .map(|((start, file), end)| (file, end - start))
    }

    /// Slots made, over every chunk (tests: memory follows the runs).
    #[cfg(all(test, not(loom)))]
    fn slots_made(&self) -> usize {
        self.slots.get().map_or(0, |chunks| {
            chunks
                .iter()
                .filter_map(OnceLock::get)
                .map(|s| s.len())
                .sum()
        })
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    const B: u64 = 4096;

    #[test]
    fn test_a_single_extent_maps_region_offsets_by_its_base() {
        let m = RegionMap::single(1 << 20, 64 << 10);
        assert_eq!(m.locate(0, 4096), 1 << 20);
        assert_eq!(m.locate(60 << 10, 4096), (1 << 20) + (60 << 10));
        assert_eq!(m.extents().collect::<Vec<_>>(), [(1 << 20, 64 << 10)]);
        assert_eq!(m.run_end(0), 64 << 10);
        assert_eq!(m.capacity(), 64 << 10);
    }

    /// A range outside the map is refused, in every build.
    #[test]
    #[should_panic(expected = "outside the map")]
    fn test_a_range_past_the_map_is_refused() {
        let _ = RegionMap::single(0, 8192).locate(4096, 8192);
    }

    #[test]
    #[should_panic(expected = "outside the map")]
    fn test_an_overflowing_range_is_refused() {
        let _ = RegionMap::single(0, 8192).locate(u64::MAX, 2);
    }

    /// Past the data's length but inside the capacity: refused.
    #[test]
    #[should_panic(expected = "outside the map")]
    fn test_a_range_past_the_data_inside_the_capacity_is_refused() {
        let _ =
            RegionMap::from_extents(&[(0, 4 * B), (100 * B, 4 * B)], 6 * B).locate(5 * B, 2 * B);
    }

    /// A range across two runs is refused, in every build.
    #[test]
    #[should_panic(expected = "crosses the run")]
    fn test_a_range_across_runs_is_refused() {
        let _ =
            RegionMap::from_extents(&[(0, 4 * B), (100 * B, 4 * B)], 8 * B).locate(3 * B, 2 * B);
    }

    /// The pieces of a range: clipped to it, run by run, at the runs' file
    /// offsets, past the data's length up to the capacity.
    #[test]
    fn test_pieces_of_a_range() {
        let m = RegionMap::from_extents(&[(100 * B, 4 * B), (10 * B, 4 * B)], 2 * B);
        assert_eq!(
            m.pieces(B, 6 * B),
            [(101 * B, B, 3 * B), (10 * B, 4 * B, 2 * B)]
        );
        assert_eq!(m.pieces(5 * B, 8 * B), [(11 * B, 5 * B, 3 * B)]);
        assert!(m.pieces(3 * B, 3 * B).is_empty());
        m.publish_len(6 * B);
        assert_eq!(m.len(), 6 * B);
        m.publish_len(4 * B);
        assert_eq!(m.len(), 6 * B, "never shrinks");
    }

    /// A growth's extent continuing the last run raises the capacity; one
    /// elsewhere is a new run; the length follows only when published; past
    /// the room a new run is refused.
    #[test]
    fn test_extend_coalesces_or_adds_a_run() {
        let m = RegionMap::single(10 * B, 4 * B).with_room(2);
        m.extend(14 * B, 2 * B);
        assert_eq!(m.extents().collect::<Vec<_>>(), [(10 * B, 6 * B)]);
        assert_eq!(m.len(), 4 * B, "not published");
        m.extend(100 * B, 2 * B);
        m.publish_len(8 * B);
        assert_eq!(m.locate(5 * B, B), 15 * B);
        assert_eq!(m.locate(6 * B, 2 * B), 100 * B);
        assert_eq!(m.run_end(B), 6 * B);
        assert_eq!(
            m.extents().collect::<Vec<_>>(),
            [(10 * B, 6 * B), (100 * B, 2 * B)]
        );
        // Continuing the new last run.
        m.extend(102 * B, B);
        m.publish_len(9 * B);
        assert_eq!(m.locate(6 * B, 3 * B), 100 * B);
        // A clone is a snapshot that maps the same.
        let c = m.clone();
        assert_eq!(
            c.extents().collect::<Vec<_>>(),
            m.extents().collect::<Vec<_>>()
        );
        assert_eq!((c.len(), c.locate(8 * B, B)), (9 * B, 102 * B));
        m.extend(50 * B, B);
        assert_eq!(c.capacity(), 9 * B, "the clone is not the original");
    }

    /// Slots are made a chunk at a time as runs arrive: none for one run,
    /// whatever the room; at most twice the runs.
    #[test]
    fn test_slots_follow_the_runs_not_the_room() {
        let m = RegionMap::single(10 * B, 4 * B).with_room(4082);
        assert_eq!(m.slots_made(), 0);
        for k in 0..5u64 {
            m.extend(100 * B + 10 * k * B, B);
        }
        // Chunks of 1, 2 and 4 runs.
        assert_eq!(m.slots_made(), 7);
        m.publish_len(9 * B);
        assert_eq!(m.locate(8 * B, B), 140 * B);
        assert_eq!(m.clone().slots_made(), 7);
        // Every run a 64 KiB header may list has a place.
        for j in 0..max_extents(16) - 1 {
            let (c, i) = place(j);
            assert!(c < CHUNKS && i < 1 << c, "{j}");
        }
        assert_eq!(
            (place(0), place(1), place(2), place(3)),
            ((0, 0), (1, 0), (1, 1), (2, 0))
        );
    }

    /// A new run past the room the region was made with is a bug: refused.
    #[test]
    #[should_panic(expected = "more runs than the region may have")]
    fn test_extend_past_the_room_is_refused() {
        let m = RegionMap::single(10 * B, 4 * B).with_room(1);
        m.extend(100 * B, B);
        m.extend(200 * B, B);
    }

    /// Room added to a map of several runs keeps them.
    #[test]
    fn test_with_room_keeps_the_runs() {
        let m = RegionMap::from_extents(&[(100 * B, 4 * B), (10 * B, 4 * B)], 8 * B).with_room(3);
        assert_eq!(m.locate(5 * B, B), 11 * B);
        m.extend(30 * B, B);
        m.publish_len(9 * B);
        assert_eq!(m.locate(8 * B, B), 30 * B);
    }

    /// Adjacent extents are one run: a range across them is one operation.
    #[test]
    fn test_adjacent_extents_are_one_run() {
        let m = RegionMap::from_extents(&[(10 * B, 2 * B), (12 * B, 2 * B)], 4 * B);
        assert_eq!(m.locate(B, 2 * B), 11 * B);
        assert_eq!(m.run_end(0), 4 * B);
        assert_eq!(m.extents().collect::<Vec<_>>(), [(10 * B, 4 * B)]);
    }

    /// Against the extents byte by byte (at block grain): `locate` of every
    /// block, `run_end` of every offset, `extents` over the capacity, and
    /// the data's length bounding `run_end`.
    #[test]
    fn test_locate_and_run_end_match_the_extents() {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move |m: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % m
        };
        for _ in 0..300 {
            let mut extents = Vec::new();
            let mut at = next(64) * B;
            for _ in 0..1 + next(6) {
                if next(2) == 0 {
                    at = next(512) * B;
                }
                let len = (1 + next(4)) * B;
                extents.push((at, len));
                at += len;
            }
            let capacity: u64 = extents.iter().map(|e| e.1).sum();
            // Anything from no data to all of it.
            let len = capacity - next(capacity / B + 1) * B;
            let m = RegionMap::from_extents(&extents, len);
            assert_eq!((m.len(), m.capacity()), (len, capacity));
            // Each block's file offset and its run's end, from the extents.
            let mut blocks = Vec::new();
            for &(file, n) in &extents {
                for k in (0..n).step_by(B as usize) {
                    blocks.push(file + k);
                }
            }
            for (i, &file) in blocks.iter().enumerate() {
                let pos = i as u64 * B;
                let mut end = i + 1;
                while end < blocks.len() && blocks[end] == blocks[end - 1] + B {
                    end += 1;
                }
                let end = end as u64 * B;
                if pos < len {
                    assert_eq!(m.locate(pos, B), file, "{extents:?} at {pos}");
                    assert_eq!(m.run_end(pos), end.min(len), "{extents:?} at {pos}");
                    assert_eq!(m.locate(pos, end.min(len) - pos), file);
                }
            }
            let mut from_runs = Vec::new();
            for (file, n) in m.extents() {
                for k in (0..n).step_by(B as usize) {
                    from_runs.push(file + k);
                }
            }
            assert_eq!(from_runs, blocks, "extents() over the capacity");
        }
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;
    use loom::sync::Arc;

    const B: u64 = 4096;

    /// A reader racing a growth that adds a run and publishes the length:
    /// whatever length it reads, every range below it maps where the runs
    /// say (the old run, or the new one), never panicking.
    #[test]
    fn loom_a_reader_racing_an_extend() {
        loom::model(|| {
            let m = Arc::new(RegionMap::single(10 * B, 2 * B).with_room(1));
            let writer = {
                let m = m.clone();
                loom::thread::spawn(move || {
                    m.extend(100 * B, 2 * B);
                    m.publish_len(4 * B);
                })
            };
            let len = m.len();
            for pos in (0..len).step_by(B as usize) {
                let want = if pos < 2 * B {
                    10 * B + pos
                } else {
                    100 * B + pos - 2 * B
                };
                assert_eq!(m.locate(pos, B), want, "at {pos} of {len}");
                assert!(m.run_end(pos) <= len);
            }
            writer.join().unwrap();
            assert_eq!(m.locate(3 * B, B), 101 * B);
        });
    }

    /// A reader racing growths that continue the last run, then add runs in
    /// two chunks: every range below the length it reads maps where the
    /// runs say.
    #[test]
    fn loom_a_reader_racing_a_coalescing_and_chunked_growth() {
        loom::model(|| {
            let m = Arc::new(RegionMap::single(10 * B, 2 * B).with_room(2));
            let writer = {
                let m = m.clone();
                loom::thread::spawn(move || {
                    m.extend(12 * B, B);
                    m.publish_len(3 * B);
                    m.extend(100 * B, B);
                    m.extend(200 * B, B);
                    m.publish_len(5 * B);
                })
            };
            let want = [10 * B, 11 * B, 12 * B, 100 * B, 200 * B];
            let len = m.len();
            for pos in (0..len).step_by(B as usize) {
                assert_eq!(
                    m.locate(pos, B),
                    want[(pos / B) as usize],
                    "at {pos} of {len}"
                );
                assert!(m.run_left(pos) > 0);
            }
            writer.join().unwrap();
            assert_eq!(
                m.extents().collect::<Vec<_>>(),
                [(10 * B, 3 * B), (100 * B, B), (200 * B, B)]
            );
        });
    }
}
