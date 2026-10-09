# store-io-platform

The I/O boundary of [store-io](https://github.com/jamesgober/store-io): the
`Platform` and `Queue` traits that the Linux, Windows and macOS backends and the
deterministic simulator implement. Operations move owned, aligned buffers in;
completions carry them back out.

Most users want the [`store-io`](https://crates.io/crates/store-io) crate instead.

Licensed under Apache-2.0 or MIT, at your option.
