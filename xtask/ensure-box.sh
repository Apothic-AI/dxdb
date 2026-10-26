#!/usr/bin/env bash
# Ensure the shared build box is reachable, starting and repairing it if needed.
#
# Why this exists: the machine has a service-state watchdog that stops it when no
# *lease* is held, and our lanes never take a lease (see the note in
# docs/TOOLCHAIN.md). So the box stops underneath us mid-session, and each recovery
# is three manual steps that are easy to get wrong:
#
#   1. `flyctl machine start`     -- a stopped machine refuses everything
#   2. `ensure-ready` via exec    -- sshd on 2222 does not self-start (no systemd)
#   3. retry the route            -- the WireGuard path flaps independently
#
# A stopped box reports "connection timed out" and looks identical to a network
# fault, which is why this checks machine state rather than only probing ssh.
#
# Usage: xtask/ensure-box.sh [--quiet]
set -uo pipefail

APP=open-surrealdb-ds-builder
MACHINE=87477e0cd01748
quiet="${1:-}"

say() { [ "$quiet" = "--quiet" ] || echo "ensure-box: $*"; }

ssh_ok() { timeout 8 ssh -o ConnectTimeout=5 -o BatchMode=yes fly-builder true >/dev/null 2>&1; }

if ssh_ok; then
    say "box reachable"
    exit 0
fi

state=$(timeout 60 flyctl machine list -a "$APP" 2>/dev/null | grep -oE 'started|stopped|suspended' | head -1)
say "ssh unreachable; machine state='${state:-unknown}'"

if [ "${state:-}" != "started" ]; then
    say "starting machine"
    timeout 120 flyctl machine start "$MACHINE" -a "$APP" >/dev/null 2>&1
    # The Fly API reports started before sshd is listening.
    for _ in $(seq 1 30); do ssh_ok && { say "up after start"; exit 0; }; sleep 4; done
fi

# Started but unreachable: the known post-restart failure. sshd on 2222 does not
# self-start, and /root/.cargo may be gone with the ephemeral rootfs. ensure-ready
# fixes both and is idempotent. machine exec times out even on success, so the
# exit status is not the signal -- reachability afterwards is.
say "running ensure-ready (exec may report a timeout even when it worked)"
timeout 300 flyctl machine exec -a "$APP" --no-container "$MACHINE" /data/bin/ensure-ready >/dev/null 2>&1

for _ in $(seq 1 30); do ssh_ok && { say "up after repair"; exit 0; }; sleep 4; done

say "FAILED: box still unreachable after start + repair"
exit 1
