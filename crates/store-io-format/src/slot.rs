//! The A/B slot: the one on-disk object every piece of store-io metadata is
//! written with (volume metadata, the region table, region headers, and named
//! small objects).
//!
//! A slot is a fixed-size, power-of-two block holding a 128-byte header
//! followed by a payload. An object is stored as a *pair* of slots in two
//! separate blocks; each update writes the slot that does not hold the current
//! version, as one device I/O, then makes it durable. A crash during the write
//! can damage only the slot being written, so the other slot still holds the
//! previous version (see [`crate::pair`] for how a winner is chosen).
//!
//! # Header layout (little-endian, 128 bytes)
//!
//! | Offset | Size | Field | Rule |
//! |---|---|---|---|
//! | 0 | 8 | `magic` | `b"SIOSLOT\0"` |
//! | 8 | 4 | `header_crc32c` | CRC-32C over `[0, header_len)` with this field read as 0 |
//! | 12 | 2 | `header_len` | 128 in format 1; `128 ≤ len ≤ slot size` |
//! | 14 | 2 | `format_major` | 1; anything else is refused |
//! | 16 | 4 | `incompat_flags` | any unknown bit: refuse to read |
//! | 20 | 4 | `ro_compat_flags` | any unknown bit: readable, never rewritten |
//! | 24 | 4 | `compat_flags` | unknown bits ignored |
//! | 28 | 4 | `payload_crc32c` | CRC-32C over the payload bytes |
//! | 32 | 16 | `volume_uuid` | must equal the expected volume |
//! | 48 | 8 | `object_id` | must equal the expected object |
//! | 56 | 8 | `generation` | ≥ 1; +1 per committed update |
//! | 64 | 8 | `slot_offset` | absolute byte offset of this slot; catches misdirected writes |
//! | 72 | 4 | `payload_len` | `≤ slot size − header_len` |
//! | 76 | 1 | `slot_index` | 0 = A, 1 = B; must match the location |
//! | 77 | 1 | `slot_count` | 2 in format 1 |
//! | 78 | 1 | `log2_slot_size` | 9 to 16 (512 B to 64 KiB) |
//! | 79 | 1 | reserved | 0 |
//! | 80 | 4 | `prev_header_crc32c` | header CRC of the previous generation |
//! | 84 | 4 | reserved | 0 |
//! | 88 | 8 | `owner_generation` | owner generation of the writer (fencing) |
//! | 96 | 32 | reserved | 0 |
//!
//! The payload starts at `header_len`; the rest of the slot is zero.

use core::fmt;

use crate::codec::{Reader, Writer};
use crate::crc32c::{crc32c, crc32c_append};

/// Magic bytes at offset 0 of every slot.
pub const MAGIC: [u8; 8] = *b"SIOSLOT\0";
/// Header length written by format 1.
pub const HEADER_LEN: usize = 128;
/// The only on-disk format this crate reads and writes.
pub const FORMAT_MAJOR: u16 = 1;
/// Number of slots in a pair.
pub const SLOT_COUNT: u8 = 2;
/// Smallest supported slot size, as a power of two (512 bytes).
pub const MIN_LOG2_SLOT_SIZE: u8 = 9;
/// Largest supported slot size, as a power of two (64 KiB).
pub const MAX_LOG2_SLOT_SIZE: u8 = 16;
/// `incompat_flags` bits this version understands (none in format 1).
pub const KNOWN_INCOMPAT: u32 = 0;
/// `ro_compat_flags` bits this version understands (none in format 1).
pub const KNOWN_RO_COMPAT: u32 = 0;

const CRC_FIELD: core::ops::Range<usize> = 8..12;

/// Which slot of a pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SlotIndex {
    /// The first slot of the pair (index 0).
    A,
    /// The second slot of the pair (index 1).
    B,
}

impl SlotIndex {
    /// The on-disk index value.
    #[inline]
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::A => 0,
            Self::B => 1,
        }
    }

    /// The other slot of the pair.
    #[inline]
    #[must_use]
    pub const fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }
}

/// Everything written into a slot header except the CRCs and constants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotFields {
    /// Volume identity; a slot from another volume is rejected as foreign.
    pub volume_uuid: [u8; 16],
    /// Identity of the object this pair stores.
    pub object_id: u64,
    /// Generation of this version; at least 1, one more than the previous.
    pub generation: u64,
    /// Owner generation of the process writing the slot.
    pub owner_generation: u64,
    /// Absolute byte offset the slot is written to.
    pub slot_offset: u64,
    /// Which slot of the pair this is.
    pub slot_index: SlotIndex,
    /// Slot size as a power of two.
    pub log2_slot_size: u8,
    /// Header CRC of the version this one replaces (0 for generation 1).
    pub prev_header_crc32c: u32,
}

/// Why a slot could not be encoded. These are caller bugs, never media errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// `log2_slot_size` is outside 9..=16.
    SlotSize,
    /// The output buffer length is not exactly the slot size.
    BufferLength,
    /// The payload does not fit after the header.
    PayloadTooLarge,
    /// Generation 0 is never written.
    ZeroGeneration,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::SlotSize => "slot size must be a power of two from 512 bytes to 64 KiB",
            Self::BufferLength => "output buffer length must equal the slot size",
            Self::PayloadTooLarge => "payload does not fit in the slot after the header",
            Self::ZeroGeneration => "generation 0 is never written",
        })
    }
}

impl std::error::Error for EncodeError {}

/// Encodes a complete slot into `out`, which must be exactly the slot size.
///
/// Returns the header CRC, which the next generation records as its
/// `prev_header_crc32c`.
///
/// # Errors
///
/// See [`EncodeError`]; every variant is a caller bug.
///
/// # Examples
///
/// ```
/// use store_io_format::slot::{encode, validate, Expect, SlotFields, SlotIndex, SlotStatus};
///
/// let fields = SlotFields {
///     volume_uuid: [7; 16], object_id: 1, generation: 1, owner_generation: 1,
///     slot_offset: 8192, slot_index: SlotIndex::A, log2_slot_size: 12,
///     prev_header_crc32c: 0,
/// };
/// let mut slot = vec![0u8; 4096];
/// encode(&mut slot, &fields, b"payload")?;
///
/// let expect = Expect {
///     volume_uuid: [7; 16], object_id: 1, slot_offset: 8192,
///     slot_index: SlotIndex::A, log2_slot_size: 12,
/// };
/// match validate(&slot, &expect) {
///     SlotStatus::Valid(info) => assert_eq!(info.payload(&slot), b"payload"),
///     other => panic!("unexpected {other:?}"),
/// }
/// # Ok::<(), store_io_format::slot::EncodeError>(())
/// ```
pub fn encode(out: &mut [u8], f: &SlotFields, payload: &[u8]) -> Result<u32, EncodeError> {
    if !(MIN_LOG2_SLOT_SIZE..=MAX_LOG2_SLOT_SIZE).contains(&f.log2_slot_size) {
        return Err(EncodeError::SlotSize);
    }
    if out.len() != 1usize << f.log2_slot_size {
        return Err(EncodeError::BufferLength);
    }
    if payload.len() > out.len() - HEADER_LEN {
        return Err(EncodeError::PayloadTooLarge);
    }
    if f.generation == 0 {
        return Err(EncodeError::ZeroGeneration);
    }
    out.fill(0);
    let (header, body) = out.split_at_mut(HEADER_LEN);
    body[..payload.len()].copy_from_slice(payload);
    let payload_len = u32::try_from(payload.len()).map_err(|_| EncodeError::PayloadTooLarge)?;
    write_header(header, f, payload_len, crc32c(payload)).map_err(|_| EncodeError::BufferLength)?;
    let crc = crc32c(header);
    header[CRC_FIELD].copy_from_slice(&crc.to_le_bytes());
    Ok(crc)
}

fn write_header(
    header: &mut [u8],
    f: &SlotFields,
    payload_len: u32,
    payload_crc: u32,
) -> Result<(), crate::codec::CodecError> {
    let mut w = Writer::new(header);
    w.bytes(&MAGIC)?;
    w.u32(0)?; // header_crc32c, filled in after the CRC is computed
    w.u16(HEADER_LEN as u16)?;
    w.u16(FORMAT_MAJOR)?;
    w.u32(0)?; // incompat_flags
    w.u32(0)?; // ro_compat_flags
    w.u32(0)?; // compat_flags
    w.u32(payload_crc)?;
    w.bytes(&f.volume_uuid)?;
    w.u64(f.object_id)?;
    w.u64(f.generation)?;
    w.u64(f.slot_offset)?;
    w.u32(payload_len)?;
    w.u8(f.slot_index.as_u8())?;
    w.u8(SLOT_COUNT)?;
    w.u8(f.log2_slot_size)?;
    w.u8(0)?;
    w.u32(f.prev_header_crc32c)?;
    w.u32(0)?;
    w.u64(f.owner_generation)?;
    w.zeros(32)
}

/// What the reader expects of a slot at a given location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expect {
    /// The volume being opened.
    pub volume_uuid: [u8; 16],
    /// The object stored in this pair.
    pub object_id: u64,
    /// Absolute byte offset the slot was read from.
    pub slot_offset: u64,
    /// Which slot of the pair was read.
    pub slot_index: SlotIndex,
    /// Configured slot size as a power of two.
    pub log2_slot_size: u8,
}

/// Facts about a slot that passed every check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotInfo {
    /// Generation of the stored version.
    pub generation: u64,
    /// Owner generation of the process that wrote it.
    pub owner_generation: u64,
    /// This slot's header CRC.
    pub header_crc32c: u32,
    /// Header CRC of the version it replaced.
    pub prev_header_crc32c: u32,
    /// Read-only compatibility flags; a nonzero unknown bit forbids rewriting.
    pub ro_compat_flags: u32,
    /// Compatible feature flags.
    pub compat_flags: u32,
    payload_start: u16,
    payload_len: u32,
}

impl SlotInfo {
    /// The payload bytes inside the validated slot buffer.
    ///
    /// `slot` must be the same buffer [`validate`] accepted; any other buffer
    /// yields an empty slice rather than a panic.
    #[must_use]
    pub fn payload<'a>(&self, slot: &'a [u8]) -> &'a [u8] {
        let start = usize::from(self.payload_start);
        let end = start.saturating_add(self.payload_len as usize);
        slot.get(start..end).unwrap_or(&[])
    }

    /// Whether this version of store-io may write a newer generation over it.
    #[must_use]
    pub fn writable(&self) -> bool {
        self.ro_compat_flags & !KNOWN_RO_COMPAT == 0
    }
}

/// Result of validating one slot, in the order the checks are applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotStatus {
    /// The device reported an error reading the slot (set by the caller).
    ReadError,
    /// The buffer is not the configured slot size (caller bug or corrupt config).
    BadLength,
    /// The header is all zero: never written.
    Blank,
    /// The magic bytes are wrong: not a slot, or overwritten.
    BadMagic,
    /// The header CRC does not match: torn or corrupted header.
    BadHeaderCrc,
    /// Written by a different on-disk format version; never read or overwritten.
    UnsupportedVersion {
        /// The format version found.
        major: u16,
    },
    /// Uses incompatible features this version does not understand.
    Incompatible {
        /// The unknown incompatible flag bits.
        flags: u32,
    },
    /// The CRC is valid but a field holds an impossible value.
    Malformed,
    /// The slot belongs to another volume.
    Foreign,
    /// The slot belongs to another object of this volume.
    WrongObject,
    /// The slot records a different location or slot index: a misdirected write.
    Misplaced,
    /// The payload CRC does not match: torn or corrupted payload.
    BadPayloadCrc,
    /// Every check passed.
    Valid(SlotInfo),
}

impl SlotStatus {
    /// Whether this status is [`SlotStatus::Valid`].
    #[inline]
    #[must_use]
    pub fn is_valid(&self) -> bool {
        matches!(self, Self::Valid(_))
    }
}

/// Validates one slot read from `e.slot_offset`.
///
/// Never panics and never allocates, whatever the input bytes.
#[must_use]
pub fn validate(slot: &[u8], e: &Expect) -> SlotStatus {
    if !(MIN_LOG2_SLOT_SIZE..=MAX_LOG2_SLOT_SIZE).contains(&e.log2_slot_size)
        || slot.len() != 1usize << e.log2_slot_size
    {
        return SlotStatus::BadLength;
    }
    let header = &slot[..HEADER_LEN];
    if header.iter().all(|&b| b == 0) {
        return SlotStatus::Blank;
    }
    match parse(slot, e) {
        Ok(status) | Err(status) => status,
    }
}

/// Field-by-field validation after the blank check. `Err` carries the first
/// failing status so `?` reads in the documented order.
fn parse(slot: &[u8], e: &Expect) -> Result<SlotStatus, SlotStatus> {
    let mut r = Reader::new(slot);
    let bad = |_| SlotStatus::BadLength;
    if r.array::<8>().map_err(bad)? != MAGIC {
        return Err(SlotStatus::BadMagic);
    }
    let stored_crc = r.u32().map_err(bad)?;
    let header_len = usize::from(r.u16().map_err(bad)?);
    if header_len < HEADER_LEN || header_len > slot.len() {
        return Err(SlotStatus::BadHeaderCrc);
    }
    let mut crc = crc32c(&slot[..CRC_FIELD.start]);
    crc = crc32c_append(crc, &[0; 4]);
    crc = crc32c_append(crc, &slot[CRC_FIELD.end..header_len]);
    if crc != stored_crc {
        return Err(SlotStatus::BadHeaderCrc);
    }
    let major = r.u16().map_err(bad)?;
    if major != FORMAT_MAJOR {
        return Err(SlotStatus::UnsupportedVersion { major });
    }
    let incompat = r.u32().map_err(bad)?;
    if incompat & !KNOWN_INCOMPAT != 0 {
        return Err(SlotStatus::Incompatible {
            flags: incompat & !KNOWN_INCOMPAT,
        });
    }
    let ro_compat_flags = r.u32().map_err(bad)?;
    let compat_flags = r.u32().map_err(bad)?;
    let payload_crc = r.u32().map_err(bad)?;
    let volume_uuid = r.array::<16>().map_err(bad)?;
    let object_id = r.u64().map_err(bad)?;
    let generation = r.u64().map_err(bad)?;
    let slot_offset = r.u64().map_err(bad)?;
    let payload_len = r.u32().map_err(bad)?;
    let slot_index = r.u8().map_err(bad)?;
    let slot_count = r.u8().map_err(bad)?;
    let log2 = r.u8().map_err(bad)?;
    let reserved0 = r.u8().map_err(bad)?;
    let prev_header_crc32c = r.u32().map_err(bad)?;
    let reserved1 = r.u32().map_err(bad)?;
    let owner_generation = r.u64().map_err(bad)?;
    let reserved2 = r.bytes(32).map_err(bad)?;
    if generation == 0
        || slot_count != SLOT_COUNT
        || slot_index > 1
        || reserved0 != 0
        || reserved1 != 0
        || reserved2.iter().any(|&b| b != 0)
    {
        return Err(SlotStatus::Malformed);
    }
    if volume_uuid != e.volume_uuid {
        return Err(SlotStatus::Foreign);
    }
    if object_id != e.object_id {
        return Err(SlotStatus::WrongObject);
    }
    if slot_offset != e.slot_offset
        || slot_index != e.slot_index.as_u8()
        || log2 != e.log2_slot_size
    {
        return Err(SlotStatus::Misplaced);
    }
    let room = slot.len() - header_len;
    let len = payload_len as usize;
    if len > room {
        return Err(SlotStatus::Malformed);
    }
    let payload = &slot[header_len..header_len + len];
    if crc32c(payload) != payload_crc {
        return Err(SlotStatus::BadPayloadCrc);
    }
    let payload_start = u16::try_from(header_len).map_err(|_| SlotStatus::Malformed)?;
    Ok(SlotStatus::Valid(SlotInfo {
        generation,
        owner_generation,
        header_crc32c: stored_crc,
        prev_header_crc32c,
        ro_compat_flags,
        compat_flags,
        payload_start,
        payload_len,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(index: SlotIndex, generation: u64) -> SlotFields {
        SlotFields {
            volume_uuid: [0xAB; 16],
            object_id: 42,
            generation,
            owner_generation: 3,
            slot_offset: if index == SlotIndex::A { 4096 } else { 8192 },
            slot_index: index,
            log2_slot_size: 12,
            prev_header_crc32c: 0,
        }
    }

    fn expect(index: SlotIndex) -> Expect {
        let f = fields(index, 1);
        Expect {
            volume_uuid: f.volume_uuid,
            object_id: f.object_id,
            slot_offset: f.slot_offset,
            slot_index: index,
            log2_slot_size: 12,
        }
    }

    fn encoded(index: SlotIndex, generation: u64, payload: &[u8]) -> Vec<u8> {
        let mut buf = vec![0u8; 4096];
        assert!(encode(&mut buf, &fields(index, generation), payload).is_ok());
        buf
    }

    #[test]
    fn test_validate_roundtrip_returns_payload_and_fields() {
        let slot = encoded(SlotIndex::A, 7, b"hello");
        let SlotStatus::Valid(info) = validate(&slot, &expect(SlotIndex::A)) else {
            panic!("not valid");
        };
        assert_eq!(info.generation, 7);
        assert_eq!(info.owner_generation, 3);
        assert_eq!(info.payload(&slot), b"hello");
        assert!(info.writable());
    }

    #[test]
    fn test_validate_blank_slot_is_blank() {
        assert_eq!(
            validate(&[0u8; 4096], &expect(SlotIndex::A)),
            SlotStatus::Blank
        );
    }

    #[test]
    fn test_validate_wrong_length_is_bad_length() {
        assert_eq!(
            validate(&[0u8; 4095], &expect(SlotIndex::A)),
            SlotStatus::BadLength
        );
    }

    #[test]
    fn test_validate_detects_each_identity_mismatch() {
        let slot = encoded(SlotIndex::A, 1, b"x");
        let mut e = expect(SlotIndex::A);
        e.volume_uuid = [0; 16];
        assert_eq!(validate(&slot, &e), SlotStatus::Foreign);
        let mut e = expect(SlotIndex::A);
        e.object_id = 43;
        assert_eq!(validate(&slot, &e), SlotStatus::WrongObject);
        let mut e = expect(SlotIndex::A);
        e.slot_offset = 12288;
        assert_eq!(validate(&slot, &e), SlotStatus::Misplaced);
        // A slot written for B but found at A's location (misdirected write).
        let b = encoded(SlotIndex::B, 1, b"x");
        assert_eq!(validate(&b, &expect(SlotIndex::A)), SlotStatus::Misplaced);
    }

    #[test]
    fn test_validate_any_single_bit_flip_is_never_valid() {
        let slot = encoded(SlotIndex::A, 5, &[0x5A; 300]);
        let e = expect(SlotIndex::A);
        for byte in 0..(HEADER_LEN + 300) {
            for bit in 0..8 {
                let mut s = slot.clone();
                s[byte] ^= 1 << bit;
                assert!(!validate(&s, &e).is_valid(), "flip byte {byte} bit {bit}");
            }
        }
    }

    #[test]
    fn test_validate_torn_overwrite_is_old_new_or_invalid_never_mixed() {
        // Generation 5 on disk is overwritten in place by generation 6; a tear
        // leaves a prefix of the new bytes over the old. Tears happen at 512-byte
        // sectors on real devices, but every byte boundary is checked here.
        let old = encoded(SlotIndex::A, 5, &[0x11; 2000]);
        let new = encoded(SlotIndex::A, 6, &[0x22; 2000]);
        let e = expect(SlotIndex::A);
        for cut in 0..=4096 {
            let mut torn = old.clone();
            torn[..cut].copy_from_slice(&new[..cut]);
            match validate(&torn, &e) {
                SlotStatus::Valid(info) => {
                    let p = info.payload(&torn);
                    let ok = (info.generation == 5 && p == &[0x11; 2000][..])
                        || (info.generation == 6 && p == &[0x22; 2000][..]);
                    assert!(ok, "mixed version accepted at cut {cut}");
                }
                SlotStatus::BadHeaderCrc | SlotStatus::BadPayloadCrc | SlotStatus::BadMagic => {}
                other => panic!("unexpected {other:?} at cut {cut}"),
            }
        }
    }

    #[test]
    fn test_validate_future_format_and_flags_are_refused() {
        let mut slot = encoded(SlotIndex::A, 1, b"x");
        slot[14..16].copy_from_slice(&2u16.to_le_bytes());
        reseal(&mut slot);
        assert_eq!(
            validate(&slot, &expect(SlotIndex::A)),
            SlotStatus::UnsupportedVersion { major: 2 }
        );

        let mut slot = encoded(SlotIndex::A, 1, b"x");
        slot[16..20].copy_from_slice(&1u32.to_le_bytes());
        reseal(&mut slot);
        assert_eq!(
            validate(&slot, &expect(SlotIndex::A)),
            SlotStatus::Incompatible { flags: 1 }
        );

        let mut slot = encoded(SlotIndex::A, 1, b"x");
        slot[20..24].copy_from_slice(&1u32.to_le_bytes());
        reseal(&mut slot);
        let SlotStatus::Valid(info) = validate(&slot, &expect(SlotIndex::A)) else {
            panic!("ro_compat slot must stay readable");
        };
        assert!(!info.writable());
    }

    #[test]
    fn test_validate_valid_crc_with_impossible_fields_is_malformed() {
        let mut slot = encoded(SlotIndex::A, 1, b"x");
        slot[56..64].copy_from_slice(&0u64.to_le_bytes()); // generation 0
        reseal(&mut slot);
        assert_eq!(
            validate(&slot, &expect(SlotIndex::A)),
            SlotStatus::Malformed
        );

        let mut slot = encoded(SlotIndex::A, 1, b"x");
        slot[72..76].copy_from_slice(&5000u32.to_le_bytes()); // payload past the slot
        reseal(&mut slot);
        assert_eq!(
            validate(&slot, &expect(SlotIndex::A)),
            SlotStatus::Malformed
        );
    }

    #[test]
    fn test_encode_rejects_caller_bugs() {
        let mut buf = vec![0u8; 4096];
        let mut f = fields(SlotIndex::A, 1);
        f.generation = 0;
        assert_eq!(encode(&mut buf, &f, b""), Err(EncodeError::ZeroGeneration));
        let f = fields(SlotIndex::A, 1);
        assert_eq!(
            encode(&mut buf, &f, &[0; 4000]),
            Err(EncodeError::PayloadTooLarge)
        );
        assert_eq!(
            encode(&mut buf[..100], &f, b""),
            Err(EncodeError::BufferLength)
        );
        let mut f = fields(SlotIndex::A, 1);
        f.log2_slot_size = 17;
        assert_eq!(encode(&mut buf, &f, b""), Err(EncodeError::SlotSize));
    }

    /// Recomputes the header CRC after a test edits header fields.
    fn reseal(slot: &mut [u8]) {
        slot[8..12].fill(0);
        let crc = crc32c(&slot[..HEADER_LEN]);
        slot[8..12].copy_from_slice(&crc.to_le_bytes());
    }
}
