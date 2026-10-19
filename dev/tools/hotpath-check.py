#!/usr/bin/env python3
"""Gate G8b: reads an `strace -f` log of tests/hotpath.rs and fails if the
thread that made the window's markers made any futex call (futex, or the
futex2 calls futex_wait, futex_waitv, futex_wake, futex_requeue) or any
timed wait between them, or made no io_uring_enter there (no I/O on a
ring: the check would prove nothing).

strace -f writes each line as `<tid> <call>(...)`; a call interrupted by
another thread's is split into `<call>(... <unfinished ...>` and
`<... <call> resumed> ...)`, both carrying the tid, so either half counts.

Usage: hotpath-check.py <strace log>
"""
import re
import sys

LINE = re.compile(r"^(?:\[pid\s+)?(\d+)\]?\s+(.*)$")
FUTEX = re.compile(r"(?:^|<\.\.\. )futex(?:_\w+)?(?:\(| resumed>)")
TIMED = re.compile(
    r"(?:^|<\.\.\. )(?:nanosleep|clock_nanosleep|timerfd_create|timerfd_settime|setitimer)(?:\(| resumed>)"
)
# A futex wait whose timeout argument is NULL (an untimed wait); anything
# else that waits is timed.
UNTIMED_WAIT = re.compile(r"FUTEX_WAIT[A-Z_]*, \d+, NULL")


def split(line):
    m = LINE.match(line)
    return (m.group(1), m.group(2)) if m else (None, line)


def main() -> int:
    lines = open(sys.argv[1], encoding="utf-8", errors="replace").read().splitlines()
    begin = next((i for i, l in enumerate(lines) if "/store-io-hotpath-begin" in l), None)
    end = next((i for i, l in enumerate(lines) if "/store-io-hotpath-end" in l), None)
    if begin is None or end is None or end <= begin:
        print("FAIL: the window's markers are not in the trace")
        return 2
    tid = split(lines[begin])[0]
    if tid is None or split(lines[end])[0] != tid:
        print("FAIL: the markers are not on one thread (run strace with -f)")
        return 2
    window = [body for t, body in map(split, lines[begin + 1:end]) if t == tid]
    futex = [b for b in window if FUTEX.search(b)]
    timed = [b for b in window if TIMED.search(b)]
    timed += [b for b in futex if "FUTEX_WAIT" in b and not UNTIMED_WAIT.search(b)]
    timed += [b for b in futex if re.search(r"(?:^|<\.\.\. )futex_wait", b)]
    others = sum(
        1 for t, b in map(split, lines[begin + 1:end]) if t != tid and FUTEX.search(b)
    )
    print(f"thread {tid}: {len(window)} traced calls in the window, {len(futex)} futex, "
          f"{len(timed)} timed waits; {others} futex calls on other threads")
    for l in (futex + timed)[:20]:
        print("  " + l)
    enters = [b for b in window if re.search(r"(?:^|<\.\.\. )io_uring_enter(?:\(| resumed>)", b)]
    print(f"  {len(enters)} io_uring_enter calls on that thread")
    if not enters:
        print("FAIL: no io_uring_enter in the window (no I/O ran on a ring; run strace with io_uring_enter traced)")
        return 2
    if futex or timed:
        print("FAIL")
        return 1
    print("PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
