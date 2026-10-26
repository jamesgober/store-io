//! Format-2 payload codecs (`meta2`): the volume record, table pages and
//! entries, and region headers, against the specification's layouts and
//! rules (`.dev/design/format2-draft.md` §5, §6, §8).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use store_io_format::meta::{
    FillPattern, MetaError, RegionKind, RegionName, RegionState, VolumeMeta,
};
use store_io_format::meta2::{
    ENTRY2_LEN, Entry2, EntryLimits, FLAG_CAPPED, FLAG_UNWRITTEN, PAGE_HEADER_LEN, PageHeader,
    RawEntry, RegionHeader2, VOLUME_META2_LEN, VolumeMeta2, decode_page, decode_region_header,
    encode_page, encode_region_header, entries_per_page, max_extents, max_object,
};

const B: u64 = 4096;
const T_LOG2: u8 = 14;

fn volume() -> VolumeMeta2 {
    VolumeMeta2 {
        base: VolumeMeta {
            log2_block_size: 12,
            log2_table_slot_size: T_LOG2,
            device_index: 0,
            owner_generation: 3,
            table_offset: 2 * B,
            created_unix_s: 1_800_000_000,
            features: 0,
        },
        table_pages: 388,
        entries_per_page: 169,
        clean_close: true,
        flags: 0,
    }
}

fn enc_volume(v: &VolumeMeta2) -> Vec<u8> {
    let mut out = vec![0u8; VOLUME_META2_LEN];
    assert_eq!(v.encode(&mut out), Ok(VOLUME_META2_LEN));
    out
}

#[test]
fn test_derived_numbers_match_the_specification() {
    let e: Vec<u16> = (12..=16).map(entries_per_page).collect();
    assert_eq!(e, [41, 83, 169, 339, 681]);
    assert_eq!(max_extents(12), 244);
    assert_eq!(volume().table_end(), 2 * B + 2 * 388 * 16384);
    assert_eq!(65_536u32.div_ceil(169), 388);
}

#[test]
fn test_volume_record_round_trips_and_is_exactly_64_bytes() {
    let v = volume();
    let bytes = enc_volume(&v);
    assert_eq!(VolumeMeta2::decode(&bytes), Ok(v));
    let mut longer = bytes.clone();
    longer.push(0);
    assert_eq!(VolumeMeta2::decode(&longer), Err(MetaError::Length));
    for n in 0..VOLUME_META2_LEN {
        assert!(VolumeMeta2::decode(&bytes[..n]).is_err(), "prefix {n}");
    }
}

#[test]
fn test_volume_record_field_rules() {
    let mut bytes = enc_volume(&volume());
    bytes[56] = 1; // flags
    assert_eq!(VolumeMeta2::decode(&bytes), Err(MetaError::Field("flags")));
    for (at, what) in [(55usize, "reserved"), (60, "reserved")] {
        let mut b = enc_volume(&volume());
        b[at] = 1;
        assert_eq!(VolumeMeta2::decode(&b), Err(MetaError::Field(what)), "{at}");
    }
    let bad = |f: &dyn Fn(&mut VolumeMeta2)| {
        let mut v = volume();
        f(&mut v);
        VolumeMeta2::decode(&enc_volume(&v))
    };
    assert_eq!(
        bad(&|v| v.table_pages = 0),
        Err(MetaError::Field("table_pages"))
    );
    assert_eq!(
        bad(&|v| v.table_pages = 1 << 30),
        Err(MetaError::Field("table_pages"))
    );
    assert_eq!(
        bad(&|v| v.entries_per_page = 168),
        Err(MetaError::Field("entries_per_page"))
    );
    assert_eq!(
        bad(&|v| v.base.table_offset = 3 * B),
        Err(MetaError::Field("table_offset"))
    );
    let mut b = enc_volume(&volume());
    b[54] = 2;
    assert_eq!(
        VolumeMeta2::decode(&b),
        Err(MetaError::Field("clean_close"))
    );
    // Format 1's newer-record marker still comes first.
    assert_eq!(
        bad(&|v| v.base.features = 1),
        Err(MetaError::Field("features"))
    );
}

fn limits() -> EntryLimits {
    EntryLimits {
        log2_block: 12,
        table_end: volume().table_end(),
        file_len: 1 << 40,
        page_meta_seq: 100,
    }
}

fn entry(seq: u32) -> Entry2 {
    let start = volume().table_end() + u64::from(seq) * (1 << 20);
    Entry2 {
        seq,
        kind: RegionKind::Append,
        flags: FLAG_CAPPED,
        compat_flags: 0x80,
        name: RegionName::new(&format!("r{seq}")).unwrap(),
        tag: 7,
        extent_count: 1,
        header_a: start,
        header_b: start + B,
        data_size: 64 * 1024,
        capacity: 128 * 1024,
        span_start: start,
        span_end: start + 2 * B + 128 * 1024,
        footprint_seq: 42,
    }
}

fn enc_page(h: &PageHeader, entries: &[RawEntry]) -> Vec<u8> {
    let mut out = vec![0u8; PAGE_HEADER_LEN + entries.len() * ENTRY2_LEN];
    assert_eq!(encode_page(&mut out, h, entries), Ok(out.len()));
    out
}

fn header(count: u16) -> PageHeader {
    PageHeader {
        page_index: 5,
        next_seq: 10,
        count,
    }
}

#[test]
fn test_a_page_round_trips_and_its_entries_check() {
    let raws = [entry(0).to_raw(), entry(3).to_raw()];
    let bytes = enc_page(&header(2), &raws);
    let page = decode_page(&bytes, 5, 169).unwrap();
    assert_eq!(page.header, header(2));
    let back: Vec<RawEntry> = page.entries().collect();
    assert_eq!(back, raws);
    assert_eq!(back[1].check(&limits()), Ok(entry(3)));
    for n in 0..bytes.len() {
        assert!(decode_page(&bytes[..n], 5, 169).is_err(), "prefix {n}");
    }
}

/// A page holding more entries than it has ever handed out sequence
/// numbers (`count > next_seq`) is lost: an entry's sequence number would
/// have to repeat.
#[test]
fn test_a_page_with_more_entries_than_its_next_seq_is_lost() {
    let raws = [entry(0).to_raw(), entry(1).to_raw()];
    let h = PageHeader {
        page_index: 5,
        next_seq: 1,
        count: 2,
    };
    let mut out = vec![0u8; PAGE_HEADER_LEN + ENTRY2_LEN * raws.len()];
    let bytes = match encode_page(&mut out, &h, &raws) {
        Ok(n) => out[..n].to_vec(),
        // The encoder refuses it too: craft it from a valid page.
        Err(_) => {
            let mut b = enc_page(&header(2), &raws);
            let at = b.windows(4).position(|w| w == 10u32.to_le_bytes()).unwrap();
            b[at..at + 4].copy_from_slice(&1u32.to_le_bytes());
            b
        }
    };
    assert_eq!(
        decode_page(&bytes, 5, 169).err(),
        Some(MetaError::Field("count"))
    );
}

#[test]
fn test_page_rules_lose_the_page() {
    let raws = [entry(0).to_raw(), entry(1).to_raw()];
    let ok = enc_page(&header(2), &raws);
    assert_eq!(
        decode_page(&ok, 6, 169).err(),
        Some(MetaError::Field("page_index"))
    );
    assert_eq!(
        decode_page(&ok, 5, 1).err(),
        Some(MetaError::Field("count"))
    );
    let mut b = ok.clone();
    b[12] = 1; // page header reserved
    assert_eq!(
        decode_page(&b, 5, 169).err(),
        Some(MetaError::Field("reserved"))
    );
    let mut b = ok.clone();
    b.extend_from_slice(&[0; 8]);
    assert_eq!(decode_page(&b, 5, 169).err(), Some(MetaError::Length));
    let few = PageHeader {
        next_seq: 1,
        ..header(2)
    };
    assert_eq!(
        decode_page(&enc_page(&few, &raws), 5, 169).err(),
        Some(MetaError::Field("count"))
    );
    let dup = [entry(1).to_raw(), entry(1).to_raw()];
    assert_eq!(
        decode_page(&enc_page(&header(2), &dup), 5, 169).err(),
        Some(MetaError::Duplicate)
    );
    let high = [entry(0).to_raw(), entry(10).to_raw()];
    assert_eq!(
        decode_page(&enc_page(&header(2), &high), 5, 169).err(),
        Some(MetaError::Field("seq"))
    );
    let mut b = ok.clone();
    b[PAGE_HEADER_LEN + 38] = 1; // an entry's reserved bytes
    assert_eq!(
        decode_page(&b, 5, 169).err(),
        Some(MetaError::Field("reserved"))
    );
}

#[test]
fn test_entry_rules_make_only_the_region_unavailable() {
    let lim = limits();
    let check = |f: &dyn Fn(&mut RawEntry)| {
        let mut r = entry(2).to_raw();
        f(&mut r);
        r.check(&lim)
    };
    assert_eq!(check(&|r| r.kind = 0), Err(MetaError::Field("kind")));
    assert_eq!(check(&|r| r.kind = 4), Err(MetaError::Field("kind")));
    assert_eq!(check(&|r| r.state = 2), Err(MetaError::Field("state")));
    assert_eq!(check(&|r| r.flags = 1 << 7), Err(MetaError::Field("flags")));
    assert_eq!(check(&|r| r.name = [0; 24]), Err(MetaError::Field("name")));
    assert_eq!(
        check(&|r| r.data_size = r.capacity + 512),
        Err(MetaError::Field("data_size"))
    );
    assert_eq!(
        check(&|r| r.span_end = r.span_start),
        Err(MetaError::Field("span"))
    );
    assert_eq!(
        check(&|r| r.span_end = (1 << 40) + B),
        Err(MetaError::Field("span"))
    );
    assert_eq!(check(&|r| r.span_start += 1), Err(MetaError::Unaligned));
    assert_eq!(
        check(&|r| r.header_b = r.header_a),
        Err(MetaError::Field("header"))
    );
    assert_eq!(
        check(&|r| r.header_a = r.span_end),
        Err(MetaError::Field("header"))
    );
    assert_eq!(check(&|r| r.header_a += 512), Err(MetaError::Unaligned));
    assert_eq!(
        check(&|r| r.header_a = u64::MAX - 4095),
        Err(MetaError::Field("header"))
    );
    assert_eq!(
        check(&|r| r.footprint_seq = 0),
        Err(MetaError::Field("footprint_seq"))
    );
    assert_eq!(
        check(&|r| r.footprint_seq = 101),
        Err(MetaError::Field("footprint_seq"))
    );
    assert_eq!(
        check(&|r| r.extent_count = 0),
        Err(MetaError::Field("extent_count"))
    );
    assert_eq!(check(&|r| r.capacity += 100), Err(MetaError::Unaligned));
    assert_eq!(
        check(&|r| {
            r.kind = 3;
            r.extent_count = 0;
        }),
        Err(MetaError::Field("slot"))
    );
    // Headers inside the table area.
    let lim2 = EntryLimits {
        table_end: entry(2).span_start + B,
        ..lim
    };
    assert_eq!(
        entry(2).to_raw().check(&lim2),
        Err(MetaError::Field("header"))
    );
}

#[test]
fn test_an_unavailable_entry_reserves_its_span_clamped() {
    let lim = limits();
    let e = entry(1).to_raw();
    assert_eq!(e.reserved_span(&lim), Some((e.span_start, e.span_end)));
    let mut wild = e;
    wild.span_start = 0;
    wild.span_end = u64::MAX;
    assert_eq!(
        wild.reserved_span(&lim),
        Some((lim.table_end, lim.file_len))
    );
    let mut inside = e;
    inside.span_start = 0;
    inside.span_end = B;
    assert_eq!(inside.reserved_span(&lim), None);
}

fn region_header(kind: RegionKind, extents: u16, object_len: u32) -> RegionHeader2 {
    RegionHeader2 {
        region_id: (5 << 32) | 3,
        kind,
        state: RegionState::Ready,
        fill: FillPattern::KeyedV1,
        flags: 0,
        compat_flags: 0x40,
        log2_block: 12,
        extent_count: extents,
        region_generation: 1,
        fill_key: [9; 16],
        object_len,
    }
}

fn enc_header(h: &RegionHeader2, extents: &[(u64, u64)], object: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; 4096 - 128];
    let n = encode_region_header(&mut out, h, extents, object).unwrap();
    out.truncate(n);
    out
}

#[test]
fn test_region_headers_round_trip_extents_and_objects() {
    let ext = [(1 << 30, 1 << 20), (1 << 31, 2 << 20)];
    let h = region_header(RegionKind::Page, 2, 0);
    let bytes = enc_header(&h, &ext, &[]);
    let v = decode_region_header(&bytes, 12, 1 << 40).unwrap();
    assert_eq!(v.header, h);
    assert_eq!(v.extents().collect::<Vec<_>>(), ext);
    let s = region_header(RegionKind::Slot, 0, 5);
    let bytes = enc_header(&s, &[], b"hello");
    let v = decode_region_header(&bytes, 12, 1 << 40).unwrap();
    assert_eq!(v.object(), b"hello");
    assert_eq!(v.extents().count(), 0);
    for n in 0..bytes.len() {
        assert!(
            decode_region_header(&bytes[..n], 12, 1 << 40).is_err(),
            "prefix {n}"
        );
    }
}

#[test]
fn test_region_header_rules() {
    let ext = [(1u64 << 30, 1u64 << 20)];
    let page = region_header(RegionKind::Page, 1, 0);
    let dec = |b: &[u8]| decode_region_header(b, 12, 1 << 40).err();
    let mut b = enc_header(&page, &ext, &[]);
    b[50] = 1; // reserved
    assert_eq!(dec(&b), Some(MetaError::Field("reserved")));
    let with = |f: &dyn Fn(&mut RegionHeader2)| {
        let mut h = page;
        f(&mut h);
        enc_header(&h, &ext, &[])
    };
    assert_eq!(
        dec(&with(&|h| h.flags = 1 << 6)),
        Some(MetaError::Field("flags"))
    );
    assert_eq!(
        dec(&with(&|h| h.log2_block = 13)),
        Some(MetaError::Field("log2_block"))
    );
    assert_eq!(
        dec(&with(&|h| h.log2_block = 9)),
        Some(MetaError::Field("log2_block"))
    );
    assert!(
        decode_region_header(
            &enc_header(
                &RegionHeader2 {
                    kind: RegionKind::Append,
                    log2_block: 9,
                    ..page
                },
                &ext,
                &[]
            ),
            12,
            1 << 40
        )
        .is_ok(),
        "512 B blocks for append regions"
    );
    assert_eq!(
        dec(&with(&|h| h.region_generation = 0)),
        Some(MetaError::Field("region_generation"))
    );
    assert_eq!(
        dec(&with(&|h| h.fill = FillPattern::Zeros)),
        Some(MetaError::Field("fill_key"))
    );
    assert_eq!(
        dec(&enc_header(&page, &[(1 << 30, 1000)], &[])),
        Some(MetaError::Unaligned)
    );
    assert_eq!(
        dec(&enc_header(&page, &[(1 << 30, 0)], &[])),
        Some(MetaError::Unaligned)
    );
    assert_eq!(
        decode_region_header(&enc_header(&page, &ext, &[]), 12, 1 << 30).err(),
        Some(MetaError::Field("extent"))
    );
    assert_eq!(
        dec(&enc_header(&page, &[(u64::MAX - 4095, 1 << 20)], &[])),
        Some(MetaError::Field("extent"))
    );
    let mut b = enc_header(&page, &ext, &[]);
    b.extend_from_slice(&[0; 16]);
    assert_eq!(dec(&b), Some(MetaError::Length));
    // A slot region lists no extents; another region carries no object.
    let mut slot_ext = enc_header(&region_header(RegionKind::Slot, 0, 0), &[], &[]);
    slot_ext[14] = 1;
    assert_eq!(dec(&slot_ext), Some(MetaError::Field("extent_count")));
    let mut obj = enc_header(&page, &ext, &[]);
    obj[40] = 1;
    assert_eq!(dec(&obj), Some(MetaError::Field("extent_count")));
}

#[test]
fn test_encode_refuses_disagreeing_counts() {
    let mut out = vec![0u8; 4096];
    let h = region_header(RegionKind::Page, 2, 0);
    assert_eq!(
        encode_region_header(&mut out, &h, &[(0, 4096)], &[]),
        Err(MetaError::Field("extent_count"))
    );
}

/// Newer-format markers win over every other field, so a newer record is
/// never mistaken for damage.
#[test]
fn test_newer_format_markers_are_checked_before_any_other_field() {
    let mut b = enc_volume(&volume());
    b[56] = 1; // flags
    b[0] = 20; // and an impossible log2_block_size
    b[4] = 1; // and a non-zero F1 reserved word
    assert_eq!(VolumeMeta2::decode(&b), Err(MetaError::Field("flags")));
    let mut b = enc_volume(&volume());
    b[32] = 1; // features
    b[56] = 1;
    b[0] = 20;
    assert_eq!(VolumeMeta2::decode(&b), Err(MetaError::Field("features")));
    let v = volume();
    assert_eq!(v.check_file_len(v.table_end()), Ok(()));
    assert_eq!(
        v.check_file_len(v.table_end() - 1),
        Err(MetaError::Field("table_pages"))
    );
}

/// The rules a deletion would otherwise not show.
#[test]
fn test_more_entry_rules() {
    let lim = limits();
    let check = |f: &dyn Fn(&mut RawEntry)| {
        let mut r = entry(2).to_raw();
        f(&mut r);
        r.check(&lim)
    };
    // The capacity fits the span (it is the extents' sum, and they lie in
    // it): one block more is refused, a capacity filling the span is not.
    assert_eq!(
        check(&|r| r.capacity = r.span_end - r.span_start + B),
        Err(MetaError::Field("capacity"))
    );
    assert!(check(&|r| r.capacity = r.span_end - r.span_start).is_ok());
    assert_eq!(
        check(&|r| {
            r.span_start = r.header_a + B;
            r.header_b = r.header_a + 2 * B;
        }),
        Err(MetaError::Field("header")),
        "header_a before the span"
    );
    assert_eq!(check(&|r| r.span_end += 512), Err(MetaError::Unaligned));
    assert_eq!(
        check(&|r| r.capacity = 0),
        Err(MetaError::Field("data_size"))
    );
    assert_eq!(
        check(&|r| {
            r.capacity = 0;
            r.data_size = 0;
        }),
        Err(MetaError::Field("capacity"))
    );
    assert_eq!(check(&|r| r.data_size = 1000), Err(MetaError::Unaligned));
    // Page regions need whole blocks; append regions 512 bytes.
    assert_eq!(
        check(&|r| {
            r.kind = 2;
            r.data_size = 512;
        }),
        Err(MetaError::Unaligned)
    );
    assert!(check(&|r| r.data_size = 512).is_ok());
    assert!(check(&|r| r.flags = FLAG_UNWRITTEN).is_ok());
    // Flags first: an unknown bit wins over a bad kind.
    assert_eq!(
        check(&|r| {
            r.flags = 1 << 7;
            r.kind = 9;
        }),
        Err(MetaError::Field("flags"))
    );
    // Slot entries: no extents, no sizes; a valid one passes.
    let slot = |f: &dyn Fn(&mut RawEntry)| {
        check(&|r| {
            r.kind = 3;
            r.extent_count = 0;
            r.capacity = 0;
            r.data_size = 0;
            f(r);
        })
    };
    assert!(slot(&|_| {}).is_ok());
    assert_eq!(slot(&|r| r.extent_count = 1), Err(MetaError::Field("slot")));
    assert_eq!(
        slot(&|r| {
            r.capacity = 4096;
            r.data_size = 4096;
        }),
        Err(MetaError::Field("slot"))
    );
    let bad_geometry = EntryLimits {
        log2_block: 64,
        ..lim
    };
    assert_eq!(
        entry(2).to_raw().check(&bad_geometry),
        Err(MetaError::Field("log2_block"))
    );
}

/// A bad entry leaves its page standing: only that region is unavailable.
#[test]
fn test_a_page_with_a_bad_entry_still_decodes() {
    let mut bad = entry(1).to_raw();
    bad.kind = 9;
    let raws = [entry(0).to_raw(), bad];
    let bytes = enc_page(&header(2), &raws);
    let page = decode_page(&bytes, 5, 169).unwrap();
    let checked: Vec<bool> = page.entries().map(|r| r.check(&limits()).is_ok()).collect();
    assert_eq!(checked, [true, false]);
    // encode_page refuses a header whose count disagrees.
    let mut out = vec![0u8; 4096];
    assert_eq!(
        encode_page(&mut out, &header(3), &raws),
        Err(MetaError::Field("count"))
    );
}

#[test]
fn test_reserved_spans_are_whole_blocks() {
    let lim = limits();
    let mut e = entry(1).to_raw();
    e.span_start += 100;
    e.span_end -= 100;
    let (a, b) = e.reserved_span(&lim).unwrap();
    assert_eq!((a % B, b % B), (0, 0));
    assert!(a <= e.span_start && b >= e.span_end);
}

#[test]
fn test_entry_and_header_agree() {
    let e = entry(3);
    let id = (5u64 << 32) | 3;
    let h = RegionHeader2 {
        kind: RegionKind::Append,
        flags: FLAG_CAPPED,
        extent_count: 1,
        ..region_header(RegionKind::Append, 1, 0)
    };
    assert_eq!(e.check_header(id, &h), Ok(()));
    let one_more = RegionHeader2 {
        extent_count: 2,
        ..h
    };
    assert_eq!(e.check_header(id, &one_more), Ok(()), "a crashed growth");
    let two_more = RegionHeader2 {
        extent_count: 3,
        ..h
    };
    assert_eq!(
        e.check_header(id, &two_more),
        Err(MetaError::Field("extent_count"))
    );
    let fewer = RegionHeader2 {
        extent_count: 0,
        ..h
    };
    assert_eq!(
        e.check_header(id, &fewer),
        Err(MetaError::Field("extent_count"))
    );
    assert_eq!(
        e.check_header(id + 1, &h),
        Err(MetaError::Field("region_id"))
    );
    let page = RegionHeader2 {
        kind: RegionKind::Page,
        ..h
    };
    assert_eq!(e.check_header(id, &page), Err(MetaError::Field("kind")));
    let flags = RegionHeader2 { flags: 0, ..h };
    assert_eq!(e.check_header(id, &flags), Err(MetaError::Field("flags")));
    let released = RegionHeader2 {
        state: RegionState::Released,
        ..h
    };
    assert_eq!(
        e.check_header(id, &released),
        Err(MetaError::Field("state"))
    );
    let small = Entry2 {
        data_size: 512,
        ..e
    };
    assert_eq!(small.check_header(id, &h), Err(MetaError::Unaligned));
    let b512 = RegionHeader2 { log2_block: 9, ..h };
    assert_eq!(small.check_header(id, &b512), Ok(()));
}

#[test]
fn test_more_region_header_rules() {
    let page = region_header(RegionKind::Page, 1, 0);
    let dec = |b: &[u8]| decode_region_header(b, 12, 1 << 40).err();
    // Extents: aligned offsets, at most max_extents, every prefix refused.
    assert_eq!(
        dec(&enc_header(&page, &[((1 << 30) + 512, 1 << 20)], &[])),
        Some(MetaError::Unaligned)
    );
    let n = max_extents(12) + 1;
    let many: Vec<(u64, u64)> = (0..n as u64).map(|i| ((i + 1) << 20, 4096)).collect();
    let big = RegionHeader2 {
        extent_count: n as u16,
        ..page
    };
    let mut out = vec![0u8; 64 + 16 * n];
    let len = encode_region_header(&mut out, &big, &many, &[]).unwrap();
    assert_eq!(dec(&out[..len]), Some(MetaError::Field("extent_count")));
    let two = enc_header(
        &region_header(RegionKind::Page, 2, 0),
        &[(1 << 30, 4096), (1 << 31, 4096)],
        &[],
    );
    for k in 0..two.len() {
        assert!(dec(&two[..k]).is_some(), "prefix {k}");
    }
    // A non-slot header listing no extents.
    let mut none = enc_header(&page, &[(1 << 30, 4096)], &[]);
    none[14] = 0;
    none.truncate(64);
    assert_eq!(dec(&none), Some(MetaError::Field("extent_count")));
    // The largest slot object fits, one byte more does not.
    let max = max_object(12);
    let fits = enc_header(
        &region_header(RegionKind::Slot, 0, max as u32),
        &[],
        &vec![7; max],
    );
    assert!(dec(&fits).is_none());
    let mut over = fits.clone();
    over.push(7);
    over[40..44].copy_from_slice(&(max as u32 + 1).to_le_bytes());
    assert_eq!(dec(&over), Some(MetaError::Length));
    // Flags first; an impossible geometry is refused.
    let mut newer = enc_header(&page, &[(1 << 30, 4096)], &[]);
    newer[11] = 1 << 7;
    newer[8] = 9;
    assert_eq!(dec(&newer), Some(MetaError::Field("flags")));
    assert_eq!(
        decode_region_header(&enc_header(&page, &[(1 << 30, 4096)], &[]), 70, 1 << 40).err(),
        Some(MetaError::Field("log2_block"))
    );
    // encode refuses a slot with extents and a page with an object.
    let mut out = vec![0u8; 4096];
    let slot = region_header(RegionKind::Slot, 1, 0);
    assert_eq!(
        encode_region_header(&mut out, &slot, &[(0, 4096)], &[]),
        Err(MetaError::Field("extent_count"))
    );
    let obj = RegionHeader2 {
        object_len: 1,
        ..page
    };
    assert_eq!(
        encode_region_header(&mut out, &obj, &[(1 << 30, 4096)], b"x"),
        Err(MetaError::Field("object_len"))
    );
}

/// The last rules a deletion would not show.
#[test]
fn test_remaining_rules() {
    let lim = limits();
    // An append entry's capacity is whole blocks, even if its data is 512 B.
    let mut r = entry(2).to_raw();
    r.capacity = 128 * 1024 + 512;
    r.data_size = 512;
    assert_eq!(r.check(&lim), Err(MetaError::Unaligned));
    // A slot entry with capacity.
    let mut r = entry(2).to_raw();
    r.kind = 3;
    r.extent_count = 0;
    r.data_size = 0;
    assert_eq!(r.check(&lim), Err(MetaError::Field("slot")));
    // A slot region's header is never one extent ahead.
    let slot = r.check(&lim).err().map(|_| {
        let mut ok = r;
        ok.capacity = 0;
        ok.check(&lim).unwrap()
    });
    let slot = slot.unwrap();
    let id = (5u64 << 32) | 2;
    let h = RegionHeader2 {
        region_id: id,
        flags: FLAG_CAPPED,
        ..region_header(RegionKind::Slot, 0, 0)
    };
    assert_eq!(slot.check_header(id, &h), Ok(()));
    let ahead = RegionHeader2 {
        extent_count: 1,
        ..h
    };
    assert_eq!(
        slot.check_header(id, &ahead),
        Err(MetaError::Field("extent_count"))
    );
    // Both reserved bytes of an entry.
    let raws = [entry(0).to_raw()];
    let mut b = enc_page(&header(1), &raws);
    b[PAGE_HEADER_LEN + 39] = 1;
    assert_eq!(
        decode_page(&b, 5, 169).err(),
        Some(MetaError::Field("reserved"))
    );
    // The geometry guard of the header decoder, below the range too.
    let page = region_header(RegionKind::Page, 1, 0);
    assert_eq!(
        decode_region_header(&enc_header(&page, &[(1 << 30, 4096)], &[]), 11, 1 << 40).err(),
        Some(MetaError::Field("log2_block"))
    );
}
