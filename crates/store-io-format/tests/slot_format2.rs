//! The format-2 slot header: `meta_seq` at byte 96, the exact header length,
//! the format each reader expects, and the header facts the tear judgement
//! reads from a slot that failed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use store_io_format::crc32c::crc32c;
use store_io_format::slot::{
    EncodeError, Expect, FORMAT_1, FORMAT_2, SlotFields, SlotIndex, SlotStatus, encode,
    header_facts, validate,
};

fn fields(meta_seq: u64) -> SlotFields {
    SlotFields {
        volume_uuid: [0xCD; 16],
        object_id: (1 << 61) | 7,
        generation: 5,
        owner_generation: 2,
        slot_offset: 16384,
        slot_index: SlotIndex::B,
        log2_slot_size: 12,
        prev_header_crc32c: 0x1234_5678,
        format_major: FORMAT_2,
        meta_seq,
        compat_flags: 0,
    }
}

fn expect(numbered: bool) -> Expect {
    let f = fields(1);
    Expect {
        volume_uuid: f.volume_uuid,
        object_id: f.object_id,
        slot_offset: f.slot_offset,
        slot_index: f.slot_index,
        log2_slot_size: 12,
        format_major: FORMAT_2,
        numbered,
    }
}

fn slot(f: &SlotFields, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; 4096];
    let _crc = encode(&mut out, f, payload).unwrap_or_else(|e| panic!("{e}"));
    out
}

/// Recomputes the header CRC after a test edits the header.
fn reseal(s: &mut [u8]) {
    s[8..12].fill(0);
    let len = usize::from(u16::from_le_bytes([s[12], s[13]]));
    let crc = crc32c(&s[..len]);
    s[8..12].copy_from_slice(&crc.to_le_bytes());
}

#[test]
fn test_format_2_round_trips_meta_seq() {
    let s = slot(&fields(41), b"page");
    assert_eq!(u64::from_le_bytes(s[96..104].try_into().unwrap()), 41);
    let SlotStatus::Valid(info) = validate(&s, &expect(true)) else {
        panic!("not valid");
    };
    assert_eq!(info.meta_seq, 41);
    assert_eq!(info.generation, 5);
    assert_eq!(info.payload(&s), b"page");
}

#[test]
fn test_each_reader_refuses_the_other_format() {
    let s = slot(&fields(1), b"x");
    let e1 = Expect {
        format_major: FORMAT_1,
        numbered: false,
        ..expect(true)
    };
    assert_eq!(
        validate(&s, &e1),
        SlotStatus::UnsupportedVersion { major: 2 }
    );
    let f1 = SlotFields {
        format_major: FORMAT_1,
        meta_seq: 0,
        ..fields(0)
    };
    let s1 = slot(&f1, b"x");
    assert_eq!(
        validate(&s1, &expect(true)),
        SlotStatus::UnsupportedVersion { major: 1 }
    );
    assert!(validate(&s1, &e1).is_valid(), "format 1 reads as before");
}

#[test]
fn test_meta_seq_must_match_whether_the_object_is_numbered() {
    // A numbered object (volume record, table page) needs meta_seq >= 1.
    let s = slot(&fields(0), b"x");
    assert_eq!(validate(&s, &expect(true)), SlotStatus::Malformed);
    assert!(validate(&s, &expect(false)).is_valid());
    // A region header carries 0.
    let s = slot(&fields(9), b"x");
    assert_eq!(validate(&s, &expect(false)), SlotStatus::Malformed);
    assert!(validate(&s, &expect(true)).is_valid());
}

#[test]
fn test_format_2_reserved_bytes_and_header_len_are_checked() {
    for at in [104usize, 115, 127] {
        let mut s = slot(&fields(3), b"x");
        s[at] = 1;
        reseal(&mut s);
        assert_eq!(validate(&s, &expect(true)), SlotStatus::Malformed, "{at}");
    }
    // A longer header: F1's range check passes, format 2 refuses it.
    let mut s = slot(&fields(3), b"");
    s[12..14].copy_from_slice(&256u16.to_le_bytes());
    reseal(&mut s);
    assert_eq!(validate(&s, &expect(true)), SlotStatus::Malformed);
}

#[test]
fn test_encode_refuses_unknown_formats_and_a_format_1_meta_seq() {
    let mut out = vec![0u8; 4096];
    let f = SlotFields {
        format_major: 3,
        ..fields(1)
    };
    assert_eq!(encode(&mut out, &f, b""), Err(EncodeError::Format));
    let f = SlotFields {
        format_major: FORMAT_1,
        ..fields(1)
    };
    assert_eq!(encode(&mut out, &f, b""), Err(EncodeError::Format));
}

#[test]
fn test_header_facts_of_valid_and_payload_damaged_slots_are_numbered() {
    let s = slot(&fields(12), b"payload");
    let facts = header_facts(&s, &expect(true)).unwrap();
    let n = facts.numbered.unwrap();
    assert_eq!((n.generation, n.meta_seq), (5, 12));
    assert_eq!(n.prev_header_crc32c, 0x1234_5678);
    assert_eq!(
        facts.stored_header_crc32c,
        u32::from_le_bytes(s[8..12].try_into().unwrap())
    );

    let mut torn = s.clone();
    torn[130] ^= 0xFF; // inside the payload
    assert_eq!(validate(&torn, &expect(true)), SlotStatus::BadPayloadCrc);
    assert_eq!(header_facts(&torn, &expect(true)), Some(facts));
}

#[test]
fn test_header_facts_of_a_damaged_header_give_only_the_stored_crc() {
    let s = slot(&fields(12), b"payload");
    let stored = u32::from_le_bytes(s[8..12].try_into().unwrap());
    let mut bad = s.clone();
    bad[60] ^= 1;
    assert_eq!(validate(&bad, &expect(true)), SlotStatus::BadHeaderCrc);
    let facts = header_facts(&bad, &expect(true)).unwrap();
    assert_eq!(facts.stored_header_crc32c, stored);
    assert_eq!(facts.numbered, None);
}

#[test]
fn test_header_facts_show_nothing_for_blank_or_foreign_bytes() {
    assert_eq!(header_facts(&[0u8; 4096], &expect(true)), None);
    let mut junk = vec![0x5Au8; 4096];
    junk[0] = b'X';
    assert_eq!(header_facts(&junk, &expect(true)), None);
    assert_eq!(header_facts(&[0u8; 100], &expect(true)), None);
    // Another volume's slot: a header, but not a numbered one here.
    let s = slot(&fields(4), b"x");
    let other = Expect {
        volume_uuid: [1; 16],
        ..expect(true)
    };
    assert_eq!(validate(&s, &other), SlotStatus::Foreign);
    assert_eq!(header_facts(&s, &other).unwrap().numbered, None);
}

#[test]
fn test_no_single_bit_flip_of_a_format_2_slot_is_valid() {
    let s = slot(&fields(77), b"some payload bytes");
    for byte in 0..(128 + 18) {
        for bit in 0..8 {
            let mut f = s.clone();
            f[byte] ^= 1 << bit;
            assert!(
                !validate(&f, &expect(true)).is_valid(),
                "byte {byte} bit {bit}"
            );
        }
    }
}

/// Format 1 is unchanged: bytes `[96, 128)` stay reserved whatever the
/// reader expects of numbering.
#[test]
fn test_format_1_keeps_bytes_96_to_128_reserved() {
    let f1 = SlotFields {
        format_major: FORMAT_1,
        meta_seq: 0,
        ..fields(0)
    };
    for at in [96usize, 103, 104, 127] {
        let mut s = slot(&f1, b"x");
        s[at] = 1;
        reseal(&mut s);
        for numbered in [false, true] {
            let e = Expect {
                format_major: FORMAT_1,
                numbered,
                ..expect(true)
            };
            assert_eq!(validate(&s, &e), SlotStatus::Malformed, "byte {at}");
        }
    }
}

/// A format no store-io knows is refused even when the caller expects it.
#[test]
fn test_an_unknown_format_is_refused_even_when_expected() {
    let mut s = slot(&fields(1), b"x");
    s[14..16].copy_from_slice(&3u16.to_le_bytes());
    reseal(&mut s);
    let e = Expect {
        format_major: 3,
        ..expect(true)
    };
    assert_eq!(
        validate(&s, &e),
        SlotStatus::UnsupportedVersion { major: 3 }
    );
}

/// Every failure past the header CRC still reports the stored CRC; only a
/// header that passes every check before the payload CRC is numbered.
#[test]
fn test_header_facts_of_each_failure_after_the_header_crc() {
    let good = slot(&fields(5), b"payload");
    let stored = |s: &[u8]| u32::from_le_bytes(s[8..12].try_into().unwrap());
    let mut cases: Vec<(&str, Vec<u8>, Expect)> = Vec::new();
    cases.push((
        "wrong object",
        good.clone(),
        Expect {
            object_id: 99,
            ..expect(true)
        },
    ));
    cases.push((
        "misplaced",
        good.clone(),
        Expect {
            slot_offset: 0,
            ..expect(true)
        },
    ));
    cases.push((
        "format 1 reader",
        good.clone(),
        Expect {
            format_major: FORMAT_1,
            numbered: false,
            ..expect(true)
        },
    ));
    let mut long_payload = good.clone();
    long_payload[72..76].copy_from_slice(&5000u32.to_le_bytes());
    reseal(&mut long_payload);
    cases.push(("payload_len past the slot", long_payload, expect(true)));
    cases.push(("meta_seq on a region header", good.clone(), expect(false)));
    let mut bad_len = good.clone();
    bad_len[12..14].copy_from_slice(&64u16.to_le_bytes());
    cases.push(("header_len below 128", bad_len, expect(true)));
    for (what, s, e) in &cases {
        assert!(!validate(s, e).is_valid(), "{what}");
        let facts = header_facts(s, e).unwrap();
        assert_eq!(facts.stored_header_crc32c, stored(s), "{what}");
        assert_eq!(facts.numbered, None, "{what}");
    }
}
