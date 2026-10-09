# store-io-engine

The engine of [store-io](https://github.com/jamesgober/store-io): volumes, append
and page regions, barriers that share device flushes between concurrent writers
without timers, unforgeable durability receipts, provisioning, and A/B slot
objects, generic over any `store-io-platform` backend.

Most users want the [`store-io`](https://crates.io/crates/store-io) crate instead.

Licensed under Apache-2.0 or MIT, at your option.
