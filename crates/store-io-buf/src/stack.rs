//! A bounded, lock-free stack of `u32` indices: the free list of each buffer
//! size class, most recently freed first.
//!
//! Why last in, first out: a buffer just returned is still warm in the CPU's
//! caches, so the next operation copies from or into memory that is already
//! there. A first-in-first-out free list rotates through every buffer of a
//! class instead; 1 MiB appends kept 8 deep measured 7-10% less bandwidth
//! that way at memory speed (a 64-buffer rotation against a 9-buffer one, on
//! CI).
//!
//! Treiber's stack over an index array. `head` packs a 40-bit tag
//! (incremented on every successful change) with `top + 1` in the low 24
//! bits (0: empty), and `next[i]` holds the entry below index `i`, plus one.
//! The tag makes a compare-and-swap fail whenever the head changed in
//! between, even back to the same index (ABA): a popper that read
//! `next[top]` before another thread popped `top`, popped the entry below it
//! and pushed `top` back cannot then install the stale `next` and hand out
//! an index that is no longer free. Only a popper stalled between its read
//! and its exchange for exactly a multiple of 2^40 changes (about six hours
//! at 20 ns a change, with the same index on top) could be fooled. Every
//! index is pushed at most once while free (the pool's ownership rule), so
//! the stack never holds more than its capacity. No `unsafe`, no allocation
//! after creation.

use crate::pad::CachePadded;
use crate::sync::{AtomicU32, AtomicU64, Ordering, spin};

/// Low bits of the head word holding `top + 1`; the tag has the rest.
const TOP_BITS: u32 = 24;
const TOP_MASK: u64 = (1 << TOP_BITS) - 1;

/// Bounded LIFO stack of `u32` values below its capacity.
pub struct IndexStack {
    head: CachePadded<AtomicU64>,
    next: Box<[AtomicU32]>,
}

/// The head word for `tag` (its low 40 bits) and `top_plus_one`.
#[inline]
fn pack(tag: u64, top_plus_one: u32) -> u64 {
    (tag << TOP_BITS) | u64::from(top_plus_one)
}

#[inline]
#[allow(clippy::cast_possible_truncation)]
fn unpack(word: u64) -> (u64, u32) {
    (word >> TOP_BITS, (word & TOP_MASK) as u32)
}

impl IndexStack {
    /// The largest capacity (2^24 - 1 values).
    pub const MAX_CAPACITY: u32 = (1 << TOP_BITS) - 1;

    /// An empty stack for the values `0..capacity`; a capacity above
    /// [`Self::MAX_CAPACITY`] is reduced to it.
    #[must_use]
    pub fn with_capacity(capacity: u32) -> Self {
        let capacity = capacity.min(Self::MAX_CAPACITY);
        Self {
            head: CachePadded::new(AtomicU64::new(0)),
            next: (0..capacity).map(|_| AtomicU32::new(0)).collect(),
        }
    }

    /// The number of values the stack can hold (every value is below it).
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.next.len()
    }

    /// Pushes `value`; returns it back if it is not below the capacity.
    /// Never blocks. `value` must not already be on the stack: pushing it
    /// twice corrupts the stack (a cycle that pops repeat, and the entries
    /// below it lost). The pool pushes an index once, when its buffer is
    /// dropped.
    pub fn push(&self, value: u32) -> Result<(), u32> {
        let Some(slot) = self.next.get(value as usize) else {
            return Err(value);
        };
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            let (tag, top) = unpack(head);
            // Published by the release below, before the value is visible.
            slot.store(top, Ordering::Relaxed);
            match self.head.compare_exchange_weak(
                head,
                pack(tag.wrapping_add(1), value + 1),
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(current) => {
                    head = current;
                    spin();
                }
            }
        }
    }

    /// Pops the most recently pushed value, or `None` if the stack is empty.
    /// Never blocks.
    pub fn pop(&self) -> Option<u32> {
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            let (tag, top) = unpack(head);
            let index = top.checked_sub(1)?;
            // `top`'s entry below it, as its pusher wrote it before the
            // release this acquire saw. If `top` was popped and pushed back
            // since, the tag has moved and the exchange below fails.
            let below = self.next[index as usize].load(Ordering::Relaxed);
            match self.head.compare_exchange_weak(
                head,
                pack(tag.wrapping_add(1), below),
                Ordering::Acquire,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(index),
                Err(current) => {
                    head = current;
                    spin();
                }
            }
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use super::*;

    #[test]
    fn test_stack_is_lifo_and_empty_and_bounded() {
        let s = IndexStack::with_capacity(4);
        assert_eq!(s.capacity(), 4);
        assert_eq!(s.pop(), None);
        for i in 0..4 {
            assert_eq!(s.push(i), Ok(()));
        }
        assert_eq!(s.push(4), Err(4), "a value outside the capacity");
        for i in (0..4).rev() {
            assert_eq!(s.pop(), Some(i));
        }
        assert_eq!(s.pop(), None);
        // The most recently returned value comes back first.
        s.push(2).unwrap();
        s.push(0).unwrap();
        assert_eq!(s.pop(), Some(0));
        s.push(3).unwrap();
        assert_eq!(s.pop(), Some(3));
        assert_eq!(s.pop(), Some(2));
        assert_eq!(s.pop(), None);
    }

    /// The largest value keeps its own index in the head word's low bits,
    /// the tag never spills into them, and a tag past 40 bits wraps to 0.
    #[test]
    fn test_stack_head_word_packing() {
        let top = IndexStack::MAX_CAPACITY - 1;
        let tag = u64::MAX >> TOP_BITS;
        assert_eq!(unpack(pack(tag, top + 1)), (tag, top + 1));
        assert_eq!(unpack(pack(3, 0)), (3, 0));
        assert_eq!(unpack(pack(tag.wrapping_add(1), 5)), (0, 5));
    }

    /// The largest capacity holds its largest value (16 million slots: too
    /// large for Miri).
    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_stack_largest_capacity_holds_its_largest_value() {
        let top = IndexStack::MAX_CAPACITY - 1;
        let s = IndexStack::with_capacity(u32::MAX);
        assert_eq!(s.capacity(), IndexStack::MAX_CAPACITY as usize);
        assert_eq!(s.push(top + 1), Err(top + 1));
        for _ in 0..3 {
            s.push(top).unwrap();
            s.push(0).unwrap();
            assert_eq!(s.pop(), Some(0));
            assert_eq!(s.pop(), Some(top));
            assert_eq!(s.pop(), None);
        }
    }

    /// Many threads taking and returning values: every value is held by at
    /// most one thread at a time, and none is lost.
    #[test]
    fn test_stack_concurrent_values_are_never_shared_or_lost() {
        const N: u32 = 64;
        let s = Arc::new(IndexStack::with_capacity(N));
        for i in 0..N {
            s.push(i).unwrap();
        }
        let held: Arc<Vec<AtomicU32>> = Arc::new((0..N).map(|_| AtomicU32::new(0)).collect());
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (s, held) = (Arc::clone(&s), Arc::clone(&held));
                std::thread::spawn(move || {
                    for _ in 0..if cfg!(miri) { 50 } else { 20_000 } {
                        if let Some(v) = s.pop() {
                            let was = held[v as usize].fetch_add(1, Ordering::AcqRel);
                            assert_eq!(was, 0, "value {v} handed out twice");
                            let _released = held[v as usize].fetch_sub(1, Ordering::AcqRel);
                            s.push(v).unwrap();
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let mut all: Vec<u32> = std::iter::from_fn(|| s.pop()).collect();
        all.sort_unstable();
        assert_eq!(
            all,
            (0..N).collect::<Vec<_>>(),
            "a value was lost or duplicated"
        );
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use loom::sync::Arc;

    use super::*;

    /// The ABA case: one thread pops while another pops two values and
    /// pushes the first back. No value may be held by both threads, or be
    /// held and still on the stack, and none may be lost.
    #[test]
    fn loom_a_stale_pop_never_hands_out_a_held_value() {
        loom::model(|| {
            let s = Arc::new(IndexStack::with_capacity(3));
            for i in 0..3 {
                s.push(i).unwrap();
            }
            let a = {
                let s = Arc::clone(&s);
                loom::thread::spawn(move || s.pop())
            };
            let b = {
                let s = Arc::clone(&s);
                loom::thread::spawn(move || {
                    let x = s.pop();
                    let y = s.pop();
                    if let Some(x) = x {
                        s.push(x).unwrap();
                    }
                    y
                })
            };
            let got_a = a.join().unwrap();
            let held_b = b.join().unwrap();
            if let (Some(va), Some(vb)) = (got_a, held_b) {
                assert_ne!(va, vb, "a value held by one thread was handed to another");
            }
            // Conservation: the stack holds exactly the values nobody holds.
            let mut rest: Vec<u32> = std::iter::from_fn(|| s.pop()).collect();
            for held in [got_a, held_b].into_iter().flatten() {
                assert!(!rest.contains(&held), "a held value is still on the stack");
            }
            rest.extend(got_a);
            rest.extend(held_b);
            rest.sort_unstable();
            assert_eq!(rest, vec![0, 1, 2]);
        });
    }
}
