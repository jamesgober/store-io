//! The simple API on the operating system's native backend.

use std::ops::Deref;
use std::path::Path;

use store_io_core::decide::{Trust, decide};
use store_io_core::error::{Error, ErrorContext, Named, NotWrittenCause, Op, OsError};
use store_io_platform::{FileName, LinuxTier, Platform};

use crate::{CopyOptions, CopyReport, NativePlatform, Probe, StoreOptions};

/// The native platform on the kernel interface `tier` asks for (D-082;
/// only Linux has a choice).
fn platform(tier: LinuxTier) -> Result<NativePlatform, Error> {
    #[cfg(target_os = "linux")]
    {
        NativePlatform::with_tier(tier).map_err(|raw| io(Op::Queue, raw))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = tier;
        Ok(NativePlatform::new())
    }
}

/// Paths accepted by the simple API.
fn path_of<P: AsRef<Path> + ?Sized>(p: &P) -> &Path {
    p.as_ref()
}

/// A region store-io appends to (see [`store_io_engine::AppendRegion`]).
pub type AppendRegion = store_io_engine::AppendRegion<NativePlatform>;
/// A region of caller-addressed blocks (see [`store_io_engine::PageRegion`]).
pub type PageRegion = store_io_engine::PageRegion<NativePlatform>;
/// A small object committed atomically (see [`store_io_engine::Slot`]).
pub type Slot = store_io_engine::Slot<NativePlatform>;
/// Many appends, one barrier (see [`store_io_engine::AppendBatch`]).
pub type AppendBatch = store_io_engine::AppendBatch<NativePlatform>;
/// Many page writes, one barrier (see [`store_io_engine::PageBatch`]).
pub type PageBatch = store_io_engine::PageBatch<NativePlatform>;
/// Space set aside for provisioning (see [`store_io_engine::Reservation`]).
pub type Reservation = store_io_engine::Reservation<NativePlatform>;
/// Many reads and writes in flight from one thread, without blocking (see
/// [`store_io_engine::Submitter`]); from [`Store::submitter`](store_io_engine::Store::submitter).
pub type Submitter = store_io_engine::Submitter<NativePlatform>;

/// A store: one container file in a directory of its own, on one device.
///
/// Every method of [`store_io_engine::Store`] is available through `Deref`:
/// provisioning and looking up regions and slots, `reserve`, `space`,
/// `report`, `poison`.
///
/// ```no_run
/// use store_io::{Store, StoreOptions};
///
/// let store = Store::create("data/orders", StoreOptions::default())?;
/// println!("{}", store.report());
/// drop(store);
/// let store = Store::open("data/orders")?;
/// let wal = store.append_region("wal");
/// # let _ = wal;
/// # Ok::<(), store_io::Error>(())
/// ```
#[derive(Clone, Debug)]
pub struct Store(store_io_engine::Store<NativePlatform>);

impl Store {
    /// Creates a new store in `path` (the directory is created if needed).
    /// The device is probed first: unsafe or unverified devices are refused
    /// unless `opts.trust` attests or overrides (and then every receipt is
    /// labelled accordingly).
    ///
    /// # Errors
    ///
    /// `AlreadyExists`, `UnsafeDevice`, `Unverified`, `NoSpace`, `Io`.
    pub fn create(path: impl AsRef<Path>, opts: StoreOptions) -> Result<Self, Error> {
        store_io_engine::Store::create(platform(opts.linux_tier)?, path_of(&path), opts).map(Self)
    }

    /// Opens an existing store for writing, with default options. Takes the
    /// ownership lock, waits out a dead owner's writes, and verifies every
    /// region against its header. Append regions must be positioned with
    /// `resume_at` before the first append.
    ///
    /// # Errors
    ///
    /// `NotFound`, `Locked`, `UnsafeDevice`, `Unverified`, `Corruption`, `Io`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        Self::open_with(path, StoreOptions::default())
    }

    /// Opens an existing store for writing with explicit options.
    ///
    /// # Errors
    ///
    /// As [`Self::open`].
    pub fn open_with(path: impl AsRef<Path>, opts: StoreOptions) -> Result<Self, Error> {
        store_io_engine::Store::open(platform(opts.linux_tier)?, path_of(&path), opts).map(Self)
    }

    /// Opens an existing store for reading and recovery only: no write
    /// rights, and nothing on disk changes.
    ///
    /// # Errors
    ///
    /// `NotFound`, `Corruption`, `Io`.
    pub fn open_readonly(path: impl AsRef<Path>) -> Result<Self, Error> {
        Self::open_readonly_with(path, StoreOptions::default())
    }

    /// Opens an existing store for reading and recovery only, with
    /// explicit options (for example `linux_tier: LinuxTier::Sync`).
    ///
    /// # Errors
    ///
    /// As [`Self::open_readonly`].
    pub fn open_readonly_with(path: impl AsRef<Path>, opts: StoreOptions) -> Result<Self, Error> {
        store_io_engine::Store::open_readonly(platform(opts.linux_tier)?, path_of(&path), opts)
            .map(Self)
    }

    /// The engine store underneath.
    #[must_use]
    pub fn engine(&self) -> &store_io_engine::Store<NativePlatform> {
        &self.0
    }
}

impl Deref for Store {
    type Target = store_io_engine::Store<NativePlatform>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<store_io_engine::Store<NativePlatform>> for Store {
    fn from(s: store_io_engine::Store<NativePlatform>) -> Self {
        Self(s)
    }
}

/// A directory of small files, each replaced atomically and durably (see
/// [`store_io_engine::Directory`]).
///
/// ```no_run
/// use store_io::Directory;
///
/// let dir = Directory::open("data/meta", true)?;
/// dir.replace("CURRENT", b"MANIFEST-000042")?;
/// assert_eq!(dir.read("CURRENT")?, b"MANIFEST-000042");
/// # Ok::<(), store_io::Error>(())
/// ```
#[derive(Debug)]
pub struct Directory(store_io_engine::Directory<NativePlatform>);

impl Directory {
    /// Opens `path` (creating it if `create`), with default trust.
    ///
    /// # Errors
    ///
    /// `NotFound`, `Io`.
    pub fn open(path: impl AsRef<Path>, create: bool) -> Result<Self, Error> {
        Self::open_with(path, create, Trust::default())
    }

    /// Opens `path` with explicit trust inputs.
    ///
    /// # Errors
    ///
    /// As [`Self::open`].
    pub fn open_with(path: impl AsRef<Path>, create: bool, trust: Trust) -> Result<Self, Error> {
        store_io_engine::Directory::open(platform(LinuxTier::Auto)?, path_of(&path), create, trust)
            .map(Self)
    }
}

impl Deref for Directory {
    type Target = store_io_engine::Directory<NativePlatform>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

fn io(op: Op, raw: OsError) -> Error {
    Error::Io {
        op,
        raw,
        ctx: ErrorContext::default(),
    }
}

/// Copies the store in `from` into a new store (container format 2) in
/// `to`, whose directory must hold no store: the way a format-1 store
/// moves to format 2, and the way to compact a format-2 one. The source is
/// opened read-only and never written; the copy is built under a temporary
/// name and renamed into place only when whole and durable, so after a
/// crash `to` holds nothing or the whole copy (run the copy again).
///
/// ```no_run
/// use store_io::CopyOptions;
///
/// let r = store_io::copy_store("data/orders", "data/orders-2", CopyOptions::default())?;
/// println!("{} regions, {} bytes", r.regions, r.bytes);
/// # Ok::<(), store_io::Error>(())
/// ```
///
/// # Errors
///
/// As [`store_io_engine::copy_store`], among them: `AlreadyExists` if `to`
/// holds a store; `Locked` while a writer holds `from` open or another
/// copy into `to` runs;
/// `NotWritten(NotReady | Frozen | TooLarge)` for a source it cannot copy
/// whole, before anything is created; `NotWritten(Misaligned)` if `to`'s
/// device needs larger blocks than the source's; `NotFound`, `Corruption`
/// or `Unsupported` opening the source; `UnsafeDevice`, `Unverified` (the
/// target's device refused: see `StoreOptions::trust`), `NoSpace`, `Io`;
/// `DurabilityUnknown` if the final rename or directory flush failed (the
/// copy may or may not be in place).
pub fn copy_store(
    from: impl AsRef<Path>,
    to: impl AsRef<Path>,
    opts: CopyOptions,
) -> Result<CopyReport, Error> {
    store_io_engine::copy_store(
        platform(opts.store.linux_tier)?,
        path_of(&from),
        path_of(&to),
        opts,
    )
}

/// Probes the device under the existing directory `dir`: the evidence
/// store-io reads and the durability class it would decide with default
/// trust. Creates, probes and removes a small temporary file there.
///
/// ```no_run
/// let p = store_io::probe("data")?;
/// println!("{:?}: {:?}", p.decision.class, p.decision.reasons);
/// # Ok::<(), store_io::Error>(())
/// ```
///
/// # Errors
///
/// `NotFound`, `Io`.
pub fn probe(dir: impl AsRef<Path>) -> Result<Probe, Error> {
    let p = NativePlatform::new();
    let d = p.open_dir(path_of(&dir), false).map_err(|raw| {
        if store_io_core::errno::is_not_found(raw) {
            Error::NotFound { what: Named::Store }
        } else {
            io(Op::Open, raw)
        }
    })?;
    // Unique per call (process, a counter and a random part, for processes
    // in different PID namespaces sharing a directory), so concurrent
    // probes of one directory never share, or remove, each other's file.
    static PROBES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = PROBES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let salt = {
        use std::hash::{BuildHasher, Hasher};
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(n);
        h.finish()
    };
    let unique = format!(".store-io-probe-{}-{n}-{salt:016x}", std::process::id());
    let name = FileName::new(&unique).map_err(|_| Error::NotWritten {
        cause: NotWrittenCause::InvalidName,
        ctx: ErrorContext::default(),
    })?;
    let file = p
        .create_file(&d, &name)
        .map_err(|raw| io(Op::Create, raw))?;
    let evidence = p
        .allocate(&file, 4096)
        .and_then(|()| p.probe(&d, &file))
        .map_err(|raw| io(Op::Probe, raw));
    drop(file);
    let _removed = p.unlink(&d, &name);
    let evidence = evidence?;
    let decision = decide(&evidence, &Trust::default());
    Ok(Probe { evidence, decision })
}
