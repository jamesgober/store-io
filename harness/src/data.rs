//! Deterministic, incompressible, self-identifying payloads.
//!
//! Every payload is a slice of one 16 MiB pseudo-random pool chosen by
//! `(stream, index)`, with its first 16 bytes overwritten by the stream and
//! index (little endian). A read-back can therefore be checked byte for byte
//! by regenerating the expected bytes, and a record found in a journal can be
//! identified from its own first 16 bytes. Copying from the pool costs a
//! memcpy, so generating a payload never dominates a measurement.

/// Pool size: large enough that two payloads rarely share bytes, small
/// enough to stay cache-friendly to build.
const POOL_LEN: usize = 16 << 20;

/// Largest payload [`Pool::fill`] produces.
pub const MAX_PAYLOAD: usize = POOL_LEN / 2;

/// Bytes of identity stamped at the start of every payload.
pub const STAMP_LEN: usize = 16;

/// The payload pool.
pub struct Pool {
    bytes: Vec<u8>,
}

/// SplitMix64 step: a fast, well-mixed 64-bit generator.
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Mixes a payload identity into a pool offset seed.
fn mix(stream: u64, index: u64) -> u64 {
    let mut s = stream.wrapping_mul(0xA24B_AED4_963E_E407) ^ index;
    splitmix(&mut s)
}

impl Pool {
    /// Builds the pool from a fixed seed (identical on every run).
    #[must_use]
    pub fn new() -> Self {
        let mut state = 0x5EED_F00D_5701_E100_u64;
        let mut bytes = vec![0u8; POOL_LEN];
        for chunk in bytes.chunks_exact_mut(8) {
            chunk.copy_from_slice(&splitmix(&mut state).to_le_bytes());
        }
        Self { bytes }
    }

    fn start(&self, stream: u64, index: u64, len: usize) -> usize {
        let span = (POOL_LEN - len) as u64 + 1;
        (mix(stream, index) % span) as usize
    }

    fn stamp(stream: u64, index: u64) -> [u8; STAMP_LEN] {
        let mut s = [0u8; STAMP_LEN];
        s[..8].copy_from_slice(&stream.to_le_bytes());
        s[8..].copy_from_slice(&index.to_le_bytes());
        s
    }

    /// Writes payload `(stream, index)` of `out.len()` bytes into `out`.
    ///
    /// # Panics
    ///
    /// If `out` is longer than [`MAX_PAYLOAD`].
    pub fn fill(&self, stream: u64, index: u64, out: &mut [u8]) {
        let len = out.len();
        assert!(
            len <= MAX_PAYLOAD,
            "payload of {len} bytes exceeds the pool"
        );
        let at = self.start(stream, index, len);
        out.copy_from_slice(&self.bytes[at..at + len]);
        let stamp = Self::stamp(stream, index);
        let n = len.min(STAMP_LEN);
        out[..n].copy_from_slice(&stamp[..n]);
    }

    /// Whether `got` is exactly payload `(stream, index)` of `got.len()`
    /// bytes.
    #[must_use]
    pub fn matches(&self, stream: u64, index: u64, got: &[u8]) -> bool {
        let len = got.len();
        if len > MAX_PAYLOAD {
            return false;
        }
        let at = self.start(stream, index, len);
        let n = len.min(STAMP_LEN);
        got[..n] == Self::stamp(stream, index)[..n] && got[n..] == self.bytes[at + n..at + len]
    }

    /// The `(stream, index)` identity stamped at the start of `payload`, if
    /// it is long enough to carry one.
    #[must_use]
    pub fn identify(payload: &[u8]) -> Option<(u64, u64)> {
        let s: [u8; 8] = payload.get(..8)?.try_into().ok()?;
        let i: [u8; 8] = payload.get(8..16)?.try_into().ok()?;
        Some((u64::from_le_bytes(s), u64::from_le_bytes(i)))
    }
}

/// A small deterministic generator for random page choices.
pub struct Rng(u64);

impl Rng {
    /// A generator from `seed`.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next value in `0..n` (`n > 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        splitmix(&mut self.0) % n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_payloads_are_deterministic_identifiable_and_checked() {
        let p = Pool::new();
        let mut a = vec![0u8; 4096];
        let mut b = vec![0u8; 4096];
        p.fill(3, 77, &mut a);
        p.fill(3, 77, &mut b);
        assert_eq!(a, b);
        assert!(p.matches(3, 77, &a));
        assert_eq!(Pool::identify(&a), Some((3, 77)));
        p.fill(3, 78, &mut b);
        assert_ne!(a, b);
        assert!(!p.matches(3, 77, &b));
        a[4000] ^= 1;
        assert!(!p.matches(3, 77, &a));
    }

    #[test]
    fn test_short_payloads_carry_a_partial_stamp() {
        let p = Pool::new();
        let mut a = vec![0u8; 12];
        p.fill(1, 2, &mut a);
        assert!(p.matches(1, 2, &a));
        assert_eq!(Pool::identify(&a), None);
    }
}
