//! Little-endian, bounds-checked byte access.
//!
//! Decoders in this crate read untrusted bytes. Every read goes through
//! [`Reader`], which checks the remaining length first and returns
//! [`CodecError::Truncated`] instead of panicking. [`Writer`] does the same for
//! encoding into a caller-provided buffer. Neither allocates.

use core::fmt;

/// A read or write ran past the end of its buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    /// The input ended before the field being read.
    Truncated,
    /// The output buffer is too small for the field being written.
    Overflow,
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("input ended before the field being read"),
            Self::Overflow => f.write_str("output buffer too small for the field being written"),
        }
    }
}

impl std::error::Error for CodecError {}

/// Sequential little-endian reader over a byte slice.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Starts reading at the beginning of `buf`.
    #[inline]
    #[must_use]
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Bytes read so far.
    #[inline]
    #[must_use]
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Bytes left to read.
    #[inline]
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Reads exactly `N` bytes.
    ///
    /// # Errors
    ///
    /// [`CodecError::Truncated`] if fewer than `N` bytes remain.
    #[inline]
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], CodecError> {
        let end = self.pos.checked_add(N).ok_or(CodecError::Truncated)?;
        let src = self.buf.get(self.pos..end).ok_or(CodecError::Truncated)?;
        let mut out = [0u8; N];
        out.copy_from_slice(src);
        self.pos = end;
        Ok(out)
    }

    /// Reads `len` bytes as a borrowed slice.
    ///
    /// # Errors
    ///
    /// [`CodecError::Truncated`] if fewer than `len` bytes remain.
    #[inline]
    pub fn bytes(&mut self, len: usize) -> Result<&'a [u8], CodecError> {
        let end = self.pos.checked_add(len).ok_or(CodecError::Truncated)?;
        let src = self.buf.get(self.pos..end).ok_or(CodecError::Truncated)?;
        self.pos = end;
        Ok(src)
    }

    /// Reads a `u8`.
    ///
    /// # Errors
    ///
    /// [`CodecError::Truncated`] at end of input.
    #[inline]
    pub fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.array::<1>()?[0])
    }

    /// Reads a little-endian `u16`.
    ///
    /// # Errors
    ///
    /// [`CodecError::Truncated`] if fewer than 2 bytes remain.
    #[inline]
    pub fn u16(&mut self) -> Result<u16, CodecError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    /// Reads a little-endian `u32`.
    ///
    /// # Errors
    ///
    /// [`CodecError::Truncated`] if fewer than 4 bytes remain.
    #[inline]
    pub fn u32(&mut self) -> Result<u32, CodecError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    /// Reads a little-endian `u64`.
    ///
    /// # Errors
    ///
    /// [`CodecError::Truncated`] if fewer than 8 bytes remain.
    #[inline]
    pub fn u64(&mut self) -> Result<u64, CodecError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
}

/// Sequential little-endian writer into a caller-provided buffer.
#[derive(Debug)]
pub struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> Writer<'a> {
    /// Starts writing at the beginning of `buf`.
    #[inline]
    #[must_use]
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Bytes written so far.
    #[inline]
    #[must_use]
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Writes raw bytes.
    ///
    /// # Errors
    ///
    /// [`CodecError::Overflow`] if `src` does not fit.
    #[inline]
    pub fn bytes(&mut self, src: &[u8]) -> Result<(), CodecError> {
        let end = self
            .pos
            .checked_add(src.len())
            .ok_or(CodecError::Overflow)?;
        let dst = self
            .buf
            .get_mut(self.pos..end)
            .ok_or(CodecError::Overflow)?;
        dst.copy_from_slice(src);
        self.pos = end;
        Ok(())
    }

    /// Writes a `u8`.
    ///
    /// # Errors
    ///
    /// [`CodecError::Overflow`] if the buffer is full.
    #[inline]
    pub fn u8(&mut self, v: u8) -> Result<(), CodecError> {
        self.bytes(&[v])
    }

    /// Writes a little-endian `u16`.
    ///
    /// # Errors
    ///
    /// [`CodecError::Overflow`] if fewer than 2 bytes remain.
    #[inline]
    pub fn u16(&mut self, v: u16) -> Result<(), CodecError> {
        self.bytes(&v.to_le_bytes())
    }

    /// Writes a little-endian `u32`.
    ///
    /// # Errors
    ///
    /// [`CodecError::Overflow`] if fewer than 4 bytes remain.
    #[inline]
    pub fn u32(&mut self, v: u32) -> Result<(), CodecError> {
        self.bytes(&v.to_le_bytes())
    }

    /// Writes a little-endian `u64`.
    ///
    /// # Errors
    ///
    /// [`CodecError::Overflow`] if fewer than 8 bytes remain.
    #[inline]
    pub fn u64(&mut self, v: u64) -> Result<(), CodecError> {
        self.bytes(&v.to_le_bytes())
    }

    /// Writes `len` zero bytes.
    ///
    /// # Errors
    ///
    /// [`CodecError::Overflow`] if fewer than `len` bytes remain.
    #[inline]
    pub fn zeros(&mut self, len: usize) -> Result<(), CodecError> {
        let end = self.pos.checked_add(len).ok_or(CodecError::Overflow)?;
        let dst = self
            .buf
            .get_mut(self.pos..end)
            .ok_or(CodecError::Overflow)?;
        dst.fill(0);
        self.pos = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reader_reads_little_endian_in_order() {
        let buf = [1, 2, 0, 3, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 9];
        let mut r = Reader::new(&buf);
        assert_eq!(r.u8(), Ok(1));
        assert_eq!(r.u16(), Ok(2));
        assert_eq!(r.u32(), Ok(3));
        assert_eq!(r.u64(), Ok(4));
        assert_eq!(r.remaining(), 1);
        assert_eq!(r.u8(), Ok(9));
        assert_eq!(r.u8(), Err(CodecError::Truncated));
    }

    #[test]
    fn test_reader_truncated_read_does_not_advance() {
        let buf = [0u8; 3];
        let mut r = Reader::new(&buf);
        assert_eq!(r.u32(), Err(CodecError::Truncated));
        assert_eq!(r.position(), 0);
        assert_eq!(r.bytes(usize::MAX), Err(CodecError::Truncated));
    }

    #[test]
    fn test_writer_roundtrips_through_reader() {
        let mut buf = [0u8; 23];
        let mut w = Writer::new(&mut buf);
        assert_eq!(w.u8(7), Ok(()));
        assert_eq!(w.u16(0xBEEF), Ok(()));
        assert_eq!(w.u32(0xDEAD_BEEF), Ok(()));
        assert_eq!(w.u64(u64::MAX - 1), Ok(()));
        assert_eq!(w.zeros(8), Ok(()));
        assert_eq!(w.position(), 23);
        assert_eq!(w.u8(1), Err(CodecError::Overflow));
        let mut r = Reader::new(&buf);
        assert_eq!(r.u8(), Ok(7));
        assert_eq!(r.u16(), Ok(0xBEEF));
        assert_eq!(r.u32(), Ok(0xDEAD_BEEF));
        assert_eq!(r.u64(), Ok(u64::MAX - 1));
        assert_eq!(r.bytes(8), Ok(&[0u8; 8][..]));
    }
}
