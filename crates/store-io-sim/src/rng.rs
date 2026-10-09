//! The simulator's only source of randomness: xoshiro256** seeded through
//! SplitMix64. In-tree so a seed means the same run on every platform and
//! compiler, forever.

/// Deterministic pseudo-random generator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rng {
    s: [u64; 4],
}

impl Rng {
    /// Seeds the generator. Every seed (including 0) gives a valid state.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        let mut sm = seed;
        let mut next = || {
            sm = sm.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = sm;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        Self {
            s: [next(), next(), next(), next()],
        }
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform value in `0..n` (`0` when `n == 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        // Lemire's multiply-shift; the tiny bias is irrelevant for simulation.
        ((u128::from(self.next_u64()) * u128::from(n)) >> 64) as u64
    }

    /// `true` with probability `per_million / 1_000_000`.
    pub fn chance(&mut self, per_million: u32) -> bool {
        self.below(1_000_000) < u64::from(per_million)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rng_is_deterministic_and_seed_sensitive() {
        let a: Vec<u64> = {
            let mut r = Rng::new(42);
            (0..8).map(|_| r.next_u64()).collect()
        };
        let b: Vec<u64> = {
            let mut r = Rng::new(42);
            (0..8).map(|_| r.next_u64()).collect()
        };
        let c: Vec<u64> = {
            let mut r = Rng::new(43);
            (0..8).map(|_| r.next_u64()).collect()
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn test_below_stays_in_range() {
        let mut r = Rng::new(1);
        assert!((0..10_000).all(|_| r.below(7) < 7));
        assert_eq!(r.below(0), 0);
        assert!(!(0..1000).any(|_| r.chance(0)));
        assert!((0..1000).all(|_| r.chance(1_000_000)));
    }
}
