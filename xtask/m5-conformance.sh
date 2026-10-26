#!/usr/bin/env bash
#
# xtask/m5-conformance.sh -- the M5 gate, in one command.
#
# PLAN.md M5: "the conformance suite runs under Wasmtime from a .wasm component with a
# preopened directory, and under Node from both the napi build and the WASI build."
#
# This runs the **ported upstream conformance suite** (dxdb-testkit's 48
# implementations.test.ts cases, the same set `make gate-matrix` uses) against
# dxdb-filesystem, four ways:
#
#   native            a baseline, so a wasm-only failure is distinguishable
#   wasmtime-wasip1   the wasip1 module under Wasmtime, --dir preopen
#   wasmtime-wasip2   the WASI 0.2 *component* under Wasmtime, --dir preopen
#   node-wasi         the wasip1 build under node:wasi, preopens option
#   node-napi         the native addon, required from Node
#
# Exit 0 only if every target that ran passed. A target that could not run is a FAILURE,
# not a skip, unless it is one of the explicit opt-outs below. There is no "green-ish"
# mode: a gate that reports a summary and exits 0 is not a gate.
#
# Local steps, and why they are unavoidable:
#   * `cargo build` for the wasm targets and the napi addon runs on the shared box
#     (never here), via cargo-remote with the same route fallback the Makefile uses.
#   * Artifacts are fetched back with `ssh cat`.
#   * Node runs them locally, because a native addon and `node:wasi` only exist inside a
#     Node process. This is not a local cargo build.
#
# Usage:
#   xtask/m5-conformance.sh                 # all targets
#   xtask/m5-conformance.sh --keep DIR      # keep artifacts and db dirs
#   xtask/m5-conformance.sh --targets native,wasmtime-wasip1
#   DXDB_ALLOW_MISSING_WASMTIME=1 ...       # opt out of the Wasmtime targets
set -uo pipefail

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$REPO_ROOT" || exit 1

WORK=${DXDB_M5_WORK:-}
TARGETS=${DXDB_M5_TARGETS:-native,wasmtime-wasip1,wasmtime-wasip2,node-wasi,node-napi}
KEEP=0
REMOTE_BUILD_DIR=${DXDB_M5_REMOTE_DIR:-/data/builds/88b6632c01641f9a}

while [ $# -gt 0 ]; do
  case "$1" in
    --keep)    KEEP=1; WORK=${2:?--keep needs a directory}; shift 2 ;;
    --targets) TARGETS=$2; shift 2 ;;
    -h|--help) sed -n '2,36p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

[ -n "$WORK" ] || WORK=$(mktemp -d "${TMPDIR:-/tmp}/dxdb-m5.XXXXXX") || exit 2
mkdir -p "$WORK" || exit 2
LOG="$WORK/m5-conformance.log"
: > "$LOG"

RED=''; GREEN=''; YELLOW=''; DIM=''; RESET=''
if [ -t 1 ]; then RED=$'\033[31m'; GREEN=$'\033[32m'; YELLOW=$'\033[33m'; DIM=$'\033[2m'; RESET=$'\033[0m'; fi

FAILED_TARGETS=()
PASSED_TARGETS=()
SKIPPED_TARGETS=()

report_pass() { printf '%sPASS%s  %-18s %s\n' "$GREEN" "$RESET" "$1" "$2"; PASSED_TARGETS+=("$1"); }
report_fail() { printf '%sFAIL%s  %-18s %s\n' "$RED" "$RESET" "$1" "$2"; FAILED_TARGETS+=("$1"); }
report_skip() { printf '%sSKIP%s  %-18s %s\n' "$YELLOW" "$RESET" "$1" "$2"; SKIPPED_TARGETS+=("$1"); }
note()        { printf '%s      %s%s\n' "$DIM" "$1" "$RESET"; }
have()        { command -v "$1" >/dev/null 2>&1; }

# ------------------------------------------------------------------- plumbing --
route() {
  DXDB_FORCE_ROUTE=${DXDB_FORCE_ROUTE:-} xtask/remote-route.sh 2>/dev/null || true
}

remote_cargo() {
  local r
  r=$(route)
  case "$r" in
    proxy:*) cargo remote-3000 -H fly-builder-proxy -p "${r#proxy:}" -d 1.95.0 "$@" ;;
    *)       cargo remote-3000 -r fly -d 1.95.0 "$@" ;;
  esac
}

ssh_box() {
  local r
  r=$(route)
  case "$r" in
    proxy:*) ssh -o ConnectTimeout=10 -p "${r#proxy:}" root@127.0.0.1 "$@" ;;
    *)       ssh -o ConnectTimeout=10 -p 2222 root@fly-builder "$@" ;;
  esac
}

sync_tree() {
  local r
  r=$(route)
  case "$r" in
    proxy:*)
      rsync -az --delete --exclude 'target/' --exclude '.git/' --exclude 'graft/' \
        --exclude 'node_modules/' -e "ssh -p ${r#proxy:}" \
        ./ root@127.0.0.1:"$REMOTE_BUILD_DIR/" >/dev/null 2>&1 ;;
    *)
      rsync -az --delete --exclude 'target/' --exclude '.git/' --exclude 'graft/' \
        --exclude 'node_modules/' -e "ssh -p 2222" \
        ./ root@fly-builder:"$REMOTE_BUILD_DIR/" >/dev/null 2>&1 ;;
  esac
}

fetch() { ssh_box "cat $1" > "$2" 2>>"$LOG"; }

SYNCED=0
ensure_synced() {
  if [ $SYNCED -eq 0 ]; then
    sync_tree || { echo "error: could not sync sources to the box" >&2; return 1; }
    SYNCED=1
  fi
  return 0
}

# A `command -v wasmtime` hit is not proof it works: mise installs a shim that exists on
# PATH but exits nonzero with "installed but not activated". Every candidate is executed.
wasmtime_bin() {
  local c
  if have wasmtime; then
    c=$(command -v wasmtime)
    "$c" --version >/dev/null 2>&1 && { printf '%s\n' "$c"; return 0; }
  fi
  c=$(ls -d "$HOME"/.local/share/mise/installs/wasmtime/*/wasmtime-v* 2>/dev/null | sort -V | tail -1)
  if [ -n "$c" ] && [ -x "$c/wasmtime" ] && "$c/wasmtime" --version >/dev/null 2>&1; then
    printf '%s\n' "$c/wasmtime"; return 0
  fi
  return 1
}

# The suite prints "N/N passed" on success and "RUN <id>" before each case, so the last
# RUN line before a trap names the failing case. Both are checked.
verdict() { # verdict <logfile> <total>
  local log=$1 total=$2
  if grep -qE "^${total}/${total} passed, 0 failed" "$log"; then
    return 0
  fi
  local running
  running=$(grep '^RUN ' "$log" | tail -1 | awk '{print $2}')
  printf 'last case before failure: %s' "${running:-unknown}" >&2
  return 1
}

build_wasm() { # build_wasm <target>
  remote_cargo build --release --package dxdb-wasi --bin dxdb-conformance \
    --target "$1" > "$WORK/build-$1.log" 2>&1
}

fetch_wasm() { # fetch_wasm <target> <dest>
  fetch "$REMOTE_BUILD_DIR/target/$1/release/dxdb-conformance.wasm" "$2"
}

TOTAL=48  # implementations.test.ts cases; asserted by dxdb-wasi's own test too

# ------------------------------------------------------------------- targets --
run_native() {
  local dir="$WORK/db-native" out code
  rm -rf "$dir"; mkdir -p "$dir"
  out=$(cargo run -q --release --package dxdb-wasi --bin dxdb-conformance -- --dir "$dir" 2>&1)
  code=$?
  printf '%s\n' "$out" >> "$LOG"
  if [ $code -eq 0 ] && verdict <(printf '%s' "$out") "$TOTAL"; then
    report_pass native "$(printf '%s' "$out" | tail -1)"
    return 0
  fi
  report_fail native "exit $code: $(printf '%s' "$out" | tail -1)"
  return 1
}

run_wasmtime() { # run_wasmtime <label> <target>
  local label=$1 target=$2 wasm="$WORK/$2.wasm" wt dbdir out code
  ensure_synced || { report_fail "$label" "sync to the box failed"; return 1; }
  build_wasm "$target" || {
    report_fail "$label" "build failed: $(grep -m1 -E '^error' "$WORK/build-$target.log" | cut -c1-120)"
    return 1
  }
  fetch_wasm "$target" "$wasm" || { report_fail "$label" "could not fetch the artifact"; return 1; }

  if ! wt=$(wasmtime_bin); then
    if [ "${DXDB_ALLOW_MISSING_WASMTIME:-0}" = "1" ]; then
      report_skip "$label" "wasmtime not installed (DXDB_ALLOW_MISSING_WASMTIME=1)"
      return 0
    fi
    report_fail "$label" "wasmtime not installed and not opted out of"
    return 1
  fi

  # The preopen. Guest sees /data and nothing else -- not /, not the host's /tmp.
  # DB_DIR is the guest-side path on purpose; passing the host path here is the trap
  # that yields ENOENT from a directory that demonstrably exists.
  dbdir="$WORK/db-$target"
  rm -rf "$dbdir"; mkdir -p "$dbdir"
  out=$("$wt" run --dir "$dbdir::/data" "$wasm" --dir /data 2>&1)
  code=$?
  printf '%s\n' "$out" >> "$LOG"
  if [ $code -eq 0 ] && verdict <(printf '%s' "$out") "$TOTAL"; then
    report_pass "$label" "$(printf '%s' "$out" | tail -1)  [wasmtime $("$wt" --version | awk '{print $2}')]"
    return 0
  fi
  report_fail "$label" "exit $code: $(verdict <(printf '%s' "$out") "$TOTAL" 2>&1 | tail -1)"
  return 1
}

run_node_wasi() {
  local target=wasm32-wasip1 wasm="$WORK/wasm32-wasip1.wasm" dbdir out code
  ensure_synced || { report_fail node-wasi "sync to the box failed"; return 1; }
  build_wasm "$target" || {
    report_fail node-wasi "build failed: $(grep -m1 -E '^error' "$WORK/build-$target.log" | cut -c1-120)"
    return 1
  }
  fetch_wasm "$target" "$wasm" || { report_fail node-wasi "could not fetch the artifact"; return 1; }

  dbdir="$WORK/db-node-wasi"; rm -rf "$dbdir"; mkdir -p "$dbdir"
  out=$(node --no-warnings "$REPO_ROOT/xtask/m5-node-wasi.mjs" "$wasm" /data "$dbdir" 2>&1)
  code=$?
  printf '%s\n' "$out" >> "$LOG"
  if [ $code -eq 0 ] && verdict <(printf '%s' "$out") "$TOTAL"; then
    report_pass node-wasi "$(printf '%s' "$out" | tail -1)  [node $(node --version)]"
    return 0
  fi
  report_fail node-wasi "exit $code: $(verdict <(printf '%s' "$out") "$TOTAL" 2>&1 | tail -1)"
  return 1
}

run_node_napi() {
  local addon="$WORK/dxdb_node.node" dbdir out code artifact
  ensure_synced || { report_fail node-napi "sync to the box failed"; return 1; }

  # The addon build is a separate artifact (release cdylib, renamed `.node`). A local
  # build step is not needed and not used: it is built on the box like everything else,
  # because a napi addon must link against the target's libc.
  remote_cargo build --release --package dxdb-node --features node-addon \
    > "$WORK/build-node-napi.log" 2>&1 || {
    report_fail node-napi "build failed: $(grep -m1 -E '^error' "$WORK/build-node-napi.log" | cut -c1-140)"
    return 1
  }
  # cargo replaces `-` with `_` in artifact names: dxdb-node -> libdxdb_node.so.
  artifact="lib$(printf 'dxdb-node' | tr '-' '_').so"
  fetch "$REMOTE_BUILD_DIR/target/release/$artifact" "$addon" || {
    report_fail node-napi "could not fetch $artifact"
    return 1
  }

  dbdir="$WORK/db-node-napi"; rm -rf "$dbdir"; mkdir -p "$dbdir"
  out=$(node "$REPO_ROOT/xtask/m5-node-napi.mjs" "$addon" "$dbdir" 2>&1)
  code=$?
  printf '%s\n' "$out" >> "$LOG"
  if [ $code -eq 0 ] && verdict <(printf '%s' "$out") "$TOTAL"; then
    report_pass node-napi "$(printf '%s' "$out" | tail -1)  [node $(node --version)]"
    return 0
  fi
  report_fail node-napi "exit $code: $(printf '%s' "$out" | tail -1)"
  return 1
}

# --------------------------------------------------------------------- main --
printf 'M5 conformance: dxdb-testkit implementations suite (%s cases) x dxdb-filesystem\n' "$TOTAL"
printf '  work: %s\n' "$WORK"
printf '  node %s   wasmtime %s\n' \
  "$(node --version 2>/dev/null || echo missing)" \
  "$( { wt=$(wasmtime_bin) && "$wt" --version | awk '{print $2}'; } 2>/dev/null || echo missing)"
note "opfs and indexeddb are not in this gate: they need the demo/ browser harness."

IFS=',' read -r -a wanted <<< "$TARGETS"
for t in "${wanted[@]}"; do
  case "$t" in
    native)          run_native ;;
    wasmtime-wasip1) run_wasmtime wasmtime-wasip1 wasm32-wasip1 ;;
    wasmtime-wasip2) run_wasmtime wasmtime-wasip2 wasm32-wasip2 ;;
    node-wasi)       run_node_wasi ;;
    node-napi)       run_node_napi ;;
    *) printf '%sFAIL%s  %-18s unknown target\n' "$RED" "$RESET" "$t"; FAILED_TARGETS+=("$t") ;;
  esac
done

printf '\n'
if [ ${#FAILED_TARGETS[@]} -gt 0 ]; then
  printf '%sM5 conformance FAILED%s: %d passed, %d failed, %d skipped\n' \
    "$RED" "$RESET" "${#PASSED_TARGETS[@]}" "${#FAILED_TARGETS[@]}" "${#SKIPPED_TARGETS[@]}"
  printf '  failed: %s\n' "${FAILED_TARGETS[*]}"
  printf '  log: %s\n' "$LOG"
  exit 1
fi
if [ ${#PASSED_TARGETS[@]} -eq 0 ]; then
  printf '%sM5 conformance FAILED%s: no target ran\n' "$RED" "$RESET"
  exit 1
fi
printf '%sM5 conformance OK%s: %d passed, 0 failed, %d skipped\n' \
  "$GREEN" "$RESET" "${#PASSED_TARGETS[@]}" "${#SKIPPED_TARGETS[@]}"
printf '  log: %s\n' "$LOG"
if [ $KEEP -eq 1 ]; then printf '  kept: %s\n' "$WORK"; else rm -rf "$WORK"; fi
exit 0
