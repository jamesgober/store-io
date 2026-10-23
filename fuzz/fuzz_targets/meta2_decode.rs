//! Format-2 payloads on hostile bytes: the volume record, a table page with
//! its entries, and a region header.
//!
//! Properties, beyond never panicking: whatever a decoder accepts encodes
//! back to exactly the bytes it read (no field or reserved byte is accepted
//! and then dropped), an entry that passes its checks converts back to the
//! raw entry it came from, and a header's extents coalesce into runs that
//! map like them (`footprint::runs`).

#![no_main]

use libfuzzer_sys::fuzz_target;
use store_io_format::footprint::{Run, runs};
use store_io_format::meta2::{
    EntryLimits, RawEntry, VOLUME_META2_LEN, VolumeMeta2, decode_page, decode_region_header,
    encode_page, encode_region_header,
};

fuzz_target!(|data: &[u8]| {
    if let Ok(v) = VolumeMeta2::decode(data) {
        let mut out = [0u8; VOLUME_META2_LEN];
        let n = v
            .encode(&mut out)
            .expect("an accepted volume record encodes");
        assert_eq!(&out[..n], data, "volume record not canonical");
    }

    // The page index is taken from the bytes, so accepted pages are reached.
    if data.len() >= 4 {
        let index = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        if let Ok(page) = decode_page(data, index, 169) {
            let raws: Vec<RawEntry> = page.entries().collect();
            assert_eq!(raws.len(), usize::from(page.header.count), "entry count");
            let mut out = vec![0u8; data.len()];
            let n = encode_page(&mut out, &page.header, &raws).expect("an accepted page encodes");
            assert_eq!(n, data.len());
            assert_eq!(out, data, "page not canonical");
            let lim = EntryLimits {
                log2_block: 12,
                table_end: 1 << 20,
                file_len: 1 << 40,
                page_meta_seq: u64::MAX,
            };
            for raw in &raws {
                if let Ok(e) = raw.check(&lim) {
                    assert_eq!(&e.to_raw(), raw, "a checked entry is not its raw form");
                }
                if let Some((a, b)) = raw.reserved_span(&lim) {
                    assert!(
                        lim.table_end <= a && a < b && b <= lim.file_len,
                        "reserved span"
                    );
                }
            }
        }
    }

    if let Ok(v) = decode_region_header(data, 12, u64::MAX) {
        let ext: Vec<(u64, u64)> = v.extents().collect();
        let mut out = vec![0u8; data.len()];
        let n = encode_region_header(&mut out, &v.header, &ext, v.object())
            .expect("an accepted region header encodes");
        assert_eq!(&out[..n], data, "region header not canonical");
        runs_map_like(&ext);
    }
});

/// The runs of any extent list (spec §11): together exactly the extents'
/// bytes, contiguous in data order, maximal, and each extent's first and
/// last bytes mapped where the extent says.
fn runs_map_like(ext: &[(u64, u64)]) {
    let r: Vec<Run> = runs(ext.iter().copied()).collect();
    let total = ext.iter().try_fold(0u64, |t, e| t.checked_add(e.1));
    let Some(total) = total else {
        return;
    };
    assert_eq!(r.iter().map(|r| r.len).sum::<u64>(), total, "run lengths");
    for w in r.windows(2) {
        assert_eq!(w[0].start + w[0].len, w[1].start, "runs not contiguous");
        assert_ne!(
            w[0].file.checked_add(w[0].len),
            Some(w[1].file),
            "runs not maximal"
        );
    }
    let mut at = 0u64;
    for &(file, len) in ext.iter().filter(|e| e.1 > 0) {
        for (pos, want) in [(at, file), (at + (len - 1), file + (len - 1))] {
            let run = r
                .iter()
                .find(|r| pos >= r.start && pos - r.start < r.len)
                .expect("an extent outside every run");
            assert_eq!(run.file + (pos - run.start), want, "an extent mapped elsewhere");
        }
        at += len;
    }
}
