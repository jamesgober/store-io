//! Fuzz entry point for the probe's parsers (built only with `--cfg
//! fuzzing`, which cargo-fuzz sets; never part of the public API).
//!
//! Every byte the probe parses comes from the kernel or the device and is
//! treated as hostile: sysfs and procfs text, `mountinfo` lines, `uevent`
//! files, NVMe Identify and SMART pages, and the names and links device
//! identity reads. [`run`] feeds one input to one
//! parser, chosen by the first byte, and checks the parser's promises
//! beyond "never panics".

use crate::probe::block::{
    bus_for, classify_dm, classify_md, clean, parse_cache_mode, parse_id_ctrl, parse_id_ns,
    parse_numa, parse_smart, remote_scsi_driver, sectors_kb_to_bytes,
};
use crate::probe::fs::{
    basename, find_mount, mount_facts, parse_mountinfo_line, uevent_devname, unescape,
};
use crate::probe::identity::{jbd2_internal, parse_dev, usb_bridge, xfs_uses_other_devices};
use crate::probe::virt::{VirtInputs, classify, cpuinfo_has_hypervisor, kernel_version};
use store_io_core::identity::{DeviceId, DeviceIdentity, DeviceNode, normalize_name};

/// Splits `s` into up to `N` fields on `'\n'`; missing fields are empty.
fn fields<const N: usize>(s: &str) -> [&str; N] {
    let mut out = [""; N];
    for (slot, part) in out.iter_mut().zip(s.splitn(N, '\n')) {
        *slot = part;
    }
    out
}

fn opt(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
}

/// Runs one parser on `data[1..]`, chosen by `data[0]`, and asserts its
/// properties. Panics on a violation (the fuzzer reports it).
pub fn run(data: &[u8]) {
    let Some((&which, rest)) = data.split_first() else {
        return;
    };
    let text = String::from_utf8_lossy(rest);
    let t = text.as_ref();
    match which % 13 {
        0 => {
            if let Some(e) = parse_mountinfo_line(t) {
                assert_eq!(parse_mountinfo_line(t), Some(e), "not deterministic");
            }
            assert!(unescape(t).len() <= t.len(), "unescaping grew the string");
        }
        1 => {
            // The id, the path, then a mountinfo table of any number of
            // lines (the last field takes the rest).
            let [id, path, table] = fields::<3>(t);
            let mnt_id = id.trim().parse::<u64>().ok();
            let dev = rest
                .first()
                .map(|&b| (u32::from(b >> 4), u32::from(b & 0xF)));
            if let Some(m) = find_mount(table, mnt_id, dev, opt(path)) {
                assert!(
                    mnt_id == Some(m.id) || dev == Some((m.major, m.minor)),
                    "a mount that matches neither the id nor the device"
                );
            }
        }
        2 => {
            if let Some(n) = uevent_devname(t) {
                assert!(!n.is_empty(), "an empty device name");
                assert!(
                    !n.contains('/') && n.len() <= 64,
                    "a device name that is not one component"
                );
            }
        }
        3 => {
            if let Some(b) = basename(t) {
                assert!(
                    !b.is_empty() && !b.contains('/'),
                    "basename is not one component"
                );
            }
        }
        4 => {
            if let Some(c) = clean(t) {
                assert!(!c.is_empty() && c.len() <= 128, "clean() out of bounds");
                assert!(
                    c.chars().all(|ch| ch.is_ascii_graphic() || ch == ' '),
                    "unprintable"
                );
                assert_eq!(c.trim(), c, "untrimmed");
            }
        }
        5 => {
            if let Some(v) = sectors_kb_to_bytes(t) {
                assert!(v % 1024 == 0 || v == u32::MAX, "not a whole KiB count");
            }
            let _ = parse_numa(t);
            let _ = parse_cache_mode(t);
            let _ = kernel_version(t);
        }
        6 => {
            let [name, uuid] = fields::<2>(t);
            let _ = classify_dm(name, uuid);
        }
        7 => {
            let [level, policy, backlog] = fields::<3>(t);
            let _ = classify_md(level, opt(policy), opt(backlog));
        }
        8 => {
            let [name, path, transports, vendor, model] = fields::<5>(t);
            let transports: Vec<&str> = transports.split(',').filter(|t| !t.is_empty()).collect();
            let _ = bus_for(name, path, &transports, opt(vendor), opt(model));
            let _ = remote_scsi_driver(vendor);
        }
        9 => {
            let [fstype, options, super_options] = fields::<3>(t);
            let _ = mount_facts(fstype, options, super_options);
        }
        10 => {
            let [vendor, product, hv, release, cpuinfo] = fields::<5>(t);
            // Device-presence flags come from the selector, not the text.
            let flags = which / 13;
            let _ = classify(&VirtInputs {
                sys_vendor: opt(vendor),
                product_name: opt(product),
                hypervisor_type: opt(hv),
                cpu_hypervisor_flag: cpuinfo_has_hypervisor(cpuinfo),
                kernel_release: opt(release),
                vmbus_devices: flags & 1 == 1,
                virtio_devices: flags & 2 == 2,
            });
        }
        11 => {
            // Device identity: a `dev` file, a device's sysfs path, a
            // device name with jbd2's entries, XFS super options, a name.
            let [dev, path, devname, entries, options, raw] = fields::<6>(t);
            if let Some((major, minor)) = parse_dev(dev) {
                assert_eq!(
                    parse_dev(&format!("{major}:{minor}")),
                    Some((major, minor)),
                    "major:minor does not round-trip"
                );
            }
            if let Some(b) = usb_bridge(path) {
                assert!(
                    b.starts_with("usb-bridge:") && b.contains("/usb"),
                    "not a USB path: {b}"
                );
                // Everything after the bridge's SCSI host is the LUN's own.
                assert_eq!(
                    usb_bridge(&format!("{path}/x")),
                    Some(b),
                    "a deeper path named another bridge"
                );
            }
            let entries: Vec<String> = entries.split(',').map(str::to_owned).collect();
            if !devname.is_empty() && !devname.contains(',') {
                // An internal journal's entry makes it internal; without
                // any entry for the device, it is not.
                let mut with = entries.clone();
                with.push(format!("{devname}-1"));
                assert!(jbd2_internal(&with, devname), "an internal entry missed");
                let without: Vec<String> = entries
                    .iter()
                    .filter(|e| !e.starts_with(devname))
                    .cloned()
                    .collect();
                assert!(!jbd2_internal(&without, devname), "no entry, yet internal");
                // Another device's entry, or a bare or inode-less one, is
                // never this device's internal journal.
                // (`sda10-8` is not `sda1`'s: what follows the name must be
                // `-` and digits only, and a second `-` never is.)
                let x: String = options.chars().take(4).collect();
                if !x.is_empty() {
                    assert!(
                        !jbd2_internal(&[format!("{devname}{x}-8")], devname),
                        "another device's journal read as internal"
                    );
                }
                assert!(
                    !jbd2_internal(&[devname.to_owned()], devname),
                    "a bare name"
                );
                assert!(
                    !jbd2_internal(&[format!("{devname}-")], devname),
                    "no inode"
                );
            }
            let _ = xfs_uses_other_devices(options);
            if let Some(n) = normalize_name(raw) {
                assert!(!n.is_empty() && n.len() <= 256, "name length {}", n.len());
                assert!(
                    n.bytes()
                        .all(|b| (b.is_ascii_graphic() && !b.is_ascii_uppercase()) || b == b' '),
                    "an unnormalized byte in {n:?}"
                );
                assert_eq!(n.trim(), n, "untrimmed");
                assert!(!n.contains("  "), "a run of spaces");
                // A shared name always makes two devices one.
                let one = |minor| {
                    let mut d = DeviceId::node(DeviceNode::Linux { major: 8, minor });
                    d.add_name(raw);
                    DeviceIdentity {
                        devices: vec![d],
                        complete: true,
                    }
                };
                assert!(!one(0).distinct(&one(16)), "a shared name read as distinct");
            }
        }
        _ => {
            // NVMe pages are binary: pad short inputs so the parsers see a
            // full page as often as a short one.
            let mut page = rest.to_vec();
            if which & 0x80 == 0x80 {
                page.resize(4096, 0);
            }
            if let Some(c) = parse_id_ctrl(&page) {
                for s in [&c.model, &c.firmware].into_iter().flatten() {
                    assert!(
                        s.chars().all(|ch| ch.is_ascii_graphic() || ch == ' '),
                        "unprintable id"
                    );
                }
            }
            if let Some(ns) = parse_id_ns(&page) {
                assert!(
                    ns.lba_bytes == 0 || (ns.lba_bytes.is_power_of_two() && ns.lba_bytes >= 512),
                    "impossible LBA size {}",
                    ns.lba_bytes
                );
            }
            let _ = parse_smart(&page);
        }
    }
}
