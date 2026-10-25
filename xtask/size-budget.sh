#!/usr/bin/env bash
# Measure and enforce the PLAN.md 5.3 WASM size budgets.
#
# Budgets are brotli sizes of the release-profile wasm, and the release profile is
# already configured for size in Cargo.toml (opt-level="s", lto="fat",
# codegen-units=1, panic="abort"). Measuring anything else measures the wrong
# artifact.
#
# Usage:
#   xtask/size-budget.sh <wasm-file> <budget-kib>
#
# Exits 0 within budget, 1 over. Prints one line either way, so CI logs are
# diffable across runs.
set -euo pipefail

wasm="${1:?usage: size-budget.sh <wasm-file> <budget-kib>}"
budget_kib="${2:?usage: size-budget.sh <wasm-file> <budget-kib>}"

if [ ! -f "$wasm" ]; then
  echo "size-budget: $wasm does not exist — build it before measuring" >&2
  exit 2
fi

# brotli -q 11 is what a CDN will serve and what the budget is stated in.
if ! command -v brotli >/dev/null 2>&1; then
  echo "size-budget: brotli not installed (apt install brotli / brew install brotli)" >&2
  exit 2
fi

actual_bytes=$(brotli -q 11 -c "$wasm" | wc -c)
actual_kib=$(( actual_bytes / 1024 ))
budget_bytes=$(( budget_kib * 1024 ))

if [ "$actual_bytes" -le "$budget_bytes" ]; then
  printf 'size-budget: OK   %s  %s KiB brotli (budget %s KiB, %s KiB headroom)\n' \
    "$wasm" "$actual_kib" "$budget_kib" "$(( budget_kib - actual_kib ))"
  exit 0
fi

printf 'size-budget: OVER %s  %s KiB brotli (budget %s KiB, %s KiB over)\n' \
  "$wasm" "$actual_kib" "$budget_kib" "$(( actual_kib - budget_kib ))" >&2
echo "size-budget: revise the budget with a measured number and a reason in docs/BENCHMARKS.md," >&2
echo "             or make the bundle smaller. Do not ignore this." >&2
exit 1
