#!/usr/bin/env bash
#
# xtask/wasm-size-budgets.sh -- the PLAN.md 5.3 size budgets, measured reproducibly.
#
# Measures each bundle against its budget with the workspace release profile
# (opt-level="s", lto="fat", codegen-units=1, panic="abort"), post-processes with the
# wasm-bindgen CLI whose version matches the crate's exactly, compresses with
# brotli -q 11, and verifies the round trip.
#
# The round trip is not paranoia: `brotli --quality=11 --best` is rejected ("quality
# already set") *while still writing output*, so a pipeline that ignores the exit status
# reports a plausible wrong number. This script uses one quality spelling and compares
# the decompressed bytes before reporting.
#
# Usage:
#   xtask/wasm-size-budgets.sh [--out DIR]
#
# Exits 0 when every measurable bundle is within budget, 1 when one is exceeded (that is
# a finding to report, not a build failure to hide), and 2 on a setup problem.
set -uo pipefail

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$REPO_ROOT" || exit 2

OUT=${DXDB_SIZE_OUT:-wasm-size-results}
while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT=$2; shift 2 ;;
    -h|--help) sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
mkdir -p "$OUT" || exit 2

# The budget rows from PLAN.md 5.3, in KiB, and the feature that builds each bundle.
# opfs is listed with its budget but no feature: the crate does not exist, and a row
# that says "not measurable: storage not implemented" is a true third state.
# Two families, deliberately measured together:
#
#   *:320 / *:900 / *:400  -- a storage linked to the binding floor. These say how small
#                              the binding layer is. They are NOT the budget test.
#   engine-*               -- the schema validator, query planner and index-string
#                              encoder linked in as well. THIS is the budget test: it is
#                              the first measurement where PLAN.md 5.3's numbers are a
#                              real constraint rather than a formality.
BUNDLES=(
  "core:250:wasm-size-fixtures"
  "memory:320:memory-bundle"
  "sqlite:900:sqlite-bundle"
  "opfs:400:opfs-bundle"
  "engine-memory:320:engine-memory-bundle"
  "engine-sqlite:900:engine-sqlite-bundle"
  "full-engine-memory:320:full-engine-memory-bundle"
  "schema-only:0:schema-only-bundle"
  "regress-only:0:regress-only-bundle"
)

have() { command -v "$1" >/dev/null 2>&1; }

# Same route selection as the Makefile and the bench script.
remote_build() { # remote_build <feature> <logfile>
  local route feat=$1 log=$2
  route=$(DXDB_FORCE_ROUTE=${DXDB_FORCE_ROUTE:-} xtask/remote-route.sh 2>/dev/null || true)
  # Each bench bin is gated behind a feature so a --lib build stays small; the size
  # fixture builds the library only.
  case "$route" in
    proxy:*)
      ssh -o ConnectTimeout=10 -p "${route#proxy:}" root@127.0.0.1 \
        "cd /data/builds/88b6632c01641f9a && /root/.cargo/bin/cargo build --release --lib \
           --package dxdb-bench --features $feat --target wasm32-unknown-unknown" > "$log" 2>&1 ;;
    *)
      cargo remote-3000 -r fly -d 1.95.0 build --release --lib --package dxdb-bench \
        --features "$feat" --target wasm32-unknown-unknown > "$log" 2>&1 ;;
  esac
}

remote_fetch() { # remote_fetch <dest>
  local route
  route=$(DXDB_FORCE_ROUTE=${DXDB_FORCE_ROUTE:-} xtask/remote-route.sh 2>/dev/null || true)
  case "$route" in
    proxy:*) ssh -o ConnectTimeout=10 -p "${route#proxy:}" root@127.0.0.1 \
               "cat /data/builds/88b6632c01641f9a/target/wasm32-unknown-unknown/release/dxdb_bench.wasm" > "$1" ;;
    *)       ssh -o ConnectTimeout=10 -p 2222 root@fly-builder \
               "cat /data/builds/88b6632c01641f9a/target/wasm32-unknown-unknown/release/dxdb_bench.wasm" > "$1" ;;
  esac
}

if [ "${1:-}" = "--no-opfs-note" ]; then :; fi
echo "wasm size budgets (PLAN.md 5.3)"
echo "  out: $OUT"

# Every row now has a crate behind it: opfs landed and passes 49/49 conformance, so its
# budget is measurable rather than a third state.

FAILED=0
SEEN_STATUS=0
: > "$OUT/results.json"
printf '{\n  "bundles": [\n' >> "$OUT/results.json"

for row in "${BUNDLES[@]}"; do
  IFS=: read -r label budget_kib feature <<< "$row"
  budget_bytes=$((budget_kib * 1024))
  # budget 0 marks an attribution fixture: measured for the breakdown, with no budget.
  [ "$budget_kib" = "0" ] && attribution=1 || attribution=0
  raw="$OUT/$label.raw.wasm"
  log="$OUT/$label.build.log"

  if ! remote_build "$feature" "$log"; then
    echo "  $label: BUILD FAILED (see $log)"
    FAILED=1
    continue
  fi
  if ! remote_fetch "$raw" || [ ! -s "$raw" ]; then
    echo "  $label: could not fetch the artifact (see $log)"
    FAILED=1
    continue
  fi

  # wasm-bindgen, then brotli, then measure with the crate's own tool so the round trip
  # is verified by the same code CI would run.
  if ! have wasm-bindgen; then
    echo "  $label: wasm-bindgen not installed"
    FAILED=1
    continue
  fi
  bindir="$OUT/$label.bindgen"
  mkdir -p "$bindir"
  if ! wasm-bindgen --target web --remove-name-section --remove-producers-section \
        --out-dir "$bindir" "$raw" >>"$log" 2>&1; then
    echo "  $label: wasm-bindgen failed (see $log)"
    FAILED=1
    continue
  fi
  wasm="$bindir/$label.wasm"
  # The CLI names the output after the crate; find whichever .wasm it wrote.
  wasm=$(find "$bindir" -maxdepth 1 -name '*.wasm' ! -name '*.d.ts' | head -1)

  line=$(cargo run --release -q --package dxdb-bench --features wasm-size --bin wasm-size -- \
           "$wasm" "$budget_bytes" "$label" 2>>"$log" | tail -1)
  json=$(cargo run --release -q --package dxdb-bench --features wasm-size --bin wasm-size -- \
           "$wasm" "$budget_bytes" "$label" 2>>"$log" | head -1)
  if [ "$attribution" = "1" ]; then
    # Attribution fixtures have no budget: they exist to split a cost, not to pass.
    bytes=$(printf '%s' "$json" | sed 's/.*"brotli_q11_bytes":\([0-9]*\).*/\1/')
    printf '  %-18s (attribution, no budget): %s B brotli\n' "$label" "$bytes"
  else
    echo "  $line"
  fi
  printf '    %s,\n' "$json" >> "$OUT/results.json"
  if [ "$attribution" != "1" ]; then
    case "$json" in
      *'"status":"EXCEEDED"'*) FAILED=1 ;;
    esac
  fi
  SEEN_STATUS=$((SEEN_STATUS + 1))
done

printf '  ]\n}\n' >> "$OUT/results.json"

echo
if [ $SEEN_STATUS -eq 0 ]; then
  echo "wasm-size-budgets: nothing measured"
  exit 2
fi
if [ $FAILED -ne 0 ]; then
  echo "wasm-size-budgets: at least one bundle EXCEEDED its budget or failed to build"
  exit 1
fi
echo "wasm-size-budgets OK: every bundle within budget"
echo "  results: $OUT/results.json"
exit 0
