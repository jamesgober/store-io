//! The format-2 tear judgement on real slots: a small model writes the
//! volume record and table pages the way the store does (each write the
//! next `meta_seq`, into the slot that does not hold the winner), then
//! tears or damages copies, and the judgement must tell the one possible
//! torn write from damage (`.dev/design/format2-draft.md` §7).

#![allow(clippy::unwrap_used, clippy::expect_used, unused_results)]

use store_io_format::judge::{
    Judgement, PairRead, PairVerdict, SlotRead, SlotVerdict, Standing, judge,
};
use store_io_format::slot::{
    Expect, FORMAT_2, SlotFields, SlotIndex, SlotStatus, encode, header_facts, validate,
};

const VOLUME: [u8; 16] = [3; 16];

/// One pair's two slots and what the next write needs.
struct Pair {
    object: u64,
    slots: [Vec<u8>; 2],
    /// The winner: slot, generation, header CRC.
    winner: Option<(usize, u64, u32)>,
}

/// The volume record (pair 0) and `pages` table pages, in memory.
struct Model {
    pairs: Vec<Pair>,
    meta_seq: u64,
}

fn offset(object: u64, slot: usize) -> u64 {
    (object % 1024) * 8192 + slot as u64 * 4096
}

fn expect(object: u64, slot: usize) -> Expect {
    Expect {
        volume_uuid: VOLUME,
        object_id: object,
        slot_offset: offset(object, slot),
        slot_index: if slot == 0 {
            SlotIndex::A
        } else {
            SlotIndex::B
        },
        log2_slot_size: 12,
        format_major: FORMAT_2,
        numbered: true,
    }
}

impl Model {
    /// A created store: every pair written once (generation 1, slot A).
    fn new(pages: usize) -> Self {
        let mut m = Model {
            pairs: (0..=pages)
                .map(|i| Pair {
                    object: if i == 0 {
                        0
                    } else {
                        (1 << 61) | (i as u64 - 1)
                    },
                    slots: [vec![0; 4096], vec![0; 4096]],
                    winner: None,
                })
                .collect(),
            meta_seq: 0,
        };
        for i in (1..=pages).chain([0]) {
            m.write(i);
        }
        m
    }

    /// Writes the next version of pair `i` and returns the slot written.
    fn write(&mut self, i: usize) -> usize {
        self.write_seq(i, None)
    }

    fn write_seq(&mut self, i: usize, seq: Option<u64>) -> usize {
        let seq = seq.unwrap_or_else(|| {
            self.meta_seq += 1;
            self.meta_seq
        });
        let p = &mut self.pairs[i];
        let (slot, generation, prev) = match p.winner {
            None => (0, 1, 0),
            Some((w, g, crc)) => (1 - w, g + 1, crc),
        };
        let f = SlotFields {
            volume_uuid: VOLUME,
            object_id: p.object,
            generation,
            owner_generation: 1,
            slot_offset: offset(p.object, slot),
            slot_index: if slot == 0 {
                SlotIndex::A
            } else {
                SlotIndex::B
            },
            log2_slot_size: 12,
            prev_header_crc32c: prev,
            format_major: FORMAT_2,
            meta_seq: seq,
        };
        let crc = encode(&mut p.slots[slot], &f, b"page payload").unwrap();
        p.winner = Some((slot, generation, crc));
        slot
    }

    /// The pair's newest copy torn: its payload damaged (the header landed).
    fn tear_payload(&mut self, i: usize) -> usize {
        let s = self.write(i);
        self.pairs[i].slots[s][130] ^= 0xFF;
        s
    }

    /// The pair's newest copy torn in its header.
    fn tear_header(&mut self, i: usize) -> usize {
        let s = self.write(i);
        self.pairs[i].slots[s][60] ^= 0xFF;
        s
    }

    fn read(&self, unreadable: &[(usize, usize)]) -> Vec<PairRead> {
        self.pairs
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let r = |s: usize| {
                    if unreadable.contains(&(i, s)) {
                        return SlotRead {
                            status: SlotStatus::ReadError,
                            facts: None,
                        };
                    }
                    let e = expect(p.object, s);
                    SlotRead {
                        status: validate(&p.slots[s], &e),
                        facts: header_facts(&p.slots[s], &e),
                    }
                };
                PairRead { a: r(0), b: r(1) }
            })
            .collect()
    }
}

fn run(m: &Model, clean: bool, unreadable: &[(usize, usize)]) -> (Judgement, Vec<PairVerdict>) {
    let reads = m.read(unreadable);
    let mut out = vec![
        PairVerdict {
            standing: Standing::Lost,
            winner: None,
            a: SlotVerdict::Damage,
            b: SlotVerdict::Damage,
        };
        reads.len()
    ];
    let j = judge(&reads, clean, &mut out).unwrap();
    (j, out)
}

fn slot_verdict(v: &PairVerdict, s: usize) -> SlotVerdict {
    if s == 0 { v.a } else { v.b }
}

fn standings(v: &[PairVerdict]) -> Vec<Standing> {
    v.iter().map(|p| p.standing).collect()
}

#[test]
fn test_a_fresh_store_is_confirmed_throughout() {
    let m = Model::new(3);
    let (j, v) = run(&m, false, &[]);
    assert_eq!(standings(&v), [Standing::Confirmed; 4]);
    assert_eq!((j.heal, j.writable), (None, true));
    assert!(v.iter().all(|p| p.b == SlotVerdict::Nothing));
}

#[test]
fn test_a_torn_page_write_after_a_crash_is_healed() {
    let mut m = Model::new(3);
    m.write(2);
    let s = m.tear_payload(1);
    let (j, v) = run(&m, false, &[]);
    assert_eq!(slot_verdict(&v[1], s), SlotVerdict::InFlight);
    assert_eq!(standings(&v), [Standing::Confirmed; 4]);
    assert_eq!(
        j.heal,
        Some((1, if s == 0 { SlotIndex::A } else { SlotIndex::B }))
    );
    assert!(j.writable);
}

#[test]
fn test_a_tear_in_the_header_is_the_torn_write_too() {
    let mut m = Model::new(2);
    let s = m.tear_header(2);
    let (j, v) = run(&m, false, &[]);
    assert_eq!(slot_verdict(&v[2], s), SlotVerdict::InFlight);
    assert!(j.heal.is_some());
}

#[test]
fn test_after_a_clean_close_every_candidate_is_damage() {
    let mut m = Model::new(2);
    let s = m.tear_payload(1);
    let (j, v) = run(&m, true, &[]);
    assert!(j.clean);
    assert_eq!(slot_verdict(&v[1], s), SlotVerdict::Damage);
    assert_eq!(v[1].standing, Standing::Unconfirmed);
    assert_eq!(j.heal, None);
}

#[test]
fn test_two_candidates_are_both_damage() {
    let mut m = Model::new(3);
    m.tear_header(1);
    m.tear_header(2);
    let (j, v) = run(&m, false, &[]);
    assert_eq!(standings(&v)[1..3], [Standing::Unconfirmed; 2]);
    assert_eq!(j.heal, None);
}

#[test]
fn test_damage_to_an_older_write_is_not_the_torn_write() {
    // Page 1's newest copy is damaged, but a later write (page 2) exists:
    // its meta_seq is not one above the rest, so it was durable once.
    let mut m = Model::new(3);
    let s = m.tear_payload(1);
    m.write(2);
    let (j, v) = run(&m, false, &[]);
    assert_eq!(slot_verdict(&v[1], s), SlotVerdict::Damage);
    assert_eq!(v[1].standing, Standing::Unconfirmed);
    assert_eq!(v[2].standing, Standing::Confirmed);
    assert_eq!(j.heal, None);
}

#[test]
fn test_an_unreadable_newest_copy_is_damage_never_a_tear() {
    let mut m = Model::new(2);
    let s = m.write(1);
    let (j, v) = run(&m, false, &[(1, s)]);
    assert_eq!(slot_verdict(&v[1], s), SlotVerdict::Damage);
    assert_eq!(v[1].standing, Standing::Unconfirmed);
    assert_eq!(j.heal, None);
}

#[test]
fn test_a_blank_slot_opposite_a_later_generation_is_damage() {
    let mut m = Model::new(2);
    m.write(1); // generation 2 in slot B
    m.write(1); // generation 3 in slot A
    m.pairs[1].slots[1].fill(0); // slot B zeroed
    let (_, v) = run(&m, false, &[]);
    assert_eq!(v[1].b, SlotVerdict::Damage);
    assert_eq!(v[1].standing, Standing::Unconfirmed);
}

#[test]
fn test_a_damaged_older_copy_is_only_reported() {
    let mut m = Model::new(2);
    m.write(1); // gen 2 in B; A holds gen 1
    m.write(1); // gen 3 in A; B holds gen 2
    // Damage the payload of B (gen 2 < 3) and, on page 2, the header of the
    // predecessor.
    m.pairs[1].slots[1][130] ^= 1;
    m.write(2); // gen 2 in B
    m.pairs[2].slots[0][60] ^= 1; // gen 1, the predecessor
    let (j, v) = run(&m, false, &[]);
    assert_eq!(v[1].b, SlotVerdict::OlderDamaged);
    assert_eq!(v[2].a, SlotVerdict::OlderDamaged);
    assert_eq!(standings(&v), [Standing::Confirmed; 3]);
    assert_eq!(j.heal, None);
}

#[test]
fn test_a_damaged_volume_record_disables_healing() {
    let mut m = Model::new(2);
    m.write(0); // the volume record's gen 2 in B
    let s = m.tear_payload(1);
    // The volume record's predecessor unreadable: Unconfirmed.
    let (j, v) = run(&m, false, &[(0, 0)]);
    assert_eq!(v[0].standing, Standing::Unconfirmed);
    assert_eq!(slot_verdict(&v[1], s), SlotVerdict::Damage);
    assert_eq!(j.heal, None);
    assert!(!j.writable, "a page had a candidate");
    // Without a page candidate the open may write.
    let mut m = Model::new(2);
    m.write(0);
    let (j, _) = run(&m, false, &[(0, 0)]);
    assert!(j.writable);
}

#[test]
fn test_a_torn_volume_record_is_healed() {
    let mut m = Model::new(2);
    let s = m.tear_payload(0);
    let (j, v) = run(&m, false, &[]);
    assert_eq!(slot_verdict(&v[0], s), SlotVerdict::InFlight);
    assert_eq!(v[0].standing, Standing::Confirmed);
    assert_eq!(
        j.heal,
        Some((0, if s == 0 { SlotIndex::A } else { SlotIndex::B }))
    );
}

#[test]
fn test_a_torn_heal_is_judged_and_healed_again() {
    // The heal rewrites the torn slot at the candidate's own meta_seq; a
    // power cut during it leaves the same kind of candidate.
    let mut m = Model::new(2);
    m.write(2);
    let s = m.tear_payload(1);
    let torn_seq = m.meta_seq;
    // "Heal": the winner's payload into the torn slot, same meta_seq; torn.
    m.pairs[1].winner = {
        // The model's winner is the torn write; step it back to the
        // previous version before healing over it.
        let other = 1 - s;
        let e = expect(m.pairs[1].object, other);
        let SlotStatus::Valid(i) = validate(&m.pairs[1].slots[other], &e) else {
            panic!("the predecessor is valid");
        };
        Some((other, i.generation, i.header_crc32c))
    };
    m.write_seq(1, Some(torn_seq));
    m.pairs[1].slots[s][130] ^= 0xFF;
    let (j, v) = run(&m, false, &[]);
    assert_eq!(slot_verdict(&v[1], s), SlotVerdict::InFlight);
    assert!(j.heal.is_some());
}

#[test]
fn test_a_candidate_must_chain_to_the_winner() {
    let mut m = Model::new(2);
    let s = m.tear_payload(1);
    // Break the chain: the torn copy names another predecessor.
    let e = expect(m.pairs[1].object, s);
    m.pairs[1].slots[s][80] ^= 1; // prev_header_crc32c
    let crc = store_io_format::crc32c::crc32c(&{
        let mut h = m.pairs[1].slots[s][..128].to_vec();
        h[8..12].fill(0);
        h
    });
    m.pairs[1].slots[s][8..12].copy_from_slice(&crc.to_le_bytes());
    assert_eq!(
        validate(&m.pairs[1].slots[s], &e),
        SlotStatus::BadPayloadCrc
    );
    let (j, v) = run(&m, false, &[]);
    assert_eq!(slot_verdict(&v[1], s), SlotVerdict::Damage);
    assert_eq!(j.heal, None);
}

#[test]
fn test_lost_pairs() {
    // Both copies damaged; a fork; a newer format.
    let mut m = Model::new(3);
    m.write(1);
    m.pairs[1].slots[0][130] ^= 1;
    m.pairs[1].slots[1][130] ^= 1;
    m.pairs[2].slots[1] = m.pairs[2].slots[0].clone();
    let (_, v) = run(&m, false, &[]);
    assert_eq!(v[1].standing, Standing::Lost);
    assert_eq!(v[1].winner, None);
    // Slot B holds slot A's bytes: misplaced, so damage, not a fork; a real
    // fork needs two valid copies of one generation.
    assert_eq!(v[2].standing, Standing::Unconfirmed);
}

#[test]
fn test_caller_bugs() {
    let mut out = [];
    assert!(judge(&[], false, &mut out).is_err());
    let m = Model::new(1);
    let reads = m.read(&[]);
    let mut out = vec![
        PairVerdict {
            standing: Standing::Lost,
            winner: None,
            a: SlotVerdict::Damage,
            b: SlotVerdict::Damage,
        };
        1
    ];
    assert!(judge(&reads, false, &mut out).is_err());
}

/// Re-seals a slot's header CRC after a test edits the header.
fn reseal(slot: &mut [u8]) {
    slot[8..12].fill(0);
    let crc = store_io_format::crc32c::crc32c(&slot[..128]);
    slot[8..12].copy_from_slice(&crc.to_le_bytes());
}

#[test]
fn test_a_fork_is_lost_and_a_newer_format_refused() {
    let mut m = Model::new(2);
    // A fork: slot B holds a valid copy of slot A's generation.
    let a = m.pairs[1].slots[0].clone();
    let mut b = a.clone();
    b[64..72].copy_from_slice(&offset(m.pairs[1].object, 1).to_le_bytes());
    b[76] = 1;
    reseal(&mut b);
    m.pairs[1].slots[1] = b;
    // A newer format's incompatible flag on page 2's slot B.
    m.write(2);
    m.pairs[2].slots[1][16] = 1;
    reseal(&mut m.pairs[2].slots[1]);
    let (j, v) = run(&m, false, &[]);
    assert_eq!(v[1].standing, Standing::Lost);
    assert_eq!(v[2].standing, Standing::Refused);
    assert!(j.refused && !j.writable);
    // A newer format version (not only a flag) is refused too.
    let mut m = Model::new(1);
    m.write(1);
    let s = m.pairs[1].winner.unwrap().0;
    m.pairs[1].slots[s][14..16].copy_from_slice(&3u16.to_le_bytes());
    reseal(&mut m.pairs[1].slots[s]);
    let (j, v) = run(&m, false, &[]);
    assert_eq!(v[1].standing, Standing::Refused);
    assert!(j.refused && !j.writable && j.heal.is_none());
}

#[test]
fn test_a_first_generation_winner_has_no_predecessor() {
    // A header-torn second write whose CRC field happens to read zero: with
    // a generation-1 winner there is no predecessor to match, so it is the
    // torn write.
    let mut m = Model::new(1);
    let s = m.tear_header(1);
    m.pairs[1].slots[s][8..12].fill(0);
    let (j, v) = run(&m, false, &[]);
    assert_eq!(slot_verdict(&v[1], s), SlotVerdict::InFlight);
    assert!(j.heal.is_some());
}

#[test]
fn test_damage_anywhere_blocks_healing() {
    // Page 1's newest write (the highest meta_seq) is damaged in its
    // payload, then a crash tears page 2's next write at one above it. Page
    // 2 qualifies, but with damage in the store a heal could undo a
    // committed write (two faults can hide the true highest number), so
    // both are damage and nothing is healed.
    let mut m = Model::new(2);
    let s1 = m.tear_payload(1);
    let s2 = m.tear_payload(2);
    let (j, v) = run(&m, false, &[]);
    assert_eq!(slot_verdict(&v[1], s1), SlotVerdict::Damage);
    assert_eq!(slot_verdict(&v[2], s2), SlotVerdict::Damage);
    assert_eq!(j.heal, None);
    // The two-fault case: an older durable write damaged in its payload,
    // the newest durable write (another page) unreadable.
    let mut m = Model::new(2);
    let older = m.tear_payload(1);
    let newest = m.write(2);
    let (j, v) = run(&m, false, &[(2, newest)]);
    assert_eq!(slot_verdict(&v[1], older), SlotVerdict::Damage);
    assert_eq!(j.heal, None);
}

#[test]
fn test_a_number_two_slots_share_is_never_one_above_the_rest() {
    // Page 1 durable at k, the highest number; page 2's newer copy, also
    // stamped k (impossible in a valid history), damaged: it is not one
    // above every other slot, since another holds its number.
    let mut m = Model::new(2);
    m.write(1);
    let k = m.meta_seq;
    let s = {
        let s = m.write_seq(2, Some(k));
        m.pairs[2].slots[s][130] ^= 0xFF;
        s
    };
    let (j, v) = run(&m, false, &[]);
    assert_eq!(slot_verdict(&v[2], s), SlotVerdict::Damage);
    assert_eq!(j.heal, None);
}

/// A copy one generation behind or two ahead of the winner, even chained,
/// is not the write in flight.
#[test]
fn test_a_candidate_must_be_exactly_the_next_generation() {
    for skew in [0u64, 2] {
        let mut m = Model::new(2);
        let (w, g, crc) = m.pairs[1].winner.unwrap();
        let s = 1 - w;
        let f = SlotFields {
            volume_uuid: VOLUME,
            object_id: m.pairs[1].object,
            generation: g + skew,
            owner_generation: 1,
            slot_offset: offset(m.pairs[1].object, s),
            slot_index: if s == 0 { SlotIndex::A } else { SlotIndex::B },
            log2_slot_size: 12,
            prev_header_crc32c: crc,
            format_major: FORMAT_2,
            meta_seq: m.meta_seq + 1,
        };
        encode(&mut m.pairs[1].slots[s], &f, b"page payload").unwrap();
        m.pairs[1].slots[s][130] ^= 0xFF;
        let (j, v) = run(&m, false, &[]);
        assert_eq!(
            slot_verdict(&v[1], s),
            SlotVerdict::Damage,
            "generation {}",
            g + skew
        );
        assert_eq!(j.heal, None);
    }
}

/// After a clean close, the next writable open's own volume-record write
/// can still tear: the volume pair's candidate is that write, healed.
#[test]
fn test_a_torn_open_after_a_clean_close_is_healed() {
    let mut m = Model::new(2);
    let s = m.tear_payload(0);
    // The winner (the closing write) says clean_close.
    let (j, v) = run(&m, true, &[]);
    assert!(!j.clean, "the volume pair has a candidate");
    assert_eq!(slot_verdict(&v[0], s), SlotVerdict::InFlight);
    assert_eq!(j.heal.map(|h| h.0), Some(0));
}

/// The clean-close rule needs a Confirmed volume record.
#[test]
fn test_clean_needs_a_confirmed_volume_record() {
    let mut m = Model::new(1);
    m.write(0);
    let (j, v) = run(&m, true, &[(0, 0)]);
    assert_eq!(v[0].standing, Standing::Unconfirmed);
    assert!(!j.clean);
}

/// A damaged volume record and a page candidate that does not qualify:
/// still read-only (any candidate counts).
#[test]
fn test_a_non_qualifying_page_candidate_with_a_damaged_volume_record() {
    let mut m = Model::new(2);
    m.write(0);
    m.tear_payload(1);
    m.write(2); // the page-1 candidate is no longer one above the rest
    let (j, _) = run(&m, false, &[(0, 0)]);
    assert!(!j.writable);
}

/// Two qualifying-looking candidates, one in the volume record: both
/// damage, the volume record Unconfirmed, read-only.
#[test]
fn test_two_candidates_one_in_the_volume_record() {
    let mut m = Model::new(1);
    m.tear_header(1);
    m.tear_header(0);
    let (j, v) = run(&m, false, &[]);
    assert_eq!(v[0].standing, Standing::Unconfirmed);
    assert_eq!(j.heal, None);
    assert!(!j.writable);
}

/// A lost volume record: nothing healed, nothing written.
#[test]
fn test_a_lost_volume_record_forbids_writing() {
    let mut m = Model::new(1);
    m.tear_header(1);
    let (j, v) = run(&m, false, &[(0, 0), (0, 1)]);
    assert_eq!(v[0].standing, Standing::Lost);
    assert_eq!(j.heal, None);
    assert!(!j.writable);
}

/// Only slot B can be blank opposite a first write.
#[test]
fn test_a_blank_slot_a_opposite_a_first_write_in_b_is_damage() {
    let mut m = Model::new(1);
    let w = m.pairs[1].slots[0].clone();
    m.pairs[1].slots[0].fill(0);
    let mut b = w;
    b[64..72].copy_from_slice(&offset(m.pairs[1].object, 1).to_le_bytes());
    b[76] = 1;
    reseal(&mut b);
    m.pairs[1].slots[1] = b;
    let (_, v) = run(&m, false, &[]);
    assert_eq!(v[1].a, SlotVerdict::Damage);
    assert_eq!(v[1].standing, Standing::Unconfirmed);
}

/// A header-damaged slot passed without its header facts (a caller bug):
/// damage, never a candidate to heal.
#[test]
fn test_a_header_damaged_slot_without_facts_is_damage() {
    let mut m = Model::new(1);
    let s = m.tear_header(1);
    let mut reads = m.read(&[]);
    let slot = if s == 0 {
        &mut reads[1].a
    } else {
        &mut reads[1].b
    };
    assert_eq!(slot.status, SlotStatus::BadHeaderCrc);
    slot.facts = None;
    let mut out = vec![
        PairVerdict {
            standing: Standing::Lost,
            winner: None,
            a: SlotVerdict::Damage,
            b: SlotVerdict::Damage,
        };
        reads.len()
    ];
    let j = judge(&reads, false, &mut out).unwrap();
    assert_eq!(slot_verdict(&out[1], s), SlotVerdict::Damage);
    assert_eq!(j.heal, None);
}

/// A store with one stale page stays writable through a crash during
/// close: the torn volume-record write is healed despite the damage, so the
/// rollback can still be accepted.
#[test]
fn test_a_torn_close_on_a_store_with_a_stale_page_is_healed() {
    let mut m = Model::new(2);
    m.tear_payload(1); // page 1's newest durable write, damaged
    m.write(2); // a later write: page 1 no longer qualifies
    let s = m.tear_payload(0); // the closing volume-record write, torn
    let (j, v) = run(&m, false, &[]);
    assert_eq!(v[1].standing, Standing::Unconfirmed);
    assert_eq!(slot_verdict(&v[0], s), SlotVerdict::InFlight);
    assert_eq!(v[0].standing, Standing::Confirmed);
    assert_eq!(j.heal.map(|h| h.0), Some(0));
    assert!(j.writable);
}

/// A refused pair: the open fails, so not even the volume record is healed.
#[test]
fn test_a_refused_page_blocks_even_a_volume_heal() {
    // Page 2's copy does not hold the highest number, so the torn
    // volume-record write still qualifies: only the refusal stops the heal.
    let mut m = Model::new(2);
    let w = m.pairs[2].winner.unwrap().0;
    m.pairs[2].slots[w][16] = 1;
    reseal(&mut m.pairs[2].slots[w]);
    m.tear_payload(0);
    let (j, _) = run(&m, false, &[]);
    assert!(j.refused);
    assert_eq!(j.heal, None);
    assert!(!j.writable);
}

/// With both copies of a pair valid, the newer generation wins, in either
/// slot: never the older copy (a silent rollback of the metadata).
#[test]
fn test_with_both_copies_valid_the_newer_generation_wins_in_either_slot() {
    let mut m = Model::new(3);
    // Pair 1: written again into B (generation 2); pair 2: twice more,
    // back into A (generation 3), B holding generation 2.
    let b = m.write(1);
    m.write(2);
    let a = m.write(2);
    assert_eq!((b, a), (1, 0));
    let (j, v) = run(&m, false, &[]);
    assert_eq!(standings(&v), [Standing::Confirmed; 4]);
    assert_eq!(v[1].winner, Some(SlotIndex::B));
    assert_eq!(
        (v[1].a, v[1].b),
        (SlotVerdict::Nothing, SlotVerdict::Winner)
    );
    assert_eq!(v[2].winner, Some(SlotIndex::A));
    assert_eq!(
        (v[2].a, v[2].b),
        (SlotVerdict::Winner, SlotVerdict::Nothing)
    );
    assert!(j.writable);
}
