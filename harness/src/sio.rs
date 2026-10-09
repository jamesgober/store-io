//! store-io setup: the platform backend for this OS, the store options every
//! workload uses, and the one-time device probe that decides whether the run
//! needs the labelled override.

use std::path::Path;

use store_io_core::decide::DurableOpen;
use store_io_engine::{Store, StoreOptions};

/// The backend under test on this OS.
#[cfg(windows)]
pub type Plat = store_io_win::WinPlatform;
/// The backend under test on this OS.
#[cfg(target_os = "linux")]
pub type Plat = store_io_posix::PosixPlatform;

/// A store on the backend under test.
pub type SioStore = Store<Plat>;

/// A fresh backend instance.
#[must_use]
pub fn platform() -> Plat {
    Plat::new()
}

/// Human-readable name of the backend.
#[cfg(windows)]
pub const BACKEND: &str = "store-io-win (WinPlatform: overlapped I/O on IOCP, NO_BUFFERING)";
/// Human-readable name of the backend.
#[cfg(target_os = "linux")]
pub const BACKEND: &str = "store-io-posix (PosixPlatform: synchronous O_DIRECT, tier T3)";

/// What the one-time probe found.
pub struct Setup {
    /// The options every store in this run is created with.
    pub opts: StoreOptions,
    /// `Some(error text)` when a default-trust create was refused and the
    /// run proceeds under `Trust::override_refusal`.
    pub refusal: Option<String>,
    /// `Store::report()` of the probe store (after any override).
    pub report_text: String,
    /// `Debug` dump of the raw evidence.
    pub evidence_dump: String,
    /// The class decision, short form.
    pub class: String,
    /// The receipt label, short form.
    pub label: String,
    /// The durable-open outcome.
    pub durable_open: String,
    /// The device model the probe read, if any.
    pub device_model: Option<String>,
    /// Filesystem kind, as the probe classified it.
    pub fs_kind: String,
    /// Block size the store chose.
    pub block_size: u64,
}

/// Creates a store in `dir` with `opts`.
///
/// # Errors
///
/// The store-io error, formatted.
pub fn create(dir: &Path, opts: &StoreOptions) -> Result<SioStore, String> {
    Store::create(platform(), dir, opts.clone())
        .map_err(|e| format!("Store::create({}) failed: {e} [{e:?}]", dir.display()))
}

/// Creates a probe store with default trust; on refusal (`Unverified` or
/// `UnsafeDevice`) records the refusal and retries with the documented,
/// labelled override (`Trust::override_refusal`). Nothing is weakened
/// silently: the refusal text is returned and printed in the report.
///
/// # Errors
///
/// When even the overridden create fails.
pub fn probe(dir: &Path) -> Result<Setup, String> {
    let mut opts = StoreOptions::default();
    let (store, refusal) = match Store::create(platform(), dir, opts.clone()) {
        Ok(s) => (s, None),
        Err(e) => {
            let text = format!("{e} [{e:?}]");
            let refused = matches!(
                e,
                store_io_core::Error::Unverified { .. } | store_io_core::Error::UnsafeDevice { .. }
            );
            if !refused {
                return Err(format!(
                    "Store::create failed for a reason other than the device class: {text}"
                ));
            }
            opts.trust.override_refusal = true;
            (create(dir, &opts)?, Some(text))
        }
    };
    let r = store.report();
    let d = &r.decision;
    let setup = Setup {
        report_text: r.to_string(),
        evidence_dump: format!("{:#?}", r.evidence),
        class: d.class.to_string(),
        label: d.label.to_string(),
        durable_open: match d.durable_open {
            DurableOpen::Allowed => "Allowed".to_owned(),
            DurableOpen::Overridden => "Overridden (Trust::override_refusal)".to_owned(),
            DurableOpen::RefusedUnsafe => "RefusedUnsafe".to_owned(),
            DurableOpen::RefusedUnverified => "RefusedUnverified".to_owned(),
        },
        device_model: r.evidence.device.model.clone(),
        fs_kind: format!("{:?}", r.evidence.fs.kind),
        block_size: r.block_size,
        opts,
        refusal,
    };
    drop(store);
    Ok(setup)
}
