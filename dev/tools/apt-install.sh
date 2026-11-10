#!/usr/bin/env bash
# CI: installs Debian packages with every apt call bounded, so a stalled
# mirror or a held dpkg lock fails the step in minutes and says which call
# stalled, instead of hanging silently until the job's timeout (twice on
# 4617ebe's xfs conformance job: no output for 28 minutes after the call).
#
# Each attempt: with --update, `apt-get update` (a failure is a warning:
# the install uses the lists it fetched), then `apt-get install`. Network
# reads time out after 30 s and are retried by apt; the dpkg lock is waited
# for at most 120 s; each call has a wall-clock bound. Three attempts, the
# later ones always updating first; a killed install is completed with
# `dpkg --configure -a` before the next.
#
# Usage: bash dev/tools/apt-install.sh [--update] PACKAGE...
set -u
opts=(-o Acquire::Retries=3 -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30
      -o DPkg::Lock::Timeout=120)
update=0
if [ "${1:-}" = --update ]; then update=1; shift; fi
[ "$#" -gt 0 ] || { echo "usage: apt-install.sh [--update] PACKAGE..."; exit 2; }
for attempt in 1 2 3; do
  if [ "$update" = 1 ]; then
    timeout 300 sudo apt-get "${opts[@]}" update -qq \
      || echo "::warning::apt-get update failed or timed out (attempt $attempt); installing from the lists it fetched"
  fi
  if timeout 600 sudo apt-get "${opts[@]}" install -y -qq "$@"; then exit 0; fi
  echo "::warning::apt-get install $* failed or timed out (attempt $attempt)"
  timeout 300 sudo dpkg --configure -a || true
  update=1
done
echo "::error::could not install $* after 3 attempts"
exit 1
