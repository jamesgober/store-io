//! The untorn unit: the largest aligned write the stack guarantees is never
//! partially persisted after a power loss.
//!
//! Untorn is not durable: an untorn write still needs a durable completion or
//! a barrier. store-io promises nothing beyond the unit reported here, and
//! reports `None` (promise nothing) whenever the evidence is incomplete.
//!
//! The rule (Phase 0 critique C3, adopted as decision D-003):
//!
//! - **Raw NVMe** (store-io's own namespace access): `base = AWUPF + 1`. When
//!   NSFEAT.NSABP is set, `unit = NAWUPF + 1`, except that NAWUPF = 0 means
//!   "same as AWUPF" (`unit = base`); the boundary is `NABSPF + 1` blocks
//!   starting at block NABO, or none when NABSPF = 0. A boundary that cannot
//!   hold the unit, or NABO beyond NABSPF, is a specification violation: fall
//!   back to `base` with no boundary. A namespace shared by several
//!   controllers without NSABP gets 1 block (AWUPF may differ per controller).
//!   The unit is rounded down to a power of two and capped at the maximum
//!   transfer.
//! - **Linux files and block devices**: only what the running kernel reports
//!   (`statx` `STATX_WRITE_ATOMIC`), never the raw fields, because the kernel
//!   decides what `RWF_ATOMIC` accepts (Linux 7.0 ignores AWUPF entirely).
//! - **Windows**: the smaller of the filesystem's reported atomicity and the
//!   device's raw unit; unknown device unit means none. The filesystem field
//!   alone overstates (4096 bytes reported over a 512-byte device unit on the
//!   development machine).
//! - **macOS, SATA, SAS, anything else**: none.

use crate::evidence::{Bus, Evidence, PlatformKind};

/// Where the unit applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UntornPath {
    /// store-io's own raw NVMe namespace access.
    RawNvme,
    /// A file or block device through the operating system.
    File,
}

/// An untorn unit and the boundary writes must not cross.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UntornUnit {
    /// Unit size in bytes (a power of two).
    pub bytes: u32,
    /// Atomic boundary, if any: `(offset_blocks, size_blocks)`; a write must
    /// not cross a multiple of `size_blocks` offset by `offset_blocks`.
    pub boundary: Option<(u32, u32)>,
    /// The device fields contradict the specification; the fallback was used.
    pub spec_violation: bool,
}

/// Computes the untorn unit for a path, or `None` when nothing is guaranteed.
#[must_use]
pub fn untorn_unit(ev: &Evidence, path: UntornPath) -> Option<UntornUnit> {
    match (ev.platform, path) {
        (PlatformKind::Linux | PlatformKind::Sim, UntornPath::RawNvme) => raw_nvme(ev),
        (PlatformKind::Linux | PlatformKind::Sim, UntornPath::File) => {
            from_os(ev.fs.os_atomic_unit)
        }
        (PlatformKind::Windows, _) => {
            let device = raw_nvme(ev)?;
            let os = ev.fs.os_atomic_unit;
            if os == 0 {
                return None;
            }
            let bytes = prev_pow2(device.bytes.min(os));
            Some(UntornUnit { bytes, ..device })
        }
        (PlatformKind::MacOs, _) => None,
    }
}

fn from_os(bytes: u32) -> Option<UntornUnit> {
    (bytes != 0).then(|| UntornUnit {
        bytes: prev_pow2(bytes),
        boundary: None,
        spec_violation: false,
    })
}

fn raw_nvme(ev: &Evidence) -> Option<UntornUnit> {
    let d = &ev.device;
    if d.bus != Bus::Nvme || d.logical_block == 0 {
        return None;
    }
    let a = d.atomic;
    let awupf = a.awupf?;
    let base = u32::from(awupf) + 1;
    let (mut unit, mut boundary, mut violation) = (base, None, false);
    if a.nsabp {
        unit = if a.nawupf == 0 {
            base
        } else {
            u32::from(a.nawupf) + 1
        };
        if a.nabspf != 0 {
            let size = u32::from(a.nabspf) + 1;
            if u32::from(a.nabo) > u32::from(a.nabspf) || unit > size {
                unit = base;
                violation = true;
            } else {
                boundary = Some((u32::from(a.nabo), size));
            }
        }
    } else if d.multi_controller {
        unit = 1;
    }
    unit = prev_pow2(unit);
    if d.max_transfer != 0 {
        unit = unit.min(prev_pow2(d.max_transfer / d.logical_block).max(1));
    }
    let bytes = unit.checked_mul(d.logical_block)?;
    Some(UntornUnit {
        bytes,
        boundary,
        spec_violation: violation,
    })
}

/// Largest power of two ≤ `v` (0 for 0).
fn prev_pow2(v: u32) -> u32 {
    if v == 0 {
        0
    } else {
        1 << (31 - v.leading_zeros())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::AtomicFields;

    fn nvme(atomic: AtomicFields, lba: u32) -> Evidence {
        let mut ev = Evidence::unknown(PlatformKind::Linux);
        ev.device.bus = Bus::Nvme;
        ev.device.logical_block = lba;
        ev.device.max_transfer = 128 * 1024;
        ev.device.atomic = atomic;
        ev
    }

    fn raw(ev: &Evidence) -> Option<u32> {
        untorn_unit(ev, UntornPath::RawNvme).map(|u| u.bytes)
    }

    #[test]
    fn test_awupf_only_gives_base_unit() {
        let ev = nvme(
            AtomicFields {
                awupf: Some(7),
                ..AtomicFields::default()
            },
            512,
        );
        assert_eq!(raw(&ev), Some(8 * 512));
    }

    #[test]
    fn test_nawupf_zero_means_same_as_awupf() {
        let ev = nvme(
            AtomicFields {
                awupf: Some(7),
                nsabp: true,
                nawupf: 0,
                ..AtomicFields::default()
            },
            512,
        );
        assert_eq!(raw(&ev), Some(8 * 512));
        let ev = nvme(
            AtomicFields {
                awupf: Some(7),
                nsabp: true,
                nawupf: 31,
                ..AtomicFields::default()
            },
            512,
        );
        assert_eq!(raw(&ev), Some(32 * 512));
    }

    #[test]
    fn test_boundary_and_spec_violation() {
        let ok = nvme(
            AtomicFields {
                awupf: Some(0),
                nsabp: true,
                nawupf: 7,
                nabspf: 15,
                nabo: 4,
            },
            4096,
        );
        let u = untorn_unit(&ok, UntornPath::RawNvme);
        assert_eq!(
            u.map(|u| (u.bytes, u.boundary, u.spec_violation)),
            Some((8 * 4096, Some((4, 16)), false))
        );
        let bad = nvme(
            AtomicFields {
                awupf: Some(0),
                nsabp: true,
                nawupf: 31,
                nabspf: 7,
                nabo: 0,
            },
            4096,
        );
        let u = untorn_unit(&bad, UntornPath::RawNvme);
        assert_eq!(u.map(|u| (u.bytes, u.spec_violation)), Some((4096, true)));
    }

    #[test]
    fn test_multi_controller_without_nsabp_is_one_block() {
        let mut ev = nvme(
            AtomicFields {
                awupf: Some(63),
                ..AtomicFields::default()
            },
            512,
        );
        ev.device.multi_controller = true;
        assert_eq!(raw(&ev), Some(512));
    }

    #[test]
    fn test_unit_capped_by_max_transfer_and_rounded_down() {
        let mut ev = nvme(
            AtomicFields {
                awupf: Some(1022),
                ..AtomicFields::default()
            },
            512,
        );
        ev.device.max_transfer = 64 * 1024;
        assert_eq!(raw(&ev), Some(64 * 1024));
        let ev = nvme(
            AtomicFields {
                awupf: Some(5),
                ..AtomicFields::default()
            },
            512,
        );
        assert_eq!(raw(&ev), Some(4 * 512)); // 6 blocks round down to 4
    }

    #[test]
    fn test_linux_files_use_only_the_kernel_report() {
        let mut ev = nvme(
            AtomicFields {
                awupf: Some(63),
                ..AtomicFields::default()
            },
            512,
        );
        assert_eq!(untorn_unit(&ev, UntornPath::File), None);
        ev.fs.os_atomic_unit = 16384;
        assert_eq!(
            untorn_unit(&ev, UntornPath::File).map(|u| u.bytes),
            Some(16384)
        );
    }

    #[test]
    fn test_windows_takes_the_smaller_and_needs_the_device_unit() {
        // Development box: filesystem says 4096, device guarantees 512.
        let mut ev = nvme(
            AtomicFields {
                awupf: Some(0),
                ..AtomicFields::default()
            },
            512,
        );
        ev.platform = PlatformKind::Windows;
        ev.fs.os_atomic_unit = 4096;
        assert_eq!(
            untorn_unit(&ev, UntornPath::File).map(|u| u.bytes),
            Some(512)
        );
        ev.device.atomic.awupf = None;
        assert_eq!(untorn_unit(&ev, UntornPath::File), None);
    }

    #[test]
    fn test_macos_and_sata_promise_nothing() {
        let mut ev = nvme(
            AtomicFields {
                awupf: Some(63),
                ..AtomicFields::default()
            },
            512,
        );
        ev.platform = PlatformKind::MacOs;
        assert_eq!(untorn_unit(&ev, UntornPath::File), None);
        let mut ev = nvme(
            AtomicFields {
                awupf: Some(63),
                ..AtomicFields::default()
            },
            512,
        );
        ev.device.bus = Bus::Sata;
        assert_eq!(raw(&ev), None);
    }
}
