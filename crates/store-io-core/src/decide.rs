//! The class decision: evidence in, durability class out.
//!
//! A pure, total function with no I/O, so every rule is tested by a table and
//! the same decision is reproducible from a printed report. The rules, in the
//! order they apply:
//!
//! 1. **Unsafe** when anything in the stack is known to drop or fake
//!    durability: an unsafe filesystem (tmpfs, FUSE, network, overlay, FAT,
//!    pass-through, bcachefs), data journaling, no barriers, disabled syncs,
//!    compressed or software-encrypted files, buffered I/O behind a direct
//!    open, flushes unsupported or suppressed, Windows "turn off write-cache
//!    buffer flushing", parity RAID with a write hole, a RAM disk, or a
//!    missing full-flush primitive on macOS.
//! 2. **Unverified** when the evidence could hide suppressed flushes: an
//!    unqualified filesystem, weak error modes, write-through reported over an
//!    unreadable cache state, USB bridges, write-behind mirrors, filter
//!    drivers, loop devices, or a hypervisor without a certificate. An
//!    attestation lifts this to flush-required.
//! 3. Otherwise **power-safe** only when the device itself reports no volatile
//!    cache, its health shows the backup capacitors working, and no layer can
//!    allocate on overwrite or cache writes; or when an exact-model
//!    certificate says so. A device reporting no cache with unreadable health
//!    is flush-required, flagged as a power-safe candidate. macOS is never
//!    power-safe.
//! 4. Otherwise **flush-required** when the OS passes flushes to a device with
//!    a volatile cache.
//!
//! Operating-system cache toggles, vendor tables and timings never promote.

use crate::class::{CertId, DurabilityClass, MissingSet, Reason, ReasonSet, ReceiptLabel};
use crate::evidence::{Bus, Evidence, FsKind, OsCacheMode, PlatformKind, StackLayer, Tri};

/// Caller-supplied trust inputs. Every field is an explicit, reported choice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Trust {
    /// A certificate for this exact device model, firmware, capacity and LBA
    /// format, backed by a power-cut rig log: may promote to power-safe.
    pub certificate: Option<CertId>,
    /// An operator attestation for this stack: lifts unverified to flush-required.
    pub attestation: Option<CertId>,
    /// Allow durable opens on unverified or unsafe devices; every receipt is
    /// labelled overridden and the most conservative primitive is used.
    pub override_refusal: bool,
}

/// Whether a durable open may proceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurableOpen {
    /// Allowed on the class's own evidence, certificate or attestation.
    Allowed,
    /// Allowed only because the caller overrode a refusal.
    Overridden,
    /// Refused: the stack is unsafe.
    RefusedUnsafe,
    /// Refused: the evidence is incomplete.
    RefusedUnverified,
}

/// The outcome of [`decide`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassDecision {
    /// The class.
    pub class: DurabilityClass,
    /// The label every receipt minted under this decision carries.
    pub label: ReceiptLabel,
    /// Why the class is not power-safe, or why it was refused.
    pub reasons: ReasonSet,
    /// Evidence that could not be obtained.
    pub missing: MissingSet,
    /// The device reports no volatile cache but its health could not be read.
    pub power_safe_candidate: bool,
    /// Whether a durable open may proceed.
    pub durable_open: DurableOpen,
}

impl ClassDecision {
    /// Whether durable writes must use the flush path (true for every class
    /// except power-safe, including overridden opens).
    #[must_use]
    pub fn needs_flush(&self) -> bool {
        self.class.needs_flush()
    }
}

/// Decides the durability class of a path from its evidence.
#[must_use]
pub fn decide(ev: &Evidence, trust: &Trust) -> ClassDecision {
    let unsafe_r = unsafe_reasons(ev);
    let unverified_r = unverified_reasons(ev);
    let missing = ev.missing;

    if !unsafe_r.is_empty() {
        return refused(
            DurabilityClass::Unsafe,
            unsafe_r.union(unverified_r),
            missing,
            trust,
        );
    }
    if !unverified_r.is_empty() {
        if let Some(id) = trust.attestation {
            return ClassDecision {
                class: DurabilityClass::FlushRequired,
                label: ReceiptLabel::Attested(id),
                reasons: unverified_r,
                missing,
                power_safe_candidate: false,
                durable_open: DurableOpen::Allowed,
            };
        }
        return refused(DurabilityClass::Unverified, unverified_r, missing, trust);
    }

    let d = &ev.device;
    let mut reasons = ReasonSet::new();
    let allocating = ev.stack.iter().any(|l| {
        matches!(
            l,
            StackLayer::Thin | StackLayer::Dedupe | StackLayer::VirtualDisk
        )
    }) || ev.fs.cow == Tri::Yes;
    let cache_layer = ev.stack.contains(&StackLayer::WriteBackCache);
    if allocating {
        reasons.insert(Reason::AllocatingLayer);
    }
    if cache_layer {
        reasons.insert(Reason::CacheLayer);
    }
    let ps_blocked = allocating || cache_layer || ev.platform == PlatformKind::MacOs;

    if let Some(id) = trust.certificate {
        if !ps_blocked && d.backup_failed != Tri::Yes {
            return allowed(
                DurabilityClass::PowerSafe,
                ReceiptLabel::Certified(id),
                reasons,
                missing,
                false,
            );
        }
    }
    if d.cache_present == Tri::No && !ps_blocked {
        return match d.backup_failed {
            Tri::No => allowed(
                DurabilityClass::PowerSafe,
                ReceiptLabel::Evidence,
                reasons,
                missing,
                false,
            ),
            Tri::Yes => {
                reasons.insert(Reason::BackupFailed);
                allowed(
                    DurabilityClass::FlushRequired,
                    ReceiptLabel::Evidence,
                    reasons,
                    missing,
                    false,
                )
            }
            Tri::Unknown => allowed(
                DurabilityClass::FlushRequired,
                ReceiptLabel::Evidence,
                reasons,
                missing,
                true,
            ),
        };
    }
    if flushes_reach_device(ev) {
        return allowed(
            DurabilityClass::FlushRequired,
            ReceiptLabel::Evidence,
            reasons,
            missing,
            false,
        );
    }
    reasons.insert(Reason::CacheStateUnknown);
    refused(DurabilityClass::Unverified, reasons, missing, trust)
}

/// Whether the OS is known to pass flushes to the device.
fn flushes_reach_device(ev: &Evidence) -> bool {
    match ev.platform {
        PlatformKind::Windows => ev.device.os_flush_supported == Tri::Yes,
        PlatformKind::MacOs => ev.fs.full_flush == Tri::Yes,
        PlatformKind::Linux | PlatformKind::Sim => {
            ev.device.os_cache_mode == OsCacheMode::WriteBack
        }
    }
}

fn unsafe_reasons(ev: &Evidence) -> ReasonSet {
    let mut r = ReasonSet::new();
    let fs = &ev.fs;
    let d = &ev.device;
    if matches!(
        fs.kind,
        FsKind::Tmpfs
            | FsKind::Fuse
            | FsKind::Network
            | FsKind::Overlay
            | FsKind::Fat
            | FsKind::Passthrough
            | FsKind::Bcachefs
    ) || d.bus == Bus::Ram
        || fs.direct_io == Tri::No
    {
        r.insert(Reason::FsUnsafe);
    }
    if fs.data_journal {
        r.insert(Reason::DataJournal);
    }
    if fs.no_barrier {
        r.insert(Reason::NoBarrier);
    }
    if fs.sync_disabled {
        r.insert(Reason::SyncDisabled);
    }
    if fs.transformed {
        r.insert(Reason::InodeFlags);
    }
    if ev.platform == PlatformKind::MacOs && fs.full_flush == Tri::No {
        r.insert(Reason::FullFlushUnsupported);
    }
    if ev.platform == PlatformKind::Windows {
        if d.os_flush_supported == Tri::No {
            r.insert(Reason::FlushUnsupported);
        }
        if d.user_power_protection == Tri::Yes {
            r.insert(Reason::UserPowerProtection);
        }
    }
    if d.os_cache_mode == OsCacheMode::WriteThrough
        && d.cache_present == Tri::Yes
        && d.cache_enabled != Tri::No
    {
        r.insert(Reason::FlushSuppressed);
    }
    if ev.stack.contains(&StackLayer::ParityUnprotected) {
        r.insert(Reason::ParityWriteHole);
    }
    r
}

fn unverified_reasons(ev: &Evidence) -> ReasonSet {
    let mut r = ReasonSet::new();
    let fs = &ev.fs;
    let d = &ev.device;
    if matches!(
        fs.kind,
        FsKind::Refs
            | FsKind::Hfs
            | FsKind::Btrfs
            | FsKind::Zfs
            | FsKind::F2fs
            | FsKind::Ntfs3
            | FsKind::Other
    ) {
        r.insert(Reason::FsUnqualified);
    }
    if fs.weak_error_mode {
        r.insert(Reason::WeakErrorMode);
    }
    if ev.platform == PlatformKind::MacOs && fs.full_flush == Tri::Unknown {
        r.insert(Reason::FullFlushUnsupported);
    }
    if d.os_cache_mode == OsCacheMode::WriteThrough && d.cache_present == Tri::Unknown {
        r.insert(Reason::CacheStateUnknown);
    }
    if d.bus == Bus::Usb {
        r.insert(Reason::UntrustedBus);
    }
    if ev.stack.contains(&StackLayer::WriteBehind) {
        r.insert(Reason::WriteBehind);
    }
    if ev.stack.contains(&StackLayer::Filter) {
        r.insert(Reason::FilterDriver);
    }
    if ev.stack.contains(&StackLayer::Loop) {
        r.insert(Reason::AllocatingLayer);
    }
    if ev.hypervisor.is_some() || d.bus == Bus::Virtual {
        r.insert(Reason::Hypervisor);
    }
    r
}

fn allowed(
    class: DurabilityClass,
    label: ReceiptLabel,
    reasons: ReasonSet,
    missing: MissingSet,
    power_safe_candidate: bool,
) -> ClassDecision {
    ClassDecision {
        class,
        label,
        reasons,
        missing,
        power_safe_candidate,
        durable_open: DurableOpen::Allowed,
    }
}

fn refused(
    class: DurabilityClass,
    reasons: ReasonSet,
    missing: MissingSet,
    trust: &Trust,
) -> ClassDecision {
    let (durable_open, label) = if trust.override_refusal {
        (DurableOpen::Overridden, ReceiptLabel::Overridden(reasons))
    } else if class == DurabilityClass::Unsafe {
        (DurableOpen::RefusedUnsafe, ReceiptLabel::Evidence)
    } else {
        (DurableOpen::RefusedUnverified, ReceiptLabel::Evidence)
    };
    ClassDecision {
        class,
        label,
        reasons,
        missing,
        power_safe_candidate: false,
        durable_open,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::class::Missing;
    use crate::evidence::Hypervisor;

    /// A healthy consumer NVMe on Linux ext4: volatile cache, OS write-back.
    fn consumer_linux() -> Evidence {
        let mut ev = Evidence::unknown(PlatformKind::Linux);
        ev.fs.kind = FsKind::Ext4;
        ev.fs.direct_io = Tri::Yes;
        ev.device.bus = Bus::Nvme;
        ev.device.cache_present = Tri::Yes;
        ev.device.cache_enabled = Tri::Yes;
        ev.device.os_cache_mode = OsCacheMode::WriteBack;
        ev.device.backup_failed = Tri::No;
        ev
    }

    /// A PLP NVMe: no volatile cache, healthy, OS write-through.
    fn plp_linux() -> Evidence {
        let mut ev = consumer_linux();
        ev.device.cache_present = Tri::No;
        ev.device.cache_enabled = Tri::No;
        ev.device.os_cache_mode = OsCacheMode::WriteThrough;
        ev
    }

    fn class(ev: &Evidence) -> (DurabilityClass, DurableOpen) {
        let d = decide(ev, &Trust::default());
        (d.class, d.durable_open)
    }

    #[test]
    fn test_consumer_nvme_is_flush_required() {
        assert_eq!(
            class(&consumer_linux()),
            (DurabilityClass::FlushRequired, DurableOpen::Allowed)
        );
    }

    #[test]
    fn test_plp_with_readable_health_is_power_safe() {
        assert_eq!(
            class(&plp_linux()),
            (DurabilityClass::PowerSafe, DurableOpen::Allowed)
        );
    }

    #[test]
    fn test_plp_without_smart_is_flush_required_candidate() {
        let mut ev = plp_linux();
        ev.device.backup_failed = Tri::Unknown;
        ev.missing.insert(Missing::Smart);
        let d = decide(&ev, &Trust::default());
        assert_eq!(d.class, DurabilityClass::FlushRequired);
        assert!(d.power_safe_candidate);
        assert!(d.missing.contains(Missing::Smart));
    }

    #[test]
    fn test_failed_backup_demotes_power_safe() {
        let mut ev = plp_linux();
        ev.device.backup_failed = Tri::Yes;
        let d = decide(&ev, &Trust::default());
        assert_eq!(d.class, DurabilityClass::FlushRequired);
        assert!(d.reasons.contains(Reason::BackupFailed));
    }

    #[test]
    fn test_os_write_through_over_volatile_cache_is_unsafe() {
        let mut ev = consumer_linux();
        ev.device.os_cache_mode = OsCacheMode::WriteThrough;
        let d = decide(&ev, &Trust::default());
        assert_eq!(d.class, DurabilityClass::Unsafe);
        assert!(d.reasons.contains(Reason::FlushSuppressed));
    }

    #[test]
    fn test_os_write_through_over_unknown_cache_is_unverified() {
        let mut ev = consumer_linux();
        ev.device.os_cache_mode = OsCacheMode::WriteThrough;
        ev.device.cache_present = Tri::Unknown;
        assert_eq!(class(&ev).0, DurabilityClass::Unverified);
    }

    #[test]
    fn test_each_unsafe_filesystem_and_mount_rule() {
        for kind in [
            FsKind::Tmpfs,
            FsKind::Fuse,
            FsKind::Network,
            FsKind::Overlay,
            FsKind::Fat,
            FsKind::Passthrough,
            FsKind::Bcachefs,
        ] {
            let mut ev = consumer_linux();
            ev.fs.kind = kind;
            assert_eq!(
                class(&ev),
                (DurabilityClass::Unsafe, DurableOpen::RefusedUnsafe),
                "{kind:?}"
            );
        }
        let edits: [fn(&mut Evidence); 5] = [
            |e| e.fs.data_journal = true,
            |e| e.fs.no_barrier = true,
            |e| e.fs.sync_disabled = true,
            |e| e.fs.transformed = true,
            |e| e.fs.direct_io = Tri::No,
        ];
        for edit in edits {
            let mut ev = consumer_linux();
            edit(&mut ev);
            assert_eq!(class(&ev).0, DurabilityClass::Unsafe);
        }
    }

    #[test]
    fn test_each_unverified_rule_and_attestation_lifts_it() {
        let edits: [fn(&mut Evidence); 6] = [
            |e| e.fs.kind = FsKind::Btrfs,
            |e| e.fs.weak_error_mode = true,
            |e| e.device.bus = Bus::Usb,
            |e| e.stack.push(StackLayer::WriteBehind),
            |e| e.stack.push(StackLayer::Loop),
            |e| e.hypervisor = Some(Hypervisor::HyperV),
        ];
        for edit in edits {
            let mut ev = consumer_linux();
            edit(&mut ev);
            assert_eq!(
                class(&ev),
                (DurabilityClass::Unverified, DurableOpen::RefusedUnverified)
            );
            let t = Trust {
                attestation: Some(CertId(7)),
                ..Trust::default()
            };
            let d = decide(&ev, &t);
            assert_eq!(d.class, DurabilityClass::FlushRequired);
            assert_eq!(d.label, ReceiptLabel::Attested(CertId(7)));
        }
    }

    #[test]
    fn test_override_allows_but_labels_every_receipt() {
        let mut ev = consumer_linux();
        ev.fs.kind = FsKind::Tmpfs;
        let d = decide(
            &ev,
            &Trust {
                override_refusal: true,
                ..Trust::default()
            },
        );
        assert_eq!(d.class, DurabilityClass::Unsafe);
        assert_eq!(d.durable_open, DurableOpen::Overridden);
        assert!(matches!(d.label, ReceiptLabel::Overridden(r) if r.contains(Reason::FsUnsafe)));
        assert!(d.needs_flush());
    }

    #[test]
    fn test_windows_flush_rules() {
        let mut ev = consumer_linux();
        ev.platform = PlatformKind::Windows;
        ev.fs.kind = FsKind::Ntfs;
        ev.device.os_cache_mode = OsCacheMode::Unknown;
        ev.device.os_flush_supported = Tri::Yes;
        ev.device.user_power_protection = Tri::No;
        assert_eq!(class(&ev).0, DurabilityClass::FlushRequired);
        ev.device.user_power_protection = Tri::Yes;
        assert_eq!(class(&ev).0, DurabilityClass::Unsafe);
        ev.device.user_power_protection = Tri::No;
        ev.device.os_flush_supported = Tri::No;
        assert_eq!(class(&ev).0, DurabilityClass::Unsafe);
        ev.device.os_flush_supported = Tri::Unknown;
        assert_eq!(class(&ev).0, DurabilityClass::Unverified);
    }

    #[test]
    fn test_layers_and_macos_block_power_safe_but_certificate_cannot_override_layers() {
        for edit in [
            (|e: &mut Evidence| e.stack.push(StackLayer::Thin)) as fn(&mut Evidence),
            |e| e.stack.push(StackLayer::WriteBackCache),
            |e| e.fs.cow = Tri::Yes,
        ] {
            let mut ev = plp_linux();
            ev.device.os_cache_mode = OsCacheMode::WriteBack;
            edit(&mut ev);
            let t = Trust {
                certificate: Some(CertId(1)),
                ..Trust::default()
            };
            assert_eq!(decide(&ev, &t).class, DurabilityClass::FlushRequired);
        }
        let mut ev = plp_linux();
        ev.platform = PlatformKind::MacOs;
        ev.fs.kind = FsKind::Apfs;
        ev.fs.full_flush = Tri::Yes;
        assert_eq!(class(&ev).0, DurabilityClass::FlushRequired);
        ev.fs.full_flush = Tri::No;
        assert_eq!(class(&ev).0, DurabilityClass::Unsafe);
    }

    #[test]
    fn test_certificate_promotes_a_clean_stack() {
        let mut ev = consumer_linux();
        ev.device.cache_present = Tri::Unknown;
        let t = Trust {
            certificate: Some(CertId(3)),
            ..Trust::default()
        };
        let d = decide(&ev, &t);
        assert_eq!(d.class, DurabilityClass::PowerSafe);
        assert_eq!(d.label, ReceiptLabel::Certified(CertId(3)));
    }

    #[test]
    fn test_missing_evidence_never_raises_the_class() {
        // Removing knowledge (turning facts to Unknown) must never move the
        // class towards power-safe.
        let rank = |c: DurabilityClass| match c {
            DurabilityClass::PowerSafe => 3,
            DurabilityClass::FlushRequired => 2,
            DurabilityClass::Unverified => 1,
            DurabilityClass::Unsafe => 0,
        };
        let base = plp_linux();
        let before = rank(class(&base).0);
        let forget: [fn(&mut Evidence); 4] = [
            |e| e.device.cache_present = Tri::Unknown,
            |e| e.device.backup_failed = Tri::Unknown,
            |e| e.device.os_cache_mode = OsCacheMode::Unknown,
            |e| e.fs.direct_io = Tri::Unknown,
        ];
        for f in forget {
            let mut ev = base.clone();
            f(&mut ev);
            assert!(rank(class(&ev).0) <= before);
        }
    }
}
