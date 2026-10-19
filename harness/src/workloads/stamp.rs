//! Block patterns for the gates' integrity checks. Every 8-byte word of a
//! 4 KiB block is derived from the block's number and the write's sequence
//! number, so a block read back from the wrong place, left over from an
//! older write, or never filled (zeros, or a recycled buffer's bytes)
//! cannot match. Stamping costs one multiply and one xor per word.

/// The unit a pattern covers.
pub const BLOCK: usize = 4096;

fn splitmix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Word `k` of block `block` written by write `seq`. Words differ within a
/// block (the multiplier is odd), so no block of all-equal bytes matches.
fn word(block: u64, seq: u64, k: u64) -> u64 {
    splitmix(block ^ splitmix(seq)) ^ k.wrapping_mul(0xD6E8_FEB8_6659_FD93)
}

/// Fills `buf` (whole blocks) as write `seq` of the blocks starting at
/// `first`.
pub fn stamp(buf: &mut [u8], first: u64, seq: u64) {
    for (b, blk) in buf.chunks_exact_mut(BLOCK).enumerate() {
        for (k, w) in blk.chunks_exact_mut(8).enumerate() {
            w.copy_from_slice(&word(first + b as u64, seq, k as u64).to_le_bytes());
        }
    }
}

/// Whether `buf` (whole blocks) holds exactly write `seq` of the blocks
/// starting at `first`.
#[must_use]
pub fn holds(buf: &[u8], first: u64, seq: u64) -> bool {
    buf.len() % BLOCK == 0
        && buf.chunks_exact(BLOCK).enumerate().all(|(b, blk)| {
            blk.chunks_exact(8)
                .enumerate()
                .all(|(k, w)| w == word(first + b as u64, seq, k as u64).to_le_bytes())
        })
}

/// The cheap check inside a measured window: the first word of each block.
#[must_use]
pub fn heads(buf: &[u8], first: u64, seq: u64) -> bool {
    buf.len() % BLOCK == 0
        && buf
            .chunks_exact(BLOCK)
            .enumerate()
            .all(|(b, blk)| blk[..8] == word(first + b as u64, seq, 0).to_le_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_a_pattern_names_its_block_and_write() {
        let mut a = vec![0u8; 2 * BLOCK];
        assert!(!holds(&a, 0, 0), "zeros never match");
        stamp(&mut a, 10, 3);
        assert!(holds(&a, 10, 3) && heads(&a, 10, 3));
        assert!(!holds(&a, 11, 3), "another place");
        assert!(!holds(&a, 10, 2), "another write");
        assert!(!heads(&a[BLOCK..], 10, 3), "a block shifted by one");
        let mut b = a.clone();
        b[BLOCK + 100] ^= 1;
        assert!(
            !holds(&b, 10, 3) && heads(&b, 10, 3),
            "one flipped bit inside a block"
        );
        assert!(!holds(&a[..100], 10, 3), "not whole blocks");
    }
}
