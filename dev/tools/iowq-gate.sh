#!/usr/bin/env bash
# D-073 gate (Linux): nothing store-io submits may run on io_uring's io-wq
# worker threads. Runs the harness's W6 (`submitter`: plain random writes
# and reads, N in flight) on the io_uring tier and samples the process's
# threads: no "iou-wrk-*" thread may appear. An io-wq worker stays for about
# 5 s after it goes idle (the last one until its thread exits), and W6
# submits from its long-lived main thread, so a sweep every few tens of
# milliseconds cannot miss one.
#
# Fails closed: a positive control first shows the sampling sees io-wq
# threads (fio's io_uring engine with an fsync after every write punts the
# fsyncs to io-wq), and the store-io run must complete with no anomalies and
# with every store-io point on io_uring.
#
# Usage: bash dev/tools/iowq-gate.sh   (needs fio and a release harness:
#        cd harness && cargo build --release; CARGO_TARGET_DIR is honoured)
set -u
root=$(cd "$(dirname "$0")/../.." && pwd)
bin=${CARGO_TARGET_DIR:-$root/harness/target}/release/store-io-harness
work=${STORE_IO_TEST_DIR:-$HOME}/.iowq-gate-$$
mkdir -p "$work"
trap 'rm -rf "$work"' EXIT

# io-wq threads in process $1 and its direct children, now.
count() {
  local pid=$1 n=0 c k
  for c in $pid $(cat /proc/"$pid"/task/*/children 2>/dev/null); do
    k=$(cat /proc/"$c"/task/*/comm 2>/dev/null | grep -c '^iou-wrk' || true)
    n=$((n + k))
  done
  echo "$n"
}

# The most io-wq threads seen while process $1 runs.
watch() {
  local pid=$1 seen=0 n
  while kill -0 "$pid" 2>/dev/null; do
    n=$(count "$pid")
    [ "$n" -gt "$seen" ] && seen=$n
    sleep 0.02
  done
  echo "$seen"
}

fio --name=c --filename="$work/control.dat" --size=64m --ioengine=io_uring --direct=1 \
    --rw=randwrite --bs=4k --iodepth=8 --fsync=1 --time_based --runtime=3s >/dev/null 2>&1 &
control=$(watch $!)
echo "control (fio, fsync on io_uring): io-wq threads seen = $control"
if [ "$control" -eq 0 ]; then
  echo "FAIL: the sampling cannot see io-wq threads here; the gate proves nothing"
  exit 2
fi

"$bin" submitter --secs 2 --tier uring --dir "$work" --out "$work" >"$work/run.log" 2>&1 &
pid=$!
seen=$(watch "$pid")
wait "$pid"
status=$?
tail -2 "$work/run.log"
report=$(ls "$work"/*submitter.md 2>/dev/null | head -1)
points=$(grep -c '^| store-io | Submitter' "$report" 2>/dev/null || true)
on_ring=$(grep '^| store-io | Submitter' "$report" 2>/dev/null | grep -c 'io: io_uring' || true)
echo "gate (store-io W6 on io_uring): io-wq threads seen = $seen; store-io points $points, on io_uring $on_ring"
# Fallbacks per point (counted and reported, D-073; not a failure).
grep -o 'io_uring fallbacks [0-9]* of [0-9]* operations' "$report" 2>/dev/null | sort | uniq -c
if [ "$status" -ne 0 ] || ! grep -q ', 0 anomalies' "$work/run.log" \
   || [ "$points" -eq 0 ] || [ "$on_ring" -ne "$points" ] || [ "$seen" -ne 0 ]; then
  echo "FAIL"
  exit 1
fi
echo "PASS"
