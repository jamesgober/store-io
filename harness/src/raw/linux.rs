//! Raw Linux primitives (see the parent module).

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use super::{FILL_CHUNK, IoCounters};
use crate::aligned::AlignedBuf;

fn check(ret: libc::c_int) -> io::Result<()> {
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// A file opened `O_DIRECT`.
pub struct RawFile {
    file: File,
}

impl RawFile {
    /// Creates (or truncates) `path`, sizes it to `len` (a multiple of
    /// 1 MiB), zero-fills it with `O_DIRECT` 1 MiB writes and flushes it
    /// (`fsync`). Returns the file and the time preparation took.
    ///
    /// # Errors
    ///
    /// Any I/O error.
    pub fn create_ready(path: &Path, len: u64) -> io::Result<(Self, Duration)> {
        let t = Instant::now();
        {
            let f = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(path)?;
            f.set_len(len)?;
        }
        let me = Self::open(path)?;
        let zero = AlignedBuf::zeroed(FILL_CHUNK);
        let mut at = 0u64;
        while at < len {
            let n = (len - at).min(FILL_CHUNK as u64) as usize;
            me.write_at(&zero.as_slice()[..n], at)?;
            at += n as u64;
        }
        me.flush_all()?;
        Ok((me, t.elapsed()))
    }

    /// Opens an existing file `O_DIRECT` for reading and writing.
    ///
    /// # Errors
    ///
    /// Any I/O error.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_DIRECT)
            .open(path)?;
        Ok(Self { file })
    }

    /// One `pwritev2(flags = 0)` of the whole of `buf` (block aligned).
    ///
    /// # Errors
    ///
    /// The OS error, or `WriteZero` on a short write.
    pub fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        let iov = libc::iovec {
            iov_base: buf.as_ptr().cast_mut().cast(),
            iov_len: buf.len(),
        };
        let off = libc::off_t::try_from(offset)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset too large"))?;
        // SAFETY: `iov` describes `buf.len()` readable bytes that outlive the
        // call; one iovec; the descriptor is live for the life of `self`.
        let n = unsafe { libc::pwritev2(self.file.as_raw_fd(), &raw const iov, 1, off, 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n as usize == buf.len() {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::WriteZero, "short write"))
        }
    }

    /// One `preadv2` into `buf` (block aligned).
    ///
    /// # Errors
    ///
    /// The OS error.
    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        let iov = libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        };
        let off = libc::off_t::try_from(offset)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset too large"))?;
        // SAFETY: `iov` describes `buf.len()` writable bytes that outlive the
        // call and are not otherwise borrowed; the descriptor is live.
        let n = unsafe { libc::preadv2(self.file.as_raw_fd(), &raw const iov, 1, off, 0) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    /// The data flush: `fdatasync`.
    ///
    /// # Errors
    ///
    /// The OS error.
    pub fn flush_data(&self) -> io::Result<()> {
        // SAFETY: fdatasync takes only a live descriptor.
        check(unsafe { libc::fdatasync(self.file.as_raw_fd()) })
    }

    /// A full flush: `fsync`.
    ///
    /// # Errors
    ///
    /// The OS error.
    pub fn flush_all(&self) -> io::Result<()> {
        // SAFETY: fsync takes only a live descriptor.
        check(unsafe { libc::fsync(self.file.as_raw_fd()) })
    }
}

/// The process's I/O counters from `/proc/self/io`.
#[must_use]
pub fn io_counters() -> Option<IoCounters> {
    let text = std::fs::read_to_string("/proc/self/io").ok()?;
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().parse::<u64>().ok())
    };
    Some(IoCounters {
        write_calls: field("syscw:")?,
        write_bytes: field("wchar:")?,
        other_calls: None,
    })
}
