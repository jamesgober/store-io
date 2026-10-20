//! The format-2 tear judgement (`.dev/design/format2-draft.md` §7): given
//! both slots of the volume record and of every table page, decide each
//! pair's standing, which failed copy (if any) is the one metadata write a
//! crash can leave torn, and whether the open may write.
//!
//! Every volume-record and table-page write carries `meta_seq`, one above
//! the highest of the other *numbered* slots (those whose header passes
//! every check before the payload CRC), and each is durable before the
//! next, so at a crash at most one is in flight. A failed newer copy is that
//! write only if its `meta_seq` is one above every other numbered slot (or
//! unreadable in its header) and it is the only such copy in the store, and
//! the store was not closed cleanly. A page's torn write is healed only when
//! nothing else in the store is damaged (damage could hide the true highest
//! number); the volume record's whenever it is the one candidate (it holds
//! nothing a heal could undo). Everything else a crash cannot explain is
//! damage.
//!
//! Pure: no I/O, no allocation; the caller passes the slots' statuses and
//! header facts and a buffer for the verdicts.

use crate::slot::{HeaderFacts, SlotIndex, SlotStatus};

/// One slot as read: its status and what its header shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotRead {
    /// The slot's validation status (`ReadError` for a failed read, after
    /// the caller's retry).
    pub status: SlotStatus,
    /// What its header shows ([`crate::slot::header_facts`]); `None` for a
    /// failed read.
    pub facts: Option<HeaderFacts>,
}

/// Both slots of one pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairRead {
    /// Slot A.
    pub a: SlotRead,
    /// Slot B.
    pub b: SlotRead,
}

/// What became of one slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotVerdict {
    /// The pair's current version.
    Winner,
    /// The previous version, intact; or a slot never written opposite a
    /// first-generation winner.
    Nothing,
    /// An older copy (the winner's predecessor), damaged: reported only.
    OlderDamaged,
    /// The one write a crash left torn: the winner stands Confirmed, and a
    /// writable open rewrites this slot (healing).
    InFlight,
    /// Damage: a crash cannot explain it.
    Damage,
}

/// A pair's standing (spec §7, §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// The winner is the latest committed version.
    Confirmed,
    /// The winner may be missing later commits.
    Unconfirmed,
    /// No usable winner.
    Lost,
    /// Written by a newer format, or with a must-understand flag this
    /// version does not know: the open fails (`Unsupported`), as format 1's.
    Refused,
}

/// The verdict on one pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairVerdict {
    /// The pair's standing.
    pub standing: Standing,
    /// The winning slot (none when Lost or Refused).
    pub winner: Option<SlotIndex>,
    /// What became of slot A.
    pub a: SlotVerdict,
    /// What became of slot B.
    pub b: SlotVerdict,
}

impl PairVerdict {
    const LOST: Self = Self {
        standing: Standing::Lost,
        winner: None,
        a: SlotVerdict::Damage,
        b: SlotVerdict::Damage,
    };
    const REFUSED: Self = Self {
        standing: Standing::Refused,
        ..Self::LOST
    };
}

/// The verdict on the whole store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Judgement {
    /// The torn write to heal: `(pair, slot)`. A page's only when nothing
    /// else in the store is damaged, lost or refused (damage elsewhere could
    /// hide the true highest number, so a heal could undo a committed
    /// write); the volume record's whenever it is the one candidate and
    /// nothing is refused.
    pub heal: Option<(usize, SlotIndex)>,
    /// A writable open may proceed. False when any pair is refused, when
    /// the volume record has no winner, or when it is Unconfirmed and some
    /// page had a candidate (writing the volume record would remove the
    /// evidence). The caller also opens read-only when a winner it will
    /// rewrite has an unknown `ro_compat_flags` bit, and reads `clean_close`
    /// from the volume winner before calling [`judge`].
    pub writable: bool,
    /// Some pair is refused: the open fails `Unsupported(FormatVersion)`.
    pub refused: bool,
    /// The clean-close rule applied (every candidate judged as damage).
    pub clean: bool,
}

/// Why the judgement could not be made (caller bugs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JudgeError {
    /// No pairs (the volume record is pair 0).
    Empty,
    /// The output buffer's length differs from the input's.
    OutputLength,
}

/// A failed slot opposite a valid winner, before the store-wide rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opposite {
    Nothing,
    OlderDamaged,
    /// A candidate for the in-flight write, with its `meta_seq` if readable.
    Candidate(Option<u64>),
    Damage,
}

fn valid_info(s: &SlotStatus) -> Option<crate::slot::SlotInfo> {
    match s {
        SlotStatus::Valid(i) => Some(*i),
        _ => None,
    }
}

/// The `meta_seq` a slot counts for, if it is numbered (`Valid`, or
/// `BadPayloadCrc` with a header that passed every other check).
fn numbered(s: &SlotRead) -> Option<u64> {
    match s.status {
        SlotStatus::Valid(i) => Some(i.meta_seq),
        SlotStatus::BadPayloadCrc => s.facts.and_then(|f| f.numbered).map(|n| n.meta_seq),
        _ => None,
    }
}

/// Judges the slot opposite a valid winner (spec §7, rule 3); `o_is_b`:
/// the failed slot is slot B (only B can be blank opposite a first write).
fn opposite(winner: &crate::slot::SlotInfo, o: &SlotRead, o_is_b: bool) -> Opposite {
    let g = winner.generation;
    match o.status {
        SlotStatus::Blank if g == 1 && o_is_b => Opposite::Nothing,
        SlotStatus::BadPayloadCrc => match o.facts.and_then(|f| f.numbered) {
            Some(n)
                if Some(n.generation) == g.checked_add(1)
                    && n.prev_header_crc32c == winner.header_crc32c =>
            {
                Opposite::Candidate(Some(n.meta_seq))
            }
            Some(n) if n.generation < g => Opposite::OlderDamaged,
            _ => Opposite::Damage,
        },
        SlotStatus::BadHeaderCrc => match o.facts {
            Some(f) if g >= 2 && f.stored_header_crc32c == winner.prev_header_crc32c => {
                Opposite::OlderDamaged
            }
            Some(_) => Opposite::Candidate(None),
            None => Opposite::Damage,
        },
        _ => Opposite::Damage,
    }
}

/// Judges every pair: `pairs[0]` is the volume record, the rest the table
/// pages; `clean_close` is the volume winner's flag (ignored if the volume
/// record has no winner). Writes one verdict per pair into `out`.
///
/// # Errors
///
/// [`JudgeError`] for caller bugs only.
pub fn judge(
    pairs: &[PairRead],
    clean_close: bool,
    out: &mut [PairVerdict],
) -> Result<Judgement, JudgeError> {
    if pairs.is_empty() {
        return Err(JudgeError::Empty);
    }
    if out.len() != pairs.len() {
        return Err(JudgeError::OutputLength);
    }

    // Pass 1: each pair on its own (rules 1-3), and the numbered values.
    let (mut max1, mut max1_count, mut max2) = (0u64, 0usize, 0u64);
    let mut note = |v: u64| {
        if v > max1 {
            max2 = max1;
            max1 = v;
            max1_count = 1;
        } else if v == max1 {
            max1_count += 1;
        } else if v > max2 {
            max2 = v;
        }
    };
    for p in pairs {
        for s in [&p.a, &p.b] {
            if let Some(v) = numbered(s) {
                note(v);
            }
        }
    }
    // The highest numbered value other than one slot holding `v`.
    let others_max = |v: Option<u64>| match v {
        Some(v) if v == max1 && max1_count == 1 => max2,
        _ => max1,
    };

    let mut candidates = 0usize;
    let mut page_had_candidate = false;
    for (i, (p, o)) in pairs.iter().zip(out.iter_mut()).enumerate() {
        *o = PairVerdict::LOST;
        let refused = |s: &SlotStatus| {
            matches!(
                s,
                SlotStatus::UnsupportedVersion { .. } | SlotStatus::Incompatible { .. }
            )
        };
        if refused(&p.a.status) || refused(&p.b.status) {
            *o = PairVerdict::REFUSED;
            continue;
        }
        match (valid_info(&p.a.status), valid_info(&p.b.status)) {
            (None, None) => {}
            (Some(a), Some(b)) => {
                if a.generation == b.generation {
                    continue; // a fork
                }
                let a_wins = a.generation > b.generation;
                *o = PairVerdict {
                    standing: Standing::Confirmed,
                    winner: Some(if a_wins { SlotIndex::A } else { SlotIndex::B }),
                    a: if a_wins {
                        SlotVerdict::Winner
                    } else {
                        SlotVerdict::Nothing
                    },
                    b: if a_wins {
                        SlotVerdict::Nothing
                    } else {
                        SlotVerdict::Winner
                    },
                };
            }
            (Some(w), None) | (None, Some(w)) => {
                let (wi, other) = if valid_info(&p.a.status).is_some() {
                    (SlotIndex::A, &p.b)
                } else {
                    (SlotIndex::B, &p.a)
                };
                let (verdict, standing) = match opposite(&w, other, wi == SlotIndex::A) {
                    Opposite::Nothing => (SlotVerdict::Nothing, Standing::Confirmed),
                    Opposite::OlderDamaged => (SlotVerdict::OlderDamaged, Standing::Confirmed),
                    Opposite::Damage => (SlotVerdict::Damage, Standing::Unconfirmed),
                    Opposite::Candidate(m) => {
                        page_had_candidate |= i > 0;
                        // Provisionally: qualifies or not (pass 2 decides).
                        let qualifies = match m {
                            None => true,
                            Some(m) => others_max(Some(m)).checked_add(1) == Some(m),
                        };
                        if qualifies {
                            candidates += 1;
                            (SlotVerdict::InFlight, Standing::Confirmed)
                        } else {
                            (SlotVerdict::Damage, Standing::Unconfirmed)
                        }
                    }
                };
                *o = PairVerdict {
                    standing,
                    winner: Some(wi),
                    a: if wi == SlotIndex::A {
                        SlotVerdict::Winner
                    } else {
                        verdict
                    },
                    b: if wi == SlotIndex::B {
                        SlotVerdict::Winner
                    } else {
                        verdict
                    },
                };
            }
        }
    }

    // Pass 2: the store-wide rules. Clean close: nothing was in flight.
    // Otherwise only a single qualifying candidate is the torn write. A
    // volume record that ends Unconfirmed may carry a stale `clean_close`,
    // so then every page candidate is damage too, and nothing is healed.
    let volume_has_candidate = [out[0].a, out[0].b].contains(&SlotVerdict::InFlight);
    let clean = out[0].standing == Standing::Confirmed && !volume_has_candidate && clean_close;
    // Anything damaged, lost or refused anywhere (a non-qualifying
    // candidate included): no heal, since damage could hide the true
    // highest number (two faults would let a heal undo a committed write).
    let troubled = out.iter().any(|v| v.standing != Standing::Confirmed);
    let refused = out.iter().any(|v| v.standing == Standing::Refused);
    let volume_lost = matches!(out[0].standing, Standing::Lost | Standing::Refused);
    // A page's torn write is healed only in an otherwise sound store. The
    // volume record's is healed whenever it is the one candidate: it holds
    // only `clean_close` and `owner_generation`, and the open rewrites it at
    // once, so healing it undoes nothing (and a store with one stale page
    // must stay writable through a crash during close, for the rollback to
    // be accepted).
    let page_demote = clean || candidates != 1 || troubled;
    let volume_demote = candidates != 1 || refused;
    let mut heal = None;
    for (i, v) in out.iter_mut().enumerate() {
        for (slot, idx) in [(&mut v.a, SlotIndex::A), (&mut v.b, SlotIndex::B)] {
            if *slot == SlotVerdict::InFlight {
                let demote = if i == 0 { volume_demote } else { page_demote };
                if demote {
                    *slot = SlotVerdict::Damage;
                    v.standing = Standing::Unconfirmed;
                } else {
                    heal = Some((i, idx));
                }
            }
        }
    }
    let writable = !refused
        && !volume_lost
        && (out[0].standing != Standing::Unconfirmed || !page_had_candidate);
    Ok(Judgement {
        heal,
        writable,
        clean,
        refused,
    })
}
