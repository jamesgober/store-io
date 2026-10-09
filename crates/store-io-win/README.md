# store-io-win

The Windows backend of [store-io](https://github.com/jamesgober/store-io):
unbuffered overlapped reads and writes reaped from an I/O completion port,
`NtFlushBuffersFileEx(FILE_DATA_SYNC_ONLY)` barriers on NTFS, POSIX-semantics
rename and delete by handle, an ownership lock on a sentinel byte, and the
storage-stack probe (volume, write-cache property, NVMe identify, health log)
that feeds store-io's durability class decision.

The crate compiles to an empty shell on non-Windows targets.

Most users want the [`store-io`](https://crates.io/crates/store-io) crate instead.

Licensed under Apache-2.0 or MIT, at your option.
