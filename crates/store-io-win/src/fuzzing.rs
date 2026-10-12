//! Fuzz entry point for the probe's parsers (built only with `--cfg
//! fuzzing`, which cargo-fuzz sets; never part of the public API).
//!
//! Every byte the probe parses comes from a driver or the device and is
//! treated as hostile: storage property descriptors, protocol-specific
//! (NVMe) answers, volume and extent structures. [`run`] feeds one input to
//! one parser, chosen by the first byte, and checks the parser's promises
//! beyond "never panics".

use crate::bytes::Reader;
use crate::probe::nvme::{
    parse_health_critical_warning, parse_identify_controller, parse_identify_namespace,
};
use crate::probe::storage::{
    bus_from_code, hypervisor_from, parse_access_alignment, parse_adapter, parse_device_descriptor,
    parse_device_number, parse_disk_extents, parse_numa, parse_write_cache, protocol_data,
    protocol_dword0,
};
use crate::probe::volume::{fs_kind, parse_storage_info};

fn printable(s: &str) -> bool {
    !s.is_empty() && s.trim() == s && s.chars().all(|c| c.is_ascii_graphic() || c == ' ')
}

/// Runs one parser on `data[1..]`, chosen by `data[0]`, and asserts its
/// properties. Panics on a violation (the fuzzer reports it).
pub fn run(data: &[u8]) {
    let Some((&which, rest)) = data.split_first() else {
        return;
    };
    // Pad half the inputs to a full NVMe page so deep offsets are reached.
    let mut page = rest.to_vec();
    if which & 0x80 == 0x80 {
        page.resize(4096, 0);
    }
    let b = page.as_slice();
    match which % 8 {
        0 => {
            if let Some(c) = parse_identify_controller(b) {
                for s in [&c.model, &c.firmware].into_iter().flatten() {
                    assert!(printable(s), "unprintable identify string {s:?}");
                }
            }
            let _ = parse_health_critical_warning(b);
        }
        1 => {
            if let Some(ns) = parse_identify_namespace(b) {
                assert!(
                    ns.lba_bytes.is_power_of_two() && ns.lba_bytes >= 512,
                    "impossible LBA size {}",
                    ns.lba_bytes
                );
            }
        }
        2 => {
            if let Some(p) = protocol_data(b) {
                // Exactly the slice the descriptor declares: the offset and
                // length fields of the protocol-specific data at byte 8.
                let r = Reader::new(b);
                let off = r.u32(8 + 16).map(|v| v as usize);
                let len = r.u32(8 + 20).map(|v| v as usize);
                let start = p.as_ptr() as usize - b.as_ptr() as usize;
                assert_eq!(
                    (Some(start), Some(p.len())),
                    (off.map(|o| 8 + o), len),
                    "protocol data not as declared"
                );
                assert!(!p.is_empty(), "empty protocol data accepted");
            }
            let _ = protocol_dword0(b);
        }
        3 => {
            if let Some(d) = parse_device_descriptor(b) {
                for s in [&d.vendor, &d.product, &d.revision].into_iter().flatten() {
                    assert!(printable(s), "unprintable descriptor string {s:?}");
                }
                let bus = bus_from_code(d.bus);
                let _ = hypervisor_from(bus, d.product.as_deref());
            }
        }
        4 => {
            let _ = parse_write_cache(b);
            let _ = parse_access_alignment(b);
            let _ = parse_adapter(b);
            let _ = parse_numa(b);
            let _ = parse_device_number(b);
            let _ = parse_storage_info(b);
        }
        5 => {
            if let Some((count, disk)) = parse_disk_extents(b) {
                let r = Reader::new(b);
                assert_eq!(
                    (Some(count), Some(disk)),
                    (r.u32(0), r.u32(8)),
                    "extents misread"
                );
                assert!(count > 0, "zero extents accepted as an answer");
            }
        }
        6 => {
            let _ = fs_kind(&String::from_utf8_lossy(rest));
        }
        _ => {
            // The reader itself: an ASCII field at any offset and length.
            let r = Reader::new(rest);
            let off = usize::from(which >> 3) * 7;
            if let Some(s) = r.ascii(off, usize::from(which)) {
                assert!(printable(&s), "unprintable field {s:?}");
                assert!(s.len() <= usize::from(which), "field longer than asked");
            }
        }
    }
}
