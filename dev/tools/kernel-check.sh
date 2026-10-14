#!/usr/bin/env bash
# Run inside a virtme-ng guest (as root) by the CI kernel matrix: the
# conformance suite, the end-to-end tests and the Linux backend's own tests
# on the guest's kernel, on a fresh ext4 file system on the attached disk.
#
# Usage: kernel-check.sh <repo> <host user home> <expect-uring: 0|1> <disk bytes>
set -eux
repo=$1
home=$2
expect=$3
bytes=$4
export RUSTUP_HOME="$home/.rustup" CARGO_HOME="$home/.cargo"
export PATH="$CARGO_HOME/bin:$PATH"
# The host build's flags, so cargo reuses what the host built.
export RUSTFLAGS="-D warnings" CARGO_INCREMENTAL=0
echo "kernel $(uname -r)"
# The attached disk: the one block device of exactly the size the host
# made (never a guess at a device name before mkfs).
disk=
for b in /sys/block/*; do
  name=$(basename "$b")
  case "$name" in loop*|ram*|zram*) continue ;; esac
  if [ "$(( $(cat "$b/size") * 512 ))" = "$bytes" ]; then
    [ -z "$disk" ] || { echo "two disks of $bytes bytes"; exit 1; }
    disk=/dev/$name
  fi
done
[ -n "$disk" ] || { echo "no disk of $bytes bytes"; ls /sys/block; exit 1; }
mkfs.ext4 -q -F -b 4096 "$disk"
mkdir -p /tmp/sio
mount "$disk" /tmp/sio
findmnt -no SOURCE,FSTYPE,OPTIONS /tmp/sio
# Every test's scratch space on that disk ($HOME too: the guest's root is
# read-only and its tmpfs has no direct I/O before 6.6).
export STORE_IO_TEST_DIR=/tmp/sio HOME=/tmp/sio TMPDIR=/tmp/sio
if [ "$expect" = 1 ]; then export STORE_IO_EXPECT_URING=1; fi
cd "$repo"
cargo test --offline --test conformance --test native -- --nocapture
cargo test --offline -p store-io-posix --lib --tests
echo "kernel $(uname -r): all passed"
