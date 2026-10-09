# store-io-posix

The Linux synchronous backend of [store-io](https://github.com/jamesgober/store-io):
`O_DIRECT` files opened relative to a confined directory descriptor, OFD
ownership locks on the I/O descriptor, `fallocate` provisioning, FIEMAP and
`cachestat` range checks, `pwritev2(RWF_DSYNC)` durable writes, and an
unprivileged evidence probe over statfs, statx, `/proc/self/mountinfo`, inode
flags, the sysfs block tree, NVMe Identify and the DMI tables. It fills in
evidence; it never decides a durability class.

On operating systems other than Linux the crate compiles to an empty shell.

Most users want the [`store-io`](https://crates.io/crates/store-io) crate instead.

Licensed under Apache-2.0 or MIT, at your option.
