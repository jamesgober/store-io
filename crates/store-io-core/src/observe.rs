//! The observer: per-stage latency histograms and event counters (D-094).
//!
//! With the `observe` feature off (the default) every type here is
//! zero-sized and every hook is an empty inline function: nothing is
//! compiled into the caller. With it on, one [`Observer`] per store holds a
//! fixed set of cache-line-aligned blocks, one per thread (modulo the block
//! count), allocated once at creation:
//! - counters are exact: one relaxed add on the thread's own block;
//! - latency is sampled: one span in [`SAMPLE`] per stage and block reads
//!   the clock, because a clock read (tens of nanoseconds) on every stage of
//!   every operation would cost more than the observer's budget (≤ 20 ns per
//!   operation, measured by the harness).
//!
//! No allocation after creation, no timer, no thread, no dependency.

/// A timed stage of an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Stage {
    /// A write, from the first submission to its last completion.
    Write = 0,
    /// A region read (`read`), from the call to its return. Scans are not
    /// timed: one span would cover a whole recovery pass.
    Read,
    /// A barrier, from the call to its return (waiting included).
    Barrier,
    /// One device flush issued by a barrier leader.
    Flush,
    /// A slot commit, end to end.
    SlotCommit,
    /// Provisioning a region (fill, flush, verify, header, table).
    Provision,
}

/// Number of [`Stage`] values.
pub const STAGES: usize = 6;

/// A counted event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Counter {
    /// Data writes (appends, page writes, batches) submitted to the device.
    /// Metadata writes (slot commits, headers, the region table) and
    /// provisioning fills are not counted; `DomainStats` counts every flush.
    Writes = 0,
    /// Bytes those writes carried.
    WriteBytes,
    /// Reads accepted by the device (region reads and scans, rescans
    /// included).
    Reads,
    /// Bytes those reads carried.
    ReadBytes,
    /// Barriers on the data path (every call, power-safe devices included).
    Barriers,
    /// Device flushes led by those barriers.
    Flushes,
    /// Single writes larger than one buffer, submitted as several device
    /// writes (a batch of several records or pages is not a split).
    Splits,
    /// Writes refused before reaching the device (media untouched).
    Refused,
    /// Unreadable ranges reported to the caller: by a read that fails with a
    /// media error, or as an unreadable range of a scan. The rescan that
    /// locates a range is not counted again.
    ///
    /// (Poisoning is not counted here: the store holds its first cause,
    /// reported by `Store::poisoned` and by every later error.)
    MediaErrors,
}

/// Number of [`Counter`] values.
pub const COUNTERS: usize = 9;

/// One span in this many is timed, per block.
pub const SAMPLE: u32 = 64;

/// Histogram buckets: bucket `b` holds durations in `[2^(b-1), 2^b)`
/// nanoseconds; bucket 0 holds 0 ns and the last bucket everything from
/// `2^62` up.
pub const BUCKETS: usize = 64;

/// The latency distribution of one stage, from the sampled spans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Histogram {
    /// Sample counts per bucket.
    pub buckets: [u64; BUCKETS],
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            buckets: [0; BUCKETS],
        }
    }
}

impl Histogram {
    /// Number of samples.
    #[must_use]
    pub fn samples(&self) -> u64 {
        self.buckets.iter().sum()
    }

    /// Upper bound in nanoseconds of the bucket holding quantile `q` (0..=1),
    /// by nearest rank; `None` without samples.
    #[must_use]
    pub fn quantile_ns(&self, q: f64) -> Option<u64> {
        let n = self.samples();
        if n == 0 {
            return None;
        }
        let q = if q.is_nan() { 1.0 } else { q.clamp(0.0, 1.0) };
        // Nearest rank: the smallest rank r with r >= q * n, at least 1.
        let rank = ((q * n as f64).ceil() as u64).clamp(1, n);
        let mut seen = 0u64;
        for (b, &c) in self.buckets.iter().enumerate() {
            seen += c;
            if seen >= rank {
                return Some(bucket_upper_ns(b));
            }
        }
        Some(bucket_upper_ns(BUCKETS - 1))
    }
}

/// The bucket for a duration of `ns` nanoseconds.
#[inline]
#[must_use]
pub fn bucket_of(ns: u64) -> usize {
    ((u64::BITS - ns.leading_zeros()) as usize).min(BUCKETS - 1)
}

/// Upper bound in nanoseconds of bucket `b` (exclusive); the last bucket is
/// unbounded and reports `u64::MAX`.
#[must_use]
pub fn bucket_upper_ns(b: usize) -> u64 {
    if b >= BUCKETS - 1 {
        u64::MAX
    } else {
        1u64 << b
    }
}

// The counts above must match the enums.
const _: () = assert!(Stage::Provision as usize + 1 == STAGES);
const _: () = assert!(Counter::MediaErrors as usize + 1 == COUNTERS);

/// A merged view of every block: exact counters and sampled latencies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    counters: [u64; COUNTERS],
    stages: [Histogram; STAGES],
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            counters: [0; COUNTERS],
            stages: [Histogram::default(); STAGES],
        }
    }
}

impl Snapshot {
    /// The total for a counter.
    #[must_use]
    pub fn counter(&self, c: Counter) -> u64 {
        self.counters[c as usize]
    }

    /// The sampled latency distribution of a stage.
    #[must_use]
    pub fn stage(&self, s: Stage) -> &Histogram {
        &self.stages[s as usize]
    }
}

/// Whether this build observes anything (the `observe` feature).
#[must_use]
pub const fn enabled() -> bool {
    cfg!(feature = "observe")
}

#[cfg(feature = "observe")]
mod on {
    use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
    use std::time::Instant;

    use super::{BUCKETS, COUNTERS, Counter, SAMPLE, STAGES, Snapshot, Stage, bucket_of};

    /// Blocks per observer: the machine's CPU count rounded up to a power of
    /// two (so picking a thread's block is a mask, not a division), at least
    /// 16 and at most 256. Threads beyond that share blocks: their counts stay
    /// exact, but their counter lines are shared between cores.
    fn block_count() -> usize {
        std::thread::available_parallelism()
            .map_or(16, std::num::NonZeroUsize::get)
            .next_power_of_two()
            .clamp(16, 256)
    }

    /// One thread's counters and histograms, on lines of its own.
    #[repr(align(128))]
    struct Block {
        counters: [AtomicU64; COUNTERS],
        /// Spans started per stage: each stage is sampled on its own, so a
        /// frequent stage never starves a rarer one of samples.
        ticks: [AtomicU32; STAGES],
        stages: [[AtomicU64; BUCKETS]; STAGES],
    }

    impl Block {
        fn new() -> Self {
            Self {
                counters: std::array::from_fn(|_| AtomicU64::new(0)),
                ticks: std::array::from_fn(|_| AtomicU32::new(0)),
                stages: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
            }
        }
    }

    /// Per-store observer (see the module docs).
    pub struct Observer {
        /// A power-of-two number of blocks (see `block_count`), about 3 KB
        /// each, built on the heap once.
        blocks: Box<[Block]>,
        /// `blocks.len() - 1`.
        mask: usize,
    }

    impl core::fmt::Debug for Observer {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.debug_struct("Observer")
                .field("blocks", &self.blocks.len())
                .finish()
        }
    }

    impl Default for Observer {
        fn default() -> Self {
            Self::new()
        }
    }

    fn thread_index() -> usize {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        thread_local! {
            static INDEX: usize = NEXT.fetch_add(1, Relaxed);
        }
        INDEX.with(|i| *i)
    }

    impl Observer {
        /// A fresh observer: every counter and histogram at zero.
        #[must_use]
        pub fn new() -> Self {
            let n = block_count();
            Self {
                blocks: (0..n).map(|_| Block::new()).collect(),
                mask: n - 1,
            }
        }

        #[inline]
        fn block(&self) -> &Block {
            &self.blocks[thread_index() & self.mask]
        }

        /// Adds `n` to a counter.
        #[inline]
        pub fn add(&self, c: Counter, n: u64) {
            let _old = self.block().counters[c as usize].fetch_add(n, Relaxed);
        }

        /// Starts timing a stage; the span records when dropped, if this
        /// span is one of the sampled ones.
        #[inline]
        pub fn span(&self, s: Stage) -> Span<'_> {
            let block = self.block();
            // The tick needs no atomic increment: a lost tick (two threads
            // sharing a block) only shifts which span is sampled.
            let tick = &block.ticks[s as usize];
            let n = tick.load(Relaxed);
            tick.store(n.wrapping_add(1), Relaxed);
            let sampled = n % SAMPLE == 0;
            Span {
                timed: sampled.then(|| (block, s, Instant::now())),
            }
        }

        /// Merges every block.
        #[must_use]
        pub fn snapshot(&self) -> Snapshot {
            let mut out = Snapshot::default();
            // Not an atomic cut across counters: each is read once, in turn.
            for b in self.blocks.iter() {
                for (sum, c) in out.counters.iter_mut().zip(&b.counters) {
                    *sum = sum.wrapping_add(c.load(Relaxed));
                }
                for (h, stage) in out.stages.iter_mut().zip(&b.stages) {
                    for (sum, c) in h.buckets.iter_mut().zip(stage) {
                        *sum = sum.wrapping_add(c.load(Relaxed));
                    }
                }
            }
            out
        }
    }

    /// A stage being timed; records on drop when sampled.
    #[must_use = "a span records when it is dropped"]
    pub struct Span<'a> {
        timed: Option<(&'a Block, Stage, Instant)>,
    }

    impl Drop for Span<'_> {
        #[inline]
        fn drop(&mut self) {
            if let Some((block, stage, start)) = self.timed {
                let ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
                let _old = block.stages[stage as usize][bucket_of(ns)].fetch_add(1, Relaxed);
            }
        }
    }
}

#[cfg(not(feature = "observe"))]
mod off {
    use super::{Counter, Snapshot, Stage};

    /// Per-store observer: zero-sized, every hook empty (feature `observe`
    /// is off).
    #[derive(Debug, Default)]
    pub struct Observer;

    impl Observer {
        /// A fresh observer.
        #[must_use]
        pub fn new() -> Self {
            Self
        }

        /// Does nothing.
        #[inline(always)]
        pub fn add(&self, _c: Counter, _n: u64) {}

        /// Does nothing.
        #[inline(always)]
        pub fn span(&self, _s: Stage) -> Span<'_> {
            Span(core::marker::PhantomData)
        }

        /// An empty snapshot.
        #[must_use]
        pub fn snapshot(&self) -> Snapshot {
            Snapshot::default()
        }
    }

    /// A stage being timed: zero-sized, records nothing.
    #[must_use = "a span records when it is dropped"]
    pub struct Span<'a>(core::marker::PhantomData<&'a ()>);
}

#[cfg(feature = "observe")]
pub use on::{Observer, Span};

#[cfg(not(feature = "observe"))]
pub use off::{Observer, Span};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buckets_split_durations_by_powers_of_two() {
        assert_eq!(bucket_of(0), 0);
        assert_eq!(bucket_of(1), 1);
        assert_eq!(bucket_of(2), 2);
        assert_eq!(bucket_of(3), 2);
        assert_eq!(bucket_of(4), 3);
        assert_eq!(bucket_of(1023), 10);
        assert_eq!(bucket_of(1024), 11);
        assert_eq!(bucket_of(1 << 62), BUCKETS - 1);
        assert_eq!(bucket_of(u64::MAX), BUCKETS - 1);
        assert_eq!(bucket_upper_ns(BUCKETS - 1), u64::MAX);
        for ns in [1u64, 7, 100, 4096, 1 << 40, (1 << 62) - 1] {
            let b = bucket_of(ns);
            assert!(ns < bucket_upper_ns(b), "{ns} not below its bucket's bound");
            assert!(
                b == 0 || ns >= bucket_upper_ns(b - 1),
                "{ns} below its bucket"
            );
        }
    }

    #[test]
    fn test_quantiles_use_nearest_rank_over_buckets() {
        let mut h = Histogram::default();
        assert_eq!(h.quantile_ns(0.5), None);
        h.buckets[bucket_of(100)] = 99;
        h.buckets[bucket_of(10_000)] = 1;
        assert_eq!(h.samples(), 100);
        assert_eq!(h.quantile_ns(0.5), Some(128));
        assert_eq!(h.quantile_ns(0.99), Some(128));
        assert_eq!(h.quantile_ns(1.0), Some(16_384));
        assert_eq!(h.quantile_ns(f64::NAN), Some(16_384));
    }

    #[cfg(not(feature = "observe"))]
    #[test]
    fn test_the_observer_is_compiled_out_when_off() {
        assert!(!enabled());
        assert_eq!(size_of::<Observer>(), 0);
        assert_eq!(size_of::<Span<'_>>(), 0);
        let o = Observer::new();
        o.add(Counter::Writes, 1);
        drop(o.span(Stage::Write));
        assert_eq!(o.snapshot(), Snapshot::default());
    }

    #[cfg(feature = "observe")]
    #[test]
    fn test_counters_are_exact_and_spans_are_sampled() {
        let o = Observer::new();
        std::thread::scope(|s| {
            for _ in 0..8 {
                let o = &o;
                let _worker = s.spawn(move || {
                    for _ in 0..1000 {
                        o.add(Counter::Writes, 1);
                        o.add(Counter::WriteBytes, 4096);
                        drop(o.span(Stage::Write));
                    }
                });
            }
        });
        let snap = o.snapshot();
        assert_eq!(snap.counter(Counter::Writes), 8000);
        assert_eq!(snap.counter(Counter::WriteBytes), 8000 * 4096);
        // One span in SAMPLE per block: each thread's first span is timed.
        let samples = snap.stage(Stage::Write).samples();
        assert!(
            samples >= 8000 / u64::from(SAMPLE),
            "too few samples: {samples}"
        );
        assert!(
            samples <= 8000 / u64::from(SAMPLE) + 16,
            "too many samples: {samples}"
        );
        assert_eq!(snap.stage(Stage::Read).samples(), 0);
    }
}
