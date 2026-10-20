//! Payload codecs of container format 2 (`.dev/design/format2-draft.md`,
//! §5, §6, §8): the 64-byte volume record, the table pages with their
//! 96-byte entries, and the region header with its extent list.
//!
//! Decoders never allocate and never panic. They check the layout and every
//! field the format defines on its own; rules that need the container (the
//! file length, the table area, the page copy's `meta_seq`) are checked
//! against [`EntryLimits`], which the caller fills from what it knows.
//! Payload lengths are exact: a longer payload is a newer layout, not extra
//! bytes to ignore.

use crate::codec::{Reader, Writer};
use crate::meta::{FillPattern, MetaError, NAME_MAX, RegionKind, RegionName, RegionState, VolumeMeta};

/// Encoded length of [`VolumeMeta2`].
pub const VOLUME_META2_LEN: usize = 64;
/// Length of a table page's header.
pub const PAGE_HEADER_LEN: usize = 24;
/// Length of one table entry.
pub const ENTRY2_LEN: usize = 96;
/// Length of a region header's fixed part.
pub const REGION_HEADER2_LEN: usize = 64;
/// Length of one extent in a region header.
pub const EXTENT_LEN: usize = 16;
/// Pages a container may have (`P < 2^30`, so region ids stay below 2^62).
pub const MAX_TABLE_PAGES: u32 = 1 << 30;
/// Entry flag: provisioned with `unwritten_ok`.
pub const FLAG_UNWRITTEN: u8 = 1 << 0;
/// Entry flag: growth limited to the capacity (`max_size`).
pub const FLAG_CAPPED: u8 = 1 << 1;
/// Must-understand entry flags this version knows.
pub const KNOWN_FLAGS: u8 = FLAG_UNWRITTEN | FLAG_CAPPED;

/// Entries per table page for a table slot of `1 << log2_table` bytes:
/// `floor((T - 128 - 24) / 96)` (169 at 16 KiB).
#[must_use]
pub const fn entries_per_page(log2_table: u8) -> u16 {
    let t = 1u32 << log2_table;
    ((t - 128 - PAGE_HEADER_LEN as u32) / ENTRY2_LEN as u32) as u16
}

/// Most extents a region header can list at blocks of `1 << log2_block`:
/// `(B - 192) / 16` (244 at 4 KiB).
#[must_use]
pub const fn max_extents(log2_block: u8) -> usize {
    ((1usize << log2_block) - 128 - REGION_HEADER2_LEN) / EXTENT_LEN
}

// ---------------------------------------------------------------------------
// Volume record (object 0)
// ---------------------------------------------------------------------------

/// The format-2 volume record: format 1's fields and the paged table's
/// geometry and clean-close flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeMeta2 {
    /// Format 1's fields (`table_offset` is `2B`; `features` 0).
    pub base: VolumeMeta,
    /// Table pages `P`, `1 <= P < 2^30`.
    pub table_pages: u32,
    /// Entries per page `E` (must equal [`entries_per_page`]).
    pub entries_per_page: u16,
    /// The last writer closed cleanly.
    pub clean_close: bool,
    /// Must-understand volume flags (none defined).
    pub flags: u32,
}

impl VolumeMeta2 {
    /// Encodes the record and returns the bytes written
    /// ([`VOLUME_META2_LEN`]).
    ///
    /// # Errors
    ///
    /// [`MetaError::Length`] if `out` is shorter than [`VOLUME_META2_LEN`].
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, MetaError> {
        let n = self.base.encode(out)?;
        let mut w = Writer::new(out.get_mut(n..).ok_or(MetaError::Length)?);
        w.u32(self.table_pages)?;
        w.u16(self.entries_per_page)?;
        w.u8(u8::from(self.clean_close))?;
        w.u8(0)?;
        w.u32(self.flags)?;
        w.u32(0)?;
        Ok(n + w.position())
    }

    /// Decodes and validates a record of exactly [`VOLUME_META2_LEN`] bytes.
    ///
    /// # Errors
    ///
    /// [`MetaError::Field`] naming `features` or `flags` for a newer record
    /// (checked first); [`MetaError`] for anything else that is not
    /// format 2.
    pub fn decode(bytes: &[u8]) -> Result<Self, MetaError> {
        if bytes.len() != VOLUME_META2_LEN {
            return Err(MetaError::Length);
        }
        let mut r = Reader::new(&bytes[48..]);
        let table_pages = r.u32()?;
        let entries = r.u16()?;
        let clean_close = r.u8()?;
        let reserved0 = r.u8()?;
        let flags = r.u32()?;
        let reserved1 = r.u32()?;
        // Newer-format markers first, as format 1 does.
        let base = VolumeMeta::decode(&bytes[..48])?;
        if flags != 0 {
            return Err(MetaError::Field("flags"));
        }
        if reserved0 != 0 || reserved1 != 0 {
            return Err(MetaError::Field("reserved"));
        }
        if base.table_offset != 2u64 << base.log2_block_size {
            return Err(MetaError::Field("table_offset"));
        }
        if table_pages == 0 || table_pages >= MAX_TABLE_PAGES {
            return Err(MetaError::Field("table_pages"));
        }
        if entries != entries_per_page(base.log2_table_slot_size) {
            return Err(MetaError::Field("entries_per_page"));
        }
        if clean_close > 1 {
            return Err(MetaError::Field("clean_close"));
        }
        Ok(Self {
            base,
            table_pages,
            entries_per_page: entries,
            clean_close: clean_close == 1,
            flags,
        })
    }

    /// The end of the table area: `2B + 2PT`.
    #[must_use]
    pub fn table_end(&self) -> u64 {
        let b = 1u64 << self.base.log2_block_size;
        let t = 1u64 << self.base.log2_table_slot_size;
        2 * b + 2 * u64::from(self.table_pages) * t
    }
}

// ---------------------------------------------------------------------------
// Table pages (object 2^61 | p)
// ---------------------------------------------------------------------------

/// A table page's header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageHeader {
    /// The page's index `p` (must match the object id).
    pub page_index: u32,
    /// The next sequence number the page gives out; never decreases.
    pub next_seq: u32,
    /// Entries that follow.
    pub count: u16,
}

/// One table entry as stored: every field, before the per-entry checks
/// (an entry that fails them makes its region unavailable, and its span is
/// still needed, so the raw fields are always available).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawEntry {
    /// Sequence number in the page; region id `p << 32 | seq`.
    pub seq: u32,
    /// Kind code (`1..=3`).
    pub kind: u8,
    /// State code (`1`).
    pub state: u8,
    /// Must-understand flags.
    pub flags: u8,
    /// May-ignore flags.
    pub compat_flags: u8,
    /// The zero-padded name bytes.
    pub name: [u8; NAME_MAX],
    /// Space-accounting tag.
    pub tag: u32,
    /// Committed extents.
    pub extent_count: u16,
    /// The 2 reserved bytes at 38 (checked at page level: zero).
    reserved: u16,
    /// Header slot A's offset.
    pub header_a: u64,
    /// Header slot B's offset.
    pub header_b: u64,
    /// Usable length.
    pub data_size: u64,
    /// Reserved length (the extents' sum).
    pub capacity: u64,
    /// Lowest offset of the footprint.
    pub span_start: u64,
    /// One past the highest.
    pub span_end: u64,
    /// `meta_seq` of the page write that last changed the footprint.
    pub footprint_seq: u64,
}

/// A table entry that passed the per-entry checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry2 {
    /// Sequence number in the page.
    pub seq: u32,
    /// Region kind.
    pub kind: RegionKind,
    /// Must-understand flags (only known ones).
    pub flags: u8,
    /// May-ignore flags, kept verbatim.
    pub compat_flags: u8,
    /// Region name.
    pub name: RegionName,
    /// Space-accounting tag.
    pub tag: u32,
    /// Committed extents.
    pub extent_count: u16,
    /// Header slot A's offset.
    pub header_a: u64,
    /// Header slot B's offset.
    pub header_b: u64,
    /// Usable length.
    pub data_size: u64,
    /// Reserved length.
    pub capacity: u64,
    /// Lowest offset of the footprint.
    pub span_start: u64,
    /// One past the highest.
    pub span_end: u64,
    /// `meta_seq` of the page write that last changed the footprint.
    pub footprint_seq: u64,
}

impl Entry2 {
    /// The entry as stored (for writing it back).
    #[must_use]
    pub fn to_raw(&self) -> RawEntry {
        let mut name = [0u8; NAME_MAX];
        let s = self.name.as_str().as_bytes();
        name[..s.len()].copy_from_slice(s);
        RawEntry {
            seq: self.seq,
            kind: self.kind.to_u8(),
            state: RegionState::Ready.to_u8(),
            flags: self.flags,
            compat_flags: self.compat_flags,
            name,
            tag: self.tag,
            extent_count: self.extent_count,
            reserved: 0,
            header_a: self.header_a,
            header_b: self.header_b,
            data_size: self.data_size,
            capacity: self.capacity,
            span_start: self.span_start,
            span_end: self.span_end,
            footprint_seq: self.footprint_seq,
        }
    }
}

/// What the per-entry checks need from the container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryLimits {
    /// `log2(B)`.
    pub log2_block: u8,
    /// The end of the table area (`2B + 2PT`): nothing of a region before it.
    pub table_end: u64,
    /// The container file's length.
    pub file_len: u64,
    /// The `meta_seq` of the page copy holding the entry.
    pub page_meta_seq: u64,
}

impl RawEntry {
    fn read(r: &mut Reader<'_>) -> Result<Self, MetaError> {
        Ok(Self {
            seq: r.u32()?,
            kind: r.u8()?,
            state: r.u8()?,
            flags: r.u8()?,
            compat_flags: r.u8()?,
            name: r.array::<NAME_MAX>()?,
            tag: r.u32()?,
            extent_count: r.u16()?,
            reserved: r.u16()?,
            header_a: r.u64()?,
            header_b: r.u64()?,
            data_size: r.u64()?,
            capacity: r.u64()?,
            span_start: r.u64()?,
            span_end: r.u64()?,
            footprint_seq: r.u64()?,
        })
    }

    fn write(&self, w: &mut Writer<'_>) -> Result<(), MetaError> {
        w.u32(self.seq)?;
        w.u8(self.kind)?;
        w.u8(self.state)?;
        w.u8(self.flags)?;
        w.u8(self.compat_flags)?;
        w.bytes(&self.name)?;
        w.u32(self.tag)?;
        w.u16(self.extent_count)?;
        w.u16(0)?;
        w.u64(self.header_a)?;
        w.u64(self.header_b)?;
        w.u64(self.data_size)?;
        w.u64(self.capacity)?;
        w.u64(self.span_start)?;
        w.u64(self.span_end)?;
        w.u64(self.footprint_seq)?;
        Ok(())
    }

    /// The per-entry checks (spec §6). An entry that fails them makes its
    /// region unavailable; the page itself stands.
    ///
    /// # Errors
    ///
    /// [`MetaError`] naming the first rule that fails.
    pub fn check(&self, lim: &EntryLimits) -> Result<Entry2, MetaError> {
        let kind = RegionKind::from_u8(self.kind)?;
        if RegionState::from_u8(self.state)? != RegionState::Ready {
            return Err(MetaError::Field("state"));
        }
        if self.flags & !KNOWN_FLAGS != 0 {
            return Err(MetaError::Field("flags"));
        }
        let name = RegionName::from_padded(self.name)?;
        if self.data_size > self.capacity {
            return Err(MetaError::Field("data_size"));
        }
        let block = 1u64 << lim.log2_block;
        if self.span_start >= self.span_end || self.span_end > lim.file_len {
            return Err(MetaError::Field("span"));
        }
        if self.span_start % block != 0 || self.span_end % block != 0 {
            return Err(MetaError::Unaligned);
        }
        for h in [self.header_a, self.header_b] {
            if h % block != 0 {
                return Err(MetaError::Unaligned);
            }
            let end = h.checked_add(block).ok_or(MetaError::Field("header"))?;
            if h < lim.table_end || h < self.span_start || end > self.span_end {
                return Err(MetaError::Field("header"));
            }
        }
        if self.header_a == self.header_b {
            return Err(MetaError::Field("header"));
        }
        if self.footprint_seq == 0 || self.footprint_seq > lim.page_meta_seq {
            return Err(MetaError::Field("footprint_seq"));
        }
        if kind == RegionKind::Slot {
            if self.extent_count != 0 || self.capacity != 0 || self.data_size != 0 {
                return Err(MetaError::Field("slot"));
            }
        } else if self.extent_count == 0 || self.capacity == 0 {
            return Err(MetaError::Field("extent_count"));
        }
        // Region data comes in whole data blocks: B, or 512 bytes for a
        // 512 B append region (whose header says so; checked against it).
        if self.capacity % 512 != 0 || self.data_size % 512 != 0 {
            return Err(MetaError::Unaligned);
        }
        Ok(Entry2 {
            seq: self.seq,
            kind,
            flags: self.flags,
            compat_flags: self.compat_flags,
            name,
            tag: self.tag,
            extent_count: self.extent_count,
            header_a: self.header_a,
            header_b: self.header_b,
            data_size: self.data_size,
            capacity: self.capacity,
            span_start: self.span_start,
            span_end: self.span_end,
            footprint_seq: self.footprint_seq,
        })
    }

    /// The bytes an unavailable entry keeps reserved: its span, clamped to
    /// the file and to past the table area (`None` when nothing is left).
    #[must_use]
    pub fn reserved_span(&self, lim: &EntryLimits) -> Option<(u64, u64)> {
        let start = self.span_start.max(lim.table_end);
        let end = self.span_end.min(lim.file_len);
        (start < end).then_some((start, end))
    }
}

/// Encodes a page and returns the payload length (`24 + 96 * entries`).
///
/// # Errors
///
/// [`MetaError::Length`] if `out` is too short or there are more than
/// `u16::MAX` entries.
pub fn encode_page(out: &mut [u8], h: &PageHeader, entries: &[RawEntry]) -> Result<usize, MetaError> {
    let count = u16::try_from(entries.len()).map_err(|_| MetaError::Length)?;
    let mut w = Writer::new(out);
    w.u32(h.page_index)?;
    w.u32(h.next_seq)?;
    w.u16(count)?;
    w.zeros(14)?;
    for e in entries {
        e.write(&mut w)?;
    }
    Ok(w.position())
}

/// A decoded page: its header and its entries, each checked or not by the
/// caller with [`RawEntry::check`].
#[derive(Debug, Clone, Copy)]
pub struct Page<'a> {
    /// The page header.
    pub header: PageHeader,
    entries: &'a [u8],
}

impl<'a> Page<'a> {
    /// The raw entries, in stored order.
    pub fn entries(&self) -> impl Iterator<Item = RawEntry> + 'a {
        self.entries.chunks_exact(ENTRY2_LEN).filter_map(|c| {
            // Every chunk is ENTRY2_LEN bytes, so reading one never fails.
            RawEntry::read(&mut Reader::new(c)).ok()
        })
    }
}

/// Decodes a page payload and applies the page checks (spec §6): a page
/// that fails them is lost.
///
/// # Errors
///
/// [`MetaError`] for the first page-level rule that fails.
pub fn decode_page(bytes: &[u8], page_index: u32, max_entries: u16) -> Result<Page<'_>, MetaError> {
    let mut r = Reader::new(bytes);
    let header = PageHeader {
        page_index: r.u32()?,
        next_seq: r.u32()?,
        count: r.u16()?,
    };
    if r.bytes(14)?.iter().any(|&b| b != 0) {
        return Err(MetaError::Field("reserved"));
    }
    if header.page_index != page_index {
        return Err(MetaError::Field("page_index"));
    }
    if header.count > max_entries || u32::from(header.count) > header.next_seq {
        return Err(MetaError::Field("count"));
    }
    let len = usize::from(header.count) * ENTRY2_LEN;
    if bytes.len() != PAGE_HEADER_LEN + len {
        return Err(MetaError::Length);
    }
    let entries = &bytes[PAGE_HEADER_LEN..];
    let page = Page { header, entries };
    // Seqs unique and below next_seq; reserved bytes zero. O(n^2) over at
    // most a few hundred entries, with no allocation.
    for (i, a) in page.entries().enumerate() {
        if a.seq >= header.next_seq {
            return Err(MetaError::Field("seq"));
        }
        if a.reserved != 0 {
            return Err(MetaError::Field("reserved"));
        }
        if page.entries().skip(i + 1).any(|b| b.seq == a.seq) {
            return Err(MetaError::Duplicate);
        }
    }
    Ok(page)
}

// ---------------------------------------------------------------------------
// Region header (object 2^62 | region_id)
// ---------------------------------------------------------------------------

/// A format-2 region header's fixed part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionHeader2 {
    /// `p << 32 | seq`.
    pub region_id: u64,
    /// Region kind (as the entry).
    pub kind: RegionKind,
    /// Ready, or Released (forensic).
    pub state: RegionState,
    /// Fill pattern.
    pub fill: FillPattern,
    /// Must-understand flags (as the entry).
    pub flags: u8,
    /// May-ignore flags, kept verbatim.
    pub compat_flags: u8,
    /// The region's data block: `log2(B)`, or 9 for a 512 B append region.
    pub log2_block: u8,
    /// Extents listed (the entry's count, or one more after a crash inside
    /// a growth beyond capacity).
    pub extent_count: u16,
    /// The region's generation (`1` at provisioning).
    pub region_generation: u64,
    /// Key of the keyed fill (zero for zeros).
    pub fill_key: [u8; 16],
    /// A slot region's object length (others 0).
    pub object_len: u32,
}

/// One extent: `(offset, length)` in the file.
pub type Extent = (u64, u64);

/// Encodes a region header: the fixed part, then `extents` (append and page
/// regions) or `object` (slot regions). Returns the payload length.
///
/// # Errors
///
/// [`MetaError::Length`] if `out` is too short; [`MetaError::Field`] if
/// `extent_count` or `object_len` disagree with what is passed.
pub fn encode_region_header(
    out: &mut [u8],
    h: &RegionHeader2,
    extents: &[Extent],
    object: &[u8],
) -> Result<usize, MetaError> {
    if usize::from(h.extent_count) != extents.len() || h.object_len as usize != object.len() {
        return Err(MetaError::Field("extent_count"));
    }
    let mut w = Writer::new(out);
    w.u64(h.region_id)?;
    w.u8(h.kind.to_u8())?;
    w.u8(h.state.to_u8())?;
    w.u8(h.fill.to_u8())?;
    w.u8(h.flags)?;
    w.u8(h.compat_flags)?;
    w.u8(h.log2_block)?;
    w.u16(h.extent_count)?;
    w.u64(h.region_generation)?;
    w.bytes(&h.fill_key)?;
    w.u32(h.object_len)?;
    w.zeros(20)?;
    for &(off, len) in extents {
        w.u64(off)?;
        w.u64(len)?;
    }
    w.bytes(object)?;
    Ok(w.position())
}

/// A decoded region header: the fixed part and its extents or object.
#[derive(Debug, Clone, Copy)]
pub struct RegionHeaderView<'a> {
    /// The fixed part.
    pub header: RegionHeader2,
    tail: &'a [u8],
}

impl<'a> RegionHeaderView<'a> {
    /// The listed extents, in data order.
    pub fn extents(&self) -> impl Iterator<Item = Extent> + 'a {
        let n = usize::from(self.header.extent_count) * EXTENT_LEN;
        self.tail.get(..n).unwrap_or(&[]).chunks_exact(EXTENT_LEN).map(|c| {
            let mut a = [0u8; 8];
            let mut b = [0u8; 8];
            a.copy_from_slice(&c[..8]);
            b.copy_from_slice(&c[8..]);
            (u64::from_le_bytes(a), u64::from_le_bytes(b))
        })
    }

    /// A slot region's object.
    #[must_use]
    pub fn object(&self) -> &'a [u8] {
        if self.header.kind == RegionKind::Slot {
            self.tail
        } else {
            &[]
        }
    }
}

/// Decodes and validates a region header payload for a container of blocks
/// of `1 << log2_block` bytes and `file_len` bytes (spec §8).
///
/// # Errors
///
/// [`MetaError`] for the first rule that fails.
pub fn decode_region_header(
    bytes: &[u8],
    log2_block: u8,
    file_len: u64,
) -> Result<RegionHeaderView<'_>, MetaError> {
    let mut r = Reader::new(bytes);
    let region_id = r.u64()?;
    let kind = RegionKind::from_u8(r.u8()?)?;
    let state = RegionState::from_u8(r.u8()?)?;
    let fill = FillPattern::from_u8(r.u8()?)?;
    let flags = r.u8()?;
    let compat_flags = r.u8()?;
    let log2 = r.u8()?;
    let extent_count = r.u16()?;
    let region_generation = r.u64()?;
    let fill_key = r.array::<16>()?;
    let object_len = r.u32()?;
    if r.bytes(20)?.iter().any(|&b| b != 0) {
        return Err(MetaError::Field("reserved"));
    }
    if flags & !KNOWN_FLAGS != 0 {
        return Err(MetaError::Field("flags"));
    }
    if !(log2 == log2_block || (log2 == 9 && kind == RegionKind::Append)) {
        return Err(MetaError::Field("log2_block"));
    }
    if region_generation == 0 {
        return Err(MetaError::Field("region_generation"));
    }
    if fill == FillPattern::Zeros && fill_key != [0; 16] {
        return Err(MetaError::Field("fill_key"));
    }
    let tail = &bytes[REGION_HEADER2_LEN.min(bytes.len())..];
    if kind == RegionKind::Slot {
        if extent_count != 0 {
            return Err(MetaError::Field("extent_count"));
        }
        if tail.len() != object_len as usize || tail.len() > (1usize << log2_block) - 192 {
            return Err(MetaError::Length);
        }
    } else {
        if object_len != 0 || extent_count == 0 || usize::from(extent_count) > max_extents(log2_block) {
            return Err(MetaError::Field("extent_count"));
        }
        if tail.len() != usize::from(extent_count) * EXTENT_LEN {
            return Err(MetaError::Length);
        }
    }
    let view = RegionHeaderView {
        header: RegionHeader2 {
            region_id,
            kind,
            state,
            fill,
            flags,
            compat_flags,
            log2_block: log2,
            extent_count,
            region_generation,
            fill_key,
            object_len,
        },
        tail,
    };
    let block = 1u64 << log2_block;
    for (off, len) in view.extents() {
        if len == 0 || off % block != 0 || len % block != 0 {
            return Err(MetaError::Unaligned);
        }
        match off.checked_add(len) {
            Some(end) if end <= file_len => {}
            _ => return Err(MetaError::Field("extent")),
        }
    }
    Ok(view)
}
