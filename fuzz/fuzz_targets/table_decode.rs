//! The region table on hostile bytes.
//!
//! Properties, beyond never panicking:
//! - a table `decode_table` accepts yields every entry it counts (none is
//!   dropped silently on iteration);
//! - the accepted entries satisfy the rules the open relies on: ids unique and
//!   below `next_region_id`, names unique among ready entries, extents
//!   block-aligned and non-overlapping;
//! - it encodes back, and decoding that encoding gives the same table.

#![no_main]

use libfuzzer_sys::fuzz_target;
use store_io_format::meta::{RegionState, TableEntry, decode_table, encode_table};

fuzz_target!(|data: &[u8]| {
    let Some((&b, bytes)) = data.split_first() else {
        return;
    };
    // Block sizes 4 KiB to 64 KiB.
    let block = 1u64 << (12 + b % 5);
    let Ok(table) = decode_table(bytes, block) else {
        return;
    };
    let entries: Vec<TableEntry> = table.iter().collect();
    assert_eq!(entries.len(), table.len(), "an accepted entry was dropped");
    for (i, e) in entries.iter().enumerate() {
        assert!(
            e.region_id < table.next_region_id,
            "id not below next_region_id"
        );
        assert_eq!(e.offset % block, 0, "unaligned extent");
        for f in &entries[..i] {
            assert_ne!(e.region_id, f.region_id, "duplicate id");
            if e.state == RegionState::Ready && f.state == RegionState::Ready {
                assert_ne!(e.name, f.name, "duplicate ready name");
            }
            let span = |x: &TableEntry| (x.offset, x.offset + 2 * block + x.data_size);
            let ((s1, e1), (s2, e2)) = (span(e), span(f));
            assert!(e1 <= s2 || e2 <= s1, "overlapping extents");
        }
    }
    let mut out = vec![0u8; 16 + 64 * entries.len()];
    let n =
        encode_table(&mut out, table.next_region_id, &entries).expect("an accepted table encodes");
    let again = decode_table(&out[..n], block).expect("a re-encoded table decodes");
    assert_eq!(again.next_region_id, table.next_region_id);
    assert!(again.iter().eq(entries.iter().copied()), "table round trip");
});
