//! A tiny allocation-free bit set over small enums, used for reason and
//! missing-evidence sets that travel inside errors and reports.

use core::fmt;
use core::marker::PhantomData;

/// An enum with at most 64 variants that can live in a [`BitSet`].
pub trait Flag: Copy + fmt::Debug + 'static {
    /// Every variant, in declaration order.
    const ALL: &'static [Self];
    /// The variant's bit position (0..64), unique per variant.
    fn bit(self) -> u32;
}

/// A set of [`Flag`] values stored in one `u64`.
pub struct BitSet<T: Flag> {
    bits: u64,
    _t: PhantomData<T>,
}

impl<T: Flag> BitSet<T> {
    /// The empty set.
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bits: 0,
            _t: PhantomData,
        }
    }

    /// Adds a value.
    #[inline]
    pub fn insert(&mut self, v: T) {
        self.bits |= 1u64 << v.bit();
    }

    /// Returns the set with `v` added.
    #[inline]
    #[must_use]
    pub fn with(mut self, v: T) -> Self {
        self.insert(v);
        self
    }

    /// Whether `v` is in the set.
    #[inline]
    #[must_use]
    pub fn contains(&self, v: T) -> bool {
        self.bits & (1u64 << v.bit()) != 0
    }

    /// Whether the set is empty.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.bits == 0
    }

    /// Number of values in the set.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> u32 {
        self.bits.count_ones()
    }

    /// The members, in declaration order.
    pub fn iter(&self) -> impl Iterator<Item = T> + '_ {
        T::ALL.iter().copied().filter(move |v| self.contains(*v))
    }

    /// Union of two sets.
    #[inline]
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self {
            bits: self.bits | other.bits,
            _t: PhantomData,
        }
    }
}

impl<T: Flag> Clone for BitSet<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: Flag> Copy for BitSet<T> {}
impl<T: Flag> Default for BitSet<T> {
    fn default() -> Self {
        Self::new()
    }
}
impl<T: Flag> PartialEq for BitSet<T> {
    fn eq(&self, other: &Self) -> bool {
        self.bits == other.bits
    }
}
impl<T: Flag> Eq for BitSet<T> {}

impl<T: Flag> fmt::Debug for BitSet<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl<T: Flag> FromIterator<T> for BitSet<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut s = Self::new();
        for v in iter {
            s.insert(v);
        }
        s
    }
}

/// Declares a `Flag` enum with documented variants and a `Display` built from
/// each variant's doc text.
macro_rules! flag_enum {
    ($(#[$meta:meta])* pub enum $name:ident { $($(#[doc = $doc:literal])+ $variant:ident = $bit:literal,)+ }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        pub enum $name {
            $($(#[doc = $doc])+ $variant,)+
        }

        impl $crate::set::Flag for $name {
            const ALL: &'static [Self] = &[$(Self::$variant,)+];
            fn bit(self) -> u32 {
                match self { $(Self::$variant => $bit,)+ }
            }
        }

        impl core::fmt::Display for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str(match self { $(Self::$variant => concat!($($doc),+).trim(),)+ })
            }
        }
    };
}
pub(crate) use flag_enum;

#[cfg(test)]
mod tests {
    use super::*;

    flag_enum! {
        /// Test flags.
        pub enum T {
            /// first
            A = 0,
            /// second
            B = 63,
        }
    }

    #[test]
    fn test_bitset_insert_contains_iter_and_display() {
        let s: BitSet<T> = [T::B].into_iter().collect();
        assert!(s.contains(T::B) && !s.contains(T::A));
        assert_eq!(s.with(T::A).iter().collect::<Vec<_>>(), vec![T::A, T::B]);
        assert_eq!(s.len(), 1);
        assert_eq!(T::B.to_string(), "second");
    }
}
