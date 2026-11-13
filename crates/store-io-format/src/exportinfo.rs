//! `regions.info`: the record a file export writes beside its data files
//! (PLANNING R5.6; `docs/FORMAT.md`, Part III).
//!
//! The record describes an export: the store's block size, whether it was
//! exported from a writable open, the bytes a restore needs at most, the
//! volume's facts (omitted entries, lost and stale table pages, the regions
//! copied in full and how far each was verified), and per region every
//! attribute a restore recreates, its stream length, and one CRC-32C per
//! [`SPAN`] bytes of its data file (the last span shorter), so a reader
//! checks each span as it reads whatever its read sizes.
//!
//! ```text
//! header      64 bytes (HEADER_LEN)
//! omitted     omitted_count x u64      region ids, strictly ascending
//! lost        lost_count x u32         table page indices, strictly ascending
//! stale       stale_count x u32        the same, none of them lost
//! verified    verified_count x 32      name (24, zero padded), u64 length;
//!                                      names strictly ascending
//! regions     region_count x (RECORD_LEN record, then span CRCs: u32 each)
//! crc         u32                      CRC-32C of every byte before it
//! ```
//!
//! Integers are little-endian. The magic and version are read first (a
//! foreign file or another version is told apart), then the whole record
//! is checked against its CRC before any other field is read; every count
//! is bounded by the bytes left before it is used; every reserved byte and
//! unknown flag must be zero, an absent field (a cap of an uncapped
//! region, a durable length of a region exported without one) is zero,
//! each list is a set in ascending order, and each region record's fields
//! agree with its kind ([`RegionRecord::check`]). So a decoded record has
//! one encoding. Decoding allocates nothing: [`decode`] returns a view, its
//! lists and regions read in place. What needs the whole record at once
//! (each name used once, each verified name a region copied in full, the
//! data files' lengths, `total` against the data) is the restore's to check.

use core::fmt;

use crate::codec::{CodecError, Reader, Writer};
use crate::crc32c::crc32c;
use crate::meta::{FillPattern, NAME_MAX, RegionKind, RegionName};

/// The record's first bytes.
pub const MAGIC: [u8; 8] = *b"SIOXINFO";
/// The record's version this crate writes and reads.
pub const VERSION: u16 = 1;
/// The bytes of a region's data file each CRC-32C covers.
pub const SPAN: u64 = 1 << 20;
/// The fixed header's length.
pub const HEADER_LEN: usize = 64;
/// One region record's fixed length (its span CRCs follow it).
pub const RECORD_LEN: usize = 128;
/// A `verified` list item's length.
pub const VERIFIED_LEN: usize = NAME_MAX + 8;
/// The longest record written or accepted: 1 GiB (about 268 million spans,
/// 256 TiB of exported data). A reader should refuse a longer file before
/// it reads it.
pub const MAX_LEN: u64 = 1 << 30;

/// Header flag: exported from a writable open.
const FLAG_WRITABLE: u16 = 1;
/// Region flags.
const R_CAPPED: u16 = 1;
const R_DURABLE: u16 = 1 << 1;
const R_FULL_COPY: u16 = 1 << 2;
const R_SUPERSEDED: u16 = 1 << 3;
const R_KNOWN: u16 = R_CAPPED | R_DURABLE | R_FULL_COPY | R_SUPERSEDED;
/// The smallest and largest store block (512 B to 64 KiB).
const LOG2_MIN: u8 = 9;
const LOG2_MAX: u8 = 16;

/// Why a record was refused (decoding), or could not be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoError {
    /// Shorter than its fixed parts, or than its counts say.
    Truncated,
    /// Longer than [`MAX_LEN`], bytes left after the last region, or a
    /// region record not [`RECORD_LEN`] long.
    Length,
    /// The CRC-32C does not match.
    Checksum,
    /// Not a record (the magic).
    Magic,
    /// A version this crate does not read.
    Version,
    /// A field out of its range or not agreeing with the others, an unknown
    /// flag, a reserved byte not zero, or a list out of order (the field
    /// named).
    Field(&'static str),
    /// The output buffer is too short ([`encode`]).
    Buffer,
}

impl fmt::Display for InfoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("export record truncated"),
            Self::Length => f.write_str("export record of the wrong length"),
            Self::Checksum => f.write_str("export record checksum mismatch"),
            Self::Magic => f.write_str("not an export record"),
            Self::Version => f.write_str("unsupported export record version"),
            Self::Field(name) => write!(f, "export record field out of range: {name}"),
            Self::Buffer => f.write_str("output buffer too short for the export record"),
        }
    }
}

impl std::error::Error for InfoError {}

impl From<CodecError> for InfoError {
    fn from(e: CodecError) -> Self {
        match e {
            CodecError::Truncated => Self::Truncated,
            CodecError::Overflow => Self::Buffer,
        }
    }
}

/// The record's fixed header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InfoHeader {
    /// Exported from a writable open (its regions as read while in use).
    pub writable: bool,
    /// The store's block size, as a power of two (9 to 16).
    pub block_log2: u8,
    /// Regions that follow.
    pub region_count: u32,
    /// Chunks the export lent its sink at once.
    pub lend: u32,
    /// The bytes a restore needs at most (untrusted: check it against the
    /// regions' sizes before using it).
    pub total: u64,
    /// List lengths.
    pub omitted_count: u32,
    /// See [`Self::omitted_count`].
    pub lost_count: u32,
    /// See [`Self::omitted_count`].
    pub stale_count: u32,
    /// See [`Self::omitted_count`].
    pub verified_count: u32,
}

impl InfoHeader {
    /// The header's bytes.
    #[must_use]
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        let mut w = Writer::new(&mut out);
        // Every write fits: the buffer is the header's length.
        let _done: Result<(), CodecError> = (|| {
            w.bytes(&MAGIC)?;
            w.u16(VERSION)?;
            w.u16(if self.writable { FLAG_WRITABLE } else { 0 })?;
            w.u8(self.block_log2)?;
            w.zeros(3)?;
            w.u32(self.region_count)?;
            w.u32(self.lend)?;
            w.u64(self.total)?;
            w.u32(self.omitted_count)?;
            w.u32(self.lost_count)?;
            w.u32(self.stale_count)?;
            w.u32(self.verified_count)?;
            w.zeros(16)
        })();
        out
    }

    /// Decodes a header (the magic, version, flags, block size and reserved
    /// bytes checked; the counts bounded by [`decode`]).
    ///
    /// # Errors
    ///
    /// [`InfoError`] as above.
    pub fn decode(bytes: &[u8]) -> Result<Self, InfoError> {
        let mut r = Reader::new(bytes);
        if r.array::<8>()? != MAGIC {
            return Err(InfoError::Magic);
        }
        if r.u16()? != VERSION {
            return Err(InfoError::Version);
        }
        let flags = r.u16()?;
        if flags & !FLAG_WRITABLE != 0 {
            return Err(InfoError::Field("flags"));
        }
        let block_log2 = r.u8()?;
        check_log2(block_log2)?;
        if r.array::<3>()? != [0; 3] {
            return Err(InfoError::Field("reserved"));
        }
        let h = Self {
            writable: flags & FLAG_WRITABLE != 0,
            block_log2,
            region_count: r.u32()?,
            lend: r.u32()?,
            total: r.u64()?,
            omitted_count: r.u32()?,
            lost_count: r.u32()?,
            stale_count: r.u32()?,
            verified_count: r.u32()?,
        };
        if r.array::<16>()? != [0; 16] {
            return Err(InfoError::Field("reserved"));
        }
        Ok(h)
    }
}

fn check_log2(log2: u8) -> Result<(), InfoError> {
    if (LOG2_MIN..=LOG2_MAX).contains(&log2) {
        Ok(())
    } else {
        Err(InfoError::Field("block_size"))
    }
}

/// One region's attributes, as a restore recreates them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionRecord {
    /// Its name.
    pub name: RegionName,
    /// Its kind.
    pub kind: RegionKind,
    /// Its fill pattern.
    pub fill: FillPattern,
    /// Its fill key.
    pub fill_key: [u8; 16],
    /// Its tag.
    pub tag: u32,
    /// Its generation (a format-1 slot's: 1), never 0.
    pub generation: u64,
    /// Its growth cap, if capped: at least `data_size`.
    pub cap: Option<u64>,
    /// Its data size as planned.
    pub data_size: u64,
    /// An append region's durable length as exported, at most `data_size`
    /// (`None`: exported unpositioned, as read, or another kind).
    pub durable_len: Option<u64>,
    /// A slot's committed object's length (0 for another kind, or none
    /// committed).
    pub object_len: u32,
    /// The bytes of its data file: a slot's `object_len`, an append
    /// region's `durable_len` when it has one, else `data_size`.
    pub stream_len: u64,
    /// Copied in full (a region of a stale page: its whole capacity; never
    /// a slot, never with a durable length).
    pub full_copy: bool,
    /// Possibly superseded (its source held it read-only).
    pub superseded: bool,
}

impl RegionRecord {
    /// Its span CRCs: one per [`SPAN`] bytes of its data file.
    #[must_use]
    pub fn spans(&self) -> u64 {
        self.stream_len.div_ceil(SPAN)
    }

    /// Whether its fields agree with each other and with its kind (what a
    /// record says of itself; checked by [`decode`] and [`encode`]).
    ///
    /// # Errors
    ///
    /// [`InfoError::Field`] naming the first field that does not.
    pub fn check(&self) -> Result<(), InfoError> {
        if self.generation == 0 {
            return Err(InfoError::Field("generation"));
        }
        if self.cap.is_some_and(|c| c < self.data_size) {
            return Err(InfoError::Field("cap"));
        }
        let slot = self.kind == RegionKind::Slot;
        if let Some(d) = self.durable_len {
            if self.kind != RegionKind::Append || self.full_copy || d > self.data_size {
                return Err(InfoError::Field("durable_len"));
            }
        }
        if self.object_len != 0 && !slot {
            return Err(InfoError::Field("object_len"));
        }
        if self.full_copy && slot {
            return Err(InfoError::Field("flags"));
        }
        let stream = if slot {
            u64::from(self.object_len)
        } else {
            self.durable_len.unwrap_or(self.data_size)
        };
        if self.stream_len != stream {
            return Err(InfoError::Field("stream_len"));
        }
        Ok(())
    }

    /// The record's bytes (its span CRCs follow it).
    #[must_use]
    pub fn encode(&self) -> [u8; RECORD_LEN] {
        let mut out = [0u8; RECORD_LEN];
        let mut flags = 0u16;
        if self.cap.is_some() {
            flags |= R_CAPPED;
        }
        if self.durable_len.is_some() {
            flags |= R_DURABLE;
        }
        if self.full_copy {
            flags |= R_FULL_COPY;
        }
        if self.superseded {
            flags |= R_SUPERSEDED;
        }
        let mut w = Writer::new(&mut out);
        // Every write fits: the buffer is the record's length.
        let _done: Result<(), CodecError> = (|| {
            w.bytes(&self.name.as_padded())?;
            w.u8(self.kind.to_u8())?;
            w.u8(self.fill.to_u8())?;
            w.u16(flags)?;
            w.u32(self.tag)?;
            w.u64(self.generation)?;
            w.u64(self.cap.unwrap_or(0))?;
            w.bytes(&self.fill_key)?;
            w.u64(self.data_size)?;
            w.u64(self.durable_len.unwrap_or(0))?;
            w.u32(self.object_len)?;
            w.zeros(4)?;
            w.u64(self.stream_len)?;
            // Reserved: zero in version 1.
            w.zeros(32)
        })();
        out
    }

    /// Decodes a record of exactly [`RECORD_LEN`] bytes (every field's form
    /// checked, and [`Self::check`]).
    ///
    /// # Errors
    ///
    /// [`InfoError`] as above.
    pub fn decode(bytes: &[u8]) -> Result<Self, InfoError> {
        if bytes.len() != RECORD_LEN {
            return Err(InfoError::Length);
        }
        let mut r = Reader::new(bytes);
        let name = RegionName::from_padded(r.array::<NAME_MAX>()?)
            .map_err(|_| InfoError::Field("name"))?;
        let kind = RegionKind::from_u8(r.u8()?).map_err(|_| InfoError::Field("kind"))?;
        let fill = FillPattern::from_u8(r.u8()?).map_err(|_| InfoError::Field("fill"))?;
        let flags = r.u16()?;
        if flags & !R_KNOWN != 0 {
            return Err(InfoError::Field("flags"));
        }
        let tag = r.u32()?;
        let generation = r.u64()?;
        let cap = r.u64()?;
        let fill_key = r.array::<16>()?;
        let data_size = r.u64()?;
        let durable = r.u64()?;
        let object_len = r.u32()?;
        if r.u32()? != 0 {
            return Err(InfoError::Field("reserved"));
        }
        let stream_len = r.u64()?;
        if r.array::<32>()? != [0; 32] {
            return Err(InfoError::Field("reserved"));
        }
        // An absent field is zero: one encoding per record.
        if flags & R_CAPPED == 0 && cap != 0 {
            return Err(InfoError::Field("cap"));
        }
        if flags & R_DURABLE == 0 && durable != 0 {
            return Err(InfoError::Field("durable_len"));
        }
        let rec = Self {
            name,
            kind,
            fill,
            fill_key,
            tag,
            generation,
            cap: (flags & R_CAPPED != 0).then_some(cap),
            data_size,
            durable_len: (flags & R_DURABLE != 0).then_some(durable),
            object_len,
            stream_len,
            full_copy: flags & R_FULL_COPY != 0,
            superseded: flags & R_SUPERSEDED != 0,
        };
        rec.check()?;
        Ok(rec)
    }
}

/// A decoded record, read in place.
#[derive(Debug, Clone, Copy)]
pub struct ExportInfo<'a> {
    /// The header.
    pub header: InfoHeader,
    omitted: &'a [u8],
    lost: &'a [u8],
    stale: &'a [u8],
    verified: &'a [u8],
    regions: &'a [u8],
}

/// `n` items of `size` bytes from `r`, bounded by what is left (the
/// reader refuses a length past its end before taking any of it).
fn items<'a>(r: &mut Reader<'a>, n: u32, size: usize) -> Result<&'a [u8], InfoError> {
    let len = (n as usize).checked_mul(size).ok_or(InfoError::Truncated)?;
    Ok(r.bytes(len)?)
}

fn u64_at(b: &[u8], i: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[i * 8..i * 8 + 8]);
    u64::from_le_bytes(a)
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[i * 4..i * 4 + 4]);
    u32::from_le_bytes(a)
}

/// Whether `it` is strictly ascending (a set, in its one order).
fn ascending<T: PartialOrd>(mut it: impl Iterator<Item = T>) -> bool {
    let Some(mut prev) = it.next() else {
        return true;
    };
    for x in it {
        if x <= prev {
            return false;
        }
        prev = x;
    }
    true
}

/// The four lists' order rules (shared by [`decode`] and [`encode`]).
fn check_lists(
    omitted: impl Iterator<Item = u64>,
    lost: impl Iterator<Item = u32> + Clone,
    stale: impl Iterator<Item = u32> + Clone,
    verified: impl Iterator<Item = [u8; NAME_MAX]>,
) -> Result<(), InfoError> {
    if !ascending(omitted) {
        return Err(InfoError::Field("omitted"));
    }
    if !ascending(lost.clone()) {
        return Err(InfoError::Field("lost"));
    }
    if !ascending(stale.clone()) {
        return Err(InfoError::Field("stale"));
    }
    // A page is lost or stale, never both (bounded walk: both ascending).
    let mut l = lost.peekable();
    for s in stale {
        while l.next_if(|&x| x < s).is_some() {}
        if l.peek() == Some(&s) {
            return Err(InfoError::Field("stale"));
        }
    }
    if !ascending(verified) {
        return Err(InfoError::Field("verified"));
    }
    Ok(())
}

/// Checks and decodes a whole record: its length, its magic and version,
/// its CRC-32C, its header, each list bounded by the bytes left and in its
/// order, each region's record and span CRCs, and nothing after the last
/// region but the CRC.
///
/// # Errors
///
/// [`InfoError`] as above.
pub fn decode(bytes: &[u8]) -> Result<ExportInfo<'_>, InfoError> {
    decode_within(bytes, MAX_LEN)
}

/// [`decode`], refusing a record longer than `max`.
fn decode_within(bytes: &[u8], max: u64) -> Result<ExportInfo<'_>, InfoError> {
    if bytes.len() as u64 > max {
        return Err(InfoError::Length);
    }
    if bytes.len() < HEADER_LEN + 4 {
        return Err(InfoError::Truncated);
    }
    // Told apart before the CRC: a foreign file, or another version (whose
    // framing this crate does not know).
    if bytes[..8] != MAGIC {
        return Err(InfoError::Magic);
    }
    if bytes[8..10] != VERSION.to_le_bytes() {
        return Err(InfoError::Version);
    }
    let (body, crc) = bytes.split_at(bytes.len() - 4);
    if crc32c(body) != u32_at(crc, 0) {
        return Err(InfoError::Checksum);
    }
    let header = InfoHeader::decode(&body[..HEADER_LEN])?;
    let mut r = Reader::new(&body[HEADER_LEN..]);
    let omitted = items(&mut r, header.omitted_count, 8)?;
    let lost = items(&mut r, header.lost_count, 4)?;
    let stale = items(&mut r, header.stale_count, 4)?;
    let verified = items(&mut r, header.verified_count, VERIFIED_LEN)?;
    for item in verified.chunks_exact(VERIFIED_LEN) {
        let _name =
            RegionName::from_padded(padded(item)).map_err(|_| InfoError::Field("verified"))?;
    }
    check_lists(
        (0..omitted.len() / 8).map(|i| u64_at(omitted, i)),
        u32s(lost),
        u32s(stale),
        verified.chunks_exact(VERIFIED_LEN).map(padded),
    )?;
    let start = r.position();
    for _ in 0..header.region_count {
        let rec = RegionRecord::decode(items(&mut r, 1, RECORD_LEN)?)?;
        let spans = u32::try_from(rec.spans()).map_err(|_| InfoError::Truncated)?;
        let _crcs = items(&mut r, spans, 4)?;
    }
    if r.remaining() != 0 {
        return Err(InfoError::Length);
    }
    let all = &body[HEADER_LEN..];
    Ok(ExportInfo {
        header,
        omitted,
        lost,
        stale,
        verified,
        regions: &all[start..],
    })
}

/// The u32 items of a list, in order.
fn u32s(b: &[u8]) -> impl Iterator<Item = u32> + Clone + '_ {
    (0..b.len() / 4).map(move |i| u32_at(b, i))
}

/// A verified item's padded name.
fn padded(item: &[u8]) -> [u8; NAME_MAX] {
    let mut raw = [0u8; NAME_MAX];
    raw.copy_from_slice(&item[..NAME_MAX]);
    raw
}

impl<'a> ExportInfo<'a> {
    /// The omitted entries' region ids, ascending.
    pub fn omitted(&self) -> impl Iterator<Item = u64> + 'a {
        let b = self.omitted;
        (0..b.len() / 8).map(move |i| u64_at(b, i))
    }

    /// The lost table pages, ascending.
    pub fn lost_pages(&self) -> impl Iterator<Item = u32> + 'a {
        let b = self.lost;
        (0..b.len() / 4).map(move |i| u32_at(b, i))
    }

    /// The stale table pages, ascending.
    pub fn stale_pages(&self) -> impl Iterator<Item = u32> + 'a {
        let b = self.stale;
        (0..b.len() / 4).map(move |i| u32_at(b, i))
    }

    /// The regions copied in full and how far each was verified, by name.
    pub fn verified(&self) -> impl Iterator<Item = (RegionName, u64)> + 'a {
        let b = self.verified;
        b.chunks_exact(VERIFIED_LEN).filter_map(|item| {
            // Checked by `decode`.
            let name = RegionName::from_padded(padded(item)).ok()?;
            Some((name, u64_at(&item[NAME_MAX..], 0)))
        })
    }

    /// The regions, in order: each record and its span CRCs.
    pub fn regions(&self) -> Regions<'a> {
        Regions { rest: self.regions }
    }
}

/// The regions of an [`ExportInfo`], in order.
#[derive(Debug, Clone)]
pub struct Regions<'a> {
    rest: &'a [u8],
}

/// A region's span CRCs, read in place.
#[derive(Debug, Clone, Copy)]
pub struct Spans<'a> {
    b: &'a [u8],
}

impl Spans<'_> {
    /// How many.
    #[must_use]
    pub fn len(&self) -> usize {
        self.b.len() / 4
    }

    /// None (an empty data file).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.b.is_empty()
    }

    /// Span `i`'s CRC-32C.
    #[must_use]
    pub fn get(&self, i: usize) -> Option<u32> {
        (i < self.len()).then(|| u32_at(self.b, i))
    }
}

impl<'a> Iterator for Regions<'a> {
    type Item = (RegionRecord, Spans<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        // Checked by `decode`: each record decodes and its spans are there.
        let rec = RegionRecord::decode(self.rest.get(..RECORD_LEN)?).ok()?;
        let n = usize::try_from(rec.spans()).ok()?.checked_mul(4)?;
        let spans = self.rest.get(RECORD_LEN..RECORD_LEN.checked_add(n)?)?;
        self.rest = &self.rest[RECORD_LEN + n..];
        Some((rec, Spans { b: spans }))
    }
}

/// The record's bytes from a decoded view (the fuzz target's canonical
/// check). `out` must hold them all.
///
/// # Errors
///
/// [`CodecError::Overflow`] if `out` is too short.
pub fn encode_into(info: &ExportInfo<'_>, out: &mut [u8]) -> Result<usize, CodecError> {
    let mut w = Writer::new(out);
    w.bytes(&info.header.encode())?;
    w.bytes(info.omitted)?;
    w.bytes(info.lost)?;
    w.bytes(info.stale)?;
    w.bytes(info.verified)?;
    for (rec, spans) in info.regions() {
        w.bytes(&rec.encode())?;
        w.bytes(spans.b)?;
    }
    let n = w.position();
    let crc = crc32c(&out[..n]);
    let mut w = Writer::new(&mut out[n..]);
    w.u32(crc)?;
    Ok(n + 4)
}

/// A region to encode: its record and its span CRCs (one per [`SPAN`] of
/// its stream length).
pub type RegionItem<'a> = (RegionRecord, &'a [u32]);

/// The encoded length of a record with these lists and regions (`None` if
/// it does not fit a `usize`).
#[must_use]
pub fn encoded_len(
    omitted: usize,
    lost: usize,
    stale: usize,
    verified: usize,
    regions: &[RegionItem<'_>],
) -> Option<usize> {
    let mut n = HEADER_LEN
        .checked_add(omitted.checked_mul(8)?)?
        .checked_add(lost.checked_add(stale)?.checked_mul(4)?)?
        .checked_add(verified.checked_mul(VERIFIED_LEN)?)?
        .checked_add(4)?;
    for (_, spans) in regions {
        n = n.checked_add(RECORD_LEN.checked_add(spans.len().checked_mul(4)?)?)?;
    }
    Some(n)
}

/// Encodes a whole record into `out`: `header` with its counts taken from
/// the lists, the lists, each region's record and span CRCs, and the
/// CRC-32C. Returns the bytes written ([`encoded_len`]). It writes only
/// what [`decode`] accepts.
///
/// # Errors
///
/// [`InfoError::Field`] for a header block size out of range, a list out
/// of order (or a page both lost and stale), a region record whose fields
/// disagree ([`RegionRecord::check`]), or span CRCs not one per [`SPAN`] of
/// its stream length; [`InfoError::Length`] for a record longer than
/// [`MAX_LEN`] or a count that does not fit its field;
/// [`InfoError::Buffer`] if `out` is too short.
pub fn encode(
    header: &InfoHeader,
    omitted: &[u64],
    lost: &[u32],
    stale: &[u32],
    verified: &[(RegionName, u64)],
    regions: &[RegionItem<'_>],
    out: &mut [u8],
) -> Result<usize, InfoError> {
    encode_within(
        header, omitted, lost, stale, verified, regions, out, MAX_LEN,
    )
}

/// [`encode`], refusing a record longer than `max`.
#[allow(clippy::too_many_arguments)]
fn encode_within(
    header: &InfoHeader,
    omitted: &[u64],
    lost: &[u32],
    stale: &[u32],
    verified: &[(RegionName, u64)],
    regions: &[RegionItem<'_>],
    out: &mut [u8],
    max: u64,
) -> Result<usize, InfoError> {
    check_log2(header.block_log2)?;
    check_lists(
        omitted.iter().copied(),
        lost.iter().copied(),
        stale.iter().copied(),
        verified.iter().map(|(n, _)| n.as_padded()),
    )?;
    for (rec, spans) in regions {
        rec.check()?;
        if spans.len() as u64 != rec.spans() {
            return Err(InfoError::Field("spans"));
        }
    }
    let len = encoded_len(
        omitted.len(),
        lost.len(),
        stale.len(),
        verified.len(),
        regions,
    )
    .ok_or(InfoError::Length)?;
    if len as u64 > max {
        return Err(InfoError::Length);
    }
    let count = |n: usize| u32::try_from(n).map_err(|_| InfoError::Length);
    let h = InfoHeader {
        region_count: count(regions.len())?,
        omitted_count: count(omitted.len())?,
        lost_count: count(lost.len())?,
        stale_count: count(stale.len())?,
        verified_count: count(verified.len())?,
        ..*header
    };
    let mut w = Writer::new(out);
    w.bytes(&h.encode())?;
    for &id in omitted {
        w.u64(id)?;
    }
    for &p in lost.iter().chain(stale) {
        w.u32(p)?;
    }
    for (name, len) in verified {
        w.bytes(&name.as_padded())?;
        w.u64(*len)?;
    }
    for (rec, spans) in regions {
        w.bytes(&rec.encode())?;
        for &c in *spans {
            w.u32(c)?;
        }
    }
    let n = w.position();
    let crc = crc32c(&out[..n]);
    let mut w = Writer::new(&mut out[n..]);
    w.u32(crc)?;
    Ok(n + 4)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn name(s: &str) -> RegionName {
        RegionName::new(s).unwrap()
    }

    fn header() -> InfoHeader {
        InfoHeader {
            writable: true,
            block_log2: 12,
            region_count: 0,
            lend: 5,
            total: 123_456_789,
            omitted_count: 0,
            lost_count: 0,
            stale_count: 0,
            verified_count: 0,
        }
    }

    /// A region whose stream is its data size (a page region's rule).
    fn record(n: &str, kind: RegionKind, stream_len: u64) -> RegionRecord {
        RegionRecord {
            name: name(n),
            kind,
            fill: FillPattern::KeyedV1,
            fill_key: [7; 16],
            tag: 42,
            generation: 3,
            cap: None,
            data_size: stream_len,
            durable_len: None,
            object_len: 0,
            stream_len,
            full_copy: false,
            superseded: false,
        }
    }

    /// The sample's regions: an append region capped with a durable
    /// length, a page region copied in full and superseded, a slot.
    fn regions() -> [RegionRecord; 3] {
        let mut a = record("log", RegionKind::Append, 3 * SPAN + 5);
        a.data_size = 4 * SPAN;
        a.cap = Some(1 << 30);
        a.durable_len = Some(3 * SPAN + 5);
        let mut b = record("pages", RegionKind::Page, 2 * SPAN);
        b.full_copy = true;
        b.superseded = true;
        b.fill = FillPattern::Zeros;
        let mut c = record("obj", RegionKind::Slot, 6);
        c.data_size = 0;
        c.object_len = 6;
        [a, b, c]
    }

    /// Three regions and every list.
    fn sample() -> Vec<u8> {
        let [a, b, c] = regions();
        let (sa, sb, sc) = ([1, 2, 3, 4], [5, 6], [7]);
        let regions: [RegionItem<'_>; 3] = [(a, &sa), (b, &sb), (c, &sc)];
        let verified = [(name("pages"), 77)];
        let len = encoded_len(2, 1, 2, 1, &regions).unwrap();
        let mut out = vec![0u8; len];
        let n = encode(
            &header(),
            &[9, 1 << 33],
            &[3],
            &[1, 2],
            &verified,
            &regions,
            &mut out,
        )
        .unwrap();
        assert_eq!(n, len);
        out
    }

    /// `b` with its CRC recomputed (a structurally wrong record that
    /// passes the checksum).
    fn resealed(mut b: Vec<u8>) -> Vec<u8> {
        let n = b.len() - 4;
        let crc = crc32c(&b[..n]).to_le_bytes();
        b[n..].copy_from_slice(&crc);
        b
    }

    /// The sample with `edits` written at their offsets, resealed, decoded.
    fn edited(edits: &[(usize, &[u8])]) -> Option<InfoError> {
        let mut x = sample();
        for (off, v) in edits {
            x[*off..*off + v.len()].copy_from_slice(v);
        }
        decode(&resealed(x)).err()
    }

    /// Where the lists and the region records start in the sample.
    const OMITTED: usize = HEADER_LEN;
    const LOST: usize = OMITTED + 16;
    const STALE: usize = LOST + 4;
    const VERIFIED: usize = STALE + 8;
    const R0: usize = VERIFIED + VERIFIED_LEN;
    const R1: usize = R0 + RECORD_LEN + 16;
    const R2: usize = R1 + RECORD_LEN + 8;

    #[test]
    fn test_a_record_round_trips_exactly() {
        let b = sample();
        let info = decode(&b).unwrap();
        assert_eq!(info.header.region_count, 3);
        assert!(info.header.writable);
        assert_eq!((info.header.lend, info.header.total), (5, 123_456_789));
        assert_eq!(info.omitted().collect::<Vec<_>>(), [9, 1 << 33]);
        assert_eq!(info.lost_pages().collect::<Vec<_>>(), [3]);
        assert_eq!(info.stale_pages().collect::<Vec<_>>(), [1, 2]);
        assert_eq!(info.verified().collect::<Vec<_>>(), [(name("pages"), 77)]);
        let got: Vec<_> = info.regions().collect();
        assert_eq!(got.len(), 3);
        for (i, want) in regions().iter().enumerate() {
            assert_eq!(&got[i].0, want, "region {i}");
        }
        let sa = got[0].1;
        assert_eq!(
            (0..sa.len())
                .map(|i| sa.get(i).unwrap())
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert_eq!(sa.get(4), None);
        assert_eq!((got[1].1.len(), got[2].1.len()), (2, 1));
        let mut again = vec![0u8; b.len()];
        assert_eq!(encode_into(&info, &mut again).unwrap(), b.len());
        assert_eq!(again, b);
    }

    #[test]
    fn test_every_flipped_bit_is_refused() {
        let b = sample();
        for i in 0..b.len() * 8 {
            let mut x = b.clone();
            x[i / 8] ^= 1 << (i % 8);
            assert!(decode(&x).is_err(), "bit {i}");
        }
    }

    #[test]
    fn test_every_truncation_and_extension_is_refused() {
        let b = sample();
        for n in 0..b.len() {
            assert!(decode(&b[..n]).is_err(), "cut at {n}");
        }
        let mut longer = b[..b.len() - 4].to_vec();
        longer.push(0);
        longer.extend_from_slice(&[0; 4]);
        assert_eq!(decode(&resealed(longer)).err(), Some(InfoError::Length));
    }

    /// A body shorter than the header, its CRC valid: refused, never read
    /// past its end.
    #[test]
    fn test_a_short_sealed_body_is_truncated() {
        let b = sample();
        for n in 4..HEADER_LEN + 4 {
            let mut x = b[..n].to_vec();
            x[n - 4..].fill(0);
            assert_eq!(
                decode(&resealed(x)).err(),
                Some(InfoError::Truncated),
                "{n}"
            );
        }
    }

    /// The magic and version are read before the CRC: a foreign file or
    /// another version is named as such.
    #[test]
    fn test_magic_and_version_are_told_before_the_checksum() {
        let mut x = sample();
        x[0] = b'X';
        assert_eq!(decode(&x).err(), Some(InfoError::Magic));
        let mut x = sample();
        x[8] = 2;
        assert_eq!(decode(&x).err(), Some(InfoError::Version));
        let mut x = sample();
        x[20] ^= 1;
        assert_eq!(decode(&x).err(), Some(InfoError::Checksum));
    }

    /// Header fields and counts, refused once the CRC is resealed.
    #[test]
    fn test_each_header_field_out_of_range_is_refused() {
        assert_eq!(
            edited(&[(10, &2u16.to_le_bytes())]),
            Some(InfoError::Field("flags"))
        );
        assert_eq!(edited(&[(12, &[8])]), Some(InfoError::Field("block_size")));
        assert_eq!(edited(&[(12, &[17])]), Some(InfoError::Field("block_size")));
        assert_eq!(edited(&[(13, &[1])]), Some(InfoError::Field("reserved")));
        assert_eq!(edited(&[(63, &[1])]), Some(InfoError::Field("reserved")));
        // Counts beyond the bytes there are.
        assert_eq!(
            edited(&[(16, &4u32.to_le_bytes())]),
            Some(InfoError::Truncated)
        );
        assert_eq!(
            edited(&[(32, &u32::MAX.to_le_bytes())]),
            Some(InfoError::Truncated)
        );
        assert_eq!(
            edited(&[(44, &u32::MAX.to_le_bytes())]),
            Some(InfoError::Truncated)
        );
        // Fewer regions than there are bytes for.
        assert_eq!(
            edited(&[(16, &2u32.to_le_bytes())]),
            Some(InfoError::Length)
        );
    }

    /// Each list is a set in ascending order; a page is lost or stale.
    #[test]
    fn test_each_list_must_be_ascending() {
        assert_eq!(
            edited(&[(OMITTED, &(1u64 << 34).to_le_bytes())]),
            Some(InfoError::Field("omitted"))
        );
        assert_eq!(
            edited(&[(OMITTED, &(1u64 << 33).to_le_bytes())]),
            Some(InfoError::Field("omitted")),
            "a duplicate"
        );
        assert_eq!(
            edited(&[(STALE, &2u32.to_le_bytes())]),
            Some(InfoError::Field("stale")),
            "a duplicate"
        );
        assert_eq!(
            edited(&[(STALE + 4, &3u32.to_le_bytes())]),
            Some(InfoError::Field("stale")),
            "both lost and stale"
        );
        assert_eq!(
            edited(&[(VERIFIED, &[0])]),
            Some(InfoError::Field("verified"))
        );
        let enc = |lost: &[u32], stale: &[u32], verified: &[(RegionName, u64)]| {
            let mut out = vec![0u8; 4096];
            encode(&header(), &[], lost, stale, verified, &[], &mut out).err()
        };
        assert_eq!(enc(&[2, 1], &[], &[]), Some(InfoError::Field("lost")));
        assert_eq!(enc(&[1], &[1], &[]), Some(InfoError::Field("stale")));
        assert_eq!(
            enc(&[], &[], &[(name("b"), 1), (name("a"), 1)]),
            Some(InfoError::Field("verified"))
        );
        assert_eq!(enc(&[1, 3], &[2, 4], &[]), None);
    }

    /// A region record's own fields, refused once the CRC is resealed.
    #[test]
    fn test_each_record_field_out_of_range_is_refused() {
        assert_eq!(edited(&[(R0, &[0])]), Some(InfoError::Field("name")));
        assert_eq!(edited(&[(R0 + 24, &[4])]), Some(InfoError::Field("kind")));
        assert_eq!(edited(&[(R0 + 25, &[2])]), Some(InfoError::Field("fill")));
        assert_eq!(
            edited(&[(R0 + 26, &[0x10])]),
            Some(InfoError::Field("flags"))
        );
        assert_eq!(
            edited(&[(R0 + 84, &[1])]),
            Some(InfoError::Field("reserved"))
        );
        assert_eq!(
            edited(&[(R0 + 127, &[1])]),
            Some(InfoError::Field("reserved"))
        );
        // A page region's data and stream far past the bytes: more spans
        // than there are.
        let big = (1u64 << 40).to_le_bytes();
        assert_eq!(
            edited(&[(R1 + 64, &big), (R1 + 88, &big)]),
            Some(InfoError::Truncated)
        );
    }

    /// A record's fields agree with each other and with its kind.
    #[test]
    fn test_a_record_s_fields_agree_with_its_kind() {
        let [a, b, c] = regions();
        for r in [a, b, c] {
            assert_eq!(r.check(), Ok(()));
            assert_eq!(RegionRecord::decode(&r.encode()), Ok(r));
        }
        let bad = |f: &dyn Fn(&mut RegionRecord), r: RegionRecord| {
            let mut r = r;
            f(&mut r);
            (r.check().err(), RegionRecord::decode(&r.encode()).err())
        };
        let both = |e: &'static str| (Some(InfoError::Field(e)), Some(InfoError::Field(e)));
        assert_eq!(bad(&|r| r.generation = 0, a), both("generation"));
        assert_eq!(bad(&|r| r.cap = Some(r.data_size - 1), a), both("cap"));
        assert_eq!(
            bad(&|r| r.durable_len = Some(r.data_size + 1), a),
            both("durable_len")
        );
        assert_eq!(bad(&|r| r.durable_len = Some(1), b), both("durable_len"));
        // Only an append region has one, even one that would fit.
        let page = record("p", RegionKind::Page, 10);
        assert_eq!(
            bad(&|r| r.durable_len = Some(10), page),
            both("durable_len")
        );
        assert_eq!(
            bad(
                &|r| {
                    r.durable_len = Some(r.stream_len);
                    r.full_copy = true;
                },
                a
            ),
            both("durable_len")
        );
        assert_eq!(bad(&|r| r.object_len = 1, b), both("object_len"));
        assert_eq!(bad(&|r| r.full_copy = true, c), both("flags"));
        assert_eq!(bad(&|r| r.stream_len += 1, a), both("stream_len"));
        assert_eq!(bad(&|r| r.stream_len -= 1, b), both("stream_len"));
        assert_eq!(bad(&|r| r.stream_len = 5, c), both("stream_len"));
        // An unpositioned append region streams its data size.
        let mut u = a;
        u.durable_len = None;
        u.stream_len = u.data_size;
        assert_eq!(u.check(), Ok(()));
    }

    /// An absent field is zero: an uncapped region's cap, a durable length
    /// of a region exported without one.
    #[test]
    fn test_an_absent_field_must_be_zero() {
        let rec = record("pages", RegionKind::Page, 10);
        let mut b = rec.encode();
        assert_eq!(RegionRecord::decode(&b).unwrap(), rec);
        b[40] = 1;
        assert_eq!(
            RegionRecord::decode(&b).err(),
            Some(InfoError::Field("cap"))
        );
        let mut b = rec.encode();
        b[72] = 1;
        assert_eq!(
            RegionRecord::decode(&b).err(),
            Some(InfoError::Field("durable_len"))
        );
        let b = rec.encode();
        assert_eq!(
            RegionRecord::decode(&b[..127]).err(),
            Some(InfoError::Length)
        );
        let mut longer = b.to_vec();
        longer.push(0);
        assert_eq!(RegionRecord::decode(&longer).err(), Some(InfoError::Length));
    }

    #[test]
    fn test_spans_cover_the_stream_length() {
        for (len, spans) in [(0, 0), (1, 1), (SPAN, 1), (SPAN + 1, 2), (3 * SPAN, 3)] {
            assert_eq!(record("x", RegionKind::Page, len).spans(), spans, "{len}");
        }
        let r = record("x", RegionKind::Page, SPAN + 1);
        let mut out = vec![0u8; 4096];
        let one: [RegionItem<'_>; 1] = [(r, &[1])];
        assert_eq!(
            encode(&header(), &[], &[], &[], &[], &one, &mut out).err(),
            Some(InfoError::Field("spans"))
        );
    }

    /// The encoder writes only what the decoder accepts: a block size out
    /// of range, a record whose fields disagree, a record too long or a
    /// buffer too short are refused.
    #[test]
    fn test_the_encoder_refuses_what_the_decoder_would() {
        let mut out = vec![0u8; 4096];
        let h = InfoHeader {
            block_log2: 20,
            ..header()
        };
        assert_eq!(
            encode(&h, &[], &[], &[], &[], &[], &mut out).err(),
            Some(InfoError::Field("block_size"))
        );
        let mut r = record("x", RegionKind::Page, 10);
        r.generation = 0;
        let one: [RegionItem<'_>; 1] = [(r, &[1])];
        assert_eq!(
            encode(&header(), &[], &[], &[], &[], &one, &mut out).err(),
            Some(InfoError::Field("generation"))
        );
        let len = encoded_len(0, 0, 0, 0, &[]).unwrap() as u64;
        assert!(encode_within(&header(), &[], &[], &[], &[], &[], &mut out, len).is_ok());
        assert_eq!(
            encode_within(&header(), &[], &[], &[], &[], &[], &mut out, len - 1).err(),
            Some(InfoError::Length)
        );
        let mut short = vec![0u8; HEADER_LEN];
        assert_eq!(
            encode(&header(), &[], &[], &[], &[], &[], &mut short).err(),
            Some(InfoError::Buffer)
        );
    }

    /// A record longer than the limit is refused before anything of it is
    /// read (the limit lowered here; [`MAX_LEN`] in `decode`).
    #[test]
    fn test_a_record_over_the_limit_is_refused_unread() {
        let b = sample();
        let n = b.len() as u64;
        assert!(decode_within(&b, n).is_ok());
        assert_eq!(decode_within(&b, n - 1).err(), Some(InfoError::Length));
        let mut bad = b.clone();
        bad[0] ^= 1;
        assert_eq!(decode_within(&bad, n - 1).err(), Some(InfoError::Length));
    }

    /// The fuzz seeds (written by `fuzz/seeds.py` from FORMAT.md's layout,
    /// independently of this encoder) decode and encode back exactly, so
    /// the documented layout and this codec agree. In the repository they
    /// must be there; a packaged crate has no fuzz directory.
    #[test]
    fn test_the_fuzz_seeds_are_valid_records() {
        let fuzz = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fuzz");
        if !std::path::Path::new(fuzz).join("seeds.py").exists() {
            return;
        }
        for (seed, regions) in [("three-regions", 3), ("empty", 0)] {
            let path = format!("{fuzz}/corpus/export_info/{seed}");
            let b = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
            let info = decode(&b).unwrap_or_else(|e| panic!("{seed}: {e}"));
            assert_eq!(info.regions().count(), regions, "{seed}");
            let mut again = vec![0u8; b.len()];
            assert_eq!(encode_into(&info, &mut again).unwrap(), b.len());
            assert_eq!(again, b, "{seed}");
        }
    }

    #[test]
    fn test_a_short_buffer_is_an_overflow() {
        let b = sample();
        let info = decode(&b).unwrap();
        let mut short = vec![0u8; b.len() - 1];
        assert_eq!(
            encode_into(&info, &mut short).err(),
            Some(CodecError::Overflow)
        );
    }

    #[test]
    fn test_r2_is_the_slot_record() {
        let b = sample();
        assert_eq!(
            RegionRecord::decode(&b[R2..R2 + RECORD_LEN]).unwrap(),
            regions()[2]
        );
    }
}
