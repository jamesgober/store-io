#!/bin/bash
# v0.4 experiments on the CI kernel (brief section 6: E1, E2, E3, E4, E6, E8,
# E9 on GitHub's ubuntu-latest). Run as root after building the tool as the
# runner user. Each filesystem is a loop image with barriers on (the runner's
# own root is mounted nobarrier). Counts are attributed by task name
# (v04-uring, iou-wrk), the same method as the WSL2 run (baseline 07).
set -u
BIN=$PWD/exp/v04-uring/target/release/v04-uring
T=/sys/kernel/tracing
IMG=${RUNNER_TEMP:-/tmp}
U=${SUDO_USER:-runner}
echo "kernel $(uname -r)"; nproc; grep -m1 'model name' /proc/cpuinfo

traced() { # $1 events  rest: binary args
  local evs=$1; shift
  echo 0 > $T/tracing_on; echo 0 > $T/events/enable; echo > $T/trace
  echo 32768 > $T/buffer_size_kb
  for e in $evs; do echo 1 > $T/events/${e/://}/enable; done
  echo 1 > $T/tracing_on
  "$BIN" "$@"
  echo 0 > $T/tracing_on
  local s="  trace:"
  for e in $evs; do
    local name=${e#*:} pat=" ${e#*:}:"
    case $name in sys_enter_*) pat=" sys_${name#sys_enter_}(" ;; esac
    case $e in
      jbd2:*|block:*|xfs:*) s="$s $name=$(grep -c -F "$pat" $T/trace)(system)" ;;
      *) s="$s $name=$(grep -E '^ *(v04-uring|iou-wrk)' $T/trace | grep -c -F "$pat")" ;;
    esac
  done
  echo "$s ($(grep -o 'entries-in-buffer/entries-written: [0-9]*/[0-9]*' $T/trace))"
  grep "io_uring_queue_async_work" $T/trace | grep -o "opcode [A-Z_]*" | sort | uniq -c
  echo 0 > $T/events/enable; echo > $T/trace
}

mount_fs() { # $1 fs  $2 mount options -> mounts at /mnt/e4
  umount /mnt/e4 2>/dev/null; losetup -D 2>/dev/null
  rm -f "$IMG/e4.img"; truncate -s 2G "$IMG/e4.img"
  mkfs."$1" -q -f "$IMG/e4.img" 2>/dev/null || mkfs."$1" -q -F "$IMG/e4.img"
  mkdir -p /mnt/e4; mount -o "loop,$2" "$IMG/e4.img" /mnt/e4
  chown "$U" /mnt/e4
  echo "-- $(findmnt -no FSTYPE,OPTIONS /mnt/e4)"
}

sec_probe() {
  mount_fs ext4 relatime
  "$BIN" probe /mnt/e4
}

sec_e4() {
  echo "== E4 punt census, QD1 4 KiB to a Ready file (cache clean), per filesystem and atime mode"
  for fs in ext4 xfs; do
    for o in relatime lazytime; do
      mount_fs $fs $o
      for m in ring-plain ring-dsync ring-fsyncop ring-nowait ring-nowait-dsync; do
            n=$(case $m in ring-plain|ring-nowait) echo 5000;; *) echo 2000;; esac)
        traced "io_uring:io_uring_queue_async_work jbd2:jbd2_start_commit xfs:xfs_log_force" qd1 defer+tf /mnt/e4/e4.dat $n $m
      done
    done
  done
  echo "== E4 raw loop block device"
  truncate -s 256M "$IMG/bdev.img"
  L=$(losetup -f --show --direct-io=on "$IMG/bdev.img")
  for m in ring-plain ring-dsync; do traced "io_uring:io_uring_queue_async_work" qd1 defer+tf "$L" 3000 $m; done
  losetup -d "$L"
}

sec_e1() {
  echo "== E1 NOP and wake"
  for c in plain coop+tf defer defer+tf+nosq; do "$BIN" nop $c 200000; done
  for m in msg eventfd futex; do "$BIN" wake $m 3000; done
}

sec_e2() {
  echo "== E2 QD1 4 KiB durable on an ext4 loop image (VM disk; software cost only)"
  mount_fs ext4 relatime
  for m in t3-plain ring-plain; do "$BIN" qd1 defer+tf /mnt/e4/e2.dat 5000 $m; done
  for m in t3-fdatasync t3-dsync ring-dsync ring-fsyncop ring-fdatasync t3-fdatasync ring-dsync; do
    "$BIN" qd1 defer+tf /mnt/e4/e2.dat 2000 $m
  done
}

sec_e3() {
  mount_fs ext4 relatime
  "$BIN" fsynchop /mnt/e4/e3.dat 200 0
}

sec_e6() {
  echo "== E6 memlock accounting and buffer clone (6.12+)"
  su "$U" -c "ulimit -l; $BIN memlock 24 3 8"
  "$BIN" memlock 24 3 8
}

sec_e9() { for c in plain defer defer+tf; do "$BIN" overflow $c; done; }

sec_e8() {
  echo "== E8 idle: 60 s window from 5 s after the program parked"
  mount_fs ext4 relatime
  for fsyncop in 0 1; do
    echo 0 > $T/tracing_on; echo 0 > $T/events/enable; echo > $T/trace
    "$BIN" idle /mnt/e4/e8.dat 1000 $fsyncop > "$IMG/e8.txt" &
    pid=$!
    until grep -q idle_start "$IMG/e8.txt" 2>/dev/null; do sleep 0.2; done
    cat "$IMG/e8.txt"; sleep 5
    echo "threads: $(cat /proc/$pid/task/*/comm | tr '\n' ' ')"
    f='comm == "v04-uring" || comm ~ "iou-wrk*"'
    for e in raw_syscalls/sys_enter sched/sched_wakeup timer/hrtimer_start timer/timer_start sched/sched_process_exit; do
      echo "$f" > $T/events/$e/filter; echo 1 > $T/events/$e/enable
    done
    echo 1 > $T/tracing_on; sleep 60; cp $T/trace "$IMG/e8w.trace"
    kill -USR1 $pid; wait $pid; sleep 0.5
    echo 0 > $T/tracing_on; cp $T/trace "$IMG/e8a.trace"
    for e in sys_enter sched_wakeup hrtimer_start timer_start sched_process_exit; do
      w=$(grep -c " $e:" "$IMG/e8w.trace"); a=$(grep -c " $e:" "$IMG/e8a.trace")
      echo "  fsyncop=$fsyncop $e window=$w control=$((a - w))"
    done
    grep -m4 -E " (sched_wakeup|timer_start):" "$IMG/e8w.trace"
    for e in raw_syscalls/sys_enter sched/sched_wakeup timer/hrtimer_start timer/timer_start sched/sched_process_exit; do
      echo 0 > $T/events/$e/enable; echo 0 > $T/events/$e/filter
    done
    tail -1 "$IMG/e8.txt"
  done
}

for s in "$@"; do echo "######## $s $(date -u +%FT%TZ)"; sec_$s; done
umount /mnt/e4 2>/dev/null || true
