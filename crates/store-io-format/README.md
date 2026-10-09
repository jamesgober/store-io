# store-io-format

On-disk formats of [store-io](https://github.com/jamesgober/store-io): the CRC-32C
checksum (hardware-accelerated on x86-64 and AArch64), the A/B slot header every
piece of store-io metadata is written with, the slot-pair winner logic, and the
payload codecs for volume, region-table and region-header records.

Pure, allocation-free, panic-free on hostile input. Most users want the
[`store-io`](https://crates.io/crates/store-io) crate instead.

Licensed under Apache-2.0 or MIT, at your option.
