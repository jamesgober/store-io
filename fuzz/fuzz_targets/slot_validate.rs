//! `slot::validate` on hostile bytes.
//!
//! Two modes, chosen by the first input byte:
//! - **raw**: the bytes as read from disk, which exercises the checks before
//!   the header CRC (length, blank, magic, header length, CRC);
//! - **fixed CRCs**: the same bytes with the magic set and both CRCs
//!   recomputed, so every later check (version, flags, impossible fields,
//!   identity, location, payload CRC) is reached with hostile values.
//!
//! Properties, beyond never panicking:
//! - validation is deterministic;
//! - an accepted slot's payload lies inside the slot;
//! - an accepted slot with no feature flags and the format-1 header length
//!   is exactly what `encode` writes for the same fields and payload (the
//!   format has one encoding per value), and its header CRC is the one
//!   `encode` returns.

#![no_main]

use libfuzzer_sys::fuzz_target;
use store_io_format::crc32c::crc32c;
use store_io_format::slot::{self, Expect, SlotFields, SlotIndex, SlotStatus};

const MAGIC: &[u8; 8] = b"SIOSLOT\0";

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut x = [0u8; 8];
    x.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(x)
}

/// Sets the magic and recomputes both CRCs as a writer would, whatever the
/// other fields say (header length and payload length are clamped only to
/// keep the CRC ranges inside the buffer).
fn fix_crcs(s: &mut [u8]) {
    s[..8].copy_from_slice(MAGIC);
    let header_len = usize::from(u16_at(s, 12)).clamp(128, s.len());
    let payload_len = (u32_at(s, 72) as usize).min(s.len() - header_len);
    let payload = crc32c(&s[header_len..header_len + payload_len]);
    s[28..32].copy_from_slice(&payload.to_le_bytes());
    s[8..12].fill(0);
    let header = crc32c(&s[..header_len]);
    s[8..12].copy_from_slice(&header.to_le_bytes());
}

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let Some((&size, rest)) = rest.split_first() else {
        return;
    };
    // 512 B to 4 KiB keeps runs fast; sizes beyond are the same code.
    let log2 = 9 + size % 4;
    let len = 1usize << log2;
    let mut s = vec![0u8; len];
    let n = rest.len().min(len);
    s[..n].copy_from_slice(&rest[..n]);
    if mode & 1 == 1 {
        fix_crcs(&mut s);
    }
    // Expect the slot's own identity half the time (to reach the payload
    // check), the input's other bytes otherwise.
    let own = mode & 2 == 2;
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&s[32..48]);
    let e = Expect {
        volume_uuid: if own { uuid } else { [mode; 16] },
        object_id: if own { u64_at(&s, 48) } else { u64::from(mode) },
        slot_offset: if own {
            u64_at(&s, 64)
        } else {
            u64::from(size) << 12
        },
        slot_index: if s[76] == 1 {
            SlotIndex::B
        } else {
            SlotIndex::A
        },
        log2_slot_size: if mode & 4 == 4 { log2 } else { mode >> 3 },
    };
    let status = slot::validate(&s, &e);
    assert_eq!(
        slot::validate(&s, &e),
        status,
        "validation is not deterministic"
    );
    let SlotStatus::Valid(info) = status else {
        return;
    };
    let payload = info.payload(&s);
    assert!(
        payload.len() <= len - 128,
        "payload longer than the slot allows"
    );
    if info.ro_compat_flags != 0 || info.compat_flags != 0 || u16_at(&s, 12) != 128 {
        return;
    }
    let fields = SlotFields {
        volume_uuid: e.volume_uuid,
        object_id: e.object_id,
        generation: info.generation,
        owner_generation: info.owner_generation,
        slot_offset: e.slot_offset,
        slot_index: e.slot_index,
        log2_slot_size: e.log2_slot_size,
        prev_header_crc32c: info.prev_header_crc32c,
    };
    let mut out = vec![0u8; len];
    let crc = slot::encode(&mut out, &fields, payload).expect("an accepted slot re-encodes");
    let end = 128 + payload.len();
    assert_eq!(
        &out[..end],
        &s[..end],
        "an accepted slot is not in canonical form"
    );
    assert_eq!(
        crc, info.header_crc32c,
        "header CRC differs from the encoder's"
    );
});
