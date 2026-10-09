# store-io-conformance

The conformance suite of [store-io](https://github.com/jamesgober/store-io): the checks every platform backend must pass, on every file system it supports. It covers the platform contract (exclusive create, sizes, many operations in flight, buffer ownership, queue limits, read-only handles, rename, unlink, directory sync, locks, range state, release, probe) and store behaviour on that platform (round trips, read-only opens that change nothing, ownership).

```rust,ignore
use store_io_conformance::Suite;

let report = Suite::new(&platform, &scratch_dir, trust).run();
assert!(report.passed(), "{report}");
```

store-io runs it on its deterministic simulator, on NTFS, and on ext4 and XFS (loop-mounted images in CI). Licensed under Apache-2.0 OR MIT.
