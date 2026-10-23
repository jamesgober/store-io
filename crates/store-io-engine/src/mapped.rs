//! Writes at region offsets, mapped to the container one run at a time.
//!
//! A region's data lies in runs (spec §11, [`RegionMap`]), and one device
//! write never crosses a run's end. [`Mapped`] wraps a [`Source`] whose
//! writes are at region offsets: each buffer goes to its run's file offset,
//! and a buffer that crosses a run's end is written in place, a piece at a
//! time: its head first, and when that comes back complete, the rest moved
//! to the buffer's front and written at the next run. No buffer is taken
//! for a piece (a pool shortage after an append's reservation would leave a
//! hole), and every other buffer keeps flowing meanwhile. A region in one
//! run writes every buffer whole, as before.

use store_io_platform::IoBuf;

use crate::exec::{Next, Source};
use crate::regionmap::RegionMap;

/// A buffer part-written across runs.
struct Part {
    /// The buffer's address: the same buffer comes back in `recycle`.
    addr: usize,
    /// The region offset of the buffer's first byte.
    pos: u64,
    /// The bytes still to write, from the buffer's front.
    left: usize,
}

/// A [`Source`] of region-offset writes, mapped to container offsets.
pub(crate) struct Mapped<'a, S> {
    map: &'a RegionMap,
    inner: &'a mut S,
    parts: Vec<Part>,
    /// Rests of split buffers, at their file offsets, written first.
    ready: Vec<(u64, IoBuf)>,
}

impl<'a, S: Source> Mapped<'a, S> {
    pub(crate) fn new(map: &'a RegionMap, inner: &'a mut S) -> Self {
        Self {
            map,
            inner,
            parts: Vec::new(),
            ready: Vec::new(),
        }
    }

    /// `buf`, whose first byte is at region offset `pos`, at its run: the
    /// whole of it, or the piece up to its run's end (the rest recorded).
    fn place(&mut self, pos: u64, mut buf: IoBuf, left: usize) -> (u64, IoBuf) {
        let piece = (self.map.run_end(pos) - pos).min(left as u64);
        // Never empty: a buffer past the data's end (a caller's bug) would
        // otherwise be retried as empty pieces for ever.
        assert!(piece > 0, "a write at {pos} past the data's end");
        if piece < left as u64 {
            self.parts.push(Part {
                addr: buf.as_ptr() as usize,
                pos,
                left,
            });
        }
        // `piece ≤ left ≤ capacity`: the length always fits.
        let _fits = buf.set_len(piece as usize);
        (self.map.locate(pos, piece), buf)
    }
}

impl<S: Source> Source for Mapped<'_, S> {
    /// As the inner source's: a write crossing a run is several device
    /// writes, but not one larger than a buffer.
    fn is_split(&self) -> bool {
        self.inner.is_split()
    }

    fn next(&mut self) -> Next {
        if let Some((at, buf)) = self.ready.pop() {
            return Next::Write(at, buf);
        }
        match self.inner.next() {
            Next::Write(pos, buf) => {
                let left = buf.len();
                let (at, buf) = self.place(pos, buf, left);
                Next::Write(at, buf)
            }
            // A split buffer still in flight comes back in `recycle`.
            Next::Done if !self.parts.is_empty() => Next::Wait,
            other => other,
        }
    }

    fn recycle(&mut self, mut buf: IoBuf) {
        let addr = buf.as_ptr() as usize;
        let Some(k) = self.parts.iter().position(|p| p.addr == addr) else {
            self.inner.recycle(buf);
            return;
        };
        let written = buf.len();
        let p = &mut self.parts[k];
        let left = p.left - written;
        if left == 0 {
            let _done = self.parts.swap_remove(k);
            self.inner.recycle(buf);
            return;
        }
        let pos = p.pos + written as u64;
        // The rest to the front: the next piece starts at the buffer's start.
        buf.as_mut_capacity()
            .copy_within(written..written + left, 0);
        let _done = self.parts.swap_remove(k);
        let (at, buf) = self.place(pos, buf, left);
        self.ready.push((at, buf));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use store_io_buf::{BufPool, PoolConfig};

    use super::*;

    const B: u64 = 4096;

    /// Buffers handed out at region offsets, and what came back.
    struct Fixed {
        writes: Vec<(u64, IoBuf)>,
        back: usize,
    }

    impl Source for Fixed {
        fn next(&mut self) -> Next {
            if self.writes.is_empty() {
                Next::Done
            } else {
                let (pos, buf) = self.writes.remove(0);
                Next::Write(pos, buf)
            }
        }

        fn recycle(&mut self, _buf: IoBuf) {
            self.back += 1;
        }
    }

    /// Runs `src` to the end as a device would, every write completing in
    /// full at once (in reverse order when `reverse`), and returns the bytes
    /// written at each file offset.
    fn drive(src: &mut impl Source, reverse: bool) -> Vec<(u64, Vec<u8>)> {
        let mut out = Vec::new();
        let mut flight: Vec<(u64, IoBuf)> = Vec::new();
        loop {
            let mut produced = false;
            while let Next::Write(at, buf) = src.next() {
                flight.push((at, buf));
                produced = true;
            }
            if flight.is_empty() {
                assert!(!produced);
                break;
            }
            if reverse {
                flight.reverse();
            }
            for (at, buf) in flight.drain(..) {
                out.push((at, buf.as_slice().to_vec()));
                src.recycle(buf);
            }
        }
        out.sort_by_key(|w| w.0);
        out
    }

    fn filled(pool: &BufPool, blocks: u64, seed: u8) -> IoBuf {
        let mut buf = pool.take((blocks * B) as usize).unwrap();
        for (i, b) in buf.as_mut_slice().iter_mut().enumerate() {
            *b = seed ^ (i / B as usize) as u8;
        }
        buf
    }

    /// Two buffers, each crossing a run (one of them two runs): every block
    /// lands at its run's file offset with its own bytes (each piece inside
    /// one run, or `locate` would panic), and each buffer goes back to the
    /// source once, whole; completions in order and reversed.
    #[test]
    fn test_buffers_crossing_runs_land_block_by_block() {
        let pool = BufPool::new(&PoolConfig::uniform(4096, 4096, 1 << 16, 8)).unwrap();
        // Runs: [0, 3B) at 100B, [3B, 5B) at 10B, [5B, 12B) at 50B.
        let map = RegionMap::from_extents(
            &[(100 * B, 3 * B), (10 * B, 2 * B), (50 * B, 7 * B)],
            12 * B,
        );
        for reverse in [false, true] {
            let mut inner = Fixed {
                writes: vec![(B, filled(&pool, 5, 0x10)), (6 * B, filled(&pool, 2, 0x20))],
                back: 0,
            };
            let mut src = Mapped::new(&map, &mut inner);
            let wrote = drive(&mut src, reverse);
            drop(src);
            assert_eq!(inner.back, 2, "each buffer back once");
            let mut want = Vec::new();
            for (pos, seed, n) in [(B, 0x10u8, 5u64), (6 * B, 0x20, 2)] {
                for j in 0..n {
                    want.push((map.locate(pos + j * B, B), vec![seed ^ j as u8; B as usize]));
                }
            }
            want.sort_by_key(|w| w.0);
            let mut got = Vec::new();
            for (at, bytes) in wrote {
                for (j, block) in bytes.chunks(B as usize).enumerate() {
                    got.push((at + j as u64 * B, block.to_vec()));
                }
            }
            got.sort_by_key(|w| w.0);
            assert_eq!(got, want, "reverse {reverse}");
        }
    }

    /// A region in one run: every buffer whole, at its file offset.
    #[test]
    fn test_one_run_writes_each_buffer_whole() {
        let pool = BufPool::new(&PoolConfig::uniform(4096, 4096, 1 << 16, 8)).unwrap();
        let map = RegionMap::single(64 * B, 16 * B);
        let mut inner = Fixed {
            writes: vec![(0, filled(&pool, 4, 1)), (4 * B, filled(&pool, 8, 2))],
            back: 0,
        };
        let mut src = Mapped::new(&map, &mut inner);
        let wrote = drive(&mut src, false);
        assert_eq!(
            wrote.iter().map(|w| (w.0, w.1.len())).collect::<Vec<_>>(),
            [(64 * B, 4 * B as usize), (68 * B, 8 * B as usize)]
        );
    }
}
