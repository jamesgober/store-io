//! A bounded, lock-free, multi-producer multi-consumer queue of `u32`
//! indices: the free list of each buffer size class.
//!
//! Dmitry Vyukov's bounded MPMC design: a power-of-two ring of cells, each
//! carrying a sequence number that says whether the cell is ready to be
//! written (`seq == pos`) or read (`seq == pos + 1`). Producers and consumers
//! claim positions with one compare-and-swap each and never block. The value
//! is an `AtomicU32` published by the cell's `Release` sequence store, so the
//! queue needs no `unsafe` code.

use crate::pad::CachePadded;
use crate::sync::{AtomicU32, AtomicUsize, Ordering};

struct Cell {
    seq: AtomicUsize,
    val: AtomicU32,
}

/// Bounded MPMC queue of `u32` values.
pub struct IndexQueue {
    cells: Box<[Cell]>,
    mask: usize,
    enqueue: CachePadded<AtomicUsize>,
    dequeue: CachePadded<AtomicUsize>,
}

impl IndexQueue {
    /// Creates an empty queue that holds at least `capacity` values
    /// (rounded up to a power of two, minimum 2).
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let cap = capacity.max(2).next_power_of_two();
        let cells = (0..cap)
            .map(|i| Cell {
                seq: AtomicUsize::new(i),
                val: AtomicU32::new(0),
            })
            .collect();
        Self {
            cells,
            mask: cap - 1,
            enqueue: CachePadded::new(AtomicUsize::new(0)),
            dequeue: CachePadded::new(AtomicUsize::new(0)),
        }
    }

    /// The number of values the queue can hold.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.cells.len()
    }

    /// Adds a value; returns it back if the queue is full. Never blocks.
    pub fn push(&self, value: u32) -> Result<(), u32> {
        let mut pos = self.enqueue.load(Ordering::Relaxed);
        loop {
            let cell = &self.cells[pos & self.mask];
            let seq = cell.seq.load(Ordering::Acquire);
            let diff = seq.wrapping_sub(pos) as isize;
            if diff == 0 {
                match self.enqueue.compare_exchange_weak(
                    pos,
                    pos.wrapping_add(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        cell.val.store(value, Ordering::Relaxed);
                        cell.seq.store(pos.wrapping_add(1), Ordering::Release);
                        return Ok(());
                    }
                    Err(current) => pos = current,
                }
            } else if diff < 0 {
                return Err(value);
            } else {
                pos = self.enqueue.load(Ordering::Relaxed);
            }
        }
    }

    /// Removes a value, or `None` if the queue is empty. Never blocks.
    pub fn pop(&self) -> Option<u32> {
        let mut pos = self.dequeue.load(Ordering::Relaxed);
        loop {
            let cell = &self.cells[pos & self.mask];
            let seq = cell.seq.load(Ordering::Acquire);
            let diff = seq.wrapping_sub(pos.wrapping_add(1)) as isize;
            if diff == 0 {
                match self.dequeue.compare_exchange_weak(
                    pos,
                    pos.wrapping_add(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        let value = cell.val.load(Ordering::Relaxed);
                        cell.seq.store(
                            pos.wrapping_add(self.mask).wrapping_add(1),
                            Ordering::Release,
                        );
                        return Some(value);
                    }
                    Err(current) => pos = current,
                }
            } else if diff < 0 {
                return None;
            } else {
                pos = self.dequeue.load(Ordering::Relaxed);
            }
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Arc;

    #[test]
    fn test_queue_fifo_and_full_and_empty() {
        let q = IndexQueue::with_capacity(3);
        assert_eq!(q.capacity(), 4);
        assert_eq!(q.pop(), None);
        for i in 0..4 {
            assert_eq!(q.push(i), Ok(()));
        }
        assert_eq!(q.push(9), Err(9));
        for i in 0..4 {
            assert_eq!(q.pop(), Some(i));
        }
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn test_queue_matches_vecdeque_model_over_wraparound() {
        let q = IndexQueue::with_capacity(8);
        let mut model = VecDeque::new();
        let mut seed = 0x1234_5678_u64;
        for step in 0..100_000u32 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            if seed % 3 == 0 {
                assert_eq!(q.pop(), model.pop_front(), "step {step}");
            } else {
                let r = q.push(step);
                if model.len() < 8 {
                    assert_eq!(r, Ok(()));
                    model.push_back(step);
                } else {
                    assert_eq!(r, Err(step));
                }
            }
        }
    }

    #[test]
    fn test_queue_concurrent_values_are_conserved() {
        // Every index taken is put back exactly once: the multiset survives.
        let q = Arc::new(IndexQueue::with_capacity(64));
        for i in 0..64 {
            assert_eq!(q.push(i), Ok(()));
        }
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let q = Arc::clone(&q);
                std::thread::spawn(move || {
                    for _ in 0..20_000 {
                        if let Some(v) = q.pop() {
                            while q.push(v).is_err() {
                                std::hint::spin_loop();
                            }
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            assert!(h.join().is_ok());
        }
        let mut seen: Vec<u32> = core::iter::from_fn(|| q.pop()).collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..64).collect::<Vec<_>>());
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;
    use loom::sync::Arc;

    #[test]
    fn loom_two_producers_two_consumers_conserve_values() {
        loom::model(|| {
            let q = Arc::new(IndexQueue::with_capacity(2));
            let p: Vec<_> = (0..2u32)
                .map(|v| {
                    let q = q.clone();
                    loom::thread::spawn(move || q.push(v).is_ok())
                })
                .collect();
            let c: Vec<_> = (0..2)
                .map(|_| {
                    let q = q.clone();
                    loom::thread::spawn(move || q.pop())
                })
                .collect();
            let pushed = p
                .into_iter()
                .map(|h| h.join().unwrap_or(false))
                .filter(|ok| *ok)
                .count();
            let mut got: Vec<u32> = c
                .into_iter()
                .filter_map(|h| h.join().ok().flatten())
                .collect();
            while let Some(v) = q.pop() {
                got.push(v);
            }
            got.sort_unstable();
            got.dedup();
            assert_eq!(got.len(), pushed);
        });
    }
}
