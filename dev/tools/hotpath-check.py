#!/usr/bin/env python3
"""Gate G8b: reads an `strace -f` log of tests/hotpath.rs and fails if, in
any of its windows, the thread that made the window's markers made any
futex call (futex, or the futex2 calls futex_wait, futex_waitv, futex_wake,
futex_requeue) or any timed wait between them, or made no io_uring_enter
there (no I/O on a ring: the check would prove nothing).

The windows are marked by `statx` calls of `/store-io-hotpath-begin<label>`
and `/store-io-hotpath-end<label>`; every label in WINDOWS must be present
(a window that vanished proves nothing either).

strace -f writes each line as `<tid> <call>(...)`; a call interrupted by
another thread's is split into `<call>(... <unfinished ...>` and
`<... <call> resumed> ...)`, both carrying the tid, so either half counts.

Usage: hotpath-check.py <strace log>
"""
import re
import sys

# tests/hotpath.rs's windows: the commit-class submitter (an idle background
# one registered), a background submitter unpaced, and paced.
WINDOWS = ["", "-bg", "-bg-paced"]

LINE = re.compile(r"^(?:\[pid\s+)?(\d+)\]?\s+(.*)$")
MARKER = re.compile(r'"/store-io-hotpath-(begin|end)([\w-]*)"')
FUTEX = re.compile(r"(?:^|<\.\.\. )futex(?:_\w+)?(?:\(| resumed>)")
TIMED = re.compile(
    r"(?:^|<\.\.\. )(?:nanosleep|clock_nanosleep|timerfd_create|timerfd_settime|setitimer)(?:\(| resumed>)"
)
# A futex wait whose timeout argument is NULL (an untimed wait); anything
# else that waits is timed.
UNTIMED_WAIT = re.compile(r"FUTEX_WAIT[A-Z_]*, \d+, NULL")
ENTER = re.compile(r"(?:^|<\.\.\. )io_uring_enter(?:\(| resumed>)")


def split(line):
    m = LINE.match(line)
    return (m.group(1), m.group(2)) if m else (None, line)


def markers(lines):
    """{label: (begin index, end index)} of the first marker of each kind."""
    found = {}
    for i, l in enumerate(lines):
        m = MARKER.search(l)
        if m:
            found.setdefault((m.group(1), m.group(2)), i)
    labels = {label for (_, label) in found}
    return {
        label: (found.get(("begin", label)), found.get(("end", label)))
        for label in labels
    }


def check(lines, label, begin, end):
    """0 if the window passes, else 1 (a futex or timed wait) or 2 (it
    proves nothing)."""
    name = f"window {label!r}"
    if begin is None or end is None or end <= begin:
        print(f"FAIL: {name}: its markers are not in the trace")
        return 2
    tid = split(lines[begin])[0]
    if tid is None or split(lines[end])[0] != tid:
        print(f"FAIL: {name}: the markers are not on one thread (run strace with -f)")
        return 2
    window = [body for t, body in map(split, lines[begin + 1:end]) if t == tid]
    futex = [b for b in window if FUTEX.search(b)]
    timed = [b for b in window if TIMED.search(b)]
    timed += [b for b in futex if "FUTEX_WAIT" in b and not UNTIMED_WAIT.search(b)]
    timed += [b for b in futex if re.search(r"(?:^|<\.\.\. )futex_wait", b)]
    others = sum(
        1 for t, b in map(split, lines[begin + 1:end]) if t != tid and FUTEX.search(b)
    )
    print(f"{name}, thread {tid}: {len(window)} traced calls, {len(futex)} futex, "
          f"{len(timed)} timed waits; {others} futex calls on other threads")
    for l in (futex + timed)[:20]:
        print("  " + l)
    enters = [b for b in window if ENTER.search(b)]
    print(f"  {len(enters)} io_uring_enter calls on that thread")
    if not enters:
        print(f"FAIL: {name}: no io_uring_enter (no I/O ran on a ring; run strace with io_uring_enter traced)")
        return 2
    if futex or timed:
        print(f"FAIL: {name}")
        return 1
    return 0


def main() -> int:
    lines = open(sys.argv[1], encoding="utf-8", errors="replace").read().splitlines()
    found = markers(lines)
    worst = 0
    for label in WINDOWS:
        begin, end = found.get(label, (None, None))
        worst = max(worst, check(lines, label, begin, end))
    extra = sorted(set(found) - set(WINDOWS))
    if extra:
        print(f"FAIL: windows this check does not know: {extra}")
        worst = max(worst, 2)
    if worst == 0:
        print("PASS")
    return worst


if __name__ == "__main__":
    sys.exit(main())
