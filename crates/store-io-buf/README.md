# store-io-buf

Aligned, pooled I/O buffers for [store-io](https://github.com/jamesgober/store-io):
one page-aligned arena per pool, power-of-two size classes with lock-free free
lists, owned `IoBuf`s that never allocate on the hot path, and a sensitive mode
that wipes buffers and keeps them out of swap and core dumps.

Most users want the [`store-io`](https://crates.io/crates/store-io) crate instead.

Licensed under Apache-2.0 or MIT, at your option.
