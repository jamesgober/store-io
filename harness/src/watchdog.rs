//! A hang guard. store-io has never run on these devices before; if a
//! workload stops making progress the harness must say so, not wait forever.
//! The watchdog checks the active workload once a second; past its limit it
//! calls the hang handler (which writes the partial report) and exits the
//! process with status 3.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

type Active = Option<(String, Instant, Duration)>;

/// The guard.
pub struct Watchdog {
    active: Arc<Mutex<Active>>,
}

impl Watchdog {
    /// Starts the watchdog thread. `on_hang(label, elapsed)` runs once, on
    /// the watchdog thread, before the process exits.
    pub fn start(on_hang: impl Fn(&str, Duration) + Send + 'static) -> Self {
        let active: Arc<Mutex<Active>> = Arc::new(Mutex::new(None));
        let watched = Arc::clone(&active);
        let _detached = std::thread::Builder::new()
            .name("watchdog".to_owned())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    let hung = watched
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .as_ref()
                        .and_then(|(label, start, limit)| {
                            let e = start.elapsed();
                            (e > *limit).then(|| (label.clone(), e))
                        });
                    if let Some((label, e)) = hung {
                        on_hang(&label, e);
                        std::process::exit(3);
                    }
                }
            });
        Self { active }
    }

    /// Marks `label` active with a time limit.
    pub fn enter(&self, label: &str, limit: Duration) {
        *self.active.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((label.to_owned(), Instant::now(), limit));
    }

    /// Clears the active workload.
    pub fn leave(&self) {
        *self.active.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}
