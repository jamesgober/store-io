//! How a region is provisioned (v0.5 E4 C10).

use store_io_core::error::{Error, ErrorContext, NotWrittenCause};

/// A region's size, and the most it may ever grow to.
///
/// ```
/// use store_io_engine::RegionOptions;
///
/// // 64 MiB now, never more than 1 GiB: one extent of 1 GiB, so the
/// // region grows to its cap without new space or a second extent.
/// let opts = RegionOptions::default().size(64 << 20).max_size(1 << 30);
/// assert!(opts.validate().is_ok());
/// assert!(RegionOptions::default().size(2).max_size(1).validate().is_err());
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RegionOptions {
    size: u64,
    max_size: Option<u64>,
}

impl RegionOptions {
    /// The data size to provision, in bytes (rounded up to the block; a
    /// page or append region holds at least one block).
    #[must_use]
    pub const fn size(mut self, bytes: u64) -> Self {
        self.size = bytes;
        self
    }

    /// The most the region may ever grow to, in bytes (rounded up to the
    /// block). Container format 2 only: the region is placed as one extent
    /// of this capacity (its space held from the start, charged to its tag)
    /// and grows within it, one run, with no new space; growth past it is
    /// `NoSpace(RegionLimit)`. Without it a region grows by new extents
    /// while the container has room.
    ///
    /// The space past the size is allocated, not written, where the system
    /// can (Linux); on Windows and macOS it is written at provisioning (a
    /// 64 GiB cap is a 64 GiB write, during which other provisioning and
    /// space reports wait), and each growth within it writes its range
    /// again (as everywhere).
    #[must_use]
    pub const fn max_size(mut self, bytes: u64) -> Self {
        self.max_size = Some(bytes);
        self
    }

    /// The data size asked for.
    #[must_use]
    pub const fn size_bytes(&self) -> u64 {
        self.size
    }

    /// The cap asked for, if any.
    #[must_use]
    pub const fn max_size_bytes(&self) -> Option<u64> {
        self.max_size
    }

    /// Checks the options alone (no store needed).
    ///
    /// # Errors
    ///
    /// `NotWritten(InvalidOptions)` for a cap below the size.
    pub fn validate(&self) -> Result<(), Error> {
        match self.max_size {
            Some(cap) if cap < self.size => Err(Error::NotWritten {
                cause: NotWrittenCause::InvalidOptions,
                ctx: ErrorContext::default(),
            }),
            _ => Ok(()),
        }
    }
}
