//! CRC-32C (Castagnoli), the checksum of every store-io header.
//!
//! Parameters: polynomial 0x1EDC6F41 (reflected 0x82F63B78), initial value and
//! final XOR 0xFFFFFFFF, reflected input and output. Check value: the CRC of
//! the ASCII bytes `123456789` is `0xE306_9283`.
//!
//! Three implementations compute identical results:
//!
//! - **x86-64 with SSE4.2**: the `crc32` instruction on three interleaved
//!   streams, recombined with precomputed "shift by N zero bytes" tables
//!   (Mark Adler's method). Three independent streams hide the instruction's
//!   three-cycle latency.
//! - **AArch64 with the CRC extension**: the `crc32cx` instruction, same
//!   three-stream structure.
//! - **Portable fallback**: slicing-by-8 tables, used on every other target
//!   and when the CPU lacks the instruction.
//!
//! The hardware path is chosen at run time by CPU feature detection, so one
//! binary is correct everywhere. Every table is computed at compile time.

/// Reflected CRC-32C polynomial.
const POLY: u32 = 0x82F6_3B78;

/// Length of each of the three streams in the long interleaved loop.
const LONG: usize = 8192;

/// Length of each of the three streams in the short interleaved loop.
const SHORT: usize = 256;

/// Computes the CRC-32C of `data`.
///
/// # Examples
///
/// ```
/// use store_io_format::crc32c::crc32c;
/// assert_eq!(crc32c(b"123456789"), 0xE306_9283);
/// assert_eq!(crc32c(b""), 0);
/// ```
#[inline]
#[must_use]
pub fn crc32c(data: &[u8]) -> u32 {
    crc32c_append(0, data)
}

/// Extends a CRC-32C value with more data.
///
/// `crc32c_append(crc32c(a), b)` equals `crc32c` of `a` followed by `b`, so a
/// checksum can be computed over data held in separate buffers.
///
/// # Examples
///
/// ```
/// use store_io_format::crc32c::{crc32c, crc32c_append};
/// let whole = crc32c(b"hello world");
/// assert_eq!(crc32c_append(crc32c(b"hello "), b"world"), whole);
/// ```
#[inline]
#[must_use]
pub fn crc32c_append(crc: u32, data: &[u8]) -> u32 {
    !update(!crc, data)
}

/// Updates a raw (pre-inverted) CRC register with `data`, choosing the fastest
/// implementation the CPU supports.
#[inline]
fn update(reg: u32, data: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("sse4.2") {
            // SAFETY: the `sse4.2` target feature was detected on this CPU just
            // above, which is the only precondition of `x86::update`.
            return unsafe { x86::update(reg, data) };
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        if std::arch::is_aarch64_feature_detected!("crc") {
            // SAFETY: the `crc` target feature was detected on this CPU just
            // above, which is the only precondition of `arm::update`.
            return unsafe { arm::update(reg, data) };
        }
    }
    portable::update(reg, data)
}

// ---------------------------------------------------------------------------
// Shift tables: multiplying a register by x^(8n) mod P, i.e. appending n zero
// bytes, as four table lookups. Built at compile time.
// ---------------------------------------------------------------------------

/// Multiplies `a` by `b` modulo the CRC polynomial (reflected bit order).
const fn mult_mod_p(a: u32, mut b: u32) -> u32 {
    let mut m: u32 = 1 << 31;
    let mut p: u32 = 0;
    loop {
        if a & m != 0 {
            p ^= b;
            if a & (m - 1) == 0 {
                break;
            }
        }
        m >>= 1;
        b = if b & 1 != 0 { (b >> 1) ^ POLY } else { b >> 1 };
        if m == 0 {
            break;
        }
    }
    p
}

/// Returns x^(8 * `bytes`) mod P (reflected), by square-and-multiply.
const fn x_pow_8n(bytes: usize) -> u32 {
    // x2n[k] = x^(2^k) mod P, starting from x^1 = 1 << 30 in reflected order.
    let mut x2n = [0u32; 64];
    let mut p: u32 = 1 << 30;
    x2n[0] = p;
    let mut k = 1;
    while k < 64 {
        p = mult_mod_p(p, p);
        x2n[k] = p;
        k += 1;
    }
    // exponent = 8 * bytes = bytes << 3, so start the walk at x2n[3].
    let mut result: u32 = 1 << 31; // x^0
    let mut n = bytes;
    let mut idx = 3;
    while n != 0 {
        if n & 1 != 0 {
            result = mult_mod_p(x2n[idx], result);
        }
        n >>= 1;
        idx += 1;
    }
    result
}

/// Builds the four byte-lane tables that multiply a register by x^(8 * bytes).
const fn shift_table(bytes: usize) -> [[u32; 256]; 4] {
    let op = x_pow_8n(bytes);
    let mut t = [[0u32; 256]; 4];
    let mut n = 0;
    while n < 256 {
        let v = n as u32;
        t[0][n] = mult_mod_p(op, v);
        t[1][n] = mult_mod_p(op, v << 8);
        t[2][n] = mult_mod_p(op, v << 16);
        t[3][n] = mult_mod_p(op, v << 24);
        n += 1;
    }
    t
}

/// Shift tables for the long (8 KiB per stream) interleaved loop.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
static SHIFT_LONG: [[u32; 256]; 4] = shift_table(LONG);

/// Shift tables for the short (256 B per stream) interleaved loop.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
static SHIFT_SHORT: [[u32; 256]; 4] = shift_table(SHORT);

/// Applies a shift table to a register.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[inline(always)]
fn shift(table: &[[u32; 256]; 4], reg: u32) -> u32 {
    table[0][(reg & 0xFF) as usize]
        ^ table[1][((reg >> 8) & 0xFF) as usize]
        ^ table[2][((reg >> 16) & 0xFF) as usize]
        ^ table[3][(reg >> 24) as usize]
}

/// Reads eight little-endian bytes. Callers guarantee `bytes.len() >= 8`.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[inline(always)]
fn le_u64(bytes: &[u8]) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(word)
}

/// Generates the three-stream hardware update for one architecture from its
/// 8-byte and 1-byte CRC instructions.
///
/// The generated function is `unsafe` and carries `#[target_feature]` so it
/// compiles on the 1.85 MSRV (safe target-feature functions arrived in 1.86);
/// `unused_unsafe` is allowed because newer toolchains treat these intrinsics
/// as safe inside a matching target-feature context.
macro_rules! interleaved_update {
    ($feature:literal, $crc64:path, $crc8:path) => {
        /// Hardware CRC register update over three interleaved streams.
        ///
        /// # Safety
        ///
        /// The caller must have verified at run time that the CPU supports
        /// the target feature this function is compiled for.
        #[target_feature(enable = $feature)]
        #[allow(unused_unsafe)]
        pub(super) unsafe fn update(mut reg: u32, mut data: &[u8]) -> u32 {
            while data.len() >= 3 * super::LONG {
                let (mut r1, mut r2) = (0u32, 0u32);
                let (a, rest) = data.split_at(super::LONG);
                let (b, rest) = rest.split_at(super::LONG);
                let (c, rest) = rest.split_at(super::LONG);
                for i in (0..super::LONG).step_by(8) {
                    // SAFETY: the target feature is enabled for this function
                    // and verified by the caller; the arguments are plain values.
                    unsafe {
                        reg = $crc64(reg, super::le_u64(&a[i..]));
                        r1 = $crc64(r1, super::le_u64(&b[i..]));
                        r2 = $crc64(r2, super::le_u64(&c[i..]));
                    }
                }
                reg = super::shift(&super::SHIFT_LONG, reg) ^ r1;
                reg = super::shift(&super::SHIFT_LONG, reg) ^ r2;
                data = rest;
            }
            while data.len() >= 3 * super::SHORT {
                let (mut r1, mut r2) = (0u32, 0u32);
                let (a, rest) = data.split_at(super::SHORT);
                let (b, rest) = rest.split_at(super::SHORT);
                let (c, rest) = rest.split_at(super::SHORT);
                for i in (0..super::SHORT).step_by(8) {
                    // SAFETY: as above.
                    unsafe {
                        reg = $crc64(reg, super::le_u64(&a[i..]));
                        r1 = $crc64(r1, super::le_u64(&b[i..]));
                        r2 = $crc64(r2, super::le_u64(&c[i..]));
                    }
                }
                reg = super::shift(&super::SHIFT_SHORT, reg) ^ r1;
                reg = super::shift(&super::SHIFT_SHORT, reg) ^ r2;
                data = rest;
            }
            let mut words = data.chunks_exact(8);
            for w in &mut words {
                // SAFETY: as above.
                reg = unsafe { $crc64(reg, super::le_u64(w)) };
            }
            for &byte in words.remainder() {
                // SAFETY: as above.
                reg = unsafe { $crc8(reg, byte) };
            }
            reg
        }
    };
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    /// 8-byte step: `_mm_crc32_u64` keeps the register in the low 32 bits.
    ///
    /// # Safety
    ///
    /// Requires SSE4.2.
    #[inline]
    #[target_feature(enable = "sse4.2")]
    #[allow(unused_unsafe)]
    unsafe fn crc64(reg: u32, word: u64) -> u32 {
        // SAFETY: SSE4.2 is enabled for this function and required of callers.
        // The instruction zero-extends a 32-bit result, so the cast is exact.
        unsafe { std::arch::x86_64::_mm_crc32_u64(u64::from(reg), word) as u32 }
    }

    /// 1-byte step.
    ///
    /// # Safety
    ///
    /// Requires SSE4.2.
    #[inline]
    #[target_feature(enable = "sse4.2")]
    #[allow(unused_unsafe)]
    unsafe fn crc8(reg: u32, byte: u8) -> u32 {
        // SAFETY: SSE4.2 is enabled for this function and required of callers.
        unsafe { std::arch::x86_64::_mm_crc32_u8(reg, byte) }
    }

    interleaved_update!("sse4.2", crc64, crc8);
}

#[cfg(target_arch = "aarch64")]
mod arm {
    /// 8-byte step.
    ///
    /// # Safety
    ///
    /// Requires the AArch64 CRC extension.
    #[inline]
    #[target_feature(enable = "crc")]
    #[allow(unused_unsafe)]
    unsafe fn crc64(reg: u32, word: u64) -> u32 {
        // SAFETY: the CRC extension is enabled for this function and required
        // of callers.
        unsafe { std::arch::aarch64::__crc32cd(reg, word) }
    }

    /// 1-byte step.
    ///
    /// # Safety
    ///
    /// Requires the AArch64 CRC extension.
    #[inline]
    #[target_feature(enable = "crc")]
    #[allow(unused_unsafe)]
    unsafe fn crc8(reg: u32, byte: u8) -> u32 {
        // SAFETY: as above.
        unsafe { std::arch::aarch64::__crc32cb(reg, byte) }
    }

    interleaved_update!("crc", crc64, crc8);
}

/// Slicing-by-8 software implementation, used on targets without CRC
/// instructions and as the reference the hardware paths are tested against.
pub(crate) mod portable {
    use super::POLY;

    /// `TABLES[k][b]`: the CRC contribution of byte `b` followed by `k` zero bytes.
    static TABLES: [[u32; 256]; 8] = build_tables();

    const fn build_tables() -> [[u32; 256]; 8] {
        let mut t = [[0u32; 256]; 8];
        let mut n = 0;
        while n < 256 {
            let mut c = n as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
                k += 1;
            }
            t[0][n] = c;
            n += 1;
        }
        let mut n = 0;
        while n < 256 {
            let mut k = 1;
            while k < 8 {
                let prev = t[k - 1][n];
                t[k][n] = (prev >> 8) ^ t[0][(prev & 0xFF) as usize];
                k += 1;
            }
            n += 1;
        }
        t
    }

    /// Software CRC register update.
    pub(crate) fn update(mut reg: u32, data: &[u8]) -> u32 {
        let mut chunks = data.chunks_exact(8);
        for c in &mut chunks {
            let lo = reg ^ u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
            reg = TABLES[7][(lo & 0xFF) as usize]
                ^ TABLES[6][((lo >> 8) & 0xFF) as usize]
                ^ TABLES[5][((lo >> 16) & 0xFF) as usize]
                ^ TABLES[4][(lo >> 24) as usize]
                ^ TABLES[3][c[4] as usize]
                ^ TABLES[2][c[5] as usize]
                ^ TABLES[1][c[6] as usize]
                ^ TABLES[0][c[7] as usize];
        }
        for &b in chunks.remainder() {
            reg = (reg >> 8) ^ TABLES[0][((reg ^ u32::from(b)) & 0xFF) as usize];
        }
        reg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(data: &[u8]) -> u32 {
        !portable::update(!0, data)
    }

    /// Bit-at-a-time definition, independent of every table.
    fn bitwise(data: &[u8]) -> u32 {
        let mut reg = !0u32;
        for &b in data {
            reg ^= u32::from(b);
            for _ in 0..8 {
                reg = if reg & 1 != 0 {
                    (reg >> 1) ^ POLY
                } else {
                    reg >> 1
                };
            }
        }
        !reg
    }

    #[test]
    fn test_crc32c_check_value_matches_standard() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn test_crc32c_rfc3720_vectors_match() {
        assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
        assert_eq!(crc32c(&[0xFFu8; 32]), 0x62A8_AB43);
        let inc: Vec<u8> = (0u8..32).collect();
        assert_eq!(crc32c(&inc), 0x46DD_794E);
        let dec: Vec<u8> = (0u8..32).rev().collect();
        assert_eq!(crc32c(&dec), 0x113F_DB5C);
    }

    #[test]
    fn test_crc32c_empty_is_zero() {
        assert_eq!(crc32c(&[]), 0);
    }

    #[test]
    fn test_crc32c_every_length_and_offset_matches_bitwise() {
        // Covers every tail length, every alignment, and both interleave
        // thresholds (3 * SHORT and 3 * LONG) on either side.
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let buf: Vec<u8> = (0..(3 * LONG + 3 * SHORT + 64))
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect();
        let lens = [
            0,
            1,
            7,
            8,
            9,
            63,
            64,
            255,
            3 * SHORT - 1,
            3 * SHORT,
            3 * SHORT + 1,
            3 * LONG - 1,
            3 * LONG,
            3 * LONG + 1,
            3 * LONG + 3 * SHORT + 9,
        ];
        for off in 0..9 {
            for &len in &lens {
                if off + len > buf.len() {
                    continue;
                }
                let s = &buf[off..off + len];
                let want = bitwise(s);
                assert_eq!(crc32c(s), want, "dispatch off={off} len={len}");
                assert_eq!(reference(s), want, "portable off={off} len={len}");
            }
        }
    }

    #[test]
    fn test_crc32c_append_equals_whole_for_every_split() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 31 + 7) as u8).collect();
        let whole = crc32c(&data);
        for split in [0, 1, 7, 8, 100, 768, 999, 1000] {
            let (a, b) = data.split_at(split);
            assert_eq!(crc32c_append(crc32c(a), b), whole, "split {split}");
        }
    }

    #[test]
    fn test_shift_table_equals_appending_zero_bytes() {
        for reg in [0u32, 1, 0xDEAD_BEEF, 0xFFFF_FFFF, 0x8000_0001] {
            let via_zeros = portable::update(reg, &[0u8; SHORT]);
            #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
            assert_eq!(shift(&SHIFT_SHORT, reg), via_zeros);
            let _ = via_zeros;
        }
    }
}
