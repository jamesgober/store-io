//! fsys 1.1.3, the comparison baseline: journal configurations and the
//! read-back check of a journal file.
//!
//! The configurations are the ones Phase 0 measured
//! (`.dev/experiments/fsys-baseline`): the library default (500 µs group-
//! commit window), the window turned off (fsys's best QD1 configuration), and
//! direct mode with the window off.

use std::collections::HashMap;
use std::path::Path;

use fsys::{JournalOptions, JournalReader};

use crate::data::Pool;
use crate::report::Verify;

/// A journal configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Journal {
    /// `JournalOptions::new()`: buffered, 500 µs group-commit window.
    Default,
    /// Buffered, `group_commit_window(None)`.
    WindowOff,
    /// `direct(true)`, `group_commit_window(None)`.
    DirectWindowOff,
}

impl Journal {
    /// The options.
    pub fn options(self) -> JournalOptions {
        match self {
            Self::Default => JournalOptions::new(),
            Self::WindowOff => JournalOptions::new().group_commit_window(None),
            Self::DirectWindowOff => JournalOptions::new().direct(true).group_commit_window(None),
        }
    }

    /// A label for tables.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "journal, default (buffered, 500 µs group-commit window)",
            Self::WindowOff => "journal, buffered, window off",
            Self::DirectWindowOff => "journal, direct, window off",
        }
    }
}

/// Describes the fsys handle (method and durability primitive).
#[must_use]
pub fn describe(h: &fsys::Handle) -> String {
    format!(
        "fsys 1.1.3 (crates.io), Handle method {:?}, durability primitive {}",
        h.active_method(),
        h.active_durability_primitive()
    )
}

/// Reads every record of the journal at `path` and checks that there are
/// exactly `expected` of them, that each one is byte-for-byte the payload
/// its stamp names, and that within each stream the indices strictly
/// increase in journal order (records from one writer thread are never
/// reordered or duplicated).
#[must_use]
pub fn verify_journal(pool: &Pool, path: &Path, expected: u64) -> Verify {
    let mut reader = match JournalReader::open(path) {
        Ok(r) => r,
        Err(e) => return Verify::Failed(format!("JournalReader::open: {e}")),
    };
    let mut count = 0u64;
    let mut last: HashMap<u64, u64> = HashMap::new();
    for rec in reader.iter() {
        let rec = match rec {
            Ok(r) => r,
            Err(e) => return Verify::Failed(format!("record {count}: {e}")),
        };
        let Some((stream, index)) = Pool::identify(&rec.payload) else {
            return Verify::Failed(format!("record {count} too short to identify"));
        };
        if !pool.matches(stream, index, &rec.payload) {
            return Verify::Failed(format!(
                "record {count} (stream {stream}, index {index}) differs from what was appended"
            ));
        }
        if let Some(prev) = last.insert(stream, index) {
            if index <= prev {
                return Verify::Failed(format!(
                    "stream {stream}: index {index} follows {prev} (reordered or duplicated)"
                ));
            }
        }
        count += 1;
    }
    if count == expected {
        Verify::Passed(format!(
            "all {count} records replayed with JournalReader and compared byte for byte"
        ))
    } else {
        Verify::Failed(format!(
            "journal holds {count} records, {expected} were appended"
        ))
    }
}
