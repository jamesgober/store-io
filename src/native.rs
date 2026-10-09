//! The simple API on the operating system's native backend.

use std::ops::Deref;
use std::path::Path;

use store_io_core::decide::{Trust, decide};
use store_io_core::error::{Error, ErrorContext, Named, NotWrittenCause, Op, OsError};
use store_io_platform::{FileName, Platform};

use crate::{NativePlatform, Probe, StoreOptions};

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
        store_io_engine::Store::create(NativePlatform::new(), path_of(&path), opts).map(Self)
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
        store_io_engine::Store::open(NativePlatform::new(), path_of(&path), opts).map(Self)
    }

    /// Opens an existing store for reading and recovery only: no write
    /// rights, and nothing on disk changes.
    ///
    /// # Errors
    ///
    /// `NotFound`, `Corruption`, `Io`.
    pub fn open_readonly(path: impl AsRef<Path>) -> Result<Self, Error> {
        store_io_engine::Store::open_readonly(
            NativePlatform::new(),
            path_of(&path),
            StoreOptions::default(),
        )
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
        store_io_engine::Directory::open(NativePlatform::new(), path_of(&path), create, trust)
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
    let name = FileName::new(&format!(".store-io-probe-{}", std::process::id())).map_err(|_| {
        Error::NotWritten {
            cause: NotWrittenCause::InvalidName,
            ctx: ErrorContext::default(),
        }
    })?;
    let _leftover = p.unlink(&d, &name);
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
