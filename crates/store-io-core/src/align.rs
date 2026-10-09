//! Checked alignment arithmetic for offsets and lengths.

/// Rounds `value` up to a multiple of `align` (a power of two).
///
/// Returns `None` if `align` is not a power of two or the result overflows.
///
/// # Examples
///
/// ```
/// use store_io_core::align::align_up;
/// assert_eq!(align_up(5000, 4096), Some(8192));
/// assert_eq!(align_up(8192, 4096), Some(8192));
/// assert_eq!(align_up(1, 3), None);
/// ```
#[inline]
#[must_use]
pub const fn align_up(value: u64, align: u64) -> Option<u64> {
    if align == 0 || !align.is_power_of_two() {
        return None;
    }
    match value.checked_add(align - 1) {
        Some(v) => Some(v & !(align - 1)),
        None => None,
    }
}

/// Whether `value` is a multiple of `align` (a power of two; `false` otherwise).
#[inline]
#[must_use]
pub const fn is_aligned(value: u64, align: u64) -> bool {
    align != 0 && align.is_power_of_two() && value & (align - 1) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_align_up_edges() {
        assert_eq!(align_up(0, 4096), Some(0));
        assert_eq!(align_up(1, 4096), Some(4096));
        assert_eq!(align_up(4095, 4096), Some(4096));
        assert_eq!(align_up(4097, 4096), Some(8192));
        assert_eq!(align_up(u64::MAX, 4096), None);
        assert_eq!(align_up(10, 0), None);
    }

    #[test]
    fn test_is_aligned_edges() {
        assert!(is_aligned(0, 512));
        assert!(is_aligned(8192, 4096));
        assert!(!is_aligned(8193, 4096));
        assert!(!is_aligned(8, 3));
        assert!(!is_aligned(8, 0));
    }
}
