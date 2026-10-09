//! Choosing the current version of an A/B slot pair.
//!
//! Each update of an object writes the slot that does not hold the current
//! version, so after any crash at least one slot still holds a complete version.
//! [`decide`] picks it and says how sure it is, without ever rewriting either
//! slot.
//!
//! # Confidence
//!
//! The winner is **confirmed** when the other slot is explained by an ordinary
//! history: it is blank (never written), torn (a header or payload CRC
//! mismatch, which a crash during the latest write produces), or an older valid
//! version. It is **unconfirmed** when the other slot cannot be read or holds
//! something an ordinary history never produces there (another volume's or
//! object's slot, a misdirected slot, non-slot bytes, or impossible fields): the
//! newer version may have been destroyed, so the caller must decide whether to
//! trust the survivor.
//!
//! Two valid slots with the same generation are a **fork** (two writers, or a
//! replayed write), and no winner is chosen. A slot written by a newer on-disk
//! format makes the pair **refused**: this version neither reads nor overwrites
//! it.

use crate::slot::{SlotIndex, SlotInfo, SlotStatus};

/// The chosen version of a pair, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Winner {
    /// Both slots are blank: the object was never written.
    Empty,
    /// The slot holds the current version and the other slot is explained.
    Confirmed(SlotIndex),
    /// The slot holds the newest readable version, but a newer one may have
    /// existed in the other slot.
    Unconfirmed(SlotIndex),
    /// Both slots are valid with the same generation.
    Fork,
    /// A slot uses a newer format or unknown incompatible features.
    Refused,
    /// Neither slot holds a valid version and they are not both blank.
    Lost,
}

/// Both slot statuses, the decision, and the anomalies found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairReport {
    /// Status of slot A.
    pub a: SlotStatus,
    /// Status of slot B.
    pub b: SlotStatus,
    /// The decision.
    pub winner: Winner,
    /// Both slots are valid but their generations are not consecutive: an
    /// update was lost or a slot was restored from elsewhere.
    pub generation_gap: bool,
    /// The winner does not record the other slot's header CRC as its
    /// predecessor although their generations are consecutive.
    pub chain_break: bool,
}

impl PairReport {
    /// The winning slot's facts, if a winner was chosen.
    #[must_use]
    pub fn winner_info(&self) -> Option<(SlotIndex, SlotInfo)> {
        let index = match self.winner {
            Winner::Confirmed(i) | Winner::Unconfirmed(i) => i,
            _ => return None,
        };
        match self.status(index) {
            SlotStatus::Valid(info) => Some((index, info)),
            _ => None,
        }
    }

    /// The status of one slot.
    #[must_use]
    pub fn status(&self, index: SlotIndex) -> SlotStatus {
        match index {
            SlotIndex::A => self.a,
            SlotIndex::B => self.b,
        }
    }

    /// Which slot the next update must write: the one not holding the winner.
    /// `None` when the pair must not be written (fork, refused, lost).
    #[must_use]
    pub fn next_write(&self) -> Option<SlotIndex> {
        match self.winner {
            Winner::Empty => Some(SlotIndex::A),
            Winner::Confirmed(i) | Winner::Unconfirmed(i) => Some(i.other()),
            Winner::Fork | Winner::Refused | Winner::Lost => None,
        }
    }
}

/// How the other slot of a single valid survivor is explained.
fn explains_survivor(other: &SlotStatus) -> bool {
    matches!(
        other,
        SlotStatus::Blank | SlotStatus::BadHeaderCrc | SlotStatus::BadPayloadCrc
    )
}

fn refused(s: &SlotStatus) -> bool {
    matches!(
        s,
        SlotStatus::UnsupportedVersion { .. } | SlotStatus::Incompatible { .. }
    )
}

/// Decides the winner of a pair from the two validated slot statuses.
///
/// # Examples
///
/// ```
/// use store_io_format::pair::{decide, Winner};
/// use store_io_format::slot::SlotStatus;
///
/// let report = decide(SlotStatus::Blank, SlotStatus::Blank);
/// assert_eq!(report.winner, Winner::Empty);
/// ```
#[must_use]
pub fn decide(a: SlotStatus, b: SlotStatus) -> PairReport {
    let mut report = PairReport {
        a,
        b,
        winner: Winner::Lost,
        generation_gap: false,
        chain_break: false,
    };
    if refused(&a) || refused(&b) {
        report.winner = Winner::Refused;
        return report;
    }
    report.winner = match (a, b) {
        (SlotStatus::Blank, SlotStatus::Blank) => Winner::Empty,
        (SlotStatus::Valid(ia), SlotStatus::Valid(ib)) => {
            if ia.generation == ib.generation {
                Winner::Fork
            } else {
                let (index, newer, older) = if ia.generation > ib.generation {
                    (SlotIndex::A, ia, ib)
                } else {
                    (SlotIndex::B, ib, ia)
                };
                let consecutive = newer.generation - older.generation == 1;
                report.generation_gap = !consecutive;
                report.chain_break = consecutive && newer.prev_header_crc32c != older.header_crc32c;
                Winner::Confirmed(index)
            }
        }
        (SlotStatus::Valid(_), other) => survivor(SlotIndex::A, &other),
        (other, SlotStatus::Valid(_)) => survivor(SlotIndex::B, &other),
        _ => Winner::Lost,
    };
    report
}

fn survivor(index: SlotIndex, other: &SlotStatus) -> Winner {
    if explains_survivor(other) {
        Winner::Confirmed(index)
    } else {
        Winner::Unconfirmed(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slot::{Expect, MAGIC, SlotFields, encode, validate};

    fn info(generation: u64, prev: u32) -> SlotStatus {
        // Build a real slot so SlotInfo carries genuine CRCs.
        let fields = SlotFields {
            volume_uuid: [1; 16],
            object_id: 9,
            generation,
            owner_generation: 1,
            slot_offset: 0,
            slot_index: SlotIndex::A,
            log2_slot_size: 12,
            prev_header_crc32c: prev,
        };
        let mut buf = vec![0u8; 4096];
        assert!(encode(&mut buf, &fields, b"p").is_ok());
        validate(
            &buf,
            &Expect {
                volume_uuid: [1; 16],
                object_id: 9,
                slot_offset: 0,
                slot_index: SlotIndex::A,
                log2_slot_size: 12,
            },
        )
    }

    fn crc_of(s: SlotStatus) -> u32 {
        match s {
            SlotStatus::Valid(i) => i.header_crc32c,
            _ => 0,
        }
    }

    #[test]
    fn test_decide_both_blank_is_empty_and_writes_a() {
        let r = decide(SlotStatus::Blank, SlotStatus::Blank);
        assert_eq!(r.winner, Winner::Empty);
        assert_eq!(r.next_write(), Some(SlotIndex::A));
    }

    #[test]
    fn test_decide_newer_generation_wins_and_next_write_targets_loser() {
        let old = info(4, 0);
        let new = info(5, crc_of(old));
        let r = decide(old, new);
        assert_eq!(r.winner, Winner::Confirmed(SlotIndex::B));
        assert_eq!(r.next_write(), Some(SlotIndex::A));
        assert!(!r.generation_gap && !r.chain_break);
        assert_eq!(r.winner_info().map(|(_, i)| i.generation), Some(5));
    }

    #[test]
    fn test_decide_flags_gap_and_chain_break() {
        let r = decide(info(4, 0), info(7, 0));
        assert!(r.generation_gap);
        let r = decide(info(4, 0), info(5, 0xDEAD));
        assert!(r.chain_break);
    }

    #[test]
    fn test_decide_equal_generations_are_a_fork_with_no_write() {
        let r = decide(info(3, 0), info(3, 1));
        assert_eq!(r.winner, Winner::Fork);
        assert_eq!(r.next_write(), None);
    }

    #[test]
    fn test_decide_torn_other_slot_confirms_survivor() {
        for torn in [
            SlotStatus::Blank,
            SlotStatus::BadHeaderCrc,
            SlotStatus::BadPayloadCrc,
        ] {
            assert_eq!(
                decide(info(2, 0), torn).winner,
                Winner::Confirmed(SlotIndex::A)
            );
            assert_eq!(
                decide(torn, info(2, 0)).winner,
                Winner::Confirmed(SlotIndex::B)
            );
        }
    }

    #[test]
    fn test_decide_unexplained_other_slot_leaves_survivor_unconfirmed() {
        for odd in [
            SlotStatus::ReadError,
            SlotStatus::Misplaced,
            SlotStatus::Foreign,
            SlotStatus::WrongObject,
            SlotStatus::BadMagic,
            SlotStatus::Malformed,
            SlotStatus::BadLength,
        ] {
            assert_eq!(
                decide(info(2, 0), odd).winner,
                Winner::Unconfirmed(SlotIndex::A),
                "{odd:?}"
            );
        }
    }

    #[test]
    fn test_decide_future_format_refuses_the_pair() {
        let r = decide(info(2, 0), SlotStatus::UnsupportedVersion { major: 2 });
        assert_eq!(r.winner, Winner::Refused);
        assert_eq!(r.next_write(), None);
        let r = decide(SlotStatus::Incompatible { flags: 4 }, SlotStatus::Blank);
        assert_eq!(r.winner, Winner::Refused);
    }

    #[test]
    fn test_decide_nothing_valid_is_lost() {
        let r = decide(SlotStatus::BadHeaderCrc, SlotStatus::ReadError);
        assert_eq!(r.winner, Winner::Lost);
        assert_eq!(r.next_write(), None);
        let _ = MAGIC;
    }
}
