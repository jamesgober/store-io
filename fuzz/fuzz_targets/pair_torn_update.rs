//! The A/B crash guarantee, fuzzed: after any torn update, the pair decides
//! for the old version or the new one, never anything else.
//!
//! The input picks a history: the slot size, which slot holds the current
//! generation `g`, whether the other slot is blank (`g == 1`) or holds `g - 1`,
//! both payloads, and the new payload. The next update writes generation
//! `g + 1` to the other slot, and a crash tears that write: each 512-byte
//! sector of the slot holds either its old or its new bytes, as the input's
//! tear mask says (the tear granularity of a 512-byte-sector device).
//!
//! Properties:
//! - the winner is `g` or `g + 1`: never lost, forked, refused, empty or older;
//! - with no sector written the winner is `g`, confirmed; with every sector
//!   written it is `g + 1`, confirmed;
//! - the next write never targets the winner's slot.

#![no_main]

use libfuzzer_sys::fuzz_target;
use store_io_format::pair::{self, Winner};
use store_io_format::slot::{self, Expect, SlotFields, SlotIndex, SlotStatus};

const SECTOR: usize = 512;
const UUID: [u8; 16] = [0x5A; 16];
const OBJECT: u64 = 7;

struct Input<'a>(&'a [u8]);

impl Input<'_> {
    fn byte(&mut self) -> u8 {
        match self.0.split_first() {
            Some((&b, rest)) => {
                self.0 = rest;
                b
            }
            None => 0,
        }
    }

    fn bytes(&mut self, max: usize) -> Vec<u8> {
        let n = usize::from(self.byte()) * max / 255;
        let n = n.min(self.0.len());
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        head.to_vec()
    }
}

fn offset(base: u64, len: usize, i: SlotIndex) -> u64 {
    match i {
        SlotIndex::A => base,
        SlotIndex::B => base + len as u64,
    }
}

/// Encodes one version into a fresh slot image; returns it and its CRC.
fn version(
    len: usize,
    log2: u8,
    base: u64,
    i: SlotIndex,
    generation: u64,
    prev: u32,
    payload: &[u8],
) -> (Vec<u8>, u32) {
    let mut s = vec![0u8; len];
    let f = SlotFields {
        volume_uuid: UUID,
        object_id: OBJECT,
        generation,
        owner_generation: generation,
        slot_offset: offset(base, len, i),
        slot_index: i,
        log2_slot_size: log2,
        prev_header_crc32c: prev,
        format_major: store_io_format::slot::FORMAT_1,
        meta_seq: 0,
        compat_flags: 0,
    };
    let crc = slot::encode(&mut s, &f, payload).expect("valid fields encode");
    (s, crc)
}

fuzz_target!(|data: &[u8]| {
    let mut inp = Input(data);
    let log2 = 9 + inp.byte() % 4;
    let len = 1usize << log2;
    let max_payload = len - 128;
    let base = u64::from(inp.byte()) << 16;
    let current = if inp.byte() & 1 == 0 {
        SlotIndex::A
    } else {
        SlotIndex::B
    };
    let other = current.other();
    let g = 1 + u64::from(inp.byte() % 4);

    // The pair before the update: `current` holds g; `other` holds g - 1, or
    // is blank when g == 1.
    let older = inp.bytes(max_payload);
    let (old_other, older_crc) = if g == 1 {
        (vec![0u8; len], 0)
    } else {
        version(len, log2, base, other, g - 1, 0, &older)
    };
    let committed = inp.bytes(max_payload);
    let (cur, cur_crc) = version(len, log2, base, current, g, older_crc, &committed);

    // The update to g + 1 goes to `other`, and tears.
    let next = inp.bytes(max_payload);
    let (new_other, _) = version(len, log2, base, other, g + 1, cur_crc, &next);
    let sectors = len / SECTOR;
    let mut torn = old_other.clone();
    let mut written = 0;
    for k in 0..sectors {
        if inp.byte() & 1 == 1 {
            torn[k * SECTOR..(k + 1) * SECTOR]
                .copy_from_slice(&new_other[k * SECTOR..(k + 1) * SECTOR]);
            written += 1;
        }
    }

    let expect = |i: SlotIndex| Expect {
        volume_uuid: UUID,
        object_id: OBJECT,
        slot_offset: offset(base, len, i),
        slot_index: i,
        log2_slot_size: log2,
        format_major: store_io_format::slot::FORMAT_1,
        numbered: false,
    };
    let (a, b) = match current {
        SlotIndex::A => (&cur, &torn),
        SlotIndex::B => (&torn, &cur),
    };
    let report = pair::decide(
        slot::validate(a, &expect(SlotIndex::A)),
        slot::validate(b, &expect(SlotIndex::B)),
    );
    let Some((index, info)) = report.winner_info() else {
        panic!("no winner after a torn update: {report:?}");
    };
    assert!(
        info.generation == g || info.generation == g + 1,
        "winner generation {} after tearing {g} -> {}",
        info.generation,
        g + 1
    );
    if info.generation == g {
        assert_eq!(index, current);
    } else {
        assert_eq!(index, other);
        assert!(matches!(
            slot::validate(&torn, &expect(other)),
            SlotStatus::Valid(_)
        ));
    }
    if written == 0 {
        assert_eq!(report.winner, Winner::Confirmed(current), "{report:?}");
    }
    if written == sectors {
        assert_eq!(report.winner, Winner::Confirmed(other), "{report:?}");
    }
    assert_ne!(
        report.next_write(),
        Some(index),
        "the next write targets the winner"
    );
});
