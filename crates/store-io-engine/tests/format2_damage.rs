//! Damage to the metadata pairs (v0.5 E7 C4, spec §7 and §9): the volume
//! record's and the pages' slots damaged in each way (torn, damaged in the
//! header, garbage, blank, unreadable, a fork, a broken chain, an older
//! generation, misplaced, another object's, another store's, a newer
//! format, an incompatible flag), alone and in pairs across pairs, after a
//! clean close, after a stop, and after a crash straight after a page
//! write. Each row states what the spec's table makes of it (healed, the
//! volume record stale, pages stale or lost, read only, refused, every
//! damaged slot) and the open must find exactly that, the regions the
//! spec leaves (with their data, writable where the spec allows); then
//! every way out the spec gives (the open itself, `accept_page_rollback`,
//! `copy_store` with `accept_stale_pages`) ends in a store that opens
//! writable, serves those regions and audits clean, and every way it
//! refuses is refused, by kind.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

mod format2_util;

use std::path::Path;

use format2_util::audit::{
    assert_closed_in, audit_closed, audit_in, audit_space, audit_space_in, judged, report_agrees,
};
use format2_util::*;
use store_io_core::error::{Capability, Error, NotWrittenCause};
use store_io_engine::{CONTAINER, CopyOptions, CopyReport, Healed, SlotDamage, Store, copy_store};
use store_io_format::slot::{FORMAT_2, SlotFields, SlotIndex, encode};
use store_io_sim::{SimConfig, SimPlatform};

const B: u64 = 4096;

/// What happened to the store before the damage.
#[derive(Debug, Clone, Copy)]
enum History {
    /// Closed cleanly.
    Clean,
    /// Closed, opened again, the power failed with it open (the last
    /// metadata write: that open's volume record).
    Stopped,
    /// The power failed with it open straight after its last numbered
    /// metadata write (page 0's, for the slot's provisioning; the slot's
    /// commit after it writes only its region header). No close ran: the
    /// volume record's slot B is blank.
    AfterPage,
}

/// A store with a page region "p" holding a block of 5s and a slot "s"
/// holding "object". Its history: the volume record A generation 1 and B
/// 2 (after a stop, A 3; after a crash past a page write, B blank); page 0
/// A generation 3 (both regions) and B 2 ("p" only); pages 1 and 2 A
/// generation 1, B blank.
fn store(seed: u64, history: History) -> SimPlatform {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    let s = create(&p);
    let r = s.provision_page_region("p", 8 * B).unwrap();
    r.write_durable(r.pos(0).unwrap(), &[5; B as usize])
        .unwrap();
    s.provision_slot("s").unwrap().commit(b"object").unwrap();
    drop(r);
    match history {
        History::Clean => drop(s),
        History::Stopped => {
            drop(s);
            crash_open(&p, open(&p));
        }
        History::AfterPage => crash_open(&p, s),
    }
    p
}

/// A metadata pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pair {
    Volume,
    Page(u32),
}

/// Which of a pair's slots (as the undamaged store has them).
#[derive(Debug, Clone, Copy)]
enum Which {
    /// The one with the higher generation.
    Newest,
    /// The other.
    Older,
    /// Slot B (of a page never written since create: blank).
    SlotB,
    /// Both.
    Both,
}

/// What is done to a slot.
#[derive(Debug, Clone, Copy)]
enum Damage {
    /// A payload byte flipped, the header intact (`BadPayloadCrc`).
    Torn,
    /// A header byte flipped (`BadHeaderCrc`, its stored CRC kept).
    Header,
    /// The magic broken (`BadMagic`).
    Garbage,
    /// Zeros (`Blank`).
    Blank,
    /// Reads of it fail (a latent sector error).
    Unreadable,
    /// An incompatible flag this version does not know, the header CRC
    /// redone (refused).
    Incompatible,
    /// The other slot's bytes, in the wrong place (`Misplaced`).
    Misplaced,
    /// A valid copy of the other slot's generation (a fork).
    Fork,
    /// A torn copy one generation up, chained to something else.
    BrokenChain,
    /// A torn copy two generations up.
    Skipped,
    /// A valid copy of generation 1, below the other slot's.
    OlderGeneration,
    /// A valid copy of another page (`WrongObject`).
    OtherObject,
    /// A valid copy of another store (`Foreign`).
    OtherStore,
    /// A valid copy of a newer format (refused).
    NewerFormat,
    /// A valid copy one up holding another page (it fails the page checks).
    WrongPage,
}

fn slots(p: &SimPlatform, pair: Pair) -> ([u64; 2], usize) {
    match pair {
        Pair::Volume => ([0, block(p)], block(p) as usize),
        Pair::Page(n) => ([page_at(p, n, 0), page_at(p, n, 1)], TABLE as usize),
    }
}

fn at(p: &SimPlatform, pair: Pair, which: Which) -> Vec<u64> {
    let (s, _) = slots(p, pair);
    let m = media(p);
    let generation = |a: u64| {
        if m[a as usize..a as usize + 8] == *b"SIOSLOT\0" {
            u64_at(&m, a as usize + 56)
        } else {
            0
        }
    };
    let newest = usize::from(generation(s[1]) > generation(s[0]));
    match which {
        Which::Newest => vec![s[newest]],
        Which::Older => vec![s[1 - newest]],
        Which::SlotB => vec![s[1]],
        Which::Both => s.to_vec(),
    }
}

/// A page slot at `a` holding page `holds`'s empty page, numbered one
/// above everything.
#[allow(clippy::too_many_arguments)]
fn crafted(
    p: &SimPlatform,
    page: u32,
    a: u64,
    generation: u64,
    prev: u32,
    object: u32,
    foreign: bool,
    holds: u32,
) -> Vec<u8> {
    let m = media(p);
    let mut volume_uuid: [u8; 16] = m[32..48].try_into().unwrap();
    if foreign {
        volume_uuid[0] ^= 1;
    }
    let mut slot = vec![0u8; TABLE as usize];
    encode(
        &mut slot,
        &SlotFields {
            volume_uuid,
            object_id: (1 << 61) | u64::from(object),
            generation,
            owner_generation: 1,
            slot_offset: a,
            slot_index: if a == page_at(p, page, 1) {
                SlotIndex::B
            } else {
                SlotIndex::A
            },
            log2_slot_size: LOG2_TABLE,
            prev_header_crc32c: prev,
            format_major: FORMAT_2,
            meta_seq: max_meta_seq(p) + 1,
            compat_flags: 0,
        },
        &empty_page(holds),
    )
    .unwrap();
    slot
}

/// A valid copy at `a` one generation up, chained, holding the next
/// page's payload.
fn next_page_copy(p: &SimPlatform, page: u32, a: u64, generation: u64, prev: u32) -> Vec<u8> {
    let mut slot = crafted(p, page, a, generation, prev, page, false, page + 1);
    // Numbered just above the page's other copy, below the newest writes: a
    // tear elsewhere stays the write in flight.
    let other = if a == page_at(p, page, 1) {
        page_at(p, page, 0)
    } else {
        page_at(p, page, 1)
    };
    let below = u64_at(&media(p), other as usize + 96) + 1;
    slot[96..104].copy_from_slice(&below.to_le_bytes());
    rehash(&mut slot);
    slot
}

fn damage(p: &SimPlatform, pair: Pair, which: Which, how: Damage) {
    let (s, len) = slots(p, pair);
    for a in at(p, pair, which) {
        let m = media(p);
        let mut slot = m[a as usize..a as usize + len].to_vec();
        let other = if a == s[0] { s[1] } else { s[0] };
        let (wg, wcrc) = (
            u64_at(&m, other as usize + 56),
            u32_at(&m, other as usize + 8),
        );
        let page = match pair {
            Pair::Page(n) => n,
            Pair::Volume => u32::MAX,
        };
        let crafted = |generation, prev, object, foreign| {
            crafted(p, page, a, generation, prev, object, foreign, page)
        };
        match how {
            Damage::Torn => slot[128] ^= 0xff,
            Damage::Header => slot[120] ^= 0xff,
            Damage::Garbage => slot[0] ^= 0xff,
            Damage::Blank => slot.fill(0),
            Damage::Unreadable => {
                let file = p.with_world(|w| w.file_id(&DIR.into(), CONTAINER).unwrap());
                p.with_world(|w| {
                    w.faults_mut().bad_ranges.push((file, a, a + len as u64));
                });
                continue;
            }
            Damage::Incompatible => {
                slot[16] |= 0x80;
                rehash(&mut slot);
            }
            Damage::Misplaced => slot = m[other as usize..other as usize + len].to_vec(),
            Damage::Fork => slot = crafted(wg, 0, page, false),
            Damage::BrokenChain => {
                slot = crafted(wg + 1, wcrc ^ 1, page, false);
                slot[128 + 4] ^= 0xff;
            }
            Damage::Skipped => {
                slot = crafted(wg + 2, wcrc, page, false);
                slot[128 + 4] ^= 0xff;
            }
            Damage::OlderGeneration => slot = crafted(1, 0, page, false),
            Damage::OtherObject => slot = crafted(wg + 1, wcrc, page + 1, false),
            Damage::OtherStore => slot = crafted(wg + 1, wcrc, page, true),
            Damage::NewerFormat => {
                slot = crafted(wg + 1, wcrc, page, false);
                // `format_major` (header offset 14), past what encode makes.
                slot[14..16].copy_from_slice(&(FORMAT_2 + 1).to_le_bytes());
                rehash(&mut slot);
            }
            Damage::WrongPage => {
                slot = next_page_copy(p, page, a, wg + 1, wcrc);
            }
        }
        put(p, a, &slot);
    }
}

/// What the spec makes of a row.
#[derive(Debug, Default)]
struct Verdict {
    /// The open fails: `Some(true)` refused (`Unsupported(FormatVersion)`),
    /// `Some(false)` corrupt (no volume record).
    fails: Option<bool>,
    healed: Option<Healed>,
    volume_stale: bool,
    stale: &'static [u32],
    lost: &'static [u32],
    read_only: bool,
    /// The regions served (with their data).
    regions: &'static [&'static str],
    /// Every damaged slot reported, and what it is.
    damaged: &'static [(Pair, Which, SlotDamage)],
}

struct Row {
    name: &'static str,
    history: History,
    damage: &'static [(Pair, Which, Damage)],
    spec: Verdict,
}

fn damaged(row: &Row, seed: u64) -> SimPlatform {
    let p = store(seed, row.history);
    for &(pair, which, how) in row.damage {
        damage(&p, pair, which, how);
    }
    p
}

fn copy_of(p: &SimPlatform, accept: bool) -> Result<CopyReport, Error> {
    copy_store(
        p.clone(),
        Path::new(DIR),
        Path::new("/copy"),
        CopyOptions {
            store: opts(),
            accept_stale_pages: accept,
        },
    )
}

/// The regions a store serves, sorted; their data as [`store`] wrote it.
fn served(s: &Store<SimPlatform>, name: &str) -> Vec<String> {
    let mut names: Vec<String> = s.regions().into_iter().map(|(n, _)| n).collect();
    names.sort();
    if names.iter().any(|n| n == "p") {
        let reg = s.page_region("p").unwrap();
        let mut back = vec![0u8; B as usize];
        reg.read(reg.pos(0).unwrap(), &mut back).unwrap();
        assert!(back == [5; B as usize], "{name}: the region's data");
    }
    if names.iter().any(|n| n == "s") {
        assert_eq!(
            s.slot("s").unwrap().read().as_deref(),
            Some(&b"object"[..]),
            "{name}: the slot's object"
        );
    }
    names
}

/// The copy: its report names the stale and lost pages; it opens
/// writable, nothing stale or frozen, serves the regions the source
/// served, and audits clean.
fn check_copy(p: &SimPlatform, name: &str, spec: &Verdict, report: &CopyReport) {
    assert_eq!(
        (report.stale_pages.as_slice(), report.lost_pages.as_slice()),
        (spec.stale, spec.lost),
        "{name}: the copy's report"
    );
    let c = Store::open(p.clone(), Path::new("/copy"), opts())
        .unwrap_or_else(|e| panic!("{name}: the copy: {e:?}"));
    let r = c.report();
    assert!(
        !r.read_only && !r.metadata.frozen && r.metadata.damaged_slots.is_empty(),
        "{name}: the copy {:?}",
        r.metadata
    );
    assert_eq!(served(&c, name), spec.regions, "{name}: the copy's regions");
    audit_space_in(p, "/copy", &c);
    drop(c);
    audit_in(p, "/copy");
    assert_closed_in(p, "/copy");
}

/// The store writable: nothing to heal or replace any more, `regions`
/// served, the region written, a slot provisioned, the media audited.
fn check_writable(p: &SimPlatform, name: &str, regions: &[&str]) {
    let s = open(p);
    let m = &s.report().metadata;
    assert!(
        !s.report().read_only
            && !m.frozen
            && m.healed.is_none()
            && !m.volume_stale
            // A damaged older copy is reported until its page's next write.
            && m.damaged_slots.iter().all(|d| d.damage == SlotDamage::Older),
        "{name}: {m:?}"
    );
    assert_eq!(served(&s, name), regions, "{name}");
    if regions.contains(&"p") {
        let r = s.page_region("p").unwrap();
        r.write_durable(r.pos(B).unwrap(), &[7; B as usize])
            .unwrap_or_else(|e| panic!("{name}: a write: {e:?}"));
    }
    s.provision_slot("new")
        .unwrap_or_else(|e| panic!("{name}: a provisioning: {e:?}"));
    audit_space(p, &s);
    drop(s);
    audit_closed(p);
}

/// Spec §9 2 (D-130): page `n` carries the trace a given-up commit may have
/// left, its regions read only while any page is stale or lost: both its
/// slots Valid with the same payload, or its older slot damaged (neither
/// Valid nor Blank; the open reports it).
fn traced(p: &SimPlatform, s: &Store<SimPlatform>, n: u32) -> bool {
    let m = media(p);
    let [a, b] = [page_at(p, n, 0), page_at(p, n, 1)].map(|x| x as usize);
    let t = TABLE as usize;
    let valid = |x: usize| {
        store_io_format::slot::validate(
            &m[x..x + t],
            &store_io_format::slot::Expect {
                volume_uuid: m[32..48].try_into().unwrap(),
                object_id: (1 << 61) | u64::from(n),
                slot_offset: x as u64,
                slot_index: if x == b { SlotIndex::B } else { SlotIndex::A },
                log2_slot_size: LOG2_TABLE,
                format_major: FORMAT_2,
                numbered: true,
            },
        )
        .is_valid()
    };
    let twins = valid(a) && valid(b) && m[a + 128..a + t] == m[b + 128..b + t];
    let older_damaged = s
        .report()
        .metadata
        .damaged_slots
        .iter()
        .any(|d| d.page == Some(n));
    twins || older_damaged
}

fn not_written(r: Result<(), Error>, cause: NotWrittenCause, what: &str) {
    match r {
        Err(Error::NotWritten { cause: c, .. }) if c == cause => {}
        r => panic!("{what}: {r:?}, not NotWritten({cause:?})"),
    }
}

#[allow(clippy::too_many_lines)]
fn run(row: &Row, seed: u64) {
    let name = row.name;
    let spec = &row.spec;
    // The slots a row names, as the undamaged store has them.
    let fresh = store(seed, row.history);
    let mut want_damaged: Vec<(Option<u32>, bool, SlotDamage)> = spec
        .damaged
        .iter()
        .flat_map(|&(pair, which, d)| {
            let b = slots(&fresh, pair).0[1];
            let page = match pair {
                Pair::Volume => None,
                Pair::Page(n) => Some(n),
            };
            at(&fresh, pair, which)
                .into_iter()
                .map(move |a| (page, a == b, d))
        })
        .collect();
    want_damaged.sort_by_key(|d| (d.0, d.1));

    let p = damaged(row, seed);
    let before = judged(&p);
    let opened = Store::open(p.clone(), Path::new(DIR), opts());
    if let Some(refused) = spec.fails {
        assert!(
            before.is_none(),
            "{name}: the judgement opens it: {before:?}"
        );
        let e = opened.map(|_| ()).unwrap_err();
        let unsupported = matches!(
            e,
            Error::Unsupported {
                what: Capability::FormatVersion
            }
        );
        if refused {
            assert!(unsupported, "{name}: {e:?}");
        } else {
            assert!(matches!(e, Error::Corruption { .. }), "{name}: {e:?}");
        }
        let ro = Store::open_readonly(p.clone(), Path::new(DIR), opts())
            .map(|_| ())
            .unwrap_err();
        let c = copy_of(&p, true).map(drop).unwrap_err();
        for (what, e) in [("a read-only open", ro), ("the copy", c)] {
            let as_open = if refused {
                matches!(
                    e,
                    Error::Unsupported {
                        what: Capability::FormatVersion
                    }
                )
            } else {
                matches!(e, Error::Corruption { .. })
            };
            assert!(as_open, "{name}: {what}: {e:?}");
        }
        return;
    }
    let s = opened.unwrap_or_else(|e| panic!("{name}: {e:?}"));
    report_agrees(
        &before.unwrap_or_else(|| panic!("{name}: the judgement refuses it")),
        s.report(),
        name,
    );
    let r = s.report();
    let m = &r.metadata;
    let found = (
        m.healed,
        m.volume_stale,
        m.stale_pages.clone(),
        m.lost_pages.clone(),
        r.read_only,
    );
    let want = (
        spec.healed,
        spec.volume_stale,
        spec.stale.to_vec(),
        spec.lost.to_vec(),
        spec.read_only,
    );
    assert_eq!(found, want, "{name}: {m:?}");
    let got_damaged: Vec<_> = m
        .damaged_slots
        .iter()
        .map(|d| (d.page, d.slot_b, d.damage))
        .collect();
    assert_eq!(got_damaged, want_damaged, "{name}: the damaged slots");
    let unreadable = row.damage.iter().any(|d| matches!(d.2, Damage::Unreadable));
    assert_eq!(m.retried_reads > 0, unreadable, "{name}: retried reads");
    let frozen = !spec.stale.is_empty() || !spec.lost.is_empty();
    assert_eq!(m.frozen, frozen, "{name}");
    assert_eq!(served(&s, name), spec.regions, "{name}");
    // What the standing allows (spec §9 2): regions on a stale page, and
    // while any page is stale or lost those of a page whose older copy is
    // damaged, are read only; nothing is provisioned while frozen.
    let page0_traced = frozen && traced(&p, &s, 0);
    let p_writable =
        !spec.read_only && spec.regions.contains(&"p") && !spec.stale.contains(&0) && !page0_traced;
    if spec.regions.contains(&"p") {
        let reg = s.page_region("p").unwrap();
        let w = reg.write_durable(reg.pos(B).unwrap(), &[7; B as usize]);
        if p_writable {
            w.unwrap_or_else(|e| panic!("{name}: a write: {e:?}"));
        } else {
            not_written(w.map(drop), NotWrittenCause::ReadOnly, name);
        }
    }
    let provisioned = s.provision_slot("x").map(drop);
    if spec.read_only {
        not_written(provisioned, NotWrittenCause::ReadOnly, name);
        not_written(
            s.accept_page_rollback(spec.stale),
            NotWrittenCause::ReadOnly,
            name,
        );
    } else if frozen {
        not_written(provisioned, NotWrittenCause::Frozen, name);
    } else {
        provisioned.unwrap_or_else(|e| panic!("{name}: a provisioning: {e:?}"));
    }
    drop(s);

    // The ways out, each from the damaged store afresh.
    if !frozen {
        // The open itself was the way out (a heal, the volume record
        // rewritten, a latent sector error remapped by that write): the
        // next open finds nothing to do.
        p.with_world(|w| w.faults_mut().bad_ranges.clear());
        let mut regions = spec.regions.to_vec();
        regions.push("x");
        regions.sort_unstable();
        check_writable(&p, name, &regions);
        return;
    }
    let p = damaged(row, seed);
    // A plain copy of a frozen store is refused; one accepting stale pages
    // copies what the source serves.
    not_written(copy_of(&p, false).map(drop), NotWrittenCause::Frozen, name);
    let report = copy_of(&p, true).unwrap_or_else(|e| panic!("{name}: copy: {e:?}"));
    check_copy(&p, name, spec, &report);
    if spec.read_only {
        // Read only: copied (the volume record's evidence kept) is the way.
        return;
    }
    // Accepting the rollback: the store found as the row says, then a
    // latent sector error remapped by the rewrite; stale pages accepted
    // (the store still frozen while a page is lost), a lost one refused.
    let p = damaged(row, seed);
    let s = open(&p);
    assert_eq!(
        (
            s.report().metadata.stale_pages.as_slice(),
            s.report().metadata.lost_pages.as_slice()
        ),
        (spec.stale, spec.lost),
        "{name}: before the accept"
    );
    p.with_world(|w| w.faults_mut().bad_ranges.clear());
    if !spec.stale.is_empty() {
        s.accept_page_rollback(spec.stale)
            .unwrap_or_else(|e| panic!("{name}: accept: {e:?}"));
    }
    if !spec.lost.is_empty() {
        assert!(s.frozen(), "{name}: unfrozen with a page lost");
        // An accepted page's two slots hold the same payload: while a page
        // is lost, its regions stay read only.
        if spec.stale.contains(&0) && spec.regions.contains(&"p") {
            assert!(traced(&p, &s, 0), "{name}: no trace of the accept");
            let reg = s.page_region("p").unwrap();
            not_written(
                reg.write_durable(reg.pos(B).unwrap(), &[7; B as usize])
                    .map(drop),
                NotWrittenCause::ReadOnly,
                name,
            );
        }
        not_written(
            s.accept_page_rollback(spec.lost),
            NotWrittenCause::NotReady,
            name,
        );
        return;
    }
    drop(s);
    check_writable(&p, name, spec.regions);
}

use Damage::*;
use History::*;
use Pair::*;
use SlotDamage::{Damage as Bad, InFlight, Older as OlderBad};
use Which::*;

const BOTH: &[&str] = &["p", "s"];
/// Page 0's older copy is the winner: from before the slot's provisioning.
const OLDER: &[&str] = &["p"];

#[allow(clippy::too_many_lines)]
fn rows() -> Vec<Row> {
    vec![
        // The volume record.
        Row {
            name: "volume newest torn",
            history: Clean,
            damage: &[(Volume, Newest, Torn)],
            spec: Verdict {
                healed: Some(Healed::VolumeRecord),
                regions: BOTH,
                damaged: &[(Volume, Newest, InFlight)],
                ..Verdict::default()
            },
        },
        Row {
            name: "volume older torn (an older copy, damaged: reported)",
            history: Clean,
            damage: &[(Volume, Older, Torn)],
            spec: Verdict {
                regions: BOTH,
                damaged: &[(Volume, Older, OlderBad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "volume newest garbage",
            history: Clean,
            damage: &[(Volume, Newest, Garbage)],
            spec: Verdict {
                volume_stale: true,
                regions: BOTH,
                damaged: &[(Volume, Newest, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "volume older garbage",
            history: Clean,
            damage: &[(Volume, Older, Garbage)],
            spec: Verdict {
                volume_stale: true,
                regions: BOTH,
                damaged: &[(Volume, Older, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            // Only create and the close wrote it: the winner is
            // generation 1, and a blank slot B beside it reads as never
            // written.
            name: "volume newest blank after a clean close (slot B, never written)",
            history: Clean,
            damage: &[(Volume, Newest, Blank)],
            spec: Verdict {
                regions: BOTH,
                ..Verdict::default()
            },
        },
        Row {
            name: "volume newest blank after a stop (generation 2 or more: damage)",
            history: Stopped,
            damage: &[(Volume, Newest, Blank)],
            spec: Verdict {
                volume_stale: true,
                regions: BOTH,
                damaged: &[(Volume, Newest, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "volume older blank (slot A: no history leaves it so)",
            history: Clean,
            damage: &[(Volume, Older, Blank)],
            spec: Verdict {
                volume_stale: true,
                regions: BOTH,
                damaged: &[(Volume, Older, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "volume newest unreadable",
            history: Clean,
            damage: &[(Volume, Newest, Unreadable)],
            spec: Verdict {
                volume_stale: true,
                regions: BOTH,
                damaged: &[(Volume, Newest, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "volume older unreadable",
            history: Clean,
            damage: &[(Volume, Older, Unreadable)],
            spec: Verdict {
                volume_stale: true,
                regions: BOTH,
                damaged: &[(Volume, Older, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "volume newest damaged header (a candidate, its number unknown)",
            history: Clean,
            damage: &[(Volume, Newest, Header)],
            spec: Verdict {
                healed: Some(Healed::VolumeRecord),
                regions: BOTH,
                damaged: &[(Volume, Newest, InFlight)],
                ..Verdict::default()
            },
        },
        Row {
            name: "volume older damaged header (the winner's predecessor)",
            history: Clean,
            damage: &[(Volume, Older, Header)],
            spec: Verdict {
                regions: BOTH,
                damaged: &[(Volume, Older, OlderBad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "volume both garbage (no volume record)",
            history: Clean,
            damage: &[(Volume, Both, Garbage)],
            spec: Verdict {
                fails: Some(false),
                ..Verdict::default()
            },
        },
        Row {
            name: "volume older incompatible (refused)",
            history: Clean,
            damage: &[(Volume, Older, Incompatible)],
            spec: Verdict {
                fails: Some(true),
                ..Verdict::default()
            },
        },
        Row {
            name: "volume newest incompatible (refused)",
            history: Clean,
            damage: &[(Volume, Newest, Incompatible)],
            spec: Verdict {
                fails: Some(true),
                ..Verdict::default()
            },
        },
        // Page 0, holding the regions (A generation 3, B 2).
        Row {
            name: "page newest torn after a clean close",
            history: Clean,
            damage: &[(Page(0), Newest, Torn)],
            spec: Verdict {
                stale: &[0],
                regions: OLDER,
                damaged: &[(Page(0), Newest, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "page newest torn after a stop (not the last write)",
            history: Stopped,
            damage: &[(Page(0), Newest, Torn)],
            spec: Verdict {
                stale: &[0],
                regions: OLDER,
                damaged: &[(Page(0), Newest, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            // Spec §7 "what cannot be told apart" (b): the last durable
            // write torn reads as the write in flight: healed, undone.
            name: "page newest torn straight after its write (healed, undone)",
            history: AfterPage,
            damage: &[(Page(0), Newest, Torn)],
            spec: Verdict {
                healed: Some(Healed::Page(0)),
                regions: OLDER,
                damaged: &[(Page(0), Newest, InFlight)],
                ..Verdict::default()
            },
        },
        Row {
            name: "page older torn (reported)",
            history: Clean,
            damage: &[(Page(0), Older, Torn)],
            spec: Verdict {
                regions: BOTH,
                damaged: &[(Page(0), Older, OlderBad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "page both torn (lost: its regions unknown)",
            history: Clean,
            damage: &[(Page(0), Both, Torn)],
            spec: Verdict {
                lost: &[0],
                damaged: &[(Page(0), Both, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "page newest garbage",
            history: Clean,
            damage: &[(Page(0), Newest, Garbage)],
            spec: Verdict {
                stale: &[0],
                regions: OLDER,
                damaged: &[(Page(0), Newest, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "page older garbage",
            history: Clean,
            damage: &[(Page(0), Older, Garbage)],
            spec: Verdict {
                stale: &[0],
                regions: BOTH,
                damaged: &[(Page(0), Older, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "page newest blank (generation 2 or more: damage)",
            history: Clean,
            damage: &[(Page(0), Newest, Blank)],
            spec: Verdict {
                stale: &[0],
                regions: OLDER,
                damaged: &[(Page(0), Newest, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "page older blank (no history leaves it blank)",
            history: Clean,
            damage: &[(Page(0), Older, Blank)],
            spec: Verdict {
                stale: &[0],
                regions: BOTH,
                damaged: &[(Page(0), Older, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "page newest unreadable",
            history: Clean,
            damage: &[(Page(0), Newest, Unreadable)],
            spec: Verdict {
                stale: &[0],
                regions: OLDER,
                damaged: &[(Page(0), Newest, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "page older unreadable",
            history: Clean,
            damage: &[(Page(0), Older, Unreadable)],
            spec: Verdict {
                stale: &[0],
                regions: BOTH,
                damaged: &[(Page(0), Older, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "page newest damaged header after a clean close",
            history: Clean,
            damage: &[(Page(0), Newest, Header)],
            spec: Verdict {
                stale: &[0],
                regions: OLDER,
                damaged: &[(Page(0), Newest, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            // Spec §7 "what cannot be told apart" (a): taken for the write
            // in flight, so the page's last commit (the slot's
            // provisioning) is undone.
            name: "page newest damaged header after a stop (healed, its commit undone)",
            history: Stopped,
            damage: &[(Page(0), Newest, Header)],
            spec: Verdict {
                healed: Some(Healed::Page(0)),
                regions: OLDER,
                damaged: &[(Page(0), Newest, InFlight)],
                ..Verdict::default()
            },
        },
        Row {
            name: "page older damaged header (the winner's predecessor)",
            history: Clean,
            damage: &[(Page(0), Older, Header)],
            spec: Verdict {
                regions: BOTH,
                damaged: &[(Page(0), Older, OlderBad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "page both garbage (lost: its regions unknown)",
            history: Clean,
            damage: &[(Page(0), Both, Garbage)],
            spec: Verdict {
                lost: &[0],
                damaged: &[(Page(0), Both, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "page older misplaced (the newest copy's bytes)",
            history: Clean,
            damage: &[(Page(0), Older, Misplaced)],
            spec: Verdict {
                stale: &[0],
                regions: BOTH,
                damaged: &[(Page(0), Older, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            // F1 §5: of two Valid slots the higher generation wins.
            name: "page older of an older generation, valid (Confirmed)",
            history: Clean,
            damage: &[(Page(0), Older, OlderGeneration)],
            spec: Verdict {
                regions: BOTH,
                ..Verdict::default()
            },
        },
        Row {
            name: "page newest incompatible (refused)",
            history: Clean,
            damage: &[(Page(0), Newest, Incompatible)],
            spec: Verdict {
                fails: Some(true),
                ..Verdict::default()
            },
        },
        Row {
            name: "page older incompatible (refused: either slot)",
            history: Clean,
            damage: &[(Page(0), Older, Incompatible)],
            spec: Verdict {
                fails: Some(true),
                ..Verdict::default()
            },
        },
        // Page 1, never written since create (A generation 1, B blank).
        Row {
            name: "unwritten page's only copy garbage (lost)",
            history: Clean,
            damage: &[(Page(1), Newest, Garbage)],
            spec: Verdict {
                lost: &[1],
                regions: BOTH,
                damaged: &[(Page(1), Both, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "unwritten page's blank slot garbage",
            history: Clean,
            damage: &[(Page(1), SlotB, Garbage)],
            spec: Verdict {
                stale: &[1],
                regions: BOTH,
                damaged: &[(Page(1), SlotB, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "unwritten page both unreadable (lost)",
            history: Clean,
            damage: &[(Page(1), Both, Unreadable)],
            spec: Verdict {
                lost: &[1],
                regions: BOTH,
                damaged: &[(Page(1), Both, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "a fork: two valid copies of one generation (lost)",
            history: Clean,
            damage: &[(Page(1), SlotB, Fork)],
            spec: Verdict {
                lost: &[1],
                regions: BOTH,
                damaged: &[(Page(1), Both, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "a torn copy one up, chained to something else (damage)",
            history: Stopped,
            damage: &[(Page(1), SlotB, BrokenChain)],
            spec: Verdict {
                stale: &[1],
                regions: BOTH,
                damaged: &[(Page(1), SlotB, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "a torn copy two generations up (damage)",
            history: Stopped,
            damage: &[(Page(1), SlotB, Skipped)],
            spec: Verdict {
                stale: &[1],
                regions: BOTH,
                damaged: &[(Page(1), SlotB, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "another page's copy (damage)",
            history: Clean,
            damage: &[(Page(1), SlotB, OtherObject)],
            spec: Verdict {
                stale: &[1],
                regions: BOTH,
                damaged: &[(Page(1), SlotB, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "another store's copy (damage)",
            history: Clean,
            damage: &[(Page(1), SlotB, OtherStore)],
            spec: Verdict {
                stale: &[1],
                regions: BOTH,
                damaged: &[(Page(1), SlotB, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "a valid copy holding another page (lost: it fails the page checks)",
            history: Clean,
            damage: &[(Page(1), SlotB, WrongPage)],
            spec: Verdict {
                lost: &[1],
                regions: BOTH,
                damaged: &[(Page(1), SlotB, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            // The judgement takes page 0's tear for the write in flight
            // (page 1's copy is Valid), but page 1 fails the page checks,
            // so the heal is dropped (spec §7: only when nothing else is
            // lost) and the tear is damage.
            name: "a page's heal dropped beside a page failing its checks",
            history: AfterPage,
            damage: &[(Page(0), Newest, Torn), (Page(1), SlotB, WrongPage)],
            spec: Verdict {
                stale: &[0],
                lost: &[1],
                regions: OLDER,
                damaged: &[(Page(0), Newest, Bad), (Page(1), SlotB, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "a copy of a newer format (refused)",
            history: Clean,
            damage: &[(Page(1), SlotB, NewerFormat)],
            spec: Verdict {
                fails: Some(true),
                ..Verdict::default()
            },
        },
        Row {
            name: "unwritten page incompatible (refused)",
            history: Clean,
            damage: &[(Page(2), Newest, Incompatible)],
            spec: Verdict {
                fails: Some(true),
                ..Verdict::default()
            },
        },
        // Pairs across pairs.
        Row {
            name: "volume stale beside a page candidate (read only: copied)",
            history: Stopped,
            damage: &[(Volume, Newest, Garbage), (Page(0), Newest, Header)],
            spec: Verdict {
                volume_stale: true,
                stale: &[0],
                read_only: true,
                regions: OLDER,
                damaged: &[(Volume, Newest, Bad), (Page(0), Newest, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "page candidate beside other damage (not the write in flight)",
            history: Stopped,
            damage: &[(Page(0), Newest, Header), (Page(1), SlotB, Garbage)],
            spec: Verdict {
                stale: &[0, 1],
                regions: OLDER,
                damaged: &[(Page(0), Newest, Bad), (Page(1), SlotB, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            // The exemption: damage elsewhere does not stop a volume
            // record's heal.
            name: "volume candidate beside page damage (healed: exempt)",
            history: Clean,
            damage: &[(Volume, Newest, Torn), (Page(0), Older, Garbage)],
            spec: Verdict {
                healed: Some(Healed::VolumeRecord),
                stale: &[0],
                regions: BOTH,
                damaged: &[(Volume, Newest, InFlight), (Page(0), Older, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            // The garbage copy held the highest number (a garbage slot's
            // counts for nothing), so the torn record's is two above the
            // rest: not the write in flight, damage (spec §7: damage can
            // hide the true highest number).
            name: "volume candidate beside the garbage newest page (its number hidden)",
            history: Clean,
            damage: &[(Volume, Newest, Torn), (Page(0), Newest, Garbage)],
            spec: Verdict {
                volume_stale: true,
                stale: &[0],
                regions: OLDER,
                damaged: &[(Volume, Newest, Bad), (Page(0), Newest, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "two candidates (neither in flight: read only)",
            history: Stopped,
            damage: &[(Volume, Newest, Header), (Page(0), Newest, Header)],
            spec: Verdict {
                volume_stale: true,
                stale: &[0],
                read_only: true,
                regions: OLDER,
                damaged: &[(Volume, Newest, Bad), (Page(0), Newest, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            name: "a stale page and a lost page",
            history: Clean,
            damage: &[(Page(0), Older, Garbage), (Page(1), Newest, Garbage)],
            spec: Verdict {
                stale: &[0],
                lost: &[1],
                regions: BOTH,
                damaged: &[(Page(0), Older, Bad), (Page(1), Both, Bad)],
                ..Verdict::default()
            },
        },
        Row {
            // D-130: while a page is stale, a Confirmed page whose older
            // copy is damaged keeps its regions read only.
            name: "a damaged older copy beside a stale page (its regions read only)",
            history: Clean,
            damage: &[(Page(0), Older, Torn), (Page(1), SlotB, Garbage)],
            spec: Verdict {
                stale: &[1],
                regions: BOTH,
                damaged: &[(Page(0), Older, OlderBad), (Page(1), SlotB, Bad)],
                ..Verdict::default()
            },
        },
    ]
}

/// Every row: the open finds what the spec's table says, and every way out
/// ends writable or is refused as documented.
#[test]
fn test_each_damage_is_judged_as_the_spec_says_and_has_its_ways_out() {
    let rows = rows();
    let mut failures = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run(row, 1900 + i as u64);
        }));
        if let Err(e) = outcome {
            let msg = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_default();
            failures.push(format!("{}: {msg}", row.name));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} rows:\n{}",
        failures.len(),
        rows.len(),
        failures.join("\n")
    );
}

/// The space audit over entries the open keeps without serving them: one
/// of a kind this version does not know (an invalid entry, its name
/// reserved) and one reaching past the file's end (unavailable, cut
/// short): their spans allocated as the engine keeps them, the store still
/// writable.
#[test]
fn test_space_keeps_what_unserved_entries_hold() {
    use format2_util::craft::{region, write_regions};
    use store_io_engine::RegionKind;
    let p = SimPlatform::new(SimConfig::volatile(1990));
    drop(create(&p));
    let base = 2 * B + 2 * u64::from(PAGES) * TABLE;
    let fine = region(0, RegionKind::Page, "fine", base, 4 * B);
    let mut odd = region(1, RegionKind::Page, "odd", base + 6 * B, 4 * B);
    odd.kind_raw = Some(9);
    let mut short = region(2, RegionKind::Page, "short", base + 12 * B, 4 * B);
    short.extend = false;
    write_regions(&p, 0, &[fine, odd, short]);
    let s = open(&p);
    assert_eq!(s.report().metadata.invalid_entries, [(0, 1)]);
    let unavailable = s.unavailable_regions();
    assert!(
        unavailable.len() == 1
            && unavailable[0].name == "short"
            && matches!(
                unavailable[0].reason,
                store_io_engine::Unavailable::ContainerShort { .. }
            ),
        "{unavailable:?}"
    );
    assert!(!s.frozen() && !s.report().read_only);
    audit_space(&p, &s);
    // The invalid entry's name is reserved too.
    let taken = s.provision_slot("odd").map(drop);
    assert!(
        matches!(taken, Err(Error::AlreadyExists { .. })),
        "{taken:?}"
    );
}
