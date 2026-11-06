//! Device identity: which physical devices a store's bytes may live on, and
//! whether two stores are on distinct ones (PLANNING R5.2).
//!
//! The rule errs one way only. Two identities are distinct only when both
//! are complete and no device of one shares a node or any reported name
//! with a device of the other; anything the probe could not see counts as
//! the same device. A wrong "same" costs a refused mirror; a wrong
//! "distinct" would let two copies share one point of failure.

/// How the operating system names a whole device.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DeviceNode {
    /// Linux: `major:minor` of a whole disk (never a partition's).
    Linux {
        /// Major number.
        major: u32,
        /// Minor number.
        minor: u32,
    },
    /// Windows: the `PhysicalDrive` number.
    Windows {
        /// Disk number.
        disk: u32,
    },
    /// macOS: the physical device's IOKit registry path
    /// (`DADevicePath`), which every disk and volume on it shares.
    Mac {
        /// Registry path.
        device_path: String,
    },
    /// The simulator's device number.
    Sim {
        /// Device number.
        device: u32,
    },
}

/// One physical device a store's bytes may live on.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceId {
    /// The operating system's name for the whole device.
    pub node: DeviceNode,
    /// Every identifier the device reports, normalized ([`normalize_name`]):
    /// serials, WWIDs, EUI-64s, NGUIDs, subsystem NQNs. Two devices sharing
    /// any one are the same device (two namespaces of one controller share
    /// its serial; two paths to one LUN share its WWID).
    pub names: Vec<String>,
}

impl DeviceId {
    /// A device known by its node alone.
    #[must_use]
    pub fn node(node: DeviceNode) -> Self {
        Self {
            node,
            names: Vec::new(),
        }
    }

    /// Adds `raw` to [`Self::names`] if it normalizes to a usable name.
    pub fn add_name(&mut self, raw: &str) {
        if let Some(n) = normalize_name(raw) {
            if !self.names.contains(&n) {
                self.names.push(n);
            }
        }
    }

    fn same(&self, other: &Self) -> bool {
        self.node == other.node || self.names.iter().any(|n| other.names.contains(n))
    }
}

/// The physical devices a store's bytes may live on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceIdentity {
    /// Every device found (every leaf of a stacked device, every disk of a
    /// spanned volume, every physical store of an APFS container).
    pub devices: Vec<DeviceId>,
    /// The walk reached the bottom of the stack and read every node: no
    /// virtual disk, remote target, storage pool, network or anonymous
    /// file system, loop device, hidden member or unreadable node in the
    /// way.
    pub complete: bool,
}

impl DeviceIdentity {
    /// An identity the probe could not establish.
    #[must_use]
    pub fn unknown() -> Self {
        Self::default()
    }

    /// Whether `self` and `other` are on distinct devices: both complete,
    /// both non-empty, and no device of one shares a node or a name with a
    /// device of the other.
    #[must_use]
    pub fn distinct(&self, other: &Self) -> bool {
        if !self.complete || !other.complete || self.devices.is_empty() || other.devices.is_empty()
        {
            return false;
        }
        !self
            .devices
            .iter()
            .any(|a| other.devices.iter().any(|b| a.same(b)))
    }
}

/// Longest name kept, in bytes; longer reports are truncated (a subsystem
/// NQN is at most 223 bytes).
const NAME_MAX: usize = 256;

/// Normalizes a reported identifier: printable ASCII only, surrounding
/// spaces trimmed, runs of spaces collapsed, lower case. `None` for a name
/// that identifies nothing: empty, or only zeros, `f`s or spaces (an
/// unassigned EUI-64 or NGUID reads as all zeros).
#[must_use]
pub fn normalize_name(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut space = false;
    for c in raw.chars() {
        if c.is_ascii_whitespace() || c == '\0' {
            space = !out.is_empty();
            continue;
        }
        if !c.is_ascii_graphic() {
            continue;
        }
        // The space a character brings counts against the bound too.
        if out.len() + usize::from(space) >= NAME_MAX {
            break;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(c.to_ascii_lowercase());
    }
    // An `eui.`, `naa.` or `t10.` prefix names the scheme, not the device;
    // a dot anywhere else is part of the name (a Windows NVMe serial ends
    // in one: `6479_A7A5_0A50_05F4.`).
    let body = match out.split_once('.') {
        Some((scheme, rest))
            if !rest.is_empty()
                && scheme.len() <= 4
                && scheme.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            rest
        }
        _ => out.as_str(),
    };
    let meaningful = body
        .chars()
        .any(|c| !matches!(c, '0' | 'f' | 'x' | '.' | '-' | '_' | ':' | ' '));
    meaningful.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linux(major: u32, minor: u32, names: &[&str]) -> DeviceId {
        let mut d = DeviceId::node(DeviceNode::Linux { major, minor });
        for n in names {
            d.add_name(n);
        }
        d
    }

    fn ident(devices: Vec<DeviceId>) -> DeviceIdentity {
        DeviceIdentity {
            devices,
            complete: true,
        }
    }

    #[test]
    fn test_two_disks_with_their_own_nodes_and_names_are_distinct() {
        let a = ident(vec![linux(259, 0, &["S4EWNX0R123"])]);
        let b = ident(vec![linux(259, 4, &["S4EWNX0R999"])]);
        assert!(a.distinct(&b));
        assert!(b.distinct(&a));
    }

    #[test]
    fn test_one_node_is_one_device_whatever_the_names() {
        let a = ident(vec![linux(259, 0, &["one"])]);
        let b = ident(vec![linux(259, 0, &["other"])]);
        assert!(!a.distinct(&b));
    }

    #[test]
    fn test_two_namespaces_of_one_controller_share_its_serial() {
        let a = ident(vec![linux(
            259,
            0,
            &["S4EWNX0R123", "eui.0025385b71b0a1a2"],
        )]);
        let b = ident(vec![linux(
            259,
            2,
            &["S4EWNX0R123", "eui.0025385b71b0a1a3"],
        )]);
        assert!(!a.distinct(&b));
    }

    #[test]
    fn test_names_compare_after_normalizing() {
        let a = ident(vec![linux(8, 0, &["  ABC  123 "])]);
        let b = ident(vec![linux(8, 16, &["abc 123"])]);
        assert!(!a.distinct(&b));
    }

    #[test]
    fn test_any_shared_device_of_a_stack_makes_two_identities_one() {
        let a = ident(vec![linux(8, 0, &[]), linux(8, 16, &[])]);
        let b = ident(vec![linux(8, 32, &[]), linux(8, 16, &[])]);
        assert!(!a.distinct(&b));
    }

    #[test]
    fn test_an_incomplete_or_empty_identity_is_never_distinct() {
        let a = ident(vec![linux(8, 0, &[])]);
        let mut b = ident(vec![linux(8, 16, &[])]);
        b.complete = false;
        assert!(!a.distinct(&b));
        assert!(!b.distinct(&a));
        assert!(!a.distinct(&ident(Vec::new())));
        assert!(!DeviceIdentity::unknown().distinct(&DeviceIdentity::unknown()));
    }

    #[test]
    fn test_unassigned_identifiers_name_nothing() {
        for raw in [
            "",
            "   ",
            "0000000000000000",
            "eui.0000000000000000",
            "ffffffff",
            "00-00",
            "0000_0000_0000_0000.",
        ] {
            assert_eq!(normalize_name(raw), None, "{raw:?}");
        }
        assert_eq!(
            normalize_name("nqn.2014-08.org.nvmexpress:uuid:1").as_deref(),
            Some("nqn.2014-08.org.nvmexpress:uuid:1")
        );
        // A short name before a dot with nothing after it is a name, not a
        // scheme.
        assert_eq!(normalize_name("ab12.").as_deref(), Some("ab12."));
        // A Windows NVMe serial, trailing dot and all.
        assert_eq!(
            normalize_name("6479_A7A5_0A50_05F4.").as_deref(),
            Some("6479_a7a5_0a50_05f4.")
        );
        assert_eq!(
            normalize_name("eui.0025385b71b0a1a2").as_deref(),
            Some("eui.0025385b71b0a1a2")
        );
        let mut d = DeviceId::node(DeviceNode::Sim { device: 1 });
        d.add_name("0000");
        d.add_name("A");
        d.add_name("a");
        assert_eq!(d.names, vec!["a".to_owned()]);
    }

    #[test]
    fn test_a_name_is_bounded() {
        let long = "a".repeat(1000);
        assert_eq!(normalize_name(&long).map(|s| s.len()), Some(NAME_MAX));
        // A space and the character after it would pass the bound by one.
        let edge = format!("{} b", "a".repeat(NAME_MAX - 1));
        assert_eq!(normalize_name(&edge).map(|s| s.len()), Some(NAME_MAX - 1));
    }
}
