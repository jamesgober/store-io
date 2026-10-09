//! Bounds-checked little-endian reads over device and filesystem output.
//!
//! Every IOCTL result is parsed through this reader rather than by casting to
//! a C struct: a short, truncated or garbage buffer yields `None`, never an
//! out-of-bounds read, and a `BOOLEAN` field is read as a byte (a C `bool`
//! with a value other than 0 or 1 would be undefined in Rust).

/// A read-only view over a byte buffer.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    /// Length of the underlying buffer.
    pub(crate) fn len(&self) -> usize {
        self.buf.len()
    }

    /// `len` bytes at `off`, if they exist.
    pub(crate) fn slice(&self, off: usize, len: usize) -> Option<&'a [u8]> {
        let end = off.checked_add(len)?;
        self.buf.get(off..end)
    }

    pub(crate) fn u8(&self, off: usize) -> Option<u8> {
        self.buf.get(off).copied()
    }

    pub(crate) fn u16(&self, off: usize) -> Option<u16> {
        let s = self.slice(off, 2)?;
        Some(u16::from_le_bytes([s[0], s[1]]))
    }

    pub(crate) fn u32(&self, off: usize) -> Option<u32> {
        let s = self.slice(off, 4)?;
        Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    pub(crate) fn u64(&self, off: usize) -> Option<u64> {
        let s = self.slice(off, 8)?;
        let mut b = [0u8; 8];
        b.copy_from_slice(s);
        Some(u64::from_le_bytes(b))
    }

    pub(crate) fn i64(&self, off: usize) -> Option<i64> {
        self.u64(off).map(|v| v as i64)
    }

    /// A NUL-terminated or space-padded ASCII string starting at `off` and
    /// at most `max` bytes long, trimmed. Non-ASCII bytes are replaced.
    pub(crate) fn ascii(&self, off: usize, max: usize) -> Option<String> {
        let s = self.slice(off, max.min(self.buf.len().checked_sub(off)?))?;
        let end = s.iter().position(|&b| b == 0).unwrap_or(s.len());
        let text: String = s[..end]
            .iter()
            .map(|&b| {
                if b.is_ascii_graphic() || b == b' ' {
                    b as char
                } else {
                    '?'
                }
            })
            .collect();
        let t = text.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reader_reads_little_endian_within_bounds() {
        let b = [
            1u8, 0, 2, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F,
        ];
        let r = Reader::new(&b);
        assert_eq!(r.u8(0), Some(1));
        assert_eq!(r.u16(0), Some(1));
        assert_eq!(r.u32(2), Some(2));
        assert_eq!(r.i64(6), Some(i64::MAX));
        assert_eq!(r.len(), 14);
    }

    #[test]
    fn test_reader_truncated_and_overflowing_reads_are_none() {
        let b = [0u8; 4];
        let r = Reader::new(&b);
        assert_eq!(r.u32(1), None);
        assert_eq!(r.u64(0), None);
        assert_eq!(r.u8(4), None);
        assert_eq!(r.slice(usize::MAX, 2), None);
        assert_eq!(r.slice(2, usize::MAX), None);
        assert_eq!(Reader::new(&[]).u16(0), None);
    }

    #[test]
    fn test_reader_ascii_trims_and_replaces() {
        let b = b"  T-FORCE \x01X\0junk";
        let r = Reader::new(b);
        assert_eq!(r.ascii(0, 40).as_deref(), Some("T-FORCE ?X"));
        assert_eq!(r.ascii(0, 2), None);
        assert_eq!(r.ascii(100, 4), None);
        assert_eq!(Reader::new(b"\0\0").ascii(0, 2), None);
    }
}
