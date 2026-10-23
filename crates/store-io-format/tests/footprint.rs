//! A region's footprint (`footprint::check_region`): the rules of spec §9
//! step 3 between an entry, its region header and the container
//! (`.dev/design/format2-draft.md`), each on its own, then every bit of a
//! valid header flipped.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use store_io_format::footprint::{Footprint, check_region, footprint_len};
use store_io_format::meta::{FillPattern, MetaError, RegionKind, RegionName, RegionState};
use store_io_format::meta2::{
    Entry2, EntryLimits, Extent, RegionHeader2, decode_region_header, encode_region_header,
};

const B: u64 = 4096;
const TABLE_END: u64 = 64 * B;
const ID: u64 = (5 << 32) | 3;

fn limits() -> EntryLimits {
    EntryLimits {
        log2_block: 12,
        table_end: TABLE_END,
        file_len: 1 << 40,
        page_meta_seq: 100,
    }
}

/// An append region at the table's end: header blocks A and B, then
/// `capacity` bytes of span for its extents.
fn entry(kind: RegionKind, extent_count: u16, capacity: u64) -> Entry2 {
    let start = TABLE_END;
    Entry2 {
        seq: 3,
        kind,
        flags: 0,
        compat_flags: 0,
        name: RegionName::new("r").unwrap(),
        tag: 0,
        extent_count,
        header_a: start,
        header_b: start + B,
        data_size: 0,
        capacity,
        span_start: start,
        span_end: start + 2 * B + capacity,
        footprint_seq: 42,
    }
}

fn header(kind: RegionKind, extents: u16, object_len: u32) -> RegionHeader2 {
    RegionHeader2 {
        region_id: ID,
        kind,
        state: RegionState::Ready,
        fill: FillPattern::Zeros,
        flags: 0,
        compat_flags: 0,
        log2_block: 12,
        extent_count: extents,
        region_generation: 1,
        fill_key: [0; 16],
        object_len,
    }
}

fn encode(kind: RegionKind, extents: &[Extent], object: &[u8]) -> Vec<u8> {
    let h = header(kind, extents.len() as u16, object.len() as u32);
    let mut out = vec![0u8; 1 << 16];
    let n = encode_region_header(&mut out, &h, extents, object).unwrap();
    out.truncate(n);
    out
}

/// Checks `entry` against a header listing `extents`.
fn check(entry: &Entry2, extents: &[Extent]) -> Result<(Footprint, Vec<Extent>), MetaError> {
    let bytes = encode(entry.kind, extents, &[]);
    let view = decode_region_header(&bytes, 12, 1 << 40).unwrap();
    let mut scratch = vec![(0, 0); footprint_len(12)];
    let fp = check_region(ID, entry, &view, &limits(), &mut scratch)?;
    Ok((fp, scratch[..fp.ranges].to_vec()))
}

const DATA: u64 = TABLE_END + 2 * B;

#[test]
fn test_one_extent_after_its_headers_is_its_footprint() {
    let e = entry(RegionKind::Append, 1, 8 * B);
    let (fp, ranges) = check(&e, &[(DATA, 8 * B)]).unwrap();
    assert_eq!(fp.extra, None);
    assert_eq!(ranges, [(TABLE_END, B), (TABLE_END + B, B), (DATA, 8 * B)]);
}

#[test]
fn test_extents_are_sorted_and_may_touch() {
    let e = entry(RegionKind::Page, 3, 6 * B);
    // Listed out of order (data order is not file order), each touching the next.
    let (fp, ranges) = check(
        &e,
        &[(DATA + 4 * B, 2 * B), (DATA, 2 * B), (DATA + 2 * B, 2 * B)],
    )
    .unwrap();
    assert_eq!(fp.ranges, 5);
    assert!(ranges.windows(2).all(|w| w[0].0 + w[0].1 <= w[1].0));
    assert_eq!(ranges[2], (DATA, 2 * B));
}

#[test]
fn test_a_slot_region_occupies_its_header_blocks_only() {
    let e = entry(RegionKind::Slot, 0, 0);
    let bytes = encode(RegionKind::Slot, &[], b"object");
    let view = decode_region_header(&bytes, 12, 1 << 40).unwrap();
    let mut scratch = [(0, 0); 2];
    let fp = check_region(ID, &e, &view, &limits(), &mut scratch).unwrap();
    assert_eq!(
        fp,
        Footprint {
            ranges: 2,
            extra: None
        }
    );
    assert_eq!(scratch, [(TABLE_END, B), (TABLE_END + B, B)]);
}

#[test]
fn test_extents_must_sum_to_the_capacity() {
    let e = entry(RegionKind::Append, 2, 8 * B);
    assert_eq!(
        check(&e, &[(DATA, 4 * B), (DATA + 4 * B, 3 * B)]).err(),
        Some(MetaError::Field("capacity"))
    );
    assert_eq!(
        check(&e, &[(DATA, 4 * B), (DATA + 4 * B, 5 * B)]).err(),
        Some(MetaError::Field("extent")),
        "past the span"
    );
    assert!(check(&e, &[(DATA, 4 * B), (DATA + 4 * B, 4 * B)]).is_ok());
}

#[test]
fn test_extents_must_lie_inside_the_span() {
    let e = entry(RegionKind::Append, 1, 8 * B);
    // One block past the span's end, and one block before its start.
    assert_eq!(
        check(&e, &[(DATA + B, 8 * B)]).err(),
        Some(MetaError::Field("extent"))
    );
    let mut wide = e;
    wide.span_end += 8 * B;
    wide.header_a += 8 * B;
    wide.header_b += 8 * B;
    wide.span_start += B;
    assert_eq!(
        check(&wide, &[(TABLE_END, 8 * B)]).err(),
        Some(MetaError::Field("extent")),
        "before the span"
    );
}

#[test]
fn test_no_extent_lies_in_the_table_area() {
    // The span starts inside the table area; the headers do not (the entry
    // checks), but an extent there must be refused here.
    let mut e = entry(RegionKind::Append, 1, 8 * B);
    e.span_start = TABLE_END - 8 * B;
    e.header_a = DATA - 2 * B + 8 * B;
    e.header_b = e.header_a + B;
    e.span_end = e.header_b + B;
    assert_eq!(
        check(&e, &[(TABLE_END - 8 * B, 8 * B)]).err(),
        Some(MetaError::Field("extent"))
    );
}

#[test]
fn test_ranges_of_the_footprint_never_overlap() {
    let e = entry(RegionKind::Append, 2, 8 * B);
    assert_eq!(
        check(&e, &[(DATA, 4 * B), (DATA + 3 * B, 4 * B)]).err(),
        Some(MetaError::Overlap),
        "two extents"
    );
    let mut e1 = entry(RegionKind::Append, 1, 8 * B);
    e1.span_end += B;
    assert_eq!(
        check(&e1, &[(TABLE_END + B, 8 * B)]).err(),
        Some(MetaError::Overlap),
        "an extent over header B"
    );
}

#[test]
fn test_an_extra_extent_is_reported_and_not_counted() {
    let e = entry(RegionKind::Append, 1, 8 * B);
    // The extra extent overlaps the first and lies outside the span: it is
    // not part of the footprint, so neither matters here.
    let (fp, ranges) = check(&e, &[(DATA, 8 * B), (DATA, 64 * B)]).unwrap();
    assert_eq!(fp.extra, Some((DATA, 64 * B)));
    assert_eq!(ranges.len(), 3);
}

#[test]
fn test_header_rules_come_first() {
    let e = entry(RegionKind::Append, 1, 8 * B);
    let bytes = encode(RegionKind::Append, &[(DATA, 8 * B)], &[]);
    let view = decode_region_header(&bytes, 12, 1 << 40).unwrap();
    let mut scratch = vec![(0, 0); footprint_len(12)];
    assert_eq!(
        check_region(ID + 1, &e, &view, &limits(), &mut scratch).err(),
        Some(MetaError::Field("region_id"))
    );
    // Two extents more than the entry: the header rules refuse it.
    let two_more = encode(
        RegionKind::Append,
        &[(DATA, 8 * B), (DATA + 8 * B, B), (DATA + 9 * B, B)],
        &[],
    );
    let view = decode_region_header(&two_more, 12, 1 << 40).unwrap();
    assert_eq!(
        check_region(ID, &e, &view, &limits(), &mut scratch).err(),
        Some(MetaError::Field("extent_count"))
    );
}

/// A short scratch is a caller's bug: debug builds assert, release builds
/// refuse it without writing past it.
#[test]
#[cfg_attr(debug_assertions, should_panic(expected = "scratch too short"))]
fn test_short_scratch_is_refused() {
    let e = entry(RegionKind::Append, 1, 8 * B);
    let bytes = encode(RegionKind::Append, &[(DATA, 8 * B)], &[]);
    let view = decode_region_header(&bytes, 12, 1 << 40).unwrap();
    let mut scratch = [(0, 0); 2];
    assert_eq!(
        check_region(ID, &e, &view, &limits(), &mut scratch).err(),
        Some(MetaError::Length)
    );
}

/// The most extents a 64 KiB header lists, interleaved across the span:
/// sorted, not compared pairwise, and checked in full.
#[test]
fn test_the_most_extents_a_header_holds() {
    let big = 1u64 << 16;
    let lim = EntryLimits {
        log2_block: 16,
        table_end: 4 * big,
        file_len: 1 << 50,
        page_meta_seq: 100,
    };
    let n = store_io_format::meta2::max_extents(16);
    assert_eq!(n, 4084);
    let start = 4 * big;
    let data = start + 2 * big;
    // Every other block, listed backwards.
    let extents: Vec<Extent> = (0..n as u64)
        .rev()
        .map(|i| (data + 2 * i * big, big))
        .collect();
    let mut e = entry(RegionKind::Append, n as u16, n as u64 * big);
    e.header_a = start;
    e.header_b = start + big;
    e.span_start = start;
    e.span_end = data + 2 * n as u64 * big;
    let mut h = header(RegionKind::Append, n as u16, 0);
    h.log2_block = 16;
    let mut out = vec![0u8; 1 << 16];
    let len = encode_region_header(&mut out, &h, &extents, &[]).unwrap();
    let view = decode_region_header(&out[..len], 16, 1 << 50).unwrap();
    let mut scratch = vec![(0, 0); footprint_len(16)];
    let fp = check_region(ID, &e, &view, &lim, &mut scratch).unwrap();
    assert_eq!(fp.ranges, n + 2);
    // One overlap at the far end is still found.
    let mut bad = extents.clone();
    bad[0].0 = bad[1].0;
    let len = encode_region_header(&mut out, &h, &bad, &[]).unwrap();
    let view = decode_region_header(&out[..len], 16, 1 << 50).unwrap();
    assert!(check_region(ID, &e, &view, &lim, &mut scratch).is_err());
}

/// Every bit of a valid two-extent header flipped: decoding and the checks
/// never panic, and whatever passes still obeys every rule.
#[test]
fn test_every_bit_flip_of_a_valid_header() {
    let e = entry(RegionKind::Append, 2, 8 * B);
    let good = encode(
        RegionKind::Append,
        &[(DATA + 4 * B, 4 * B), (DATA, 4 * B)],
        &[],
    );
    let mut scratch = vec![(0, 0); footprint_len(12)];
    let mut passed = 0;
    for bit in 0..good.len() * 8 {
        let mut b = good.clone();
        b[bit / 8] ^= 1 << (bit % 8);
        let Ok(view) = decode_region_header(&b, 12, 1 << 40) else {
            continue;
        };
        let Ok(fp) = check_region(ID, &e, &view, &limits(), &mut scratch) else {
            continue;
        };
        passed += 1;
        let r = &scratch[..fp.ranges];
        assert!(
            r.windows(2).all(|w| w[0].0 + w[0].1 <= w[1].0),
            "bit {bit}: overlap"
        );
        assert!(
            r.contains(&(e.header_a, B)) && r.contains(&(e.header_b, B)),
            "bit {bit}: headers"
        );
        let mut listed: Vec<Extent> = view.extents().take(2).collect();
        listed.sort_unstable();
        assert_eq!(&r[2..], &listed[..], "bit {bit}: extents");
        let sum: u64 = r[..].iter().filter(|x| x.0 >= DATA).map(|x| x.1).sum();
        assert_eq!(sum, e.capacity, "bit {bit}: capacity");
        assert!(
            r.iter()
                .all(|x| x.0 >= e.span_start && x.0 + x.1 <= e.span_end),
            "bit {bit}: span"
        );
    }
    // Flips in fields the footprint does not read (compat flags, the
    // generation) still pass.
    assert!(passed > 0);
}

/// The bounds are inclusive where the spec allows: an extent may start at
/// the span's start and at the table's end, and end at the span's end.
#[test]
fn test_an_extent_may_sit_exactly_on_every_bound() {
    // Header blocks at the span's end, the extent first.
    let mut e = entry(RegionKind::Append, 1, 8 * B);
    e.header_a = TABLE_END + 8 * B;
    e.header_b = TABLE_END + 9 * B;
    let (fp, ranges) = check(&e, &[(TABLE_END, 8 * B)]).unwrap();
    assert_eq!(fp.ranges, 3);
    assert_eq!(ranges[0], (TABLE_END, 8 * B));
    // And the extent last, ending at the span's end.
    let e = entry(RegionKind::Append, 1, 8 * B);
    assert!(check(&e, &[(e.span_end - 8 * B, 8 * B)]).is_ok());
}

/// Header B below header A, and an extent over header A.
#[test]
fn test_both_header_blocks_are_in_the_footprint_in_either_order() {
    let mut e = entry(RegionKind::Append, 1, 8 * B);
    core::mem::swap(&mut e.header_a, &mut e.header_b);
    let (_, ranges) = check(&e, &[(DATA, 8 * B)]).unwrap();
    assert_eq!(ranges[..2], [(TABLE_END, B), (TABLE_END + B, B)]);
    let mut over_a = entry(RegionKind::Append, 1, 8 * B);
    over_a.header_a = TABLE_END + 8 * B;
    over_a.header_b = TABLE_END + 9 * B;
    over_a.span_end += B;
    assert_eq!(
        check(&over_a, &[(TABLE_END + B, 8 * B)]).err(),
        Some(MetaError::Overlap),
        "an extent over header A"
    );
}

/// A 512 B append region in a 4 KiB container: its header blocks are
/// container blocks, its data size a multiple of 512.
#[test]
fn test_a_512_byte_append_region_uses_container_header_blocks() {
    let mut e = entry(RegionKind::Append, 1, 8 * B);
    e.data_size = 512 * 3;
    let mut h = header(RegionKind::Append, 1, 0);
    h.log2_block = 9;
    let mut out = vec![0u8; 4096];
    let n = encode_region_header(&mut out, &h, &[(DATA, 8 * B)], &[]).unwrap();
    let view = decode_region_header(&out[..n], 12, 1 << 40).unwrap();
    let mut scratch = vec![(0, 0); footprint_len(12)];
    let fp = check_region(ID, &e, &view, &limits(), &mut scratch).unwrap();
    assert_eq!(
        scratch[..fp.ranges][..2],
        [(TABLE_END, B), (TABLE_END + B, B)]
    );
}

/// Extents whose lengths overflow a u64 when summed are refused for the
/// sum, before any overlap is looked at.
#[test]
fn test_a_sum_that_overflows_is_refused() {
    let big = 1u64 << 63;
    let mut e = entry(RegionKind::Append, 2, B);
    // What a wrapping sum would make of the two lengths.
    e.capacity = B;
    e.span_end = u64::MAX - (u64::MAX % B);
    let bytes = encode(RegionKind::Append, &[(DATA, big), (DATA, big + B)], &[]);
    let view = decode_region_header(&bytes, 12, u64::MAX).unwrap();
    let mut lim = limits();
    lim.file_len = u64::MAX;
    let mut scratch = vec![(0, 0); footprint_len(12)];
    assert_eq!(
        check_region(ID, &e, &view, &lim, &mut scratch).err(),
        Some(MetaError::Field("capacity"))
    );
}
