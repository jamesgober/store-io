//! Device pacing (PLANNING R5.4, D-146): background work is split into
//! pieces and, while a device's commits are active, admitted one piece at a
//! time per issuer, so commits on the device stay near their idle latency.
//!
//! Layer 0 of the engine (FILEMAP's layer rule): this module names no engine
//! module. Stores join a process-wide [`Registry`] by their device's keys
//! and get a [`Qos`] member; a background call holds an [`Issuer`] record
//! and charges each piece it submits ([`Issuer::admit`]), the [`Charge`]
//! released when the piece is reaped or abandoned. Commits mark activity
//! ([`Qos::commit`]); a [`CommitHold`] keeps it.
//!
//! **Groups.** Stores whose keys meet share a group: a device node, a
//! device name, a btrfs member disk, or the device the file system reports.
//! A store meeting two groups merges them, the younger into the older, no
//! count ever moved: each member keeps its own counters, the group's
//! immutable [`Record`] lists them, and the group admits against their sum.
//! A store whose keys name no device node is **unknown**: never merged,
//! paced by activity shared across its scope instead ([`ScopeActivity`]). A
//! store on a memory file system joins nothing and is never paced.
//!
//! **Activity, without a timer.** A commit-class barrier entry stores the
//! fine clock in its member's `last_commit` (at most once a millisecond: a
//! compare before the store); background issuers read it at their own
//! admissions. Nothing waits for it, nothing wakes because of it.
//!
//! **Pacing is latency only**: it never changes what is written or when it
//! is durable, and every refusal is back-pressure the caller resolves by
//! reaping its own pieces.

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError, Weak};

#[cfg(loom)]
use loom::sync::{Condvar, Mutex, RwLock};
#[cfg(not(loom))]
use std::sync::{Condvar, Mutex, RwLock};

use store_io_buf::CachePadded;
use store_io_core::identity::{DeviceId, DeviceIdentity, DeviceNode};

use crate::sync::{AtomicBool, AtomicU64, Ordering};

/// Nanoseconds between two stores of `last_commit` (R5.4: at most once a
/// millisecond, so the line stays shared).
const COMMIT_STRIDE_NS: u64 = 1_000_000;

// Time: every call that needs it is given `now`, its store's platform's
// fine clock (`monotonic_ns`), read by the caller (the store holds the
// platform; this module holds none). Members of a group compare their own
// stamps with each other's, so the clocks of one scope share one time base:
// the operating system's clock for every real backend in a process, and a
// world's clock in the simulator, whose worlds are scopes of their own.

/// A member's pacing options (`StoreOptions`'s, R5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QosOptions {
    /// Pacing on (`background_pacing`, default true).
    pub(crate) pacing: bool,
    /// How long after a commit commits stay active (`commit_quiet`, default
    /// 5 s).
    pub(crate) commit_quiet_ns: u64,
    /// The piece size background I/O is split at (`background_io`, default
    /// 256 KiB).
    pub(crate) background_io: u32,
    /// Pieces per issuer while commits are not active (`background_unpaced`,
    /// default 8).
    pub(crate) background_unpaced: u32,
    /// Auto-tuning of the group's budget (`background_autotune`, default
    /// off; V3f).
    pub(crate) autotune: bool,
    /// Auto-tuning's ceiling (`background_depth_max`, default 8).
    pub(crate) depth_max: u32,
}

impl Default for QosOptions {
    fn default() -> Self {
        Self {
            pacing: true,
            commit_quiet_ns: 5_000_000_000,
            background_io: 256 << 10,
            background_unpaced: 8,
            autotune: false,
            depth_max: 8,
        }
    }
}

impl QosOptions {
    /// The group rule for stores that disagree (D-153): pacing on if any
    /// member asks for it, the largest quiet, the smallest caps, auto-tuning
    /// only if every member asks for it. `background_io` stays each
    /// issuer's own store's.
    fn merge(self, other: Self) -> Self {
        Self {
            pacing: self.pacing || other.pacing,
            commit_quiet_ns: self.commit_quiet_ns.max(other.commit_quiet_ns),
            background_io: self.background_io,
            background_unpaced: self.background_unpaced.min(other.background_unpaced),
            autotune: self.autotune && other.autotune,
            depth_max: self.depth_max.min(other.depth_max),
        }
    }
}

/// How a store is placed for pacing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Placement {
    /// Its device's keys: identity nodes and names, btrfs members, the file
    /// system's device.
    Keyed {
        /// `DeviceFacts::identity`.
        identity: DeviceIdentity,
        /// `DeviceFacts::members`.
        members: Vec<DeviceId>,
        /// `FsFacts::device`.
        fs_device: Option<u64>,
    },
    /// A memory file system (tmpfs, ramfs): no device to protect; never
    /// paced, its commits activity for nobody.
    Memory,
}

/// One key a group is found by, within a scope.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Node(String),
    Name(String),
    FsDevice(u64),
}

/// The keys of a placement, and whether any names a device node (else the
/// store is unknown).
fn keys_of(
    identity: &DeviceIdentity,
    members: &[DeviceId],
    fs_device: Option<u64>,
) -> (Vec<Key>, bool) {
    let mut keys = Vec::new();
    let mut named = false;
    for d in identity.devices.iter().chain(members) {
        named = true;
        keys.push(Key::Node(node_key(&d.node)));
        keys.extend(d.names.iter().cloned().map(Key::Name));
    }
    if let Some(dev) = fs_device {
        keys.push(Key::FsDevice(dev));
    }
    keys.sort();
    keys.dedup();
    (keys, named)
}

fn node_key(n: &DeviceNode) -> String {
    format!("{n:?}")
}

// ------------------------------------------------------------- counters

/// One member's counts, listed by its group's [`Record`]: owned by the
/// member and the records, never moved between groups.
#[derive(Default)]
pub(crate) struct Counters {
    /// The fine clock at the member's last commit-class barrier entry (0:
    /// none yet). Read and compared at every commit: on a line of its own,
    /// so background pieces and holds never bounce it.
    last_commit: CachePadded<AtomicU64>,
    /// Live [`CommitHold`]s on the member.
    holds: AtomicU64,
    /// Background pieces charged to the member and not yet released.
    in_flight: AtomicU64,
}

impl Counters {
    fn active(&self, now: u64, quiet: u64) -> bool {
        self.holds.load(Ordering::Relaxed) > 0
            || recent(self.last_commit.load(Ordering::Relaxed), now, quiet)
    }
}

/// Whether a commit at `at` (0: never) is within `quiet` of `now`.
fn recent(at: u64, now: u64, quiet: u64) -> bool {
    at != 0 && now.saturating_sub(at) < quiet
}

/// Raises `word` to `now` unless it is within a millisecond of it (the
/// compare before the write, so the line stays shared; the write a
/// `fetch_max`, so a thread pre-empted between the two never lowers it).
fn stamp(word: &AtomicU64, now: u64) {
    let at = word.load(Ordering::Relaxed);
    if now >= at.saturating_add(COMMIT_STRIDE_NS) || at == 0 {
        let _was = word.fetch_max(now.max(1), Ordering::Relaxed);
    }
}

/// A group's members and options, immutable once published: a merge or a
/// join publishes a new one to every member.
pub(crate) struct Record {
    counters: Vec<Arc<Counters>>,
    opts: QosOptions,
    /// The latest commit of members pruned from the record (closed stores):
    /// still activity within the quiet.
    retired: Arc<AtomicU64>,
}

impl Record {
    fn in_flight(&self) -> u64 {
        self.counters
            .iter()
            .map(|c| c.in_flight.load(Ordering::Relaxed))
            .sum()
    }

    fn active(&self, now: u64) -> bool {
        let quiet = self.opts.commit_quiet_ns;
        recent(self.retired.load(Ordering::Relaxed), now, quiet)
            || self.counters.iter().any(|c| c.active(now, quiet))
    }
}

/// A group's published record, shared by its members (every member of a
/// merged group points at the same record through its own cell).
type Cell = RwLock<Arc<Record>>;

fn read(cell: &Cell) -> Arc<Record> {
    Arc::clone(&cell.read().unwrap_or_else(PoisonError::into_inner))
}

// ---------------------------------------------------------------- scope

/// Activity shared across one scope (D-153, FILEMAP draft 23): unknown
/// stores are paced whenever any store of their scope commits, and their
/// commits pace every group's background work, without merging groups.
#[derive(Default)]
pub(crate) struct ScopeActivity {
    /// Unknown stores open in the scope: changed under the registry's lock
    /// at each unknown store's join and close, never at a group's drop.
    /// Read at every commit of the scope: on a line of its own, so holds
    /// taken and dropped never bounce it.
    unknown_members: CachePadded<AtomicU64>,
    /// The latest commit of any store of the scope, written while
    /// `unknown_members > 0` (seeded at 0 to 1).
    any_commit: AtomicU64,
    /// The latest commit of an unknown store.
    unknown_commit: AtomicU64,
    /// Every live hold of the scope.
    holds: AtomicU64,
    /// Every live hold of an unknown store.
    unknown_holds: AtomicU64,
}

impl ScopeActivity {
    fn has_unknown(&self) -> bool {
        self.unknown_members.load(Ordering::Relaxed) > 0
    }
}

// ------------------------------------------------------------- registry

/// One registered group.
struct Entry {
    /// The order groups were made in: a merge keeps the oldest.
    seq: u64,
    scope: u64,
    keys: Vec<Key>,
    /// Every member's cell (each points at the group's record).
    cells: Vec<Weak<Cell>>,
    /// The group's current record (also reachable through the cells).
    record: Weak<Record>,
    retired: Arc<AtomicU64>,
}

#[derive(Default)]
struct Groups {
    next: u64,
    entries: Vec<Entry>,
    scopes: BTreeMap<u64, Weak<ScopeActivity>>,
    /// The unknown group of each scope (its cells and record, as an entry
    /// with no keys).
    unknown: BTreeMap<u64, Entry>,
}

/// The process-wide registry of device groups (R5.4: per device, not per
/// store). Joins and closes take its lock; nothing on the I/O path does.
pub(crate) struct Registry {
    groups: Mutex<Groups>,
}

impl Registry {
    pub(crate) fn new() -> Self {
        Self {
            groups: Mutex::new(Groups::default()),
        }
    }

    /// The process's registry. (Under loom it exists too, never reached:
    /// the models make their own registries and open no store.)
    pub(crate) fn global() -> &'static Registry {
        static R: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
        R.get_or_init(Registry::new)
    }

    /// Joins a store, placed by `placement` in `scope` (its platform's
    /// `identity_scope`), to its group: the one its keys meet (merging every
    /// group they meet into the oldest), a new one, or its scope's unknown
    /// group. Never fails: what the evidence does not name pacing treats in
    /// the safe direction.
    pub(crate) fn join(
        &'static self,
        scope: u64,
        placement: &Placement,
        opts: QosOptions,
        now: u64,
    ) -> Arc<Qos> {
        let mut g = self.groups.lock().unwrap_or_else(PoisonError::into_inner);
        let activity = g.scope(scope);
        let counters = Arc::new(Counters::default());
        let Placement::Keyed {
            identity,
            members,
            fs_device,
        } = placement
        else {
            return Arc::new(Qos {
                counters,
                cell: None,
                scope: activity,
                kind: Kind::Unpaced,
                opts,
                registry: self,
            });
        };
        let (keys, named) = keys_of(identity, members, *fs_device);
        g.prune();
        let cell = if named {
            g.join_keyed(scope, keys, &counters, opts)
        } else {
            if activity.unknown_members.load(Ordering::Relaxed) == 0 {
                // Seeded under the lock with the join's own time: a commit
                // racing the join (its stamp and its read of the count both
                // relaxed) may be missed by both sides, so the first unknown
                // store counts commits active for `commit_quiet` from its
                // join (background work over-paced at most that long: the
                // safe direction), never reads a stale word as quiet.
                let latest = g.latest_commit(scope).max(now);
                let _was = activity.any_commit.fetch_max(latest, Ordering::Relaxed);
            }
            let _was = activity.unknown_members.fetch_add(1, Ordering::Relaxed);
            g.join_unknown(scope, &counters, opts)
        };
        Arc::new(Qos {
            counters,
            cell: Some(cell),
            scope: activity,
            kind: if named { Kind::Named } else { Kind::Unknown },
            opts,
            registry: self,
        })
    }

    fn closed(&self, q: &Qos) {
        if q.kind == Kind::Unknown {
            let _g = self.groups.lock().unwrap_or_else(PoisonError::into_inner);
            let was = q.scope.unknown_members.fetch_sub(1, Ordering::Relaxed);
            if was == 1 {
                // The last unknown store: its commits pace nobody any more,
                // and a later one starts from its own seed (the scope's word
                // is written only while unknown stores live).
                q.scope.unknown_commit.store(0, Ordering::Relaxed);
                q.scope.any_commit.store(0, Ordering::Relaxed);
            }
        }
    }
}

impl Groups {
    fn scope(&mut self, scope: u64) -> Arc<ScopeActivity> {
        if let Some(a) = self.scopes.get(&scope).and_then(Weak::upgrade) {
            return a;
        }
        let a = Arc::new(ScopeActivity::default());
        let _old = self.scopes.insert(scope, Arc::downgrade(&a));
        a
    }

    /// Drops groups no member holds any more, and records' counters only
    /// the record holds (closed stores with nothing in flight and no hold),
    /// folding their last commit into the group's retired word.
    fn prune(&mut self) {
        self.entries
            .retain(|e| e.cells.iter().any(|c| c.strong_count() > 0));
        self.unknown
            .retain(|_, e| e.cells.iter().any(|c| c.strong_count() > 0));
        self.scopes.retain(|_, s| s.strong_count() > 0);
        for e in self.entries.iter_mut().chain(self.unknown.values_mut()) {
            e.cells.retain(|c| c.strong_count() > 0);
            let Some(rec) = e.record.upgrade() else {
                continue;
            };
            let dead = |c: &Arc<Counters>| {
                Arc::strong_count(c) == 1
                    && c.in_flight.load(Ordering::Relaxed) == 0
                    && c.holds.load(Ordering::Relaxed) == 0
            };
            if rec.counters.iter().any(dead) {
                for c in rec.counters.iter().filter(|c| dead(c)) {
                    let _was = e
                        .retired
                        .fetch_max(c.last_commit.load(Ordering::Relaxed), Ordering::Relaxed);
                }
                let counters = rec.counters.iter().filter(|c| !dead(c)).cloned().collect();
                let next = Arc::new(Record {
                    counters,
                    opts: rec.opts,
                    retired: Arc::clone(&e.retired),
                });
                publish(e, &next);
            }
        }
    }

    /// The latest commit any group of `scope` has seen.
    fn latest_commit(&self, scope: u64) -> u64 {
        self.entries
            .iter()
            .filter(|e| e.scope == scope)
            .filter_map(|e| e.record.upgrade())
            .map(|r| {
                r.counters
                    .iter()
                    .map(|c| c.last_commit.load(Ordering::Relaxed))
                    .chain([r.retired.load(Ordering::Relaxed)])
                    .max()
                    .unwrap_or(0)
            })
            .max()
            .unwrap_or(0)
    }

    fn join_keyed(
        &mut self,
        scope: u64,
        keys: Vec<Key>,
        counters: &Arc<Counters>,
        opts: QosOptions,
    ) -> Arc<Cell> {
        let met: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                e.scope == scope && e.keys.iter().any(|k| keys.binary_search(k).is_ok())
            })
            .map(|(i, _)| i)
            .collect();
        let mut merged_keys = keys;
        let mut cells = Vec::new();
        let mut members: Vec<Arc<Counters>> = Vec::new();
        let mut merged_opts = opts;
        let mut retired_at = 0;
        let mut seq = None;
        // Oldest first: the merge keeps the oldest group's place.
        let mut met: Vec<Entry> = met
            .into_iter()
            .rev()
            .map(|i| self.entries.swap_remove(i))
            .collect();
        met.sort_by_key(|e| e.seq);
        for e in met {
            let _first = seq.get_or_insert(e.seq);
            merged_keys.extend(e.keys);
            cells.extend(e.cells);
            retired_at = retired_at.max(e.retired.load(Ordering::Relaxed));
            if let Some(r) = e.record.upgrade() {
                members.extend(r.counters.iter().cloned());
                merged_opts = merged_opts.merge(r.opts);
            }
        }
        merged_keys.sort();
        merged_keys.dedup();
        members.push(Arc::clone(counters));
        let seq = seq.unwrap_or_else(|| {
            self.next += 1;
            self.next
        });
        let retired = Arc::new(AtomicU64::new(retired_at));
        let record = Arc::new(Record {
            counters: members,
            opts: merged_opts,
            retired: Arc::clone(&retired),
        });
        let mine = Arc::new(RwLock::new(Arc::clone(&record)));
        cells.push(Arc::downgrade(&mine));
        let mut e = Entry {
            seq,
            scope,
            keys: merged_keys,
            cells,
            record: Arc::downgrade(&record),
            retired,
        };
        publish(&mut e, &record);
        self.entries.push(e);
        mine
    }

    fn join_unknown(
        &mut self,
        scope: u64,
        counters: &Arc<Counters>,
        opts: QosOptions,
    ) -> Arc<Cell> {
        let e = self.unknown.entry(scope).or_insert_with(|| Entry {
            seq: 0,
            scope,
            keys: Vec::new(),
            cells: Vec::new(),
            record: Weak::new(),
            retired: Arc::new(AtomicU64::new(0)),
        });
        let (mut members, merged) = match e.record.upgrade() {
            Some(r) => (r.counters.clone(), r.opts.merge(opts)),
            None => (Vec::new(), opts),
        };
        members.push(Arc::clone(counters));
        let record = Arc::new(Record {
            counters: members,
            opts: merged,
            retired: Arc::clone(&e.retired),
        });
        let mine = Arc::new(RwLock::new(Arc::clone(&record)));
        e.cells.push(Arc::downgrade(&mine));
        e.record = Arc::downgrade(&record);
        publish(e, &record);
        mine
    }
}

/// Points every live cell of `e` at `record`.
fn publish(e: &mut Entry, record: &Arc<Record>) {
    e.record = Arc::downgrade(record);
    for c in e.cells.iter().filter_map(Weak::upgrade) {
        *c.write().unwrap_or_else(PoisonError::into_inner) = Arc::clone(record);
    }
}

// ------------------------------------------------------------------ Qos

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Named,
    Unknown,
    Unpaced,
}

/// A store's place in its device group.
pub(crate) struct Qos {
    counters: Arc<Counters>,
    /// The group's record (`None`: a memory file system's store, never
    /// paced).
    cell: Option<Arc<Cell>>,
    scope: Arc<ScopeActivity>,
    kind: Kind,
    opts: QosOptions,
    registry: &'static Registry,
}

impl Drop for Qos {
    fn drop(&mut self) {
        self.registry.closed(self);
    }
}

impl Qos {
    /// A commit-class barrier's entry (leading or joining) or a commit-class
    /// `dsync` write on a power-safe store: marks the group's commits
    /// active. One clock read, a relaxed load and a compare; a store at most
    /// once a millisecond.
    pub(crate) fn commit(&self, now: u64) {
        match self.kind {
            Kind::Unpaced => {}
            Kind::Named | Kind::Unknown => {
                stamp(&self.counters.last_commit, now);
                if self.kind == Kind::Unknown {
                    stamp(&self.scope.unknown_commit, now);
                }
                if self.scope.has_unknown() {
                    stamp(&self.scope.any_commit, now);
                }
            }
        }
    }

    /// Holds commits active until the hold drops (`Store::commit_hold`).
    /// A memory file system's store's hold paces nobody.
    pub(crate) fn hold(self: &Arc<Self>) -> CommitHold {
        if self.kind != Kind::Unpaced {
            let _h = self.counters.holds.fetch_add(1, Ordering::Relaxed);
            let _s = self.scope.holds.fetch_add(1, Ordering::Relaxed);
            if self.kind == Kind::Unknown {
                let _u = self.scope.unknown_holds.fetch_add(1, Ordering::Relaxed);
            }
        }
        CommitHold {
            qos: Arc::clone(self),
        }
    }

    /// The group's record (`None` for a never-paced store), cloned: for
    /// reports, off the I/O path.
    fn record(&self) -> Option<Arc<Record>> {
        self.cell.as_deref().map(read)
    }

    /// Runs `f` on the group's record under its cell's read guard, without
    /// cloning it (no shared count touched): the admission path's one read.
    /// The cell's lock is written only by joins and closes (a merge's
    /// publish), never by I/O, so the read never waits on the I/O path.
    fn with_record<R>(&self, f: impl FnOnce(Option<&Record>) -> R) -> R {
        match self.cell.as_deref() {
            None => f(None),
            Some(c) => {
                let g = c.read().unwrap_or_else(PoisonError::into_inner);
                f(Some(&**g))
            }
        }
    }

    /// Whether `a` and `b` are paced as one device group now (one
    /// published record): a copy's reader then shares its fill's issuer
    /// record (R5.4: one record per device group a call uses). Stores never
    /// paced (a memory file system's) are in no group.
    pub(crate) fn same_group(a: &Self, b: &Self) -> bool {
        match (a.record(), b.record()) {
            (Some(x), Some(y)) => Arc::ptr_eq(&x, &y),
            _ => false,
        }
    }

    /// Whether the group's commits are active now.
    pub(crate) fn active(&self, now: u64) -> bool {
        self.with_record(|r| r.is_some_and(|r| self.active_in(r, now)))
    }

    fn active_in(&self, rec: &Record, now: u64) -> bool {
        let quiet = rec.opts.commit_quiet_ns;
        let s = &self.scope;
        rec.active(now)
            || match self.kind {
                Kind::Named => {
                    s.has_unknown()
                        && (s.unknown_holds.load(Ordering::Relaxed) > 0
                            || recent(s.unknown_commit.load(Ordering::Relaxed), now, quiet))
                }
                Kind::Unknown => {
                    s.holds.load(Ordering::Relaxed) > 0
                        || recent(s.any_commit.load(Ordering::Relaxed), now, quiet)
                }
                Kind::Unpaced => false,
            }
    }

    /// The piece size this store's background I/O is split at.
    pub(crate) fn background_io(&self) -> u32 {
        self.opts.background_io
    }

    /// What `Store::qos` reports.
    pub(crate) fn report(&self, now: u64) -> QosReport {
        let rec = self.record();
        QosReport {
            paced: rec.as_ref().is_some_and(|r| r.opts.pacing),
            autotune: rec
                .as_ref()
                .is_some_and(|r| r.opts.pacing && r.opts.autotune),
            depth_max: rec
                .as_ref()
                .map_or(self.opts.depth_max, |r| r.opts.depth_max),
            active: self.active(now),
            budget: 1,
            in_flight: rec.as_ref().map_or(0, |r| r.in_flight()),
            members: rec.as_ref().map_or(1, |r| r.counters.len()),
            unknown: self.kind == Kind::Unknown,
            background_io: self.opts.background_io,
        }
    }
}

/// What a store's device pacing reports ([`crate::Store::qos`], PLANNING
/// R5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct QosReport {
    /// Pacing is on for the store's device group.
    pub paced: bool,
    /// Auto-tuning is on for the group (every member asked for it, and
    /// pacing is on). Not yet wired (V3f3): hidden until then.
    #[doc(hidden)]
    pub autotune: bool,
    /// Auto-tuning's ceiling for the group (its members' smallest
    /// `background_depth_max`). Not yet wired (V3f3).
    #[doc(hidden)]
    pub depth_max: u32,
    /// The group's commits are active now (a commit within `commit_quiet`,
    /// or a live [`CommitHold`]).
    pub active: bool,
    /// Background pieces in flight the group admits beyond each background
    /// call's own one while commits are active (the floor, 1).
    pub budget: u64,
    /// Background pieces in flight in the group now.
    pub in_flight: u64,
    /// Stores in the group (closed ones pruned at the next join).
    pub members: usize,
    /// The store's device could not be named: it is paced by every commit
    /// of its process instead of by its device's.
    pub unknown: bool,
    /// The piece this store's background I/O is split at, in bytes.
    pub background_io: u32,
}

/// Keeps a store's device group's commits active while it lives
/// ([`crate::Store::commit_hold`], PLANNING R5.4): background work on the
/// device stays paced for as long as the hold lives.
#[must_use = "a hold keeps commits active only while it lives"]
pub struct CommitHold {
    qos: Arc<Qos>,
}

impl core::fmt::Debug for CommitHold {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("CommitHold")
    }
}

impl Drop for CommitHold {
    fn drop(&mut self) {
        let q = &self.qos;
        if q.kind == Kind::Unpaced {
            return;
        }
        let _h = q.counters.holds.fetch_sub(1, Ordering::Relaxed);
        let _s = q.scope.holds.fetch_sub(1, Ordering::Relaxed);
        if q.kind == Kind::Unknown {
            let _u = q.scope.unknown_holds.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

// --------------------------------------------------------------- issuers

/// Where poison reaches an issuer record (`qos` names no engine module: the
/// domain implements this, D-153).
///
/// Contract: `poisoned` takes no lock that is held while a registered wake
/// runs, and a wake never runs inside `poisoned` (a waiter calls `poisoned`
/// under its record's lock, and a wake takes that lock). The domain's poison
/// is a `OnceLock` read, and its wakers run with no domain lock held.
pub(crate) trait PoisonSource: Send + Sync {
    /// Whether the store is poisoned.
    fn poisoned(&self) -> bool;
    /// Calls `wake` when the store is poisoned after this registration
    /// (never for a poison already set), until the registration drops.
    fn on_poison(&self, wake: Arc<dyn Fn() + Send + Sync>) -> Box<dyn Send + Sync>;
}

/// A watched store and its poison registration (dropped with the record).
type Watched = (Arc<dyn PoisonSource>, Box<dyn Send + Sync>);

/// Why a wait on an issuer record ended without an admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ended {
    /// Another part of the call ended with an error (its caller reports
    /// that part's own error).
    Ended,
    /// One of the call's own stores is poisoned.
    Poisoned,
}

/// One background call's record (R5.4: an issuer is a call, not a thread or
/// a queue): its pieces in flight, shared by every thread of the call.
pub(crate) struct Issuer {
    qos: Arc<Qos>,
    /// This call's pieces in flight.
    in_flight: AtomicU64,
    /// Threads waiting for a release (read by a read-modify-write after a
    /// release's decrement).
    waiters: AtomicU64,
    /// A part of the call ended with an error.
    ended: AtomicBool,
    lock: Mutex<()>,
    released: Condvar,
    sources: Mutex<Vec<Watched>>,
}

impl Issuer {
    /// A record for a background call on `qos`'s group.
    pub(crate) fn new(qos: &Arc<Qos>) -> Arc<Self> {
        Arc::new(Self {
            qos: Arc::clone(qos),
            in_flight: AtomicU64::new(0),
            waiters: AtomicU64::new(0),
            ended: AtomicBool::new(false),
            lock: Mutex::new(()),
            released: Condvar::new(),
            sources: Mutex::new(Vec::new()),
        })
    }

    /// Watches one of the call's own stores for poison (at binding, never on
    /// the submit path; D-153: only the call's own stores).
    pub(crate) fn watch(self: &Arc<Self>, source: Arc<dyn PoisonSource>) {
        let me = Arc::downgrade(self);
        let reg = source.on_poison(Arc::new(move || {
            if let Some(i) = me.upgrade() {
                i.wake();
            }
        }));
        self.sources
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((source, reg));
        // A poison set before the registration wakes nobody, and one set
        // between it and the push woke a waiter that could not yet see this
        // source: wake now that it can.
        self.wake();
    }

    /// Admits one piece, or refuses it: paced (pacing on and the group's
    /// commits active), only if the call has none in flight or the group's
    /// pieces are below the budget; unpaced, at most `background_unpaced`
    /// pieces of this call. Admitted, the piece is charged until the
    /// returned [`Charge`] drops. One clock read and one read of the group's
    /// record, under its guard; no allocation.
    ///
    /// Activity is read here, at every admission, never cached: so an issuer
    /// registered during continuous commits is paced from its first piece
    /// (R5.4's "starts paced": it never reads a stale quiet).
    pub(crate) fn admit(self: &Arc<Self>, now: u64) -> Option<Charge> {
        self.admit_leaving(now, 0)
    }

    /// Whether the group paces background work at `now` (pacing on and its
    /// commits active), and the pieces a call may keep in flight while it
    /// does not (`background_unpaced`): what [`Self::admit`] reads, for a
    /// call sizing its next piece (D-156). A hint: activity may begin
    /// after the piece is sized and before it is admitted (a fill sizes it
    /// before it fetches its content, which may wait on the call's reader),
    /// so at a switch to active one quiet-sized piece may still go out.
    pub(crate) fn pacing(self: &Arc<Self>, now: u64) -> (bool, u64) {
        self.qos.with_record(|rec| {
            let paced = rec.is_some_and(|r| self.qos.active_in(r, now) && r.opts.pacing);
            let cap = rec.map_or(self.qos.opts.background_unpaced, |r| {
                r.opts.background_unpaced
            });
            (paced, u64::from(cap.max(1)))
        })
    }

    /// As [`Self::admit`], leaving `leave` of the call's unpaced pieces to
    /// its other threads (a copy's reader leaves one to the fill sharing
    /// its record, so the reader's read-ahead never refuses the fill).
    /// With nothing of the call in flight a piece is admitted whatever
    /// `leave`; paced, `leave` changes nothing (the call has one piece).
    pub(crate) fn admit_leaving(self: &Arc<Self>, now: u64, leave: u64) -> Option<Charge> {
        let admitted = self.qos.with_record(|rec| {
            let paced = rec.is_some_and(|r| r.opts.pacing && self.qos.active_in(r, now));
            let cap = u64::from(
                rec.map_or(self.qos.opts.background_unpaced, |r| {
                    r.opts.background_unpaced
                })
                .max(1),
            );
            // The piece is charged to the member first, tentatively, so a
            // thread that sees another's compare-and-swap below also sees its
            // charge in the group's sum (the sum counts this tentative
            // charge: hence `<=` the budget); then a compare-and-swap on the
            // call's count, so two threads of one call never both take its
            // one paced piece. A refusal undoes the charge.
            let _t = self.qos.counters.in_flight.fetch_add(1, Ordering::SeqCst);
            let mut mine = self.in_flight.load(Ordering::SeqCst);
            loop {
                let ok = if paced {
                    mine == 0 || rec.is_some_and(|r| r.in_flight() <= 1)
                } else {
                    mine + leave < cap || mine == 0
                };
                if !ok {
                    let _u = self.qos.counters.in_flight.fetch_sub(1, Ordering::SeqCst);
                    return false;
                }
                match self.in_flight.compare_exchange_weak(
                    mine,
                    mine + 1,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => return true,
                    Err(now) => mine = now,
                }
            }
        });
        admitted.then(|| Charge {
            issuer: Arc::clone(self),
        })
    }

    /// Waits until a piece is admitted (leaving `leave`, as
    /// [`Self::admit_leaving`]), the call ends with an error elsewhere, or
    /// one of its stores is poisoned. Only a thread holding none of the
    /// call's pieces waits (R5.4's progress rule).
    pub(crate) fn wait_admit(
        self: &Arc<Self>,
        clock: &dyn Fn() -> u64,
        leave: u64,
    ) -> Result<Charge, Ended> {
        let mut g = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let _w = self.waiters.fetch_add(1, Ordering::SeqCst);
        let out = loop {
            if let Some(c) = self.admit_leaving(clock(), leave) {
                break Ok(c);
            }
            if self.ended.load(Ordering::SeqCst) {
                break Err(Ended::Ended);
            }
            if self.poisoned() {
                break Err(Ended::Poisoned);
            }
            g = self
                .released
                .wait(g)
                .unwrap_or_else(PoisonError::into_inner);
        };
        let _w = self.waiters.fetch_sub(1, Ordering::SeqCst);
        drop(g);
        out
    }

    fn poisoned(&self) -> bool {
        self.sources
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|(s, _)| s.poisoned())
    }

    /// Wakes the record's waiters (a release, an end, a poison): the lock
    /// is taken only when someone waits.
    fn wake(&self) {
        if self.waiters.fetch_add(0, Ordering::SeqCst) > 0 {
            let _g = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
            self.released.notify_all();
        }
    }

    /// Marks a part of the call ended with an error and wakes its waiters.
    pub(crate) fn end(&self) {
        self.ended.store(true, Ordering::SeqCst);
        self.wake();
    }

    /// This call's pieces in flight.
    pub(crate) fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// The store whose group the call is paced by.
    pub(crate) fn qos(&self) -> &Arc<Qos> {
        &self.qos
    }
}

/// One admitted piece's charge: released on drop, whether the piece was
/// reaped or abandoned (an error return, a dropped queue, a panic's unwind),
/// so no exit path keeps a charge.
pub(crate) struct Charge {
    issuer: Arc<Issuer>,
}

impl Drop for Charge {
    fn drop(&mut self) {
        let i = &self.issuer;
        let _c = i.qos.counters.in_flight.fetch_sub(1, Ordering::Relaxed);
        let _m = i.in_flight.fetch_sub(1, Ordering::SeqCst);
        i.wake();
    }
}

/// A thread's part of a call (each thread of a background call binds its
/// record while it works): dropped without [`Binding::finish`] (an error
/// return, a panic's unwind) it marks the call ended, so the call's other
/// threads never wait for it.
pub(crate) struct Binding {
    issuer: Arc<Issuer>,
    done: bool,
}

impl Binding {
    pub(crate) fn new(issuer: &Arc<Issuer>) -> Self {
        Self {
            issuer: Arc::clone(issuer),
            done: false,
        }
    }

    /// This thread's part ended without an error.
    pub(crate) fn finish(mut self) {
        self.done = true;
    }
}

impl Drop for Binding {
    fn drop(&mut self) {
        if !self.done {
            self.issuer.end();
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, unused_results)]

    use super::*;

    type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

    const MS: u64 = 1_000_000;

    /// A registry of its own (tests never share the process's) and a clock
    /// tests move.
    fn fixture() -> (&'static Registry, Arc<AtomicU64>, Clock) {
        let r: &'static Registry = Box::leak(Box::new(Registry::new()));
        let t = Arc::new(AtomicU64::new(1_000 * MS));
        let c = Arc::clone(&t);
        (r, t, Arc::new(move || c.load(Ordering::Relaxed)))
    }

    fn disk(n: u32) -> DeviceIdentity {
        DeviceIdentity {
            devices: vec![DeviceId::node(DeviceNode::Sim { device: n })],
            complete: true,
        }
    }

    fn on(n: u32) -> Placement {
        Placement::Keyed {
            identity: disk(n),
            members: Vec::new(),
            fs_device: Some(u64::from(n)),
        }
    }

    fn unknown(fs: Option<u64>) -> Placement {
        Placement::Keyed {
            identity: DeviceIdentity::unknown(),
            members: Vec::new(),
            fs_device: fs,
        }
    }

    fn join(r: &'static Registry, p: &Placement, c: &Clock) -> Arc<Qos> {
        r.join(0, p, QosOptions::default(), c())
    }

    fn named(n: u32, serial: &str) -> Placement {
        let mut d = DeviceId::node(DeviceNode::Sim { device: n });
        d.add_name(serial);
        Placement::Keyed {
            identity: DeviceIdentity {
                devices: vec![d],
                complete: true,
            },
            members: Vec::new(),
            fs_device: None,
        }
    }

    #[test]
    fn test_stores_on_one_device_share_a_group_and_other_devices_do_not() {
        let (r, t, c) = fixture();
        let (a, b, x) = (
            join(r, &on(1), &c),
            join(r, &on(1), &c),
            join(r, &on(2), &c),
        );
        a.commit(c());
        assert!(b.active(c()), "a commit on a paces b on the same device");
        assert!(!x.active(c()), "another device is quiet");
        let _was = t.fetch_add(5_000 * MS, Ordering::Relaxed);
        assert!(!b.active(c()), "past commit_quiet");
        assert_eq!(a.report(c()).members, 2);
    }

    #[test]
    fn test_same_group_follows_the_published_record() {
        let (r, _t, c) = fixture();
        let (a, b, x) = (
            join(r, &on(1), &c),
            join(r, &on(1), &c),
            join(r, &on(2), &c),
        );
        assert!(Qos::same_group(&a, &b), "one device");
        assert!(!Qos::same_group(&a, &x), "two devices");
        let m = join(r, &Placement::Memory, &c);
        assert!(!Qos::same_group(&m, &m), "a memory store is in no group");
        // A store meeting both groups merges them: one record for all.
        let both = Placement::Keyed {
            identity: DeviceIdentity {
                devices: vec![
                    DeviceId::node(DeviceNode::Sim { device: 1 }),
                    DeviceId::node(DeviceNode::Sim { device: 2 }),
                ],
                complete: true,
            },
            members: Vec::new(),
            fs_device: None,
        };
        let _s = join(r, &both, &c);
        assert!(Qos::same_group(&a, &x), "merged");
    }

    #[test]
    fn test_a_store_meeting_two_groups_merges_them() {
        let (r, _t, c) = fixture();
        let (a, b) = (join(r, &on(1), &c), join(r, &on(2), &c));
        let both = Placement::Keyed {
            identity: DeviceIdentity {
                devices: vec![
                    DeviceId::node(DeviceNode::Sim { device: 1 }),
                    DeviceId::node(DeviceNode::Sim { device: 2 }),
                ],
                complete: true,
            },
            members: Vec::new(),
            fs_device: None,
        };
        let s = join(r, &both, &c);
        b.commit(c());
        assert!(a.active(c()) && s.active(c()), "one group after the merge");
        assert_eq!(a.report(c()).members, 3);
    }

    #[test]
    fn test_btrfs_members_and_device_names_key_a_group() {
        let (r, _t, c) = fixture();
        let a = join(r, &on(7), &c);
        let sub = Placement::Keyed {
            identity: DeviceIdentity::unknown(),
            members: vec![DeviceId::node(DeviceNode::Sim { device: 7 })],
            fs_device: Some(99),
        };
        let b = join(r, &sub, &c);
        a.commit(c());
        assert!(
            b.active(c()) && !b.report(c()).unknown,
            "a member disk names the device"
        );
        let (x, y) = (
            join(r, &named(50, "serial-1"), &c),
            join(r, &named(51, "serial-1"), &c),
        );
        x.commit(c());
        assert!(y.active(c()), "a shared name is one device");
    }

    #[test]
    fn test_unknown_stores_share_activity_with_their_scope_both_ways() {
        let (r, t, c) = fixture();
        let n = join(r, &on(1), &c);
        let u = join(r, &unknown(Some(500)), &c);
        assert!(u.report(c()).unknown);
        n.commit(c());
        assert!(u.active(c()), "a named store's commit paces an unknown one");
        let _was = t.fetch_add(6_000 * MS, Ordering::Relaxed);
        assert!(!u.active(c()) && !n.active(c()));
        u.commit(c());
        assert!(n.active(c()), "an unknown store's commit paces every group");
        let _was = t.fetch_add(6_000 * MS, Ordering::Relaxed);
        let h = n.hold();
        assert!(
            u.active(c()),
            "a hold anywhere in the scope paces unknown stores"
        );
        drop(h);
        let h = u.hold();
        assert!(n.active(c()), "an unknown store's hold paces every group");
        drop(h);
        assert!(!n.active(c()) && !u.active(c()));
        // Never merged: the named group still has one member.
        assert_eq!(n.report(c()).members, 1);
    }

    #[test]
    fn test_the_first_unknown_store_is_seeded_with_the_scope_last_commit() {
        let (r, _t, c) = fixture();
        let n = join(r, &on(1), &c);
        n.commit(c());
        // No unknown member yet: the scope word was not written.
        let u = join(r, &unknown(Some(500)), &c);
        assert!(u.active(c()), "seeded at 0 to 1, never read as quiet");
    }

    #[test]
    fn test_a_lone_unknown_store_starts_active_from_its_join() {
        let (r, t, c) = fixture();
        let u = join(r, &unknown(None), &c);
        assert!(
            u.active(c()),
            "no commit anywhere yet: the seed is the join's time"
        );
        let _was = t.fetch_add(6_000 * MS, Ordering::Relaxed);
        assert!(!u.active(c()));
    }

    #[test]
    fn test_unknown_activity_stops_when_the_last_unknown_store_closes() {
        let (r, t, c) = fixture();
        let n = join(r, &on(1), &c);
        let u = join(r, &unknown(Some(500)), &c);
        u.commit(c());
        assert!(n.active(c()));
        drop(u);
        assert_eq!(n.scope.unknown_members.load(Ordering::Relaxed), 0);
        assert_eq!(n.scope.any_commit.load(Ordering::Relaxed), 0);
        assert!(!n.active(c()), "a closed unknown store paces nobody");
        let u2 = join(r, &unknown(None), &c);
        assert!(
            u2.active(c()),
            "a first unknown store starts active (its seed)"
        );
        assert!(
            !n.active(c()),
            "the closed unknown store's commit came back"
        );
        let _was = t.fetch_add(6_000 * MS, Ordering::Relaxed);
        assert!(
            !u2.active(c()) && !n.active(c()),
            "the old unknown commit is gone"
        );
        n.commit(c());
        assert!(u2.active(c()), "a later unknown store counts again");
    }

    #[test]
    fn test_a_named_store_s_hold_keeps_its_group_active() {
        let (r, t, c) = fixture();
        let (a, b) = (join(r, &on(1), &c), join(r, &on(1), &c));
        let h = a.hold();
        assert!(a.active(c()) && b.active(c()));
        let _was = t.fetch_add(60_000 * MS, Ordering::Relaxed);
        assert!(b.active(c()), "a hold needs no commit and never expires");
        drop(h);
        assert!(!a.active(c()) && !b.active(c()));
    }

    #[test]
    fn test_a_memory_store_s_hold_paces_nobody() {
        let (r, _t, c) = fixture();
        let u = join(r, &unknown(None), &c);
        let m = join(r, &Placement::Memory, &c);
        let h = m.hold();
        // The unknown store's own seed is still within its quiet: check the
        // scope's hold count, which only a paced store's hold raises.
        assert_eq!(u.scope.holds.load(Ordering::Relaxed), 0);
        drop(h);
    }

    #[test]
    fn test_a_refused_admission_undoes_its_charge() {
        let (r, _t, c) = fixture();
        let s = join(r, &on(1), &c);
        let i = Issuer::new(&s);
        s.commit(c());
        let a = i.admit(c()).expect("admitted");
        assert!(i.admit(c()).is_none());
        assert_eq!(
            s.report(c()).in_flight,
            1,
            "the refusal's tentative charge is undone"
        );
        drop(a);
        assert_eq!(
            s.report(c()).in_flight,
            0,
            "a released charge leaves the member"
        );
    }

    #[test]
    fn test_a_watch_after_the_poison_still_ends_a_sleeping_waiter() {
        let (r, _t, c) = fixture();
        let s = join(r, &on(1), &c);
        let i = Issuer::new(&s);
        s.commit(c());
        let held = i.admit(c()).expect("admitted");
        let src = Arc::new(FakePoison {
            poisoned: AtomicBool::new(true),
            wake: Mutex::new(None),
        });
        let i2 = Arc::clone(&i);
        let i3 = Arc::clone(&i);
        let got = waited(
            &i,
            move || i2.wait_admit(&|| 1_000_000_000, 0).map(|_| ()),
            move || i3.watch(src as Arc<dyn PoisonSource>),
        );
        drop(held);
        assert_eq!(got, Err(Ended::Poisoned), "the watch's own wake ended it");
    }

    #[test]
    fn test_an_unknown_store_s_commit_is_stamped_for_the_scope_only_while_unknown_stores_live() {
        let (r, t, c) = fixture();
        let n = join(r, &on(1), &c);
        n.commit(c());
        assert_eq!(
            n.scope.any_commit.load(Ordering::Relaxed),
            0,
            "no unknown store: no scope stamp"
        );
        let u = join(r, &unknown(None), &c);
        let _was = t.fetch_add(6_000 * MS, Ordering::Relaxed);
        u.commit(c());
        assert_eq!(
            u.scope.unknown_commit.load(Ordering::Relaxed),
            c(),
            "an unknown commit is stamped"
        );
        let _was = t.fetch_add(2 * MS, Ordering::Relaxed);
        n.commit(c());
        assert_eq!(
            n.scope.any_commit.load(Ordering::Relaxed),
            c(),
            "while unknown stores live"
        );
    }

    #[test]
    fn test_scopes_never_share_groups_or_activity() {
        let (r, t, c) = fixture();
        let opts = QosOptions::default();
        let a = r.join(1, &on(1), opts, c());
        let b = r.join(2, &on(1), opts, c());
        let ua = r.join(1, &unknown(None), opts, c());
        let ub = r.join(2, &unknown(None), opts, c());
        // The first unknown store of each scope starts active (its seed).
        let _was = t.fetch_add(6_000 * MS, Ordering::Relaxed);
        a.commit(c());
        ua.commit(c());
        assert!(!b.active(c()) && !ub.active(c()));
        assert!(a.active(c()) && ua.active(c()));
    }

    #[test]
    fn test_an_issuer_leaving_a_piece_stops_one_short_of_its_cap() {
        let (r, _t, c) = fixture();
        let m = join(r, &Placement::Memory, &c);
        let i = Issuer::new(&m);
        let mut held: Vec<_> = (0..7)
            .map(|_| i.admit_leaving(c(), 1).expect("admitted"))
            .collect();
        assert!(i.admit_leaving(c(), 1).is_none(), "one left");
        held.push(i.admit(c()).expect("the one left, for another thread"));
        drop(held);
        let one = i
            .admit_leaving(c(), 8)
            .expect("nothing in flight: admitted");
        drop(one);
    }

    #[test]
    fn test_a_memory_store_is_never_paced_and_paces_nobody() {
        let (r, _t, c) = fixture();
        let n = join(r, &on(1), &c);
        let m = join(r, &Placement::Memory, &c);
        m.commit(c());
        assert!(!n.active(c()) && !m.active(c()));
        let i = Issuer::new(&m);
        let held: Vec<_> = (0..8).map(|_| i.admit(c()).expect("admitted")).collect();
        assert!(i.admit(c()).is_none(), "the unpaced cap still bounds it");
        drop(held);
    }

    #[test]
    fn test_an_issuer_is_paced_while_commits_are_active_and_unpaced_after() {
        let (r, t, c) = fixture();
        let s = join(r, &on(1), &c);
        let i = Issuer::new(&s);
        s.commit(c());
        let one = i.admit(c()).expect("admitted");
        assert!(
            i.admit(c()).is_none(),
            "paced: one piece while commits are active"
        );
        drop(one);
        let _was = t.fetch_add(6_000 * MS, Ordering::Relaxed);
        let held: Vec<_> = (0..8).map(|_| i.admit(c()).expect("admitted")).collect();
        assert!(i.admit(c()).is_none(), "unpaced: background_unpaced pieces");
        drop(held);
        // Registered during continuous commits: paced from its first piece.
        let late = Issuer::new(&s);
        s.commit(c());
        let a = late.admit(c()).expect("admitted");
        assert!(late.admit(c()).is_none());
        drop(a);
    }

    #[test]
    fn test_an_issuer_with_no_commit_ever_is_unpaced() {
        let (r, _t, c) = fixture();
        let s = join(r, &on(1), &c);
        let i = Issuer::new(&s);
        // Commits never active: unpaced from the first piece.
        let held: Vec<_> = (0..8).map(|_| i.admit(c()).expect("admitted")).collect();
        assert!(i.admit(c()).is_none());
        drop(held);
    }

    #[test]
    fn test_issuers_with_none_in_flight_are_admitted_whatever_the_group_holds() {
        let (r, _t, c) = fixture();
        let s = join(r, &on(1), &c);
        let s2 = join(r, &on(1), &c);
        let (i, j) = (Issuer::new(&s), Issuer::new(&s2));
        s.commit(c());
        let a = i.admit(c()).expect("admitted");
        let b = j
            .admit(c())
            .expect("an issuer with none in flight is admitted");
        assert!(i.admit(c()).is_none() && j.admit(c()).is_none());
        assert_eq!(s.report(c()).in_flight, 2);
        drop((a, b));
        assert_eq!(s.report(c()).in_flight, 0);
    }

    #[test]
    fn test_pacing_off_on_every_member_admits_unpaced() {
        let (r, _t, c) = fixture();
        let off = QosOptions {
            pacing: false,
            ..QosOptions::default()
        };
        let s = r.join(0, &on(1), off, c());
        let s2 = r.join(0, &on(1), off, c());
        s.commit(c());
        let i = Issuer::new(&s2);
        let held: Vec<_> = (0..8).map(|_| i.admit(c()).expect("admitted")).collect();
        drop(held);
        // One member asking for pacing paces the group.
        let _p = join(r, &on(1), &c);
        let a = i.admit(c()).expect("admitted");
        assert!(i.admit(c()).is_none());
        drop(a);
    }

    #[test]
    fn test_the_group_takes_the_largest_quiet_and_the_smallest_cap() {
        let (r, t, c) = fixture();
        let long = QosOptions {
            commit_quiet_ns: 60_000 * MS,
            background_unpaced: 2,
            ..QosOptions::default()
        };
        let a = r.join(0, &on(1), long, c());
        let b = join(r, &on(1), &c);
        b.commit(c());
        let _was = t.fetch_add(10_000 * MS, Ordering::Relaxed);
        assert!(
            a.active(c()) && b.active(c()),
            "the largest quiet is the group's"
        );
        let _was = t.fetch_add(60_000 * MS, Ordering::Relaxed);
        let i = Issuer::new(&b);
        let held: Vec<_> = (0..2).map(|_| i.admit(c()).expect("admitted")).collect();
        assert!(i.admit(c()).is_none(), "the smallest cap is the group's");
        drop(held);
    }

    #[test]
    fn test_auto_tuning_is_the_group_s_only_if_every_member_asks_for_it() {
        let tuned = QosOptions {
            autotune: true,
            depth_max: 4,
            ..QosOptions::default()
        };
        let off = QosOptions::default();
        assert!(tuned.merge(tuned).autotune);
        assert!(!tuned.merge(off).autotune && !off.merge(tuned).autotune);
        assert_eq!(tuned.merge(off).depth_max, 4, "the smallest ceiling");
        assert_eq!(off.merge(tuned).depth_max, 4);
        let (r, _t, c) = fixture();
        let a = r.join(0, &on(1), tuned, c());
        assert!(a.report(c()).autotune);
        let b = r.join(0, &on(1), off, c());
        assert!(
            !a.report(c()).autotune && !b.report(c()).autotune,
            "merged off"
        );
        assert_eq!(b.report(c()).depth_max, 4, "the group's smallest ceiling");
        let quiet = QosOptions {
            pacing: false,
            ..tuned
        };
        let z = r.join(0, &on(2), quiet, c());
        assert!(!z.report(c()).autotune, "nothing to tune without pacing");
    }

    #[test]
    fn test_a_closed_store_s_last_commit_still_counts_after_pruning() {
        let (r, t, c) = fixture();
        let a = join(r, &on(1), &c);
        let b = join(r, &on(1), &c);
        a.commit(c());
        drop(a);
        // A join prunes the closed member; its commit is kept as retired.
        let x = join(r, &on(1), &c);
        assert_eq!(b.report(c()).members, 2);
        assert!(x.active(c()) && b.active(c()));
        let _was = t.fetch_add(6_000 * MS, Ordering::Relaxed);
        assert!(!b.active(c()));
    }

    #[test]
    fn test_commits_are_stamped_at_most_once_a_millisecond() {
        let (r, t, c) = fixture();
        let a = join(r, &on(1), &c);
        a.commit(c());
        let first = a.counters.last_commit.load(Ordering::Relaxed);
        let _was = t.fetch_add(MS / 2, Ordering::Relaxed);
        a.commit(c());
        assert_eq!(a.counters.last_commit.load(Ordering::Relaxed), first);
        let _was = t.fetch_add(MS, Ordering::Relaxed);
        a.commit(c());
        assert!(a.counters.last_commit.load(Ordering::Relaxed) > first);
    }

    /// Runs `wait` on another thread, waits until it is registered as a
    /// waiter on `i` and asleep (so the act can only reach it through a
    /// wake), runs `act`, and returns the
    /// waiter's outcome, bounded: a missed wake fails in 10 s, never hangs.
    fn waited<T: Send + 'static>(
        i: &Arc<Issuer>,
        wait: impl FnOnce() -> T + Send + 'static,
        act: impl FnOnce(),
    ) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        let t = std::thread::spawn(move || {
            let _sent = tx.send(wait());
        });
        let start = std::time::Instant::now();
        while i.waiters.load(Ordering::SeqCst) == 0 {
            assert!(
                start.elapsed() < std::time::Duration::from_secs(10),
                "the waiter never waited"
            );
            std::thread::yield_now();
        }
        // The waiter registered under the record's lock and keeps it until it
        // sleeps: once the lock is free, it sleeps, and only a wake reaches it.
        drop(i.lock.lock().unwrap_or_else(PoisonError::into_inner));
        act();
        match rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(got) => {
                let _joined = t.join();
                got
            }
            // Never joined: the waiter is asleep for good.
            Err(_) => panic!("the waiter was never woken"),
        }
    }

    #[test]
    fn test_a_failed_binding_ends_the_call_and_wakes_its_waiter() {
        let (r, _t, c) = fixture();
        let s = join(r, &on(1), &c);
        let i = Issuer::new(&s);
        s.commit(c());
        let held = i.admit(c()).expect("admitted");
        let i2 = Arc::clone(&i);
        let got = waited(
            &i,
            move || i2.wait_admit(&|| 1_000_000_000, 0).map(|_| ()),
            || drop(Binding::new(&i)),
        );
        drop(held);
        assert_eq!(got, Err(Ended::Ended));
    }

    #[test]
    fn test_a_panic_s_unwind_ends_the_binding() {
        let (r, _t, c) = fixture();
        let s = join(r, &on(1), &c);
        let i = Issuer::new(&s);
        let i2 = Arc::clone(&i);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _b = Binding::new(&i2);
            panic!("a part of the call panics");
        }));
        assert!(r.is_err());
        assert!(i.ended.load(Ordering::SeqCst));
    }

    #[test]
    fn test_a_finished_binding_ends_nothing() {
        let (r, _t, c) = fixture();
        let s = join(r, &on(1), &c);
        let i = Issuer::new(&s);
        Binding::new(&i).finish();
        assert!(!i.ended.load(Ordering::SeqCst));
    }

    #[test]
    fn test_a_released_charge_admits_the_waiter() {
        let (r, _t, c) = fixture();
        let s = join(r, &on(1), &c);
        let i = Issuer::new(&s);
        s.commit(c());
        let held = i.admit(c()).expect("admitted");
        let i2 = Arc::clone(&i);
        let got = waited(
            &i,
            move || i2.wait_admit(&|| 1_000_000_000, 0).is_ok(),
            move || drop(held),
        );
        assert!(got, "the release did not admit the waiter");
    }

    struct FakePoison {
        poisoned: AtomicBool,
        wake: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    }

    impl PoisonSource for FakePoison {
        fn poisoned(&self) -> bool {
            self.poisoned.load(Ordering::SeqCst)
        }
        fn on_poison(&self, wake: Arc<dyn Fn() + Send + Sync>) -> Box<dyn Send + Sync> {
            *self.wake.lock().unwrap() = Some(wake);
            Box::new(())
        }
    }

    #[test]
    fn test_a_poison_of_the_call_s_store_wakes_its_waiter() {
        let (r, _t, c) = fixture();
        let s = join(r, &on(1), &c);
        let i = Issuer::new(&s);
        let src = Arc::new(FakePoison {
            poisoned: AtomicBool::new(false),
            wake: Mutex::new(None),
        });
        i.watch(Arc::clone(&src) as Arc<dyn PoisonSource>);
        s.commit(c());
        let held = i.admit(c()).expect("admitted");
        let i2 = Arc::clone(&i);
        let s2 = Arc::clone(&src);
        let got = waited(
            &i,
            move || i2.wait_admit(&|| 1_000_000_000, 0).map(|_| ()),
            move || {
                s2.poisoned.store(true, Ordering::SeqCst);
                let w = s2.wake.lock().unwrap().clone().unwrap();
                w();
            },
        );
        drop(held);
        assert_eq!(got, Err(Ended::Poisoned));
        // A poison already set is read before any wait (bounded: on a thread,
        // its outcome awaited with a timeout).
        let j = Issuer::new(&s);
        j.watch(src as Arc<dyn PoisonSource>);
        let h = j.admit(c()).expect("admitted");
        let j2 = Arc::clone(&j);
        let (tx, rx) = std::sync::mpsc::channel();
        let _t =
            std::thread::spawn(move || tx.send(j2.wait_admit(&|| 1_000_000_000, 0).map(|_| ())));
        let got = rx.recv_timeout(std::time::Duration::from_secs(10));
        drop(h);
        assert_eq!(
            got.expect("a set poison was not read"),
            Err(Ended::Poisoned)
        );
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, unused_results)]

    use super::*;

    /// The models' time: constant (activity is set by a commit at it).
    fn c() -> u64 {
        1_000_000_000
    }

    fn fixture() -> Arc<Qos> {
        let r: &'static Registry = Box::leak(Box::new(Registry::new()));
        let q = r.join(
            0,
            &Placement::Keyed {
                identity: DeviceIdentity {
                    devices: vec![DeviceId::node(DeviceNode::Sim { device: 1 })],
                    complete: true,
                },
                members: Vec::new(),
                fs_device: None,
            },
            QosOptions::default(),
            c(),
        );
        q.commit(c());
        q
    }

    /// Two threads of one paced call race for its one piece: exactly one
    /// wins (the compare-and-swap).
    #[test]
    fn loom_one_paced_piece_per_call() {
        loom::model(|| {
            let q = fixture();
            let i = Issuer::new(&q);
            let t = {
                let i = Arc::clone(&i);
                // The charge is returned, so it is held until both decided.
                loom::thread::spawn(move || i.admit(c()))
            };
            let mine = i.admit(c());
            let theirs = t.join().unwrap();
            assert_eq!(u8::from(mine.is_some()) + u8::from(theirs.is_some()), 1);
            assert_eq!(i.in_flight(), 1);
            drop((mine, theirs));
            assert_eq!(i.in_flight(), 0);
        });
    }

    /// The waiter protocol: a release racing a waiter's last check never
    /// leaves it asleep (it is admitted).
    #[test]
    fn loom_a_release_never_misses_a_waiter() {
        loom::model(|| {
            let q = fixture();
            let i = Issuer::new(&q);
            let held = i.admit(c()).expect("admitted");
            let t = {
                let i = Arc::clone(&i);
                loom::thread::spawn(move || i.wait_admit(&|| 1_000_000_000, 0).is_ok())
            };
            drop(held);
            assert!(t.join().unwrap());
        });
    }

    fn placed(device: u32) -> Placement {
        Placement::Keyed {
            identity: DeviceIdentity {
                devices: vec![DeviceId::node(DeviceNode::Sim { device })],
                complete: true,
            },
            members: Vec::new(),
            fs_device: None,
        }
    }

    /// A merge racing a charge and its release: no count is moved, so the
    /// merged group ends with nothing in flight and every member listed.
    #[test]
    fn loom_a_merge_racing_a_charge_and_its_release() {
        loom::model(|| {
            let r: &'static Registry = Box::leak(Box::new(Registry::new()));
            let a = r.join(0, &placed(1), QosOptions::default(), c());
            let _b = r.join(0, &placed(2), QosOptions::default(), c());
            let i = Issuer::new(&a);
            let t = loom::thread::spawn(move || drop(i.admit(c())));
            let both = Placement::Keyed {
                identity: DeviceIdentity {
                    devices: vec![
                        DeviceId::node(DeviceNode::Sim { device: 1 }),
                        DeviceId::node(DeviceNode::Sim { device: 2 }),
                    ],
                    complete: true,
                },
                members: Vec::new(),
                fs_device: None,
            };
            let s = r.join(0, &both, QosOptions::default(), c());
            t.join().unwrap();
            assert_eq!(s.report(c()).in_flight, 0);
            assert_eq!(a.report(c()).members, 3);
        });
    }

    /// An unknown store joining while the scope's last unknown store closes:
    /// the count, changed under the registry's lock, ends at one.
    #[test]
    fn loom_an_unknown_join_racing_the_last_unknown_close() {
        loom::model(|| {
            let r: &'static Registry = Box::leak(Box::new(Registry::new()));
            let unknown = Placement::Keyed {
                identity: DeviceIdentity::unknown(),
                members: Vec::new(),
                fs_device: None,
            };
            let u1 = r.join(0, &unknown, QosOptions::default(), c());
            let t = loom::thread::spawn(move || drop(u1));
            let u2 = r.join(0, &unknown, QosOptions::default(), c());
            t.join().unwrap();
            assert_eq!(u2.scope.unknown_members.load(Ordering::Relaxed), 1);
        });
    }

    struct Poison {
        set: AtomicBool,
    }

    impl PoisonSource for Poison {
        fn poisoned(&self) -> bool {
            self.set.load(Ordering::SeqCst)
        }
        fn on_poison(&self, _wake: Arc<dyn Fn() + Send + Sync>) -> Box<dyn Send + Sync> {
            // Already poisoned when watched: never called back.
            Box::new(())
        }
    }

    /// A poison set before a watch, racing a waiter: the watch's own wake
    /// (after the push) never leaves the waiter asleep.
    #[test]
    fn loom_a_watch_after_a_poison_never_misses_a_waiter() {
        loom::model(|| {
            let q = fixture();
            let i = Issuer::new(&q);
            let held = i.admit(c()).unwrap();
            let t = {
                let i = Arc::clone(&i);
                loom::thread::spawn(move || i.wait_admit(&|| 1_000_000_000, 0).map(|_| ()))
            };
            let src = Arc::new(Poison {
                set: AtomicBool::new(true),
            });
            i.watch(src as Arc<dyn PoisonSource>);
            assert_eq!(t.join().unwrap(), Err(Ended::Poisoned));
            drop(held);
        });
    }

    /// An end racing a waiter's last check never leaves it asleep.
    #[test]
    fn loom_an_end_never_misses_a_waiter() {
        loom::model(|| {
            let q = fixture();
            let i = Issuer::new(&q);
            let held = i.admit(c()).expect("admitted");
            let t = {
                let i = Arc::clone(&i);
                loom::thread::spawn(move || i.wait_admit(&|| 1_000_000_000, 0).map(|_| ()))
            };
            i.end();
            let got = t.join().unwrap();
            drop(held);
            assert_eq!(got, Err(Ended::Ended));
        });
    }
}
