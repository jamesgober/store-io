//! Parsers for the NVMe structures returned through
//! `StorageDeviceProtocolSpecificProperty`: Identify Controller (CNS 01h),
//! Identify Namespace (CNS 00h), the SMART / Health log (LID 02h) and the
//! Volatile Write Cache feature (FID 06h).
//!
//! Offsets are those of the NVMe base specification. Every field is read
//! with bounds checks; an undersized answer is `None`.

use crate::bytes::Reader;

/// Identify Controller fields store-io uses.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct IdentifyController {
    /// Model number (bytes 24..64).
    pub model: Option<String>,
    /// Firmware revision (bytes 64..72).
    pub firmware: Option<String>,
    /// CMIC bit 1: the subsystem may contain two or more controllers.
    pub multi_controller: bool,
    /// MDTS (byte 77), in units of the minimum memory page size, 0 = none.
    pub mdts: u8,
    /// VWC bit 0: a volatile write cache is present.
    pub vwc_present: bool,
    /// AWUPF (bytes 528..530), 0-based.
    pub awupf: u16,
}

/// Parses a 4096-byte Identify Controller structure (shorter answers that
/// still contain every field used are accepted).
pub(crate) fn parse_identify_controller(buf: &[u8]) -> Option<IdentifyController> {
    let r = Reader::new(buf);
    Some(IdentifyController {
        model: r.ascii(24, 40),
        firmware: r.ascii(64, 8),
        multi_controller: r.u8(76)? & 0b10 != 0,
        mdts: r.u8(77)?,
        vwc_present: r.u8(525)? & 1 != 0,
        awupf: r.u16(528)?,
    })
}

/// Identify Namespace fields store-io uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct IdentifyNamespace {
    /// NSFEAT bit 3: NAWUPF, NABSPF and NABO are valid.
    pub nsabp: bool,
    /// NAWUPF (bytes 36..38), 0-based; 0 means "same as AWUPF".
    pub nawupf: u16,
    /// NABO (bytes 42..44).
    pub nabo: u16,
    /// NABSPF (bytes 44..46), 0-based; 0 means no boundary.
    pub nabspf: u16,
    /// NPWG (bytes 64..66), 0-based, valid when NSFEAT bit 4 (OPTPERF).
    pub npwg: Option<u16>,
    /// Bytes per logical block of the format in use (from LBAF[FLBAS]).
    pub lba_bytes: u32,
}

/// Parses an Identify Namespace structure. The LBA format index is
/// `FLBAS[3:0]`, extended with `FLBAS[6:5]` when NLBAF reports more than 16
/// formats (NVMe 2.0).
pub(crate) fn parse_identify_namespace(buf: &[u8]) -> Option<IdentifyNamespace> {
    let r = Reader::new(buf);
    let nsfeat = r.u8(24)?;
    let nlbaf = r.u8(25)?;
    let flbas = r.u8(26)?;
    let mut index = usize::from(flbas & 0x0F);
    if nlbaf >= 16 {
        index |= usize::from((flbas >> 5) & 0b11) << 4;
    }
    if index > usize::from(nlbaf) {
        return None;
    }
    let lbads = r.u8(128 + index * 4 + 2)?;
    if !(9..=31).contains(&lbads) {
        return None;
    }
    let nsabp = nsfeat & 0b1000 != 0;
    Some(IdentifyNamespace {
        nsabp,
        nawupf: if nsabp { r.u16(36)? } else { 0 },
        nabo: if nsabp { r.u16(42)? } else { 0 },
        nabspf: if nsabp { r.u16(44)? } else { 0 },
        npwg: if nsfeat & 0b1_0000 != 0 {
            r.u16(64)
        } else {
            None
        },
        lba_bytes: 1u32 << lbads,
    })
}

/// Critical Warning byte of the SMART / Health log. Bit 4: volatile memory
/// backup device has failed.
pub(crate) fn parse_health_critical_warning(buf: &[u8]) -> Option<u8> {
    Reader::new(buf).u8(0)
}

/// Whether the Volatile Write Cache feature's completion dword 0 reports
/// the cache enabled (bit 0).
pub(crate) fn vwc_enabled(dword0: u32) -> bool {
    dword0 & 1 != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controller() -> Vec<u8> {
        let mut b = vec![0u8; 4096];
        b[24..24 + 18].copy_from_slice(b"T-FORCE TM8FPZ004T");
        b[64..72].copy_from_slice(b"EIFM70.3");
        b[76] = 0b11;
        b[77] = 5;
        b[525] = 0x07;
        b[528..530].copy_from_slice(&3u16.to_le_bytes());
        b
    }

    #[test]
    fn test_identify_controller_fields_and_vwc_bit0_only() {
        let c = parse_identify_controller(&controller()).unwrap_or_default();
        assert_eq!(c.model.as_deref(), Some("T-FORCE TM8FPZ004T"));
        assert_eq!(c.firmware.as_deref(), Some("EIFM70.3"));
        assert!(c.multi_controller);
        assert_eq!(c.mdts, 5);
        assert!(c.vwc_present);
        assert_eq!(c.awupf, 3);
        let mut fb_only = controller();
        fb_only[525] = 0b110; // FB bits set, VWC bit 0 clear
        assert!(
            !parse_identify_controller(&fb_only)
                .unwrap_or_default()
                .vwc_present
        );
    }

    #[test]
    fn test_identify_controller_truncated_is_none() {
        let c = controller();
        assert_eq!(parse_identify_controller(&c[..529]), None);
        assert_eq!(parse_identify_controller(&c[..100]), None);
        assert_eq!(parse_identify_controller(&[]), None);
        // 530 bytes hold every used field.
        assert!(parse_identify_controller(&c[..530]).is_some());
    }

    fn namespace(flbas: u8, nlbaf: u8) -> Vec<u8> {
        let mut b = vec![0u8; 4096];
        b[24] = 0b1_1000; // NSABP + OPTPERF
        b[25] = nlbaf;
        b[26] = flbas;
        b[36..38].copy_from_slice(&7u16.to_le_bytes());
        b[42..44].copy_from_slice(&2u16.to_le_bytes());
        b[44..46].copy_from_slice(&15u16.to_le_bytes());
        b[64..66].copy_from_slice(&31u16.to_le_bytes());
        b[128 + 2] = 9; // LBAF0: 512 B
        b[132 + 2] = 12; // LBAF1: 4 KiB
        b
    }

    #[test]
    fn test_identify_namespace_fields_and_lba_format_index() {
        let n = parse_identify_namespace(&namespace(0, 1)).unwrap_or_default();
        assert_eq!(
            n,
            IdentifyNamespace {
                nsabp: true,
                nawupf: 7,
                nabo: 2,
                nabspf: 15,
                npwg: Some(31),
                lba_bytes: 512,
            }
        );
        assert_eq!(
            parse_identify_namespace(&namespace(1, 1)).map(|n| n.lba_bytes),
            Some(4096)
        );
        // NLBAF ≥ 16 enables the FLBAS[6:5] extension: index 1 | (1 << 4) = 17.
        let mut wide = namespace(0b0010_0001, 17);
        wide[128 + 17 * 4 + 2] = 13;
        assert_eq!(
            parse_identify_namespace(&wide).map(|n| n.lba_bytes),
            Some(8192)
        );
        // Without NLBAF ≥ 16 the upper bits are ignored.
        assert_eq!(
            parse_identify_namespace(&namespace(0b0010_0001, 1)).map(|n| n.lba_bytes),
            Some(4096)
        );
    }

    #[test]
    fn test_identify_namespace_without_nsabp_ignores_reserved_atomic_fields() {
        let mut b = namespace(0, 1);
        b[24] = 0; // NSABP and OPTPERF clear
        let n = parse_identify_namespace(&b).unwrap_or_default();
        assert!(!n.nsabp);
        assert_eq!((n.nawupf, n.nabo, n.nabspf, n.npwg), (0, 0, 0, None));
    }

    #[test]
    fn test_identify_namespace_rejects_garbage() {
        // Format index beyond NLBAF.
        assert_eq!(parse_identify_namespace(&namespace(3, 1)), None);
        // LBADS outside 9..=31 (0 means "unused format"; ≥ 32 would overflow).
        let mut zero = namespace(0, 1);
        zero[130] = 0;
        assert_eq!(parse_identify_namespace(&zero), None);
        let mut big = namespace(0, 1);
        big[130] = 40;
        assert_eq!(parse_identify_namespace(&big), None);
        assert_eq!(parse_identify_namespace(&namespace(0, 1)[..130]), None);
        assert_eq!(parse_identify_namespace(&[0u8; 10]), None);
        assert_eq!(parse_identify_namespace(&[]), None);
    }

    #[test]
    fn test_health_and_feature_bits() {
        assert_eq!(parse_health_critical_warning(&[0x10, 0, 0]), Some(0x10));
        assert_eq!(parse_health_critical_warning(&[]), None);
        assert!(vwc_enabled(1));
        assert!(vwc_enabled(0xFFFF_FFFF));
        assert!(!vwc_enabled(0b10));
    }
}
