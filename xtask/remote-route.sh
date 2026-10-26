#!/usr/bin/env bash
# Choose a working route to the shared build box.
#
# Primary is the WireGuard 6PN address (Host fly-builder). When that route is
# broken from this machine -- "No route to host" while the box itself is healthy
# and listening on 2222, which has been the case since a machine restart -- fall
# back to Fly's API proxy (Host fly-builder-proxy on 127.0.0.1:2223), starting it
# if it is not already running.
#
# This exists so lanes keep running `make check` and never need to know which
# route is up. It only chooses the route; it does not bypass the build lock, and
# callers still go through xtask/with-build-lock.sh.
set -uo pipefail

PROXY_PORT="${DXDB_PROXY_PORT:-2223}"

route_ok() { timeout 6 ssh -o ConnectTimeout=4 -o BatchMode=yes "$1" true >/dev/null 2>&1; }

if route_ok fly-builder; then
    printf 'fly'
    exit 0
fi

if ! route_ok fly-builder-proxy; then
    # Start (or restart) the proxy and give it a moment to accept connections.
    pkill -f "flyctl proxy ${PROXY_PORT}:2222" >/dev/null 2>&1 || true
    setsid nohup flyctl proxy "${PROXY_PORT}:2222" -a open-surrealdb-ds-builder \
        >/tmp/fly-builder-proxy.log 2>&1 &
    for _ in $(seq 1 20); do
        if route_ok fly-builder-proxy; then break; fi
        sleep 1
    done
fi

if route_ok fly-builder-proxy; then
    printf 'proxy:%s' "$PROXY_PORT"
    exit 0
fi

echo "remote-shim: neither fly-builder nor fly-builder-proxy is reachable." >&2
echo "  The box is probably healthy but unreachable from here. Check with:" >&2
echo "    flyctl ssh console -a open-surrealdb-ds-builder -C 'cat /proc/net/tcp6'" >&2
exit 1
