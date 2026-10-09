//! Payload codecs for the three store-io metadata objects: the volume record,
//! the region table and the region header. Each payload is stored inside an
//! A/B slot pair (see [`crate::slot`]); this module only turns typed records
//! into bytes and back, validating everything it reads.
//!
//! All integers are little-endian. Decoders never allocate and never panic.

use core::fmt;

use crate::codec::{CodecError, Reader, Writer};

/// Why a payload failed to decode or encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaError {
    /// The payload is shorter than its fixed layout, or the output too small.
    Length,
    /// A field holds a value this format does not define.
    Field(&'static str),
    /// Two table entries share an id or a name.
    Duplicate,
    /// Two table entries overlap on disk.
    Overlap,
    /// An offset or size is not a multiple of the volume's block size.
    Unaligned,
}

impl fmt::Display for MetaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length => f.write_str("payload length does not match its layout"),
            Self::Field(name) => write!(f, "field `{name}` holds an undefined value"),
            Self::Duplicate => f.write_str("two region-table entries share an id or a name"),
            Self::Overlap => f.write_str("two region-table entries overlap on disk"),
            Self::Unaligned => f.write_str("an offset or size is not block-aligned"),
        }
    }
}

impl std::error::Error for MetaError {}

impl From<CodecError> for MetaError {
    fn from(_: CodecError) -> Self {
        Self::Length
    }
}

// ---------------------------------------------------------------------------
// Volume record (object 0)
// ---------------------------------------------------------------------------

/// The volume record: geometry and ownership of one container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeMeta {
    /// Block (slot) size as a power of two; every slot and region is aligned to it.
    pub log2_block_size: u8,
    /// Region-table slot size as a power of two.
    pub log2_table_slot_size: u8,
    /// Index of this device in a multi-device store (0 for single-device).
    pub device_index: u16,
    /// Bumped and made durable on every writable open (forensic evidence of
    /// who wrote what; the lock is the fence).
    pub owner_generation: u64,
    /// Byte offset of region-table slot A.
    pub table_offset: u64,
    /// Creation time, seconds since the Unix epoch (informative only).
    pub created_unix_s: u64,
    /// Feature flags (none defined in format 1).
    pub features: u64,
}

/// Encoded length of [`VolumeMeta`].
pub const VOLUME_META_LEN: usize = 48;

impl VolumeMeta {
    /// Encodes the record into `out` and returns the bytes written.
    ///
    /// # Errors
    ///
    /// [`MetaError::Length`] if `out` is shorter than [`VOLUME_META_LEN`].
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, MetaError> {
        let mut w = Writer::new(out);
        w.u8(self.log2_block_size)?;
        w.u8(self.log2_table_slot_size)?;
        w.u16(self.device_index)?;
        w.u32(0)?;
        w.u64(self.owner_generation)?;
        w.u64(self.table_offset)?;
        w.u64(self.created_unix_s)?;
        w.u64(self.features)?;
        w.zeros(8)?;
        Ok(w.position())
    }

    /// Decodes and validates a record.
    ///
    /// # Errors
    ///
    /// [`MetaError`] if the payload is short or a field is out of range.
    pub fn decode(bytes: &[u8]) -> Result<Self, MetaError> {
        let mut r = Reader::new(bytes);
        let log2_block_size = r.u8()?;
        let log2_table_slot_size = r.u8()?;
        let device_index = r.u16()?;
        if r.u32()? != 0 {
            return Err(MetaError::Field("reserved"));
        }
        let owner_generation = r.u64()?;
        let table_offset = r.u64()?;
        let created_unix_s = r.u64()?;
        let features = r.u64()?;
        let _reserved = r.bytes(8)?;
        if !(12..=16).contains(&log2_block_size) {
            return Err(MetaError::Field("log2_block_size"));
        }
        if !(log2_block_size..=16).contains(&log2_table_slot_size) {
            return Err(MetaError::Field("log2_table_slot_size"));
        }
        if table_offset % (1u64 << log2_block_size) != 0 {
            return Err(MetaError::Unaligned);
        }
        if features != 0 {
            return Err(MetaError::Field("features"));
        }
        Ok(Self {
            log2_block_size,
            log2_table_slot_size,
            device_index,
            owner_generation,
            table_offset,
            created_unix_s,
            features,
        })
    }
}

// ---------------------------------------------------------------------------
// Region kinds, states, fill patterns
// ---------------------------------------------------------------------------

/// What a region is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RegionKind {
    /// Positions are allocated by store-io from a tail; blocks are never rewritten.
    Append,
    /// Caller-chosen positions; overwritten in place.
    Page,
    /// A small object stored in the region's own header slot pair.
    Slot,
}

impl RegionKind {
    fn to_u8(self) -> u8 {
        match self {
            Self::Append => 1,
            Self::Page => 2,
            Self::Slot => 3,
        }
    }

    fn from_u8(v: u8) -> Result<Self, MetaError> {
        match v {
            1 => Ok(Self::Append),
            2 => Ok(Self::Page),
            3 => Ok(Self::Slot),
            _ => Err(MetaError::Field("kind")),
        }
    }
}

/// Lifecycle state of a region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RegionState {
    /// Provisioned and usable.
    Ready,
    /// Released; its space may be reused after a new provisioning.
    Released,
}

impl RegionState {
    fn to_u8(self) -> u8 {
        match self {
            Self::Ready => 1,
            Self::Released => 2,
        }
    }

    fn from_u8(v: u8) -> Result<Self, MetaError> {
        match v {
            1 => Ok(Self::Ready),
            2 => Ok(Self::Released),
            _ => Err(MetaError::Field("state")),
        }
    }
}

/// How a region's blocks were filled at provisioning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FillPattern {
    /// Zeros (physical stacks).
    Zeros,
    /// The keyed incompressible pattern of [`crate::fill`], version 1
    /// (thin, virtual, deduplicating or zero-detecting stacks).
    KeyedV1,
}

impl FillPattern {
    fn to_u8(self) -> u8 {
        match self {
            Self::Zeros => 0,
            Self::KeyedV1 => 1,
        }
    }

    fn from_u8(v: u8) -> Result<Self, MetaError> {
        match v {
            0 => Ok(Self::Zeros),
            1 => Ok(Self::KeyedV1),
            _ => Err(MetaError::Field("fill_pattern")),
        }
    }
}

// ---------------------------------------------------------------------------
// Region name
// ---------------------------------------------------------------------------

/// Maximum region-name length in bytes.
pub const NAME_MAX: usize = 24;

/// A region name: 1 to 24 bytes of UTF-8, no NUL bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct RegionName {
    bytes: [u8; NAME_MAX],
    len: u8,
}

impl RegionName {
    /// Validates and stores a name.
    ///
    /// # Errors
    ///
    /// [`MetaError::Field`] if the name is empty, longer than 24 bytes, or
    /// contains a NUL byte.
    pub fn new(name: &str) -> Result<Self, MetaError> {
        let b = name.as_bytes();
        if b.is_empty() || b.len() > NAME_MAX || b.contains(&0) {
            return Err(MetaError::Field("name"));
        }
        let mut bytes = [0u8; NAME_MAX];
        bytes[..b.len()].copy_from_slice(b);
        Ok(Self {
            bytes,
            len: b.len() as u8,
        })
    }

    /// The name as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // Constructed only from validated UTF-8.
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("")
    }

    fn from_padded(raw: [u8; NAME_MAX]) -> Result<Self, MetaError> {
        let len = raw.iter().position(|&b| b == 0).unwrap_or(NAME_MAX);
        if len == 0 || raw[len..].iter().any(|&b| b != 0) {
            return Err(MetaError::Field("name"));
        }
        let s = core::str::from_utf8(&raw[..len]).map_err(|_| MetaError::Field("name"))?;
        Self::new(s)
    }
}

impl fmt::Debug for RegionName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

// ---------------------------------------------------------------------------
// Region table (object 1)
// ---------------------------------------------------------------------------

/// One region-table entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableEntry {
    /// Region id (never reused within a volume).
    pub region_id: u32,
    /// Region kind.
    pub kind: RegionKind,
    /// Lifecycle state.
    pub state: RegionState,
    /// Region name.
    pub name: RegionName,
    /// Byte offset of the region's header slot A.
    pub offset: u64,
    /// Bytes of data area after the two header slots (0 for slot regions).
    pub data_size: u64,
    /// Fill pattern used at provisioning.
    pub fill: FillPattern,
    /// The caller's space-accounting tag (tenant, class); opaque to
    /// store-io. 0 when provisioned without a reservation.
    pub tag: u32,
}

impl TableEntry {
    /// Total on-disk extent: two header blocks plus the data area.
    #[must_use]
    pub fn extent_len(&self, block: u64) -> Option<u64> {
        block.checked_mul(2)?.checked_add(self.data_size)
    }
}

/// Encoded length of one table entry.
pub const ENTRY_LEN: usize = 64;
/// Encoded length of the table header.
pub const TABLE_HEADER_LEN: usize = 16;

/// Encodes a region table: `next_region_id`, then the entries.
///
/// # Errors
///
/// [`MetaError::Length`] if `out` cannot hold the table.
pub fn encode_table(
    out: &mut [u8],
    next_region_id: u32,
    entries: &[TableEntry],
) -> Result<usize, MetaError> {
    let count = u32::try_from(entries.len()).map_err(|_| MetaError::Length)?;
    let mut w = Writer::new(out);
    w.u32(next_region_id)?;
    w.u32(count)?;
    w.zeros(8)?;
    for e in entries {
        w.u32(e.region_id)?;
        w.u8(e.kind.to_u8())?;
        w.u8(e.state.to_u8())?;
        w.u8(e.fill.to_u8())?;
        w.u8(0)?;
        w.bytes(&e.name.bytes)?;
        w.u64(e.offset)?;
        w.u64(e.data_size)?;
        w.u32(e.tag)?;
        w.zeros(12)?;
    }
    Ok(w.position())
}

/// A decoded, validated region table, borrowing the payload bytes.
#[derive(Debug, Clone, Copy)]
pub struct Table<'a> {
    /// The next id to assign; every entry's id is below it.
    pub next_region_id: u32,
    entries: &'a [u8],
}

impl<'a> Table<'a> {
    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len() / ENTRY_LEN
    }

    /// Whether the table has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterates over the entries (already validated by [`decode_table`]).
    pub fn iter(&self) -> impl Iterator<Item = TableEntry> + 'a {
        self.entries
            .chunks_exact(ENTRY_LEN)
            .filter_map(|c| decode_entry(c).ok())
    }
}

fn decode_entry(chunk: &[u8]) -> Result<TableEntry, MetaError> {
    let mut r = Reader::new(chunk);
    let region_id = r.u32()?;
    let kind = RegionKind::from_u8(r.u8()?)?;
    let state = RegionState::from_u8(r.u8()?)?;
    let fill = FillPattern::from_u8(r.u8()?)?;
    if r.u8()? != 0 {
        return Err(MetaError::Field("reserved"));
    }
    let name = RegionName::from_padded(r.array::<NAME_MAX>()?)?;
    let offset = r.u64()?;
    let data_size = r.u64()?;
    let tag = r.u32()?;
    if r.bytes(12)?.iter().any(|&b| b != 0) {
        return Err(MetaError::Field("reserved"));
    }
    Ok(TableEntry {
        region_id,
        kind,
        state,
        name,
        offset,
        data_size,
        fill,
        tag,
    })
}

/// Decodes and validates a region table against the volume's block size.
///
/// Checks: every entry decodes; ids are unique and below `next_region_id`;
/// names are unique among non-released entries; offsets and sizes are
/// block-aligned; slot regions have no data area; no two extents overlap.
/// The overlap check is quadratic and allocation-free (at most 1,021 entries, in a 64 KiB table slot).
///
/// # Errors
///
/// The first [`MetaError`] found.
pub fn decode_table(bytes: &[u8], block: u64) -> Result<Table<'_>, MetaError> {
    let mut r = Reader::new(bytes);
    let next_region_id = r.u32()?;
    let count = r.u32()? as usize;
    if r.bytes(8)?.iter().any(|&b| b != 0) {
        return Err(MetaError::Field("reserved"));
    }
    let len = count.checked_mul(ENTRY_LEN).ok_or(MetaError::Length)?;
    let entries = r.bytes(len)?;
    if block == 0 || !block.is_power_of_two() {
        return Err(MetaError::Field("block"));
    }
    let table = Table {
        next_region_id,
        entries,
    };
    for (i, chunk) in entries.chunks_exact(ENTRY_LEN).enumerate() {
        let e = decode_entry(chunk)?;
        if e.region_id >= next_region_id {
            return Err(MetaError::Field("region_id"));
        }
        if e.offset % block != 0 || e.data_size % block != 0 {
            return Err(MetaError::Unaligned);
        }
        if e.kind == RegionKind::Slot && e.data_size != 0 {
            return Err(MetaError::Field("data_size"));
        }
        let end = e
            .extent_len(block)
            .and_then(|l| e.offset.checked_add(l))
            .ok_or(MetaError::Overlap)?;
        for other in entries.chunks_exact(ENTRY_LEN).take(i) {
            let o = decode_entry(other)?;
            if o.region_id == e.region_id
                || (o.name == e.name
                    && o.state == RegionState::Ready
                    && e.state == RegionState::Ready)
            {
                return Err(MetaError::Duplicate);
            }
            let o_end = o
                .extent_len(block)
                .and_then(|l| o.offset.checked_add(l))
                .ok_or(MetaError::Overlap)?;
            if e.offset < o_end && o.offset < end {
                return Err(MetaError::Overlap);
            }
        }
    }
    Ok(table)
}

// ---------------------------------------------------------------------------
// Region header (object 2^32 + id)
// ---------------------------------------------------------------------------

/// The authoritative description of one region, stored in its header pair.
/// The region's generation is the slot generation of its header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionMeta {
    /// Region id; must match the table entry and the slot's object id.
    pub region_id: u32,
    /// Region kind.
    pub kind: RegionKind,
    /// Lifecycle state (`Released` is written before the table flip).
    pub state: RegionState,
    /// Fill pattern of the data area.
    pub fill: FillPattern,
    /// Byte offset of the data area.
    pub data_offset: u64,
    /// Size of the data area.
    pub data_size: u64,
    /// Key of the keyed fill pattern (zero for `Zeros`).
    pub fill_key: [u8; 16],
}

/// Encoded length of [`RegionMeta`] (excluding a slot region's payload).
pub const REGION_META_LEN: usize = 48;

impl RegionMeta {
    /// Encodes the header payload.
    ///
    /// # Errors
    ///
    /// [`MetaError::Length`] if `out` is shorter than [`REGION_META_LEN`].
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, MetaError> {
        let mut w = Writer::new(out);
        w.u32(self.region_id)?;
        w.u8(self.kind.to_u8())?;
        w.u8(self.state.to_u8())?;
        w.u8(self.fill.to_u8())?;
        w.u8(0)?;
        w.u64(self.data_offset)?;
        w.u64(self.data_size)?;
        w.bytes(&self.fill_key)?;
        w.zeros(8)?;
        Ok(w.position())
    }

    /// Decodes and validates a header payload. For slot regions the caller's
    /// object follows at [`REGION_META_LEN`].
    ///
    /// # Errors
    ///
    /// [`MetaError`] if the payload is short or a field is undefined.
    pub fn decode(bytes: &[u8]) -> Result<Self, MetaError> {
        let mut r = Reader::new(bytes);
        let region_id = r.u32()?;
        let kind = RegionKind::from_u8(r.u8()?)?;
        let state = RegionState::from_u8(r.u8()?)?;
        let fill = FillPattern::from_u8(r.u8()?)?;
        if r.u8()? != 0 {
            return Err(MetaError::Field("reserved"));
        }
        let data_offset = r.u64()?;
        let data_size = r.u64()?;
        let fill_key = r.array::<16>()?;
        if r.bytes(8)?.iter().any(|&b| b != 0) {
            return Err(MetaError::Field("reserved"));
        }
        Ok(Self {
            region_id,
            kind,
            state,
            fill,
            data_offset,
            data_size,
            fill_key,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u32, name: &str, offset: u64, data: u64) -> TableEntry {
        TableEntry {
            region_id: id,
            kind: RegionKind::Append,
            state: RegionState::Ready,
            name: RegionName::new(name).unwrap_or(RegionName {
                bytes: [b'x'; NAME_MAX],
                len: 1,
            }),
            offset,
            data_size: data,
            fill: FillPattern::Zeros,
            tag: 0,
        }
    }

    #[test]
    fn test_volume_meta_roundtrip_and_rejections() {
        let v = VolumeMeta {
            log2_block_size: 12,
            log2_table_slot_size: 14,
            device_index: 0,
            owner_generation: 9,
            table_offset: 8192,
            created_unix_s: 1_700_000_000,
            features: 0,
        };
        let mut buf = [0u8; VOLUME_META_LEN];
        assert_eq!(v.encode(&mut buf), Ok(VOLUME_META_LEN));
        assert_eq!(VolumeMeta::decode(&buf), Ok(v));
        let mut bad = v;
        bad.log2_block_size = 11;
        assert!(bad.encode(&mut buf).is_ok());
        assert_eq!(
            VolumeMeta::decode(&buf),
            Err(MetaError::Field("log2_block_size"))
        );
        assert_eq!(VolumeMeta::decode(&buf[..10]), Err(MetaError::Length));
    }

    #[test]
    fn test_region_name_rules() {
        assert!(RegionName::new("").is_err());
        assert!(RegionName::new("a\0b").is_err());
        assert!(RegionName::new(&"x".repeat(25)).is_err());
        assert_eq!(
            RegionName::new("wal").map(|n| n.as_str().to_owned()),
            Ok("wal".to_owned())
        );
        assert!(RegionName::new(&"é".repeat(12)).is_ok()); // 24 bytes
    }

    #[test]
    fn test_table_roundtrip() {
        let mut tagged = entry(1, "pages", 16384 + 4096 * 12, 4096);
        tagged.tag = 0xDEAD_BEEF;
        let entries = [entry(0, "wal", 16384, 4096 * 10), tagged];
        let mut buf = vec![0u8; 512];
        let n = encode_table(&mut buf, 2, &entries).unwrap_or(0);
        let t = decode_table(&buf[..n], 4096);
        let Ok(t) = t else { panic!("{t:?}") };
        assert_eq!(t.next_region_id, 2);
        assert_eq!(t.iter().collect::<Vec<_>>(), entries);
    }

    #[test]
    fn test_table_rejects_overlap_duplicates_unaligned_and_stale_ids() {
        let mut buf = vec![0u8; 512];
        let check = |buf: &mut Vec<u8>, next: u32, e: &[TableEntry]| {
            let n = encode_table(buf, next, e).unwrap_or(0);
            decode_table(&buf[..n], 4096).map(|_| ())
        };
        assert_eq!(
            check(
                &mut buf,
                2,
                &[entry(0, "a", 0, 8192), entry(1, "b", 8192, 4096)]
            ),
            Err(MetaError::Overlap)
        );
        assert_eq!(
            check(&mut buf, 2, &[entry(0, "a", 0, 0), entry(0, "b", 65536, 0)]),
            Err(MetaError::Duplicate)
        );
        assert_eq!(
            check(&mut buf, 2, &[entry(0, "a", 0, 0), entry(1, "a", 65536, 0)]),
            Err(MetaError::Duplicate)
        );
        assert_eq!(
            check(&mut buf, 2, &[entry(0, "a", 100, 0)]),
            Err(MetaError::Unaligned)
        );
        assert_eq!(
            check(&mut buf, 1, &[entry(1, "a", 0, 0)]),
            Err(MetaError::Field("region_id"))
        );
        // A released entry may share a name with a ready one.
        let mut released = entry(0, "a", 0, 0);
        released.state = RegionState::Released;
        assert_eq!(
            check(&mut buf, 2, &[released, entry(1, "a", 65536, 0)]),
            Ok(())
        );
    }

    #[test]
    fn test_region_meta_roundtrip() {
        let m = RegionMeta {
            region_id: 3,
            kind: RegionKind::Page,
            state: RegionState::Released,
            fill: FillPattern::KeyedV1,
            data_offset: 65536,
            data_size: 1 << 20,
            fill_key: [7; 16],
        };
        let mut buf = [0u8; REGION_META_LEN];
        assert_eq!(m.encode(&mut buf), Ok(REGION_META_LEN));
        assert_eq!(RegionMeta::decode(&buf), Ok(m));
        buf[4] = 9;
        assert_eq!(RegionMeta::decode(&buf), Err(MetaError::Field("kind")));
    }
}
