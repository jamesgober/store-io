//! The fixed-size metadata records on hostile bytes: the volume record and
//! the region header record.
//!
//! Properties, beyond never panicking: whatever a decoder accepts encodes
//! back, and decoding that encoding gives the same value (decode, encode and
//! decode agree), so a record store-io rewrites keeps every field it read.

#![no_main]

use libfuzzer_sys::fuzz_target;
use store_io_format::meta::{REGION_META_LEN, RegionMeta, VOLUME_META_LEN, VolumeMeta};

fuzz_target!(|data: &[u8]| {
    if let Ok(v) = VolumeMeta::decode(data) {
        let mut out = [0u8; VOLUME_META_LEN];
        let n = v
            .encode(&mut out)
            .expect("an accepted volume record encodes");
        assert_eq!(
            VolumeMeta::decode(&out[..n]),
            Ok(v),
            "volume record round trip"
        );
    }
    if let Ok(r) = RegionMeta::decode(data) {
        let mut out = [0u8; REGION_META_LEN];
        let n = r
            .encode(&mut out)
            .expect("an accepted region header encodes");
        assert_eq!(
            RegionMeta::decode(&out[..n]),
            Ok(r),
            "region header round trip"
        );
    }
});
