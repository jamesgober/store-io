//! The observer: per-stage latency histograms and event counters (D-094).
//!
//! With the `observe` feature off (the default) every type here is
//! zero-sized and every hook is an empty inline function: nothing is
//! compiled into the caller. With it on, one [`Observer`] per store holds a
//! fixed set of cache-line-aligned blocks, allocated once at creation. Each
//! live thread holds a process-wide slot; a thread whose slot is below the
//! block count owns that block alone, any other thread spreads over as many
//! shared blocks (D-100):
//! - counters are exact: a plain load and store on an owned block (one
//!   writer, no locked instruction), an atomic add on a shared one;
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
    use std::sync::atomic::{
        AtomicU32, AtomicU64, AtomicUsize,
        Ordering::{Acquire, Relaxed, Release},
    };
    use std::time::Instant;

    use super::{BUCKETS, COUNTERS, Counter, SAMPLE, STAGES, Snapshot, Stage, bucket_of};

    /// Blocks per observer: the machine's CPU count rounded up to a power of
    /// two, at least 16 and at most [`SLOTS`]. A thread whose slot is below
    /// it owns that block alone; any other thread uses a shared block.
    fn block_count() -> usize {
        std::thread::available_parallelism()
            .map_or(16, std::num::NonZeroUsize::get)
            .next_power_of_two()
            .clamp(16, SLOTS)
    }

    /// Process-wide thread slots, one bit each. A power of two, and at least
    /// every block count: an index of `SLOTS` or more (a thread without a
    /// slot) can never name an owned block, and masking it spreads it over
    /// the shared blocks.
    const SLOTS: usize = 256;
    const _: () = assert!(SLOTS.is_power_of_two());
    static TAKEN: [AtomicU64; SLOTS / 64] = [const { AtomicU64::new(0) }; SLOTS / 64];

    /// Claims the lowest free slot, if any. Acquire: the slot's previous
    /// owner's last writes to its blocks happen before this thread's first.
    #[cold]
    fn claim() -> Option<usize> {
        for (w, word) in TAKEN.iter().enumerate() {
            let mut seen = word.load(Relaxed);
            while seen != u64::MAX {
                let bit = (!seen).trailing_zeros() as usize;
                let was = word.fetch_or(1 << bit, Acquire);
                if was & (1 << bit) == 0 {
                    return Some(w * 64 + bit);
                }
                seen = was;
            }
        }
        None
    }

    /// A thread's place: its slot, held until the thread exits (no two live
    /// threads hold the same one, so a block indexed by it has one writer).
    /// `index` is the slot, or for a thread that found none, [`SLOTS`] plus
    /// a spread over the shared blocks: an observer uses its own block when
    /// `index` is below its block count, else a shared block.
    struct Place {
        index: usize,
    }

    impl Place {
        #[cold]
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            Self {
                index: claim().unwrap_or_else(|| SLOTS + NEXT.fetch_add(1, Relaxed) % SLOTS),
            }
        }
    }

    impl Drop for Place {
        fn drop(&mut self) {
            let i = self.index;
            if i < SLOTS {
                // Release: this thread's writes happen before the next owner's.
                let _was = TAKEN[i / 64].fetch_and(!(1 << (i % 64)), Release);
            }
        }
    }

    /// While a thread's thread-locals are destroyed (it may no longer hold
    /// a slot): never an owned block.
    const GONE: usize = usize::MAX;

    /// This thread's index (see [`Place`]). Read only, so the lookups of one
    /// operation's hooks can be merged; a thread that found no free slot
    /// keeps to the shared blocks for its life (a retry here measured about
    /// 8 ns more per durable write).
    #[inline]
    fn index() -> usize {
        thread_local! {
            static PLACE: Place = Place::new();
        }
        PLACE.try_with(|p| p.index).unwrap_or(GONE)
    }

    /// Counters and histograms, on lines of their own: owned by one thread,
    /// or shared.
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
        /// One block per thread slot below `block_count`, about 3 KB each,
        /// built on the heap once; each has one writer.
        blocks: Box<[Block]>,
        /// As many blocks again for every other thread (more live threads
        /// than blocks), spread as every thread was before blocks were
        /// owned: atomic adds only.
        shared: Box<[Block]>,
        /// `shared.len() - 1` (a power of two).
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

    /// Adds `n` to a cell: a plain load and store in a block with one
    /// writer (exact, no locked instruction), an atomic add in a shared one.
    /// (Not from a signal handler: one could interrupt the owner between
    /// its load and its store.)
    #[inline]
    fn bump(cell: &AtomicU64, n: u64, own: bool) {
        if own {
            cell.store(cell.load(Relaxed).wrapping_add(n), Relaxed);
        } else {
            let _old = cell.fetch_add(n, Relaxed);
        }
    }

    impl Observer {
        /// A fresh observer: every counter and histogram at zero.
        #[must_use]
        pub fn new() -> Self {
            let n = block_count();
            Self {
                blocks: (0..n).map(|_| Block::new()).collect(),
                shared: (0..n).map(|_| Block::new()).collect(),
                mask: n - 1,
            }
        }

        /// The block for `index`, and whether the caller is its one writer.
        #[inline]
        fn block(&self, index: usize) -> (&Block, bool) {
            match self.blocks.get(index) {
                Some(b) => (b, true),
                None => (&self.shared[index & self.mask], false),
            }
        }

        /// Adds `n` to a counter.
        #[inline]
        pub fn add(&self, c: Counter, n: u64) {
            let (block, own) = self.block(index());
            bump(&block.counters[c as usize], n, own);
        }

        /// Starts timing a stage; the span records when dropped, if this
        /// span is one of the sampled ones.
        #[inline]
        pub fn span(&self, s: Stage) -> Span<'_> {
            let index = index();
            let (block, _) = self.block(index);
            // The tick needs no atomic increment: a lost tick (threads
            // sharing a shared block) only shifts which span is sampled.
            let tick = &block.ticks[s as usize];
            let n = tick.load(Relaxed);
            tick.store(n.wrapping_add(1), Relaxed);
            let sampled = n % SAMPLE == 0;
            Span {
                timed: sampled.then(|| (self, index, s, Instant::now())),
                _thread: core::marker::PhantomData,
            }
        }

        /// Merges every block.
        #[must_use]
        pub fn snapshot(&self) -> Snapshot {
            let mut out = Snapshot::default();
            // Not an atomic cut across counters: each is read once, in turn.
            for b in self.blocks.iter().chain(self.shared.iter()) {
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
        /// Sampled: the observer, the thread's index when it started, the
        /// stage and the start.
        timed: Option<(&'a Observer, usize, Stage, Instant)>,
        /// Dropped on the thread that started it.
        _thread: core::marker::PhantomData<*const ()>,
    }

    impl Drop for Span<'_> {
        #[inline]
        fn drop(&mut self) {
            if let Some((observer, started, stage, start)) = self.timed {
                let ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
                // A span dropped during the thread's teardown may outlive
                // its slot, whose block may have a new owner: then it is
                // recorded in a shared block.
                let now = if index() == started { started } else { GONE };
                let (block, own) = observer.block(now);
                bump(&block.stages[stage as usize][bucket_of(ns)], 1, own);
            }
        }
    }

    #[cfg(test)]
    mod blocks {
        use super::Observer;

        /// A block index past the per-thread blocks is a shared block,
        /// written with atomic adds; only a thread's own block is written
        /// plainly (D-100). Deterministic: no collision is needed.
        #[test]
        fn test_only_a_threads_own_block_is_written_plainly() {
            let o = Observer::new();
            let n = o.blocks.len();
            assert!(o.block(0).1, "an owned block is written plainly");
            for k in [n, n + 1, n + 7, 2 * n + 3] {
                let (b, own) = o.block(k);
                assert!(!own, "index {k}: a shared block must use atomic adds");
                assert!(core::ptr::eq(b, &o.shared[k & o.mask]));
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

    /// A stage being timed: zero-sized, records nothing. Bound to its
    /// thread, as with the feature on (features are additive).
    #[must_use = "a span records when it is dropped"]
    pub struct Span<'a>(core::marker::PhantomData<(&'a (), *const ())>);
}

#[cfg(feature = "observe")]
pub use on::{Observer, Span};

#[cfg(not(feature = "observe"))]
pub use off::{Observer, Span};

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the tests that spawn threads using an observer: slots are
    /// process-wide.
    #[cfg(feature = "observe")]
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        ONE.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

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
        // Alone: with another test's threads holding the slots, these could
        // share a block, and shared ticks may be lost (sampling shifts).
        let _one = serial();
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

    /// More live threads than blocks (and than process-wide slots): the
    /// extra threads use shared blocks with atomic adds, owners write their
    /// own, and every count is exact. Every thread is joined (its slot freed)
    /// before the second round, whose threads reuse the slots.
    #[cfg(feature = "observe")]
    #[test]
    fn test_counts_stay_exact_beyond_the_blocks_and_across_slot_reuse() {
        let _one = serial();
        let o = Observer::new();
        for round in 1..=2u64 {
            let start = std::sync::Barrier::new(300);
            std::thread::scope(|s| {
                let workers: Vec<_> = (0..300)
                    .map(|_| {
                        let (o, start) = (&o, &start);
                        s.spawn(move || {
                            // All 300 alive at once.
                            let _all = start.wait();
                            for _ in 0..5_000 {
                                o.add(Counter::Writes, 1);
                                drop(o.span(Stage::Write));
                            }
                        })
                    })
                    .collect();
                for w in workers {
                    assert!(w.join().is_ok());
                }
            });
            assert_eq!(o.snapshot().counter(Counter::Writes), round * 300 * 5_000);
        }
    }
}
