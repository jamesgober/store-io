//! The buffer pool: fixed size classes carved out of one arena, each with a
//! lock-free free list. Taking a buffer never allocates and never blocks.

use core::fmt;
use core::ptr::NonNull;
use std::sync::Arc;

use crate::arena::{Arena, ArenaError, Protection};
use crate::queue::IndexQueue;
use crate::wipe::wipe;

/// One size class: `count` buffers of `size` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassSpec {
    /// Buffer size in bytes (a power of two, at least the pool alignment).
    pub size: usize,
    /// Number of buffers.
    pub count: u32,
}

/// Pool configuration. Every field is an explicit choice reported by the
/// owning store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolConfig {
    /// Alignment of every buffer (a power of two; the direct-I/O memory
    /// alignment, at least the page size in practice).
    pub align: usize,
    /// Size classes, any order.
    pub classes: Vec<ClassSpec>,
    /// Wipe buffers when they return to the pool, and lock the arena in RAM
    /// and out of core dumps where the OS allows it.
    pub sensitive: bool,
}

impl PoolConfig {
    /// Power-of-two classes from `min` to `max` bytes inclusive, `count`
    /// buffers each.
    #[must_use]
    pub fn uniform(align: usize, min: usize, max: usize, count: u32) -> Self {
        let mut classes = Vec::new();
        let mut size = min.max(align).next_power_of_two();
        while size <= max {
            classes.push(ClassSpec { size, count });
            size = match size.checked_mul(2) {
                Some(s) => s,
                None => break,
            };
        }
        Self {
            align,
            classes,
            sensitive: false,
        }
    }
}

/// Why a pool could not be created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolError {
    /// The configuration is invalid (alignment or a class size is not a
    /// power of two, a class is smaller than the alignment, a count is zero,
    /// or the total size overflows).
    Config(&'static str),
    /// Mapping the arena failed.
    Arena(ArenaError),
}

impl fmt::Display for PoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(why) => write!(f, "invalid buffer pool configuration: {why}"),
            Self::Arena(e) => write!(f, "mapping the buffer arena failed (OS error {})", e.code),
        }
    }
}

impl std::error::Error for PoolError {}

/// No buffer of the requested size (or larger) is free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exhausted;

struct Class {
    size: usize,
    base: usize,
    count: u32,
    free: IndexQueue,
}

struct Inner {
    arena: Arena,
    offset: usize,
    classes: Box<[Class]>,
    sensitive: bool,
    protection: Protection,
}

/// A pool of aligned I/O buffers. Cheap to clone (shared).
#[derive(Clone)]
pub struct BufPool {
    inner: Arc<Inner>,
}

impl fmt::Debug for BufPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BufPool")
            .field(
                "classes",
                &self
                    .inner
                    .classes
                    .iter()
                    .map(|c| (c.size, c.count))
                    .collect::<Vec<_>>(),
            )
            .field("sensitive", &self.inner.sensitive)
            .finish()
    }
}

impl BufPool {
    /// Creates a pool, mapping one arena for every buffer up front.
    ///
    /// # Errors
    ///
    /// [`PoolError`] for an invalid configuration or a failed mapping.
    pub fn new(cfg: &PoolConfig) -> Result<Self, PoolError> {
        if cfg.align == 0 || !cfg.align.is_power_of_two() {
            return Err(PoolError::Config("alignment must be a power of two"));
        }
        let mut specs = cfg.classes.clone();
        specs.sort_by_key(|c| c.size);
        let mut total: usize = 0;
        for c in &specs {
            if !c.size.is_power_of_two() || c.size < cfg.align {
                return Err(PoolError::Config(
                    "class size must be a power of two ≥ the alignment",
                ));
            }
            if c.count == 0 {
                return Err(PoolError::Config("class count must be at least 1"));
            }
            let bytes = c
                .size
                .checked_mul(c.count as usize)
                .ok_or(PoolError::Config("pool size overflows"))?;
            total = total
                .checked_add(bytes)
                .ok_or(PoolError::Config("pool size overflows"))?;
        }
        if specs.is_empty() {
            return Err(PoolError::Config("at least one size class is required"));
        }
        let mapped = total
            .checked_add(cfg.align)
            .ok_or(PoolError::Config("pool size overflows"))?;
        let arena = Arena::new(mapped).map_err(PoolError::Arena)?;
        let addr = arena.base().as_ptr() as usize;
        let offset = addr.next_multiple_of(cfg.align) - addr;
        let mut base = 0usize;
        let mut classes = Vec::with_capacity(specs.len());
        for c in &specs {
            let free = IndexQueue::with_capacity(c.count as usize);
            for i in 0..c.count {
                // Capacity ≥ count, so every push succeeds.
                let _initial_push = free.push(i);
            }
            classes.push(Class {
                size: c.size,
                base,
                count: c.count,
                free,
            });
            base += c.size * c.count as usize;
        }
        let protection = if cfg.sensitive {
            arena.protect_sensitive()
        } else {
            Protection::default()
        };
        Ok(Self {
            inner: Arc::new(Inner {
                arena,
                offset,
                classes: classes.into_boxed_slice(),
                sensitive: cfg.sensitive,
                protection,
            }),
        })
    }

    /// Takes a buffer of at least `len` bytes, from the smallest class that
    /// fits and has a free buffer. Never allocates, never blocks.
    ///
    /// The buffer's length is set to `len`; its contents are whatever the
    /// previous user left (zeros for a sensitive pool, which wipes on return).
    ///
    /// # Errors
    ///
    /// [`Exhausted`] when no class of at least `len` bytes has a free buffer,
    /// or `len` exceeds the largest class.
    pub fn take(&self, len: usize) -> Result<IoBuf, Exhausted> {
        let inner = &self.inner;
        for (ci, class) in inner.classes.iter().enumerate() {
            if class.size < len {
                continue;
            }
            if let Some(index) = class.free.pop() {
                let at = inner.offset + class.base + index as usize * class.size;
                // SAFETY: `at + class.size ≤ offset + total ≤ arena.len()` by
                // construction in `new`, so the pointer stays inside the arena.
                let ptr = unsafe { NonNull::new_unchecked(inner.arena.base().as_ptr().add(at)) };
                return Ok(IoBuf {
                    pool: Arc::clone(&self.inner),
                    ptr,
                    cap: class.size,
                    len,
                    class: ci as u16,
                    index,
                });
            }
        }
        Err(Exhausted)
    }

    /// The largest buffer the pool can hand out.
    #[must_use]
    pub fn max_len(&self) -> usize {
        self.inner.classes.last().map_or(0, |c| c.size)
    }

    /// Buffers currently free in each class, as `(size, free, total)`.
    /// Approximate under concurrency.
    #[must_use]
    pub fn occupancy(&self) -> Vec<(usize, usize, u32)> {
        self.inner
            .classes
            .iter()
            .map(|c| {
                let mut n = 0;
                let mut held = Vec::new();
                while let Some(i) = c.free.pop() {
                    held.push(i);
                    n += 1;
                }
                for i in held {
                    let _return_push = c.free.push(i);
                }
                (c.size, n, c.count)
            })
            .collect()
    }

    /// Protections applied to a sensitive pool.
    #[must_use]
    pub fn protection(&self) -> Protection {
        self.inner.protection
    }
}

/// An owned, aligned I/O buffer from a [`BufPool`].
///
/// It moves into an I/O operation and comes back in the operation's
/// completion; while the device owns it, nobody else can touch it. Dropping it
/// returns it to its pool (wiped first if the pool is sensitive).
pub struct IoBuf {
    pool: Arc<Inner>,
    ptr: NonNull<u8>,
    cap: usize,
    len: usize,
    class: u16,
    index: u32,
}

// SAFETY: an IoBuf exclusively owns its slot of the arena (the free list hands
// each index to one IoBuf at a time), so moving it to another thread moves
// that exclusive ownership.
unsafe impl Send for IoBuf {}
// SAFETY: `&IoBuf` only gives read access to the owned slot.
unsafe impl Sync for IoBuf {}

impl IoBuf {
    /// The buffer's bytes (`len` of them).
    #[inline]
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` points to `cap ≥ len` bytes of the arena owned
        // exclusively by this buffer; the arena outlives `self` via the Arc.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// The buffer's bytes, mutable.
    #[inline]
    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as in `as_slice`, and `&mut self` makes the access unique.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    /// Raw pointer for the device (FFI). Valid while `self` is alive.
    #[inline]
    #[must_use]
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }

    /// Raw mutable pointer for the device (reads into the buffer). Valid
    /// while `self` is alive.
    #[inline]
    #[must_use]
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// Current length.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the length is zero.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Capacity (the size class).
    #[inline]
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Sets the length. Returns `false` (and changes nothing) if `len`
    /// exceeds the capacity.
    #[inline]
    #[must_use]
    pub fn set_len(&mut self, len: usize) -> bool {
        if len > self.cap {
            return false;
        }
        self.len = len;
        true
    }

    /// The whole capacity as a mutable slice (for filling padding).
    #[inline]
    #[must_use]
    pub fn as_mut_capacity(&mut self) -> &mut [u8] {
        // SAFETY: the slot is `cap` bytes, owned exclusively, `&mut self`.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.cap) }
    }
}

impl fmt::Debug for IoBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print contents: buffers may hold caller payload.
        f.debug_struct("IoBuf")
            .field("len", &self.len)
            .field("capacity", &self.cap)
            .finish()
    }
}

impl Drop for IoBuf {
    fn drop(&mut self) {
        if self.pool.sensitive {
            wipe(self.as_mut_capacity());
        }
        if let Some(class) = self.pool.classes.get(usize::from(self.class)) {
            // The queue holds every index of the class, so this always fits.
            let _returned = class.free.push(self.index);
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    fn pool(count: u32) -> BufPool {
        let cfg = PoolConfig::uniform(4096, 4096, 65536, count);
        match BufPool::new(&cfg) {
            Ok(p) => p,
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn test_take_is_aligned_sized_and_returns_on_drop() {
        let p = pool(2);
        let b = p.take(5000).unwrap_or_else(|_| panic!("exhausted"));
        assert_eq!(b.capacity(), 8192);
        assert_eq!(b.len(), 5000);
        assert_eq!(b.as_ptr() as usize % 4096, 0);
        drop(b);
        let free: usize = p.occupancy().iter().map(|c| c.1).sum();
        assert_eq!(free, 2 * 5);
    }

    #[test]
    fn test_take_falls_back_to_larger_class_then_exhausts() {
        let p = pool(1);
        let a = p.take(4096);
        let b = p.take(4096); // 4 KiB class empty → 8 KiB
        assert_eq!(b.as_ref().map(IoBuf::capacity).ok(), Some(8192));
        let _rest: Vec<_> = (0..3).filter_map(|_| p.take(1).ok()).collect();
        assert!(p.take(1).is_err());
        assert!(p.take(65537).is_err());
        drop(a);
        assert!(p.take(1).is_ok());
    }

    #[test]
    fn test_buffers_do_not_overlap() {
        let p = pool(4);
        let mut bufs: Vec<IoBuf> = (0..20).filter_map(|_| p.take(1).ok()).collect();
        assert_eq!(bufs.len(), 20);
        for (i, b) in bufs.iter_mut().enumerate() {
            let len = b.capacity();
            assert!(b.set_len(len));
            b.as_mut_slice().fill(i as u8);
        }
        for (i, b) in bufs.iter().enumerate() {
            assert!(b.as_slice().iter().all(|&x| x == i as u8));
        }
    }

    #[test]
    fn test_set_len_rejects_beyond_capacity() {
        let p = pool(1);
        let mut b = p.take(10).unwrap_or_else(|_| panic!("exhausted"));
        assert!(!b.set_len(4097));
        assert_eq!(b.len(), 10);
        assert!(b.set_len(4096));
    }

    #[test]
    fn test_sensitive_pool_wipes_on_return() {
        let cfg = PoolConfig {
            sensitive: true,
            ..PoolConfig::uniform(4096, 4096, 4096, 1)
        };
        let p = BufPool::new(&cfg).unwrap_or_else(|e| panic!("{e}"));
        let mut b = p.take(4096).unwrap_or_else(|_| panic!("exhausted"));
        b.as_mut_slice().fill(0xA5);
        drop(b);
        let b = p.take(4096).unwrap_or_else(|_| panic!("exhausted"));
        assert!(b.as_slice().iter().all(|&x| x == 0));
    }

    #[test]
    fn test_invalid_configs_are_rejected() {
        assert!(
            BufPool::new(&PoolConfig {
                align: 3,
                classes: vec![],
                sensitive: false
            })
            .is_err()
        );
        assert!(
            BufPool::new(&PoolConfig {
                align: 4096,
                classes: vec![],
                sensitive: false
            })
            .is_err()
        );
        let small = PoolConfig {
            align: 4096,
            classes: vec![ClassSpec {
                size: 512,
                count: 1,
            }],
            sensitive: false,
        };
        assert!(BufPool::new(&small).is_err());
        let zero = PoolConfig {
            align: 4096,
            classes: vec![ClassSpec {
                size: 4096,
                count: 0,
            }],
            sensitive: false,
        };
        assert!(BufPool::new(&zero).is_err());
    }

    #[test]
    fn test_concurrent_take_and_drop_never_share_a_slot() {
        let p = pool(8);
        let handles: Vec<_> = (0..8u8)
            .map(|t| {
                let p = p.clone();
                std::thread::spawn(move || {
                    for _ in 0..5_000 {
                        if let Ok(mut b) = p.take(4096) {
                            b.as_mut_slice().fill(t);
                            std::hint::spin_loop();
                            assert!(b.as_slice().iter().all(|&x| x == t), "slot shared");
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            assert!(h.join().is_ok());
        }
    }
}
