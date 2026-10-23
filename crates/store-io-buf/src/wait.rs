//! Sleeping until a 32-bit word changes, and waking every sleeper with one
//! call: the futex pattern, for waiters that must not take a lock to sleep
//! or to leave (D-126).
//!
//! A waker changes what the sleepers wait for, [`WaitWord::bump`]s the word,
//! then calls [`WaitWord::wake_all`]. A sleeper reads the word with
//! [`WaitWord::load`] before it checks its condition, and passes that value
//! to [`WaitWord::wait`], which returns at once if the word has moved since.
//! So a bump between the sleeper's check and its wait is never lost, and a
//! woken sleeper returns without re-taking any lock.
//!
//! Linux and Android use `futex(2)`, Windows `WaitOnAddress`: the kernel
//! wakes every sleeper of the word at once. Elsewhere, and under loom, a
//! mutex and condvar per word stand in (correct, but each woken sleeper
//! re-takes that mutex on its way out).

use core::fmt;

use crate::sync::{AtomicU32, Ordering};

#[cfg(any(loom, not(any(target_os = "linux", target_os = "android", windows))))]
use crate::sync::{Condvar, Mutex};

/// A 32-bit word threads sleep on until it changes.
pub struct WaitWord {
    word: AtomicU32,
    #[cfg(any(loom, not(any(target_os = "linux", target_os = "android", windows))))]
    lock: Mutex<()>,
    #[cfg(any(loom, not(any(target_os = "linux", target_os = "android", windows))))]
    woken: Condvar,
}

impl WaitWord {
    /// A word holding 0, with nobody asleep on it.
    #[must_use]
    pub fn new() -> Self {
        Self {
            word: AtomicU32::new(0),
            #[cfg(any(loom, not(any(target_os = "linux", target_os = "android", windows))))]
            lock: Mutex::new(()),
            #[cfg(any(loom, not(any(target_os = "linux", target_os = "android", windows))))]
            woken: Condvar::new(),
        }
    }

    /// The value to pass to [`Self::wait`]: read before the sleeper checks
    /// what it waits for.
    #[inline]
    #[must_use]
    pub fn load(&self) -> u32 {
        self.word.load(Ordering::SeqCst)
    }

    /// Changes the word, after the waker changed what the sleepers wait
    /// for: a sleeper that read an older value never blocks on it.
    #[inline]
    pub fn bump(&self) {
        let _previous = self.word.fetch_add(1, Ordering::SeqCst);
    }

    /// Sleeps while the word holds `seen`. May return spuriously: the
    /// caller re-checks.
    #[inline]
    pub fn wait(&self, seen: u32) {
        #[cfg(all(not(loom), any(target_os = "linux", target_os = "android", windows)))]
        sys::wait(&self.word, seen);
        #[cfg(any(loom, not(any(target_os = "linux", target_os = "android", windows))))]
        {
            let g = self
                .lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Checked under the lock the waker takes after its bump: a bump
            // before this is seen, one after it finds this sleeper waiting.
            if self.word.load(Ordering::SeqCst) == seen {
                let _g = self
                    .woken
                    .wait(g)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
    }

    /// Wakes every thread asleep on the word.
    #[inline]
    pub fn wake_all(&self) {
        #[cfg(all(not(loom), any(target_os = "linux", target_os = "android", windows)))]
        sys::wake_all(&self.word);
        #[cfg(any(loom, not(any(target_os = "linux", target_os = "android", windows))))]
        {
            drop(
                self.lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            self.woken.notify_all();
        }
    }
}

impl Default for WaitWord {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for WaitWord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WaitWord")
            .field("word", &self.load())
            .finish_non_exhaustive()
    }
}

// The word is all a native sleeper touches.
#[cfg(all(not(loom), any(target_os = "linux", target_os = "android", windows)))]
const _: () = assert!(core::mem::size_of::<WaitWord>() == core::mem::size_of::<u32>());

#[cfg(all(not(loom), any(target_os = "linux", target_os = "android")))]
mod sys {
    use super::AtomicU32;

    // 32-bit RISC-V has only the 64-bit-time futex call (same arguments
    // here: no timeout is passed).
    #[cfg(not(target_arch = "riscv32"))]
    const SYS_FUTEX: libc::c_long = libc::SYS_futex;
    #[cfg(target_arch = "riscv32")]
    const SYS_FUTEX: libc::c_long = libc::SYS_futex_time64;

    pub(super) fn wait(word: &AtomicU32, seen: u32) {
        // SAFETY: FUTEX_WAIT reads the aligned u32 behind `word`, which the
        // caller's borrow keeps alive for the call; a null timeout waits
        // without limit. EAGAIN (the word moved) and EINTR are returns the
        // caller re-checks.
        let _status = unsafe {
            libc::syscall(
                SYS_FUTEX,
                word.as_ptr(),
                libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
                seen,
                core::ptr::null::<libc::timespec>(),
            )
        };
    }

    pub(super) fn wake_all(word: &AtomicU32) {
        // SAFETY: FUTEX_WAKE only uses the address as a key; `word` is live
        // for the call.
        let _woken = unsafe {
            libc::syscall(
                SYS_FUTEX,
                word.as_ptr(),
                libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
                libc::c_int::MAX,
            )
        };
    }
}

#[cfg(all(not(loom), windows))]
mod sys {
    use super::AtomicU32;
    use windows_sys::Win32::System::Threading::{INFINITE, WaitOnAddress, WakeByAddressAll};

    pub(super) fn wait(word: &AtomicU32, seen: u32) {
        // SAFETY: both pointers are to live, aligned 4-byte values for the
        // call's duration, and the size passed is theirs. A spurious return
        // is re-checked by the caller.
        let _woke = unsafe {
            WaitOnAddress(
                word.as_ptr().cast(),
                (&raw const seen).cast(),
                core::mem::size_of::<u32>(),
                INFINITE,
            )
        };
    }

    pub(super) fn wake_all(word: &AtomicU32) {
        // SAFETY: the address is only a key; `word` is live for the call.
        unsafe { WakeByAddressAll(word.as_ptr().cast()) };
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::Duration;

    const LONG: Duration = Duration::from_secs(10);

    /// Sleeps until the word moves past `seen`, then reports on `tx`.
    fn sleeper(w: &Arc<WaitWord>, seen: u32, tx: mpsc::Sender<()>) {
        let w = Arc::clone(w);
        let _h = std::thread::spawn(move || {
            while w.load() == seen {
                w.wait(seen);
            }
            let _sent = tx.send(());
        });
    }

    #[test]
    fn test_wait_returns_at_once_when_the_word_has_moved() {
        let w = WaitWord::new();
        let seen = w.load();
        w.bump();
        w.wait(seen);
        assert_eq!(w.load(), seen.wrapping_add(1));
    }

    /// One bump and one wake wake every sleeper, asleep yet or not.
    #[test]
    fn test_one_wake_wakes_every_sleeper() {
        let w = Arc::new(WaitWord::new());
        let seen = w.load();
        let (tx, rx) = mpsc::channel();
        for _ in 0..8 {
            sleeper(&w, seen, tx.clone());
        }
        std::thread::sleep(Duration::from_millis(20));
        w.bump();
        w.wake_all();
        for _ in 0..8 {
            assert_eq!(rx.recv_timeout(LONG), Ok(()), "a sleeper slept on");
        }
    }

    /// A wake with no bump leaves a sleeper asleep (it re-checks and waits
    /// again), and a wake on a word nobody sleeps on does nothing.
    #[test]
    fn test_a_wake_without_a_bump_changes_nothing() {
        let w = Arc::new(WaitWord::new());
        w.wake_all();
        let seen = w.load();
        let (tx, rx) = mpsc::channel();
        sleeper(&w, seen, tx);
        std::thread::sleep(Duration::from_millis(20));
        w.wake_all();
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        w.bump();
        w.wake_all();
        assert_eq!(rx.recv_timeout(LONG), Ok(()));
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;
    use loom::sync::Arc;
    use loom::sync::atomic::AtomicBool;

    /// A sleeper that read the word before the waker's bump is never left
    /// asleep, wherever the bump and wake fall against its wait.
    #[test]
    fn loom_a_bump_and_wake_is_never_lost() {
        loom::model(|| {
            let w = Arc::new(WaitWord::new());
            let flag = Arc::new(AtomicBool::new(false));
            let sleeper = {
                let (w, flag) = (w.clone(), flag.clone());
                loom::thread::spawn(move || {
                    loop {
                        let seen = w.load();
                        if flag.load(Ordering::SeqCst) {
                            break;
                        }
                        w.wait(seen);
                    }
                })
            };
            flag.store(true, Ordering::SeqCst);
            w.bump();
            w.wake_all();
            assert!(sleeper.join().is_ok());
        });
    }
}
