#!/usr/bin/env bash
# G19's device (PLANNING R5.14 steps 1-4, as D-157 amends them): a serial
# server with a FIFO queue and a flush of its own cost, calibrated with fio,
# then left mounted for the gate.
#
#   null_blk (configfs): memory_backed, a 1 GiB volatile write cache, FUA
#     off, one submit queue of depth 1, timer completions after N ns: one
#     request served at a time, each taking N; a 256 KiB piece is one
#     request.
#   mq-deadline on it as a FIFO (write_expire 0, fifo_batch 1, 256
#     requests): a writer's queued requests wait in order ahead of a later
#     commit, as in an NVMe submission queue. Under `none` (R5.14's first
#     form) blk-mq wakes the waiters for the one tag in rotation, so a
#     writer never had more than one request ahead of a commit whatever its
#     depth, and depth 8 did no worse than depth 1 (measured on CI).
#   dm-delay over it: reads and writes undelayed, flushes delayed D, a
#     whole number of kernel ticks, at least 4 (and of milliseconds).
#   ext4 on top.
#
# FUA is off so a durable write (store-io's `RWF_DSYNC`, fio's `sync=dsync`:
# the same kernel path) is a write and a cache flush: with FUA it would be a
# FUA write and never meet D.
#
# For N in a ladder from D/4 to 2D, by steps of sqrt(2): fio's commit shape
# (QD1 4 KiB durable writes) alone, then beside a 256 KiB sequential writer
# at depth 1, 2 and 8, then alone again. A rung counts when its two idle
# p99s agree within 1.5x; the device is the first counted N at which the
# commit p99 is at most 1.5x idle beside depth 1 and at least 3x beside
# depth 8. Predicted beside each: a commit's write waits behind the
# writer's k pieces (k - 1 queued and one in service), its flush (sent to
# the head of the dispatch list) behind the one in service:
# (2N + D + (k + 1)N) / (2N + D).
# The band of N that separates is narrow (on CI 2.0-2.8 ms on one runner,
# 2.8-4.0 on another, D = 4 ms), so the ladder's steps are sqrt(2), not 2.
# No rung separating is a rejection: the script exits 1 and G19 fails.
#
# Usage: sudo -E bash harness/scripts/g19_device.sh --out DIR --mount DIR
#          [--secs S] [--all]
#   --secs  seconds per fio run (default 8)
#   --all   measure every rung (a calibration study), still choosing the first
# Writes DIR/g19_device.md (the ladder) and DIR/g19_device.env (G19_N_NS,
# G19_D_MS, G19_HZ, G19_DEV, G19_MOUNT, G19_REF_MBPS: fio's rate at the
# paced shape alone, 256 KiB one in flight); leaves the chosen device
# mounted, owned by the invoking user ($SUDO_USER).
# Needs root, fio, jq, mkfs.ext4, dmsetup, null_blk and dm-delay.
set -euo pipefail
# A failure inside $(...) fails the script too.
shopt -s inherit_errexit

usage="usage: g19_device.sh --out DIR --mount DIR [--secs S] [--all]"
out="" mnt="" secs=8 all=0
while [ $# -gt 0 ]; do
  case "$1" in
    --out) out=$2; shift 2 ;;
    --mount) mnt=$2; shift 2 ;;
    --secs) secs=$2; shift 2 ;;
    --all) all=1; shift ;;
    *) echo "$usage"; exit 2 ;;
  esac
done
[ -n "$out" ] && [ -n "$mnt" ] || { echo "$usage"; exit 2; }
[ "$(id -u)" = 0 ] || { echo "FAIL: g19_device.sh needs root"; exit 2; }
for t in fio jq mkfs.ext4 dmsetup; do
  command -v "$t" >/dev/null || { echo "FAIL: $t is not installed"; exit 2; }
done
mkdir -p "$out" "$mnt"
md="$out/g19_device.md"
cfs=/sys/kernel/config/nullb
name=g19          # the dm-delay device
nb=sio_g19        # the null_blk device (configfs item)

# On stderr: it also runs inside $(...).
reject() { echo "FAIL: $*" >&2; echo "**Rejected:** $*" >>"$md"; exit 1; }

# Step 1: D, whole ticks, at least 4, and a whole number of milliseconds
# (dm-delay's unit).
hz=$(sed -n 's/^CONFIG_HZ=\([0-9]*\)$/\1/p' "/boot/config-$(uname -r)" 2>/dev/null || true)
[ -n "$hz" ] || reject "CONFIG_HZ not found in /boot/config-$(uname -r)"
ticks=4
while [ $((ticks * 1000 % hz)) -ne 0 ]; do ticks=$((ticks + 1)); done
d_ms=$((ticks * 1000 / hz))

modprobe null_blk nr_devices=0 || reject "null_blk cannot be loaded"
modprobe dm-delay || reject "dm-delay cannot be loaded"
mountpoint -q /sys/kernel/config || mount -t configfs none /sys/kernel/config
[ -d "$cfs" ] || reject "null_blk has no configfs interface ($cfs)"

dev="" bdev=""
destroy() {
  mountpoint -q "$mnt" && umount "$mnt"
  dmsetup info "$name" >/dev/null 2>&1 && dmsetup remove --retry "$name"
  if [ -d "$cfs/$nb" ]; then
    echo 0 >"$cfs/$nb/power"
    rmdir "$cfs/$nb"
  fi
  dev="" bdev=""
}
trap 'rc=$?; [ "$rc" -ne 0 ] && destroy; exit "$rc"' EXIT

# A device whose requests each take $1 ns, mounted at $mnt.
make() {
  local n=$1 a idx disk=""
  mkdir "$cfs/$nb"
  a="$cfs/$nb"
  echo 2048 >"$a/size"            # MiB
  echo 4096 >"$a/blocksize"
  echo 1 >"$a/memory_backed"
  echo 1024 >"$a/cache_size"      # MiB
  echo 2 >"$a/queue_mode"         # blk-mq
  echo 1 >"$a/submit_queues"
  echo 1 >"$a/hw_queue_depth"
  echo 2 >"$a/irqmode"            # timer
  echo "$n" >"$a/completion_nsec"
  [ -e "$a/fua" ] || reject "null_blk has no fua attribute: FUA cannot be turned off"
  echo 0 >"$a/fua"
  [ -e "$a/max_sectors" ] || reject "null_blk has no max_sectors attribute"
  echo 512 >"$a/max_sectors"
  echo 1 >"$a/power"
  idx=$(cat "$a/index")
  # Named after the configfs item on newer kernels, nullb<index> before.
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    for c in "$nb" "nullb$idx"; do [ -b "/dev/$c" ] && disk=$c; done
    [ -n "$disk" ] && break
    sleep 0.2
  done
  [ -n "$disk" ] || reject "neither /dev/$nb nor /dev/nullb$idx appeared"
  bdev=/dev/$disk
  local q=/sys/block/$disk/queue
  echo mq-deadline >"$q/scheduler" || reject "mq-deadline is not available"
  echo 256 >"$q/nr_requests"
  echo 0 >"$q/iosched/write_expire"
  echo 0 >"$q/iosched/read_expire"
  echo 1 >"$q/iosched/fifo_batch"
  echo 0 >"$q/iosched/front_merges"
  local sectors dm
  sectors=$(blockdev --getsz "$bdev")
  dmsetup create "$name" --table "0 $sectors delay $bdev 0 0 $bdev 0 0 $bdev 0 $d_ms"
  dev=/dev/mapper/$name
  dm=$(basename "$(readlink -f "$dev")")
  [ "$(cat "/sys/block/$dm/queue/write_cache")" = "write back" ] \
    || reject "$dev has no volatile write cache"
  [ "$(cat "/sys/block/$dm/queue/fua")" = 0 ] || reject "$dev still reports FUA"
  # Inode tables and journal written now, not by ext4lazyinit beside the
  # runs.
  mkfs.ext4 -q -F -E lazy_itable_init=0,lazy_journal_init=0 "$dev"
  mount "$dev" "$mnt"
  # Lay out the files once: the runs overwrite allocated blocks.
  fio --name=lay --filename="$mnt/commit.dat" --size=64m --rw=write --bs=1m --direct=1 >/dev/null
  fio --name=lay --filename="$mnt/bg.dat" --size=1g --rw=write --bs=1m --direct=1 >/dev/null
  sync
}

# The commit p99 (us) with no writer ($1 = 0) or beside one at depth $1.
p99() {
  local k=$1 j
  j=$(mktemp)
  local args=(--output-format=json --lat_percentiles=1 --percentile_list=99
    --name=commit --filename="$mnt/commit.dat" --size=64m --rw=write --bs=4k
    --ioengine=psync --direct=1 --sync=dsync --time_based --runtime="$secs")
  if [ "$k" -gt 0 ]; then
    args+=(--name=bg --filename="$mnt/bg.dat" --size=1g --rw=write --bs=256k
      --ioengine=libaio --iodepth="$k" --direct=1 --time_based --runtime="$secs")
  fi
  local v
  fio "${args[@]}" >"$j" || reject "fio failed (commit p99 beside depth $k)"
  v=$(jq -r '[.jobs[] | select(.jobname == "commit") | .write.lat_ns.percentile["99.000000"]][0] / 1000 | floor' "$j") \
    || reject "fio's output has no commit p99 (beside depth $k)"
  rm -f "$j"
  [[ "$v" =~ ^[0-9]+$ ]] && [ "$v" -gt 0 ] || reject "fio's commit p99 beside depth $k is '$v'"
  echo "$v"
}

lo=$((d_ms * 1000000 / 4)) hi=$((d_ms * 2000000))
{
  echo "# G19 device calibration"
  echo
  echo "kernel $(uname -r), HZ $hz, D = $ticks ticks = $d_ms ms (flushes), mq-deadline as a FIFO, N from $((lo / 1000)) to $((hi / 1000)) us by sqrt(2), fio $(fio --version), $(nproc) CPUs (dm-delay polls from a kernel thread while bios are delayed: about one of them)"
  echo
  echo "| N (us) | idle p99 (us) | idle again | depth 1 | depth 2 | depth 8 | predicted d1 / d2 / d8 | separates |"
  echo "|---|---|---|---|---|---|---|---|"
} >"$md"

chosen=""
n=$lo
while [ "$n" -le "$hi" ]; do
  make "$n"
  i0=$(p99 0); l1=$(p99 1); l2=$(p99 2); l8=$(p99 8); i1=$(p99 0)
  destroy
  read -r r1 r2 r8 p1 p2 p8 ok < <(awk -v x="$i0" -v y="$i1" -v a="$l1" -v b="$l2" -v c="$l8" \
    -v n="$((n / 1000))" -v d="$((d_ms * 1000))" 'BEGIN {
      i = (x + y) / 2
      base = 2 * n + d
      agree = (x <= 1.5 * y && y <= 1.5 * x)
      ok = !agree ? "no (idle runs disagree)" : (a <= 1.5 * i && c >= 3 * i) ? "yes" : "no"
      printf "%.2f %.2f %.2f %.2f %.2f %.2f %s\n", a / i, b / i, c / i,
        (base + 2 * n) / base, (base + 3 * n) / base, (base + 9 * n) / base, ok }')
  echo "| $((n / 1000)) | $i0 | $i1 | $l1 (${r1}x) | $l2 (${r2}x) | $l8 (${r8}x) | ${p1}x / ${p2}x / ${p8}x | $ok |" >>"$md"
  echo "N $((n / 1000)) us: idle $i0/$i1, d1 ${r1}x, d2 ${r2}x, d8 ${r8}x (predicted ${p1}/${p2}/${p8}): $ok"
  if [ "$ok" = yes ] && [ -z "$chosen" ]; then
    chosen=$n
    [ "$all" = 1 ] || break
  fi
  n=$(awk -v n="$n" 'BEGIN { printf "%d", n * sqrt(2) + 0.5 }')
done

[ -n "$chosen" ] || reject "no N from $((lo / 1000)) us to $((hi / 1000)) us separates depth 1 (at most 1.5x) from depth 8 (at least 3x)"
make "$chosen"
# G19's throughput reference: the paced shape (256 KiB, one in flight)
# alone, no commits.
j=$(mktemp)
fio --output-format=json --name=bg --filename="$mnt/bg.dat" --size=1g --rw=write --bs=256k \
  --ioengine=libaio --iodepth=1 --direct=1 --time_based --runtime="$secs" >"$j" \
  || reject "fio failed (the paced shape alone)"
ref=$(jq -r '[.jobs[] | select(.jobname == "bg") | .write.bw_bytes][0] / 1000000' "$j")
[[ "$ref" =~ ^[0-9.]+$ ]] || reject "fio's rate at the paced shape alone is '$ref'"
rm -f "$j" "$mnt/commit.dat" "$mnt/bg.dat"
[ -n "${SUDO_USER:-}" ] && chown "$SUDO_USER" "$mnt"
echo "**Device:** N = $((chosen / 1000)) us, D = $d_ms ms; the paced shape alone (256 KiB, one in flight): $ref MB/s." >>"$md"
cat >"$out/g19_device.env" <<EOF
G19_N_NS=$chosen
G19_D_MS=$d_ms
G19_HZ=$hz
G19_DEV=$dev
G19_MOUNT=$mnt
G19_REF_MBPS=$ref
EOF
echo "device: N $((chosen / 1000)) us, D $d_ms ms, $dev at $mnt, paced shape alone $ref MB/s"
