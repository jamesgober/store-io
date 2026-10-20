//! The format-2 tear judgement on fuzz-chosen slots, against a brute-force
//! reference written straight from the specification (§7).
//!
//! Each of 1 to 8 pairs (pair 0 the volume record) gets two slots built
//! from a recipe in the input: blank, valid, torn in its payload or header,
//! a header CRC field of zero, unreadable, or misplaced, with any
//! generation and `meta_seq`; slot B may be chained to slot A (the next
//! generation, naming A's real header CRC), which is how a real torn write
//! looks. Checked:
//! - the heal, the refusal and whether the open may write equal the
//!   reference's;
//! - a pair is Lost or Refused exactly when it has no winner, and a winner
//!   is valid;
//! - at most one slot is in flight, it is the one healed, under a
//!   Confirmed winner; a pair with a damaged slot is never Confirmed.

#![no_main]

use libfuzzer_sys::fuzz_target;
use store_io_format::judge::{PairRead, PairVerdict, SlotRead, SlotVerdict, Standing, judge};
use store_io_format::slot::{
    Expect, FORMAT_2, SlotFields, SlotIndex, SlotInfo, SlotStatus, encode, header_facts, validate,
};

const VOLUME: [u8; 16] = [5; 16];

fn expect(object: u64, slot: usize) -> Expect {
    Expect {
        volume_uuid: VOLUME,
        object_id: object,
        slot_offset: (object % 64) * 1024 + slot as u64 * 512,
        slot_index: if slot == 0 {
            SlotIndex::A
        } else {
            SlotIndex::B
        },
        log2_slot_size: 9,
        format_major: FORMAT_2,
        numbered: true,
    }
}

/// Builds one slot from its recipe; `chain` is slot A's (generation,
/// header CRC) when slot B should follow it.
fn build(
    object: u64,
    slot: usize,
    r: &[u8],
    chain: Option<(u64, u32)>,
) -> (SlotRead, Option<(u64, u32)>) {
    let byte = |i: usize| r.get(i).copied().unwrap_or(0);
    let e = expect(object, slot);
    let kind = byte(0) % 8;
    if kind == 0 {
        let blank = [0u8; 512];
        return (
            SlotRead {
                status: validate(&blank, &e),
                facts: header_facts(&blank, &e),
            },
            None,
        );
    }
    if kind == 1 {
        return (
            SlotRead {
                status: SlotStatus::ReadError,
                facts: None,
            },
            None,
        );
    }
    let chained = byte(5) & 1 == 1;
    let (generation, prev) = match chain {
        Some((g, crc)) if chained => (g + 1, crc),
        _ => (u64::from(byte(1) % 6) + 1, u32::from(byte(2))),
    };
    let f = SlotFields {
        volume_uuid: VOLUME,
        object_id: object,
        generation,
        owner_generation: 1,
        slot_offset: if kind == 6 {
            e.slot_offset + 512
        } else {
            e.slot_offset
        },
        slot_index: e.slot_index,
        log2_slot_size: 9,
        prev_header_crc32c: prev,
        format_major: FORMAT_2,
        meta_seq: u64::from(byte(4) % 12) + 1,
    };
    let mut s = [0u8; 512];
    let Ok(crc) = encode(&mut s, &f, b"payload") else {
        return (
            SlotRead {
                status: SlotStatus::ReadError,
                facts: None,
            },
            None,
        );
    };
    match kind {
        3 => s[130] ^= 0xFF,
        4 => s[60] ^= 0xFF,
        5 => s[8..12].fill(0),
        7 => {
            s[16] = 1; // an unknown incompatible flag
            s[8..12].fill(0);
            let c = store_io_format::crc32c::crc32c(&s[..128]);
            s[8..12].copy_from_slice(&c.to_le_bytes());
        }
        _ => {}
    }
    (
        SlotRead {
            status: validate(&s, &e),
            facts: header_facts(&s, &e),
        },
        Some((generation, crc)),
    )
}

/// What the reference makes of a pair before the store-wide rules.
#[derive(Clone, Copy, PartialEq)]
enum Base {
    Refused,
    Lost,
    Fine,
    Damaged,
    /// A candidate: its slot and its `meta_seq` if readable.
    Candidate(usize, Option<u64>),
}

fn info(s: &SlotRead) -> Option<SlotInfo> {
    if let SlotStatus::Valid(i) = s.status {
        Some(i)
    } else {
        None
    }
}

fn base(p: &PairRead) -> Base {
    let refused = |s: &SlotRead| {
        matches!(
            s.status,
            SlotStatus::UnsupportedVersion { .. } | SlotStatus::Incompatible { .. }
        )
    };
    if refused(&p.a) || refused(&p.b) {
        return Base::Refused;
    }
    match (info(&p.a), info(&p.b)) {
        (None, None) => Base::Lost,
        (Some(a), Some(b)) => {
            if a.generation == b.generation {
                Base::Lost
            } else {
                Base::Fine
            }
        }
        (Some(w), None) | (None, Some(w)) => {
            let o_is_b = info(&p.a).is_some();
            let (o, oi) = if o_is_b { (&p.b, 1) } else { (&p.a, 0) };
            match o.status {
                SlotStatus::Blank => {
                    if w.generation == 1 && o_is_b {
                        Base::Fine
                    } else {
                        Base::Damaged
                    }
                }
                SlotStatus::BadPayloadCrc => match o.facts.and_then(|f| f.numbered) {
                    Some(n)
                        if n.generation == w.generation + 1
                            && n.prev_header_crc32c == w.header_crc32c =>
                    {
                        Base::Candidate(oi, Some(n.meta_seq))
                    }
                    Some(n) if n.generation < w.generation => Base::Fine,
                    _ => Base::Damaged,
                },
                SlotStatus::BadHeaderCrc => match o.facts {
                    Some(f)
                        if w.generation >= 2 && f.stored_header_crc32c == w.prev_header_crc32c =>
                    {
                        Base::Fine
                    }
                    Some(_) => Base::Candidate(oi, None),
                    None => Base::Damaged,
                },
                _ => Base::Damaged,
            }
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&head, rest)) = data.split_first() else {
        return;
    };
    let n = usize::from(head % 8) + 1;
    let clean_close = head & 0x80 != 0;
    let mut pairs = Vec::with_capacity(n);
    for i in 0..n {
        let object = if i == 0 { 0 } else { (1 << 61) | i as u64 };
        let at = |k: usize| rest.get((i * 2 + k) * 6..).unwrap_or(&[]);
        let (a, chain) = build(object, 0, at(0), None);
        let (b, _) = build(object, 1, at(1), chain);
        pairs.push(PairRead { a, b });
    }
    let mut out = vec![
        PairVerdict {
            standing: Standing::Lost,
            winner: None,
            a: SlotVerdict::Damage,
            b: SlotVerdict::Damage
        };
        n
    ];
    let j = judge(&pairs, clean_close, &mut out).expect("well-formed input");

    // The reference.
    let numbered: Vec<(usize, usize, u64)> = pairs
        .iter()
        .enumerate()
        .flat_map(|(i, p)| [(0usize, &p.a), (1, &p.b)].map(|(k, s)| (i, k, s)))
        .filter_map(|(i, k, s)| match s.status {
            SlotStatus::Valid(info) => Some((i, k, info.meta_seq)),
            SlotStatus::BadPayloadCrc => {
                s.facts.and_then(|f| f.numbered).map(|h| (i, k, h.meta_seq))
            }
            _ => None,
        })
        .collect();
    let qualifies = |i: usize, k: usize, m: Option<u64>| match m {
        None => true,
        Some(m) => {
            let others = numbered
                .iter()
                .filter(|&&(pi, pk, _)| (pi, pk) != (i, k))
                .map(|&(_, _, v)| v)
                .max()
                .unwrap_or(0);
            m == others + 1
        }
    };
    let bases: Vec<Base> = pairs.iter().map(base).collect();
    let mut qualifying = Vec::new();
    let mut troubled = false;
    let mut page_candidate = false;
    for (i, b) in bases.iter().enumerate() {
        match *b {
            Base::Refused | Base::Lost | Base::Damaged => troubled = true,
            Base::Candidate(k, m) => {
                page_candidate |= i > 0;
                if qualifies(i, k, m) {
                    qualifying.push((i, k))
                } else {
                    troubled = true
                }
            }
            Base::Fine => {}
        }
    }
    let clean = bases[0] == Base::Fine && clean_close;
    let refused = bases.iter().any(|b| *b == Base::Refused);
    // A page's torn write heals only in an otherwise sound store; the
    // volume record's whenever it is the one candidate (and nothing is
    // refused).
    let heals = qualifying.len() == 1
        && if qualifying[0].0 == 0 {
            !refused
        } else {
            !clean && !troubled
        };
    let want_heal = heals.then(|| {
        let (i, k) = qualifying[0];
        (i, if k == 0 { SlotIndex::A } else { SlotIndex::B })
    });
    let volume_unconfirmed = match bases[0] {
        Base::Damaged => true,
        Base::Candidate(..) => !heals,
        _ => false,
    };
    let volume_lost = matches!(bases[0], Base::Lost | Base::Refused);
    let want_writable = !refused && !volume_lost && (!volume_unconfirmed || !page_candidate);
    assert_eq!(j.heal, want_heal, "heal");
    assert_eq!(j.refused, refused, "refused");
    assert_eq!(j.clean, clean, "clean");
    assert_eq!(j.writable, want_writable, "writable");
    for (i, (b, v)) in bases.iter().zip(&out).enumerate() {
        let want = match *b {
            Base::Refused => Standing::Refused,
            Base::Lost => Standing::Lost,
            Base::Fine => Standing::Confirmed,
            Base::Damaged => Standing::Unconfirmed,
            Base::Candidate(..) if want_heal.map(|h| h.0) == Some(i) => Standing::Confirmed,
            Base::Candidate(..) => Standing::Unconfirmed,
        };
        assert_eq!(v.standing, want, "pair {i} standing");
    }

    // Invariants of the verdicts.
    let mut in_flight = Vec::new();
    for (i, (p, v)) in pairs.iter().zip(&out).enumerate() {
        assert_eq!(
            matches!(v.standing, Standing::Lost | Standing::Refused),
            v.winner.is_none(),
            "no winner exactly when lost or refused"
        );
        if let Some(w) = v.winner {
            assert!(
                if w == SlotIndex::A { &p.a } else { &p.b }
                    .status
                    .is_valid(),
                "a winner is valid"
            );
        }
        for (verdict, idx) in [(v.a, SlotIndex::A), (v.b, SlotIndex::B)] {
            if verdict == SlotVerdict::InFlight {
                in_flight.push((i, idx));
                assert_eq!(v.standing, Standing::Confirmed);
            }
        }
        if v.standing == Standing::Confirmed {
            assert!(
                v.a != SlotVerdict::Damage && v.b != SlotVerdict::Damage,
                "damage is never confirmed"
            );
        }
    }
    assert!(in_flight.len() <= 1);
    assert_eq!(j.heal, in_flight.first().copied());
});
