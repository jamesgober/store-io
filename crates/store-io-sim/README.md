# store-io-sim

A deterministic simulated platform for [store-io](https://github.com/jamesgober/store-io).
Each simulated device has a volatile write cache; flushes persist exactly the
writes that completed before they were submitted; directory entries are durable
only after a directory sync; crashes keep any subset of un-flushed writes; faults
(I/O errors, lying flushes, lost and misdirected writes, short writes, out of
space) are injected on demand. Same seed, same run, byte for byte.

Used by store-io's own tests and by `store-io-conformance`.

Licensed under Apache-2.0 or MIT, at your option.
