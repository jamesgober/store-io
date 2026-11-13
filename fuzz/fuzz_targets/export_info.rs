//! The export record (`regions.info`) on hostile bytes.
//!
//! Each input is decoded twice: as given, and resealed (its last four bytes
//! replaced by the CRC-32C of the rest), so mutations reach the structural
//! checks behind the checksum instead of stopping at it.
//!
//! Properties, beyond never panicking:
//! - a record `decode` accepts yields every region it counts, each with one
//!   span CRC per `SPAN` of its stream length, each record agreeing with
//!   its kind (none dropped on iteration);
//! - its lists hold exactly the counts its header gives, each strictly
//!   ascending;
//! - it encodes back to the same bytes (one encoding per record), and
//!   decoding that encoding gives the same record.

#![no_main]

use libfuzzer_sys::fuzz_target;
use store_io_format::crc32c::crc32c;
use store_io_format::exportinfo::{decode, encode_into};

fn check(data: &[u8]) {
    let Ok(info) = decode(data) else {
        return;
    };
    let h = info.header;
    assert_eq!(info.regions().count(), h.region_count as usize, "a region dropped");
    for (rec, spans) in info.regions() {
        assert_eq!(spans.len() as u64, rec.spans(), "spans not one per SPAN");
        assert!(rec.check().is_ok(), "an accepted record disagrees with its kind");
    }
    assert_eq!(info.omitted().count(), h.omitted_count as usize);
    assert_eq!(info.lost_pages().count(), h.lost_count as usize);
    assert_eq!(info.stale_pages().count(), h.stale_count as usize);
    assert_eq!(info.verified().count(), h.verified_count as usize);
    let ids: Vec<u64> = info.omitted().collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "omitted out of order");
    let mut out = vec![0u8; data.len()];
    let n = encode_into(&info, &mut out).expect("an accepted record encodes");
    assert_eq!(&out[..n], data, "not its one encoding");
    let again = decode(&out[..n]).expect("a re-encoded record decodes");
    assert_eq!(again.header, info.header);
}

fuzz_target!(|data: &[u8]| {
    check(data);
    if data.len() >= 4 {
        let mut sealed = data.to_vec();
        let n = sealed.len() - 4;
        let crc = crc32c(&sealed[..n]).to_le_bytes();
        sealed[n..].copy_from_slice(&crc);
        check(&sealed);
    }
});
