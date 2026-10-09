//! The keyed fill pattern used to provision regions on thin, virtual,
//! deduplicating, compressing or zero-detecting storage stacks.
//!
//! On such stacks, writing zeros may be turned into an unmap and the space is
//! never really allocated; a repeated pattern may be deduplicated or
//! compressed away. Pattern version 1 makes every 8-byte word of a region
//! unique and incompressible, so the stack has to store every block.
//!
//! Word `j` of a region (byte offset `8 * j` within the data area) is output
//! `j` of the SplitMix64 stream seeded with `k0`, XORed with `k1`:
//! `mix(k0 + (j + 1) * GAMMA) ^ k1`, where `k0` and `k1` are the two
//! little-endian halves of the region's 16-byte fill key and `GAMMA` is
//! `0x9E37_79B9_7F4A_7C15`. The definition is byte-exact and stable: version 1
//! never changes; a new pattern gets a new [`crate::meta::FillPattern`] value.

const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

#[inline(always)]
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Fills `out` with the version-1 pattern for the bytes that start at
/// `region_offset` within the data area.
///
/// `region_offset` and `out.len()` must be multiples of 8 (they are block
/// multiples in practice); any trailing bytes past the last full word are
/// filled from the next word's bytes so the function never panics.
///
/// # Examples
///
/// ```
/// use store_io_format::fill::fill_keyed_v1;
/// let key = [1u8; 16];
/// let mut a = [0u8; 64];
/// let mut b = [0u8; 32];
/// fill_keyed_v1(&key, 0, &mut a);
/// fill_keyed_v1(&key, 32, &mut b);
/// assert_eq!(&a[32..], &b[..]); // position-addressable
/// ```
pub fn fill_keyed_v1(key: &[u8; 16], region_offset: u64, out: &mut [u8]) {
    let mut k = [0u8; 8];
    k.copy_from_slice(&key[..8]);
    let k0 = u64::from_le_bytes(k);
    k.copy_from_slice(&key[8..]);
    let k1 = u64::from_le_bytes(k);
    let mut j = (region_offset / 8).wrapping_add(1);
    let mut chunks = out.chunks_exact_mut(8);
    for c in &mut chunks {
        c.copy_from_slice(&(mix(k0.wrapping_add(j.wrapping_mul(GAMMA))) ^ k1).to_le_bytes());
        j = j.wrapping_add(1);
    }
    let rest = chunks.into_remainder();
    if !rest.is_empty() {
        let w = (mix(k0.wrapping_add(j.wrapping_mul(GAMMA))) ^ k1).to_le_bytes();
        let n = rest.len();
        rest.copy_from_slice(&w[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fill_golden_vector_is_stable() {
        // Pinned output: changing it breaks every keyed region on disk.
        let mut out = [0u8; 16];
        fill_keyed_v1(&[0u8; 16], 0, &mut out);
        // With a zero key the pattern is the SplitMix64 stream for seed 0, whose
        // first two outputs are published reference values.
        assert_eq!(
            u64::from_le_bytes(out[..8].try_into().unwrap_or([0; 8])),
            0xE220_A839_7B1D_CDAF
        );
        assert_eq!(
            u64::from_le_bytes(out[8..].try_into().unwrap_or([0; 8])),
            0x6E78_9E6A_A1B9_65F4
        );
    }

    #[test]
    fn test_fill_blocks_are_unique_and_keys_differ() {
        let key = [3u8; 16];
        let mut a = vec![0u8; 4096 * 64];
        fill_keyed_v1(&key, 0, &mut a);
        let blocks: Vec<&[u8]> = a.chunks(4096).collect();
        for i in 0..blocks.len() {
            for j in 0..i {
                assert_ne!(blocks[i], blocks[j]);
            }
        }
        let mut b = vec![0u8; 4096];
        fill_keyed_v1(&[4u8; 16], 0, &mut b);
        assert_ne!(&a[..4096], &b[..]);
    }

    #[test]
    fn test_fill_has_no_repeated_words_in_a_megabyte() {
        let mut a = vec![0u8; 1 << 20];
        fill_keyed_v1(&[9u8; 16], 0, &mut a);
        let mut words: Vec<u64> = a
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap_or([0; 8])))
            .collect();
        words.sort_unstable();
        words.dedup();
        assert_eq!(words.len(), (1 << 20) / 8);
    }
}
