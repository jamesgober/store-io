#!/bin/bash
# E11 (v0.4 brief): dm-log-writes replay of a store workload. Run as root on a
# CI runner (needs dm-log-writes and xfstests' replay-log). Every replayed
# state is a crash state the kernel could have left: replay to each receipt
# mark (all receipts issued so far must hold), to every flush entry and to
# every entry after the store became ready (receipts whose mark precedes the
# entry must hold, nothing torn after the intact prefix).
set -euo pipefail
FS=${1:-ext4}
N=${2:-32}
T=${RUNNER_TEMP:-/tmp}
BIN=$PWD/exp/logwrites/target/release/sio-logwrites
RL=$T/replay-log
OUT=$T/lw-$FS
mkdir -p "$OUT" /mnt/lw /mnt/lwr

truncate -s 256M "$T/lw-data.img"; truncate -s 1G "$T/lw-log.img"; truncate -s 256M "$T/lw-replay.img"
DATA=$(losetup -f --show "$T/lw-data.img")
LOG=$(losetup -f --show "$T/lw-log.img")
REPLAY=$(losetup -f --show "$T/lw-replay.img")
SECT=$(blockdev --getsz "$DATA")
modprobe dm-log-writes
dmsetup create sio-lw --table "0 $SECT log-writes $DATA $LOG"
mkfs."$FS" -q /dev/mapper/sio-lw
dmsetup message sio-lw 0 mark mkfs
mount /dev/mapper/sio-lw /mnt/lw
"$BIN" write /mnt/lw "$N" sio-lw | tee "$OUT/write.txt"
umount /mnt/lw
dmsetup remove sio-lw

TOTAL=$("$RL" --log "$LOG" --number-entries | grep -o '[0-9]\+' | tail -1)
echo "log entries: $TOTAL"
# Entry number of each receipt mark.
declare -a MARK
for i in $(seq 0 $((N - 1))); do
  MARK[$i]=$("$RL" --log "$LOG" --find --end-mark "r$i" | grep -o '[0-9]\+' | tail -1)
done
READY=$("$RL" --log "$LOG" --find --end-mark ready | grep -o '[0-9]\+' | tail -1)
echo "ready at entry $READY; receipt marks: ${MARK[*]}"

# Receipts that must hold once `entry` entries have been replayed.
need_at() {
  local e=$1 n=0
  for i in $(seq 0 $((N - 1))); do
    if [ "${MARK[$i]}" -lt "$e" ]; then n=$((i + 1)); fi
  done
  echo $n
}

FAILS=0
CHECKS=0
check_limit() { # $1 entries to replay  $2 label
  blkdiscard -z "$REPLAY" 2>/dev/null || dd if=/dev/zero of="$REPLAY" bs=1M count=256 status=none
  "$RL" --log "$LOG" --replay "$REPLAY" --limit "$1" > /dev/null
  local need
  need=$(need_at "$1")
  if [ "$1" -le "$READY" ]; then need=0; fi
  if ! mount "$REPLAY" /mnt/lwr 2> "$OUT/mount.err"; then
    if [ "$need" -eq 0 ]; then echo "entries=$1 ($2): no mountable fs yet (need 0)"; return; fi
    echo "entries=$1 ($2): MOUNT FAILED with need=$need"; FAILS=$((FAILS + 1)); return
  fi
  CHECKS=$((CHECKS + 1))
  if ! "$BIN" check /mnt/lwr "$need" > "$OUT/check.txt"; then FAILS=$((FAILS + 1)); fi
  echo "entries=$1 ($2): $(cat "$OUT/check.txt")"
  umount /mnt/lwr
}

echo "== replay to every receipt mark"
for i in $(seq 0 $((N - 1))); do check_limit $((MARK[$i] + 1)) "mark r$i"; done
echo "== replay to every flush entry"
start=0
while f=$("$RL" --log "$LOG" --find --next-flush --start-entry "$start" 2>/dev/null | grep -o '[0-9]\+' | tail -1) && [ -n "$f" ]; do
  check_limit $((f + 1)) "flush"
  start=$((f + 1))
  [ "$start" -lt "$TOTAL" ] || break
done
echo "== replay to every entry after ready (prefix crash states)"
for e in $(seq $((READY + 1)) "$TOTAL"); do check_limit "$e" "entry"; done

losetup -d "$DATA" "$LOG" "$REPLAY"
echo "RESULT fs=$FS receipts=$N entries=$TOTAL checks=$CHECKS failures=$FAILS"
[ "$FAILS" -eq 0 ]
