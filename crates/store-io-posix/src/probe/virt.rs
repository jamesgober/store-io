//! Hypervisor detection (DMI, `/sys/hypervisor`, the CPU `hypervisor` flag,
//! vmbus and virtio buses, the kernel release) and the kernel version.

use std::path::Path;

use store_io_core::evidence::Hypervisor;

use super::block::{read_bounded, read_sysfs};

/// Longest `/proc/cpuinfo` read.
const CPUINFO_MAX: usize = 4 << 20;

/// Raw inputs to [`classify`], each optional.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct VirtInputs<'a> {
    /// `/sys/class/dmi/id/sys_vendor`.
    pub sys_vendor: Option<&'a str>,
    /// `/sys/class/dmi/id/product_name`.
    pub product_name: Option<&'a str>,
    /// `/sys/hypervisor/type`.
    pub hypervisor_type: Option<&'a str>,
    /// `/proc/cpuinfo` lists the `hypervisor` flag.
    pub cpu_hypervisor_flag: bool,
    /// `uname` release.
    pub kernel_release: Option<&'a str>,
    /// `/sys/bus/vmbus/devices` is non-empty.
    pub vmbus_devices: bool,
    /// `/sys/bus/virtio/devices` is non-empty.
    pub virtio_devices: bool,
}

/// Decides the hypervisor from the inputs; `None` when nothing indicates one.
pub(crate) fn classify(i: &VirtInputs<'_>) -> Option<Hypervisor> {
    let v = i.sys_vendor.unwrap_or("").trim().to_ascii_lowercase();
    let p = i.product_name.unwrap_or("").trim().to_ascii_lowercase();
    let r = i.kernel_release.unwrap_or("").to_ascii_lowercase();
    if i.hypervisor_type
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("xen"))
    {
        return Some(Hypervisor::Xen);
    }
    if v.contains("xen") || p.contains("hvm domu") {
        return Some(Hypervisor::Xen);
    }
    if v.contains("amazon ec2") {
        // Bare-metal instances carry the same DMI vendor but no CPUID flag.
        return i.cpu_hypervisor_flag.then_some(Hypervisor::Nitro);
    }
    if v.contains("vmware") || p.contains("vmware") {
        return Some(Hypervisor::Vmware);
    }
    if v.contains("qemu")
        || p.contains("qemu")
        || v.contains("kvm")
        || p.contains("kvm")
        || v.contains("bochs")
        || v.contains("red hat")
        || v.contains("google")
        || v.contains("digitalocean")
        || v.contains("openstack")
        || p.contains("openstack")
    {
        return Some(Hypervisor::Kvm);
    }
    if v.contains("microsoft") && p.contains("virtual machine") {
        return Some(Hypervisor::HyperV);
    }
    if r.contains("microsoft") || r.contains("wsl") || i.vmbus_devices {
        return Some(Hypervisor::HyperV);
    }
    if i.cpu_hypervisor_flag || i.virtio_devices {
        return Some(Hypervisor::Other);
    }
    None
}

/// Reads the inputs and classifies.
pub(crate) fn detect(kernel_release: Option<&str>) -> Option<Hypervisor> {
    let dmi = Path::new("/sys/class/dmi/id");
    let sys_vendor = read_sysfs(&dmi.join("sys_vendor"));
    let product_name = read_sysfs(&dmi.join("product_name"));
    let hypervisor_type = read_sysfs(Path::new("/sys/hypervisor/type"));
    let cpu_hypervisor_flag = read_bounded(Path::new("/proc/cpuinfo"), CPUINFO_MAX)
        .is_some_and(|t| cpuinfo_has_hypervisor(&t));
    classify(&VirtInputs {
        sys_vendor: sys_vendor.as_deref(),
        product_name: product_name.as_deref(),
        hypervisor_type: hypervisor_type.as_deref(),
        cpu_hypervisor_flag,
        kernel_release,
        vmbus_devices: dir_has_entries("/sys/bus/vmbus/devices"),
        virtio_devices: dir_has_entries("/sys/bus/virtio/devices"),
    })
}

fn dir_has_entries(path: &str) -> bool {
    std::fs::read_dir(path).is_ok_and(|mut rd| rd.next().is_some())
}

/// Whether a `flags` line of `/proc/cpuinfo` lists `hypervisor`.
pub(crate) fn cpuinfo_has_hypervisor(text: &str) -> bool {
    text.lines()
        .take(1 << 16)
        .filter(|l| l.starts_with("flags") || l.starts_with("Features"))
        .any(|l| {
            l.split_once(':')
                .is_some_and(|(_, v)| v.split_whitespace().any(|f| f == "hypervisor"))
        })
}

/// `(major, minor, patch)` from a release such as
/// `6.6.87.2-microsoft-standard-WSL2`; a missing patch reads as 0.
pub(crate) fn kernel_version(release: &str) -> Option<(u16, u16, u16)> {
    let numeric: &str = release
        .split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .next()
        .unwrap_or("");
    let mut parts = numeric.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kernel_version_parses_common_releases() {
        assert_eq!(
            kernel_version("6.6.87.2-microsoft-standard-WSL2"),
            Some((6, 6, 87))
        );
        assert_eq!(kernel_version("6.1.0-18-amd64"), Some((6, 1, 0)));
        assert_eq!(kernel_version("5.15"), Some((5, 15, 0)));
        assert_eq!(kernel_version("7.3.0-rc6"), Some((7, 3, 0)));
        assert_eq!(kernel_version(""), None);
        assert_eq!(kernel_version("abc"), None);
        assert_eq!(kernel_version("6"), None);
        assert_eq!(kernel_version("99999.1.1"), None);
        assert_eq!(kernel_version("6..1"), None);
    }

    #[test]
    fn test_cpuinfo_flag() {
        assert!(cpuinfo_has_hypervisor(
            "processor\t: 0\nflags\t\t: fpu vme hypervisor lahf_lm\n"
        ));
        assert!(!cpuinfo_has_hypervisor("flags\t\t: fpu vme hypervisorx\n"));
        assert!(!cpuinfo_has_hypervisor("model name: hypervisor\n"));
        assert!(!cpuinfo_has_hypervisor(""));
    }

    #[test]
    fn test_classify_hypervisors() {
        let none = VirtInputs::default();
        assert_eq!(classify(&none), None);
        let wsl = VirtInputs {
            cpu_hypervisor_flag: true,
            kernel_release: Some("6.6.87.2-microsoft-standard-WSL2"),
            vmbus_devices: true,
            ..VirtInputs::default()
        };
        assert_eq!(classify(&wsl), Some(Hypervisor::HyperV));
        let hv = VirtInputs {
            sys_vendor: Some("Microsoft Corporation"),
            product_name: Some("Virtual Machine"),
            ..VirtInputs::default()
        };
        assert_eq!(classify(&hv), Some(Hypervisor::HyperV));
        let kvm = VirtInputs {
            sys_vendor: Some("QEMU"),
            product_name: Some("Standard PC (Q35 + ICH9, 2009)"),
            cpu_hypervisor_flag: true,
            ..VirtInputs::default()
        };
        assert_eq!(classify(&kvm), Some(Hypervisor::Kvm));
        let gce = VirtInputs {
            sys_vendor: Some("Google"),
            product_name: Some("Google Compute Engine"),
            ..VirtInputs::default()
        };
        assert_eq!(classify(&gce), Some(Hypervisor::Kvm));
        let vmw = VirtInputs {
            sys_vendor: Some("VMware, Inc."),
            product_name: Some("VMware Virtual Platform"),
            ..VirtInputs::default()
        };
        assert_eq!(classify(&vmw), Some(Hypervisor::Vmware));
        let xen = VirtInputs {
            hypervisor_type: Some("xen\n"),
            ..VirtInputs::default()
        };
        assert_eq!(classify(&xen), Some(Hypervisor::Xen));
        let xen_hvm = VirtInputs {
            sys_vendor: Some("Xen"),
            product_name: Some("HVM domU"),
            ..VirtInputs::default()
        };
        assert_eq!(classify(&xen_hvm), Some(Hypervisor::Xen));
        let nitro = VirtInputs {
            sys_vendor: Some("Amazon EC2"),
            product_name: Some("c6i.large"),
            cpu_hypervisor_flag: true,
            ..VirtInputs::default()
        };
        assert_eq!(classify(&nitro), Some(Hypervisor::Nitro));
        let metal = VirtInputs {
            sys_vendor: Some("Amazon EC2"),
            product_name: Some("c6i.metal"),
            ..VirtInputs::default()
        };
        assert_eq!(classify(&metal), None);
        let flag_only = VirtInputs {
            cpu_hypervisor_flag: true,
            ..VirtInputs::default()
        };
        assert_eq!(classify(&flag_only), Some(Hypervisor::Other));
        let virtio_only = VirtInputs {
            virtio_devices: true,
            ..VirtInputs::default()
        };
        assert_eq!(classify(&virtio_only), Some(Hypervisor::Other));
        let bare = VirtInputs {
            sys_vendor: Some("ASUSTeK COMPUTER INC."),
            product_name: Some("ROG STRIX"),
            kernel_release: Some("6.8.0-45-generic"),
            ..VirtInputs::default()
        };
        assert_eq!(classify(&bare), None);
        let long = "x".repeat(10_000);
        let garbage = VirtInputs {
            sys_vendor: Some("\u{0}\u{FFFD}\n"),
            product_name: Some(&long),
            hypervisor_type: Some("???"),
            ..VirtInputs::default()
        };
        assert_eq!(classify(&garbage), None);
    }
}
