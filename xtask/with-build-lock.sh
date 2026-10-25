#!/usr/bin/env bash
# Run a command under the shared remote-build lock, reentrantly.
#
# Why this exists
# ---------------
# Eight lanes share one Fly build box with four build jobs. Compiles are
# serialised through an flock so they queue instead of thrashing. That is a
# throughput decision, not a correctness one — cargo is safe to run concurrently.
#
# The naive form of that, `flock /tmp/x.lock <command>`, deadlocked this fleet for
# 75 minutes. A lane wrapped a make target as `flock /tmp/x.lock make check`, which
# took the lock; `make check` then ran `flock /tmp/x.lock cargo check`, which waited
# forever for a lock its own ancestor held. flock is not reentrant and a blocking
# wait has no timeout, so one mistake blocked every other lane behind it, and the
# lanes looked busy while making no progress.
#
# The fix has to prevent the class of bug, not this instance, because nine agents
# writing their own shell wrappers will keep finding new instances. So the lock is
# reentrant by construction: the holder exports a marker, the marker is inherited
# by every descendant, and a descendant that sees the marker runs without taking
# the lock again.
#
# A timeout makes a genuinely stuck lock fail loudly instead of hanging silently,
# so the next incident produces an error message rather than an hour of quiet.
#
# Usage:
#   xtask/with-build-lock.sh cargo remote-3000 -r fly -d 1.95.0 check --workspace
#
# Makefile targets use it as $(LOCKED). Lanes should call `make check` etc. and let
# the Makefile lock; wrapping a make target in flock is harmless now but pointless.

set -euo pipefail

LOCK="${DXDB_BUILD_LOCK:-/tmp/dxdb-remote-build.lock}"
TIMEOUT="${DXDB_BUILD_LOCK_TIMEOUT:-3600}"

if [ "$#" -eq 0 ]; then
    echo "with-build-lock: no command given" >&2
    echo "usage: $0 <command> [args...]" >&2
    exit 2
fi

if [ -n "${DXDB_BUILD_LOCKED:-}" ]; then
    # An ancestor already holds the lock. Do not take it again; that is the
    # deadlock this script exists to prevent.
    exec "$@"
fi

exec flock -x -w "$TIMEOUT" "$LOCK" \
    env DXDB_BUILD_LOCKED=1 "$@" \
  || {
    status=$?
    if [ "$status" -eq 1 ]; then
        echo "with-build-lock: timed out after ${TIMEOUT}s waiting for ${LOCK}." >&2
        echo "  Another lane is probably mid-build. Check 'make busy' and retry," >&2
        echo "  or inspect the holder with: fuser -v ${LOCK}" >&2
    fi
    exit "$status"
  }
