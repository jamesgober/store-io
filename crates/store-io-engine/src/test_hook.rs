//! Step points of the dependent-write race (R5.13), for the engine's own
//! tests only.
//!
//! A dependent write ([`crate::AppendRegion::append_after`] and the rest)
//! reports each step of its order, and a recycle or release reports its
//! drain, to a callback a test installs. The callback may park the calling
//! thread until the test releases it, so a test can start a recycle at any
//! step and see where the two meet.
//!
//! Without the engine's `sim-hooks` feature the module is private, its
//! step reports compile to nothing and there is no way to install a hook.
//! The feature is unstable and for tests only: the engine enables it for
//! its own integration tests through a dev-dependency on itself, never for
//! a release build.

/// A point a dependent write or a retirement reports.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Step {
    /// A dependent write, its dependencies checked, before it takes its
    /// lane (a `Submitter`'s takes none: before its passes).
    BeforeLane,
    /// Holding its lane, before its passes.
    BeforePasses,
    /// Holding its passes, before its reservation (a page write, which
    /// reserves nothing: before its issue).
    BeforeReserve,
    /// Reserved (a page write: about to issue), its passes still held; they
    /// are dropped next.
    Reserved,
    /// A recycle or release has retired the region's gate and is about to
    /// wait for the passes in flight (it holds no lock yet).
    RecycleDraining,
    /// That wait returned: no pass is left, none will be admitted.
    RecycleDrained,
}

#[cfg(feature = "sim-hooks")]
static HOOK: std::sync::RwLock<Option<fn(Step)>> = std::sync::RwLock::new(None);

/// Installs `hook` (or removes it, with `None`) for every later step in the
/// process.
#[cfg(feature = "sim-hooks")]
#[doc(hidden)]
pub fn install(hook: Option<fn(Step)>) {
    *HOOK
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = hook;
}

#[cfg(feature = "sim-hooks")]
std::thread_local! {
    static UNSEEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `f` as another process would: the opens, resumes and closes it
/// makes on this thread are not recorded in the process's registry of
/// opens (`frontier.rs`), as an open by another process never is.
#[cfg(feature = "sim-hooks")]
#[doc(hidden)]
pub fn unseen<R>(f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            UNSEEN.with(|u| u.set(self.0));
        }
    }
    let _restore = Restore(UNSEEN.with(|u| u.replace(true)));
    f()
}

/// Whether this thread's opens are recorded (always, without the
/// feature).
#[inline]
pub(crate) fn seen() -> bool {
    #[cfg(feature = "sim-hooks")]
    {
        !UNSEEN.with(std::cell::Cell::get)
    }
    #[cfg(not(feature = "sim-hooks"))]
    {
        true
    }
}

/// Reports `s` to the installed hook, if any. The pointer is copied out
/// first, so the hook runs with no lock held.
#[inline]
pub(crate) fn step(s: Step) {
    #[cfg(feature = "sim-hooks")]
    {
        let hook = *HOOK
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(hook) = hook {
            hook(s);
        }
    }
    #[cfg(not(feature = "sim-hooks"))]
    let _unused = s;
}
