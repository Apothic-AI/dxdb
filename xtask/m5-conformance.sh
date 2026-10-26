#!/usr/bin/env bash
#
# xtask/m5-conformance.sh -- the M5 gate, in one command.
#
# PLAN.md M5: "the conformance suite runs under Wasmtime from a .wasm component
# with a preopened directory, and under Node from both the napi build and the WASI
# build."
#
# So this runs the same suite four ways and reports pass/fail per target:
#
#   1. wasmtime-wasip1   the wasip1 core module under Wasmtime, --dir preopen
#   2. wasmtime-wasip2   the wasip2 component under Wasmtime, --dir preopen
#   3. node-wasi         the wasip1 build under node:wasi, preopens option
#   4. node-napi         the native addon, required from Node
#
# and a native run first as a baseline, because "the wasm one failed and the
# native one passed" is a different bug from "everything failed".
#
# Exit status is 0 only if every target that ran passed. A target that could not
# run at all is a FAILURE, not a skip, unless it is one of the explicitly
# documented opt-outs below -- this script does not have a "green-ish" mode.
#
# Local steps are deliberate and unavoidable:
#   * `cargo build --target wasm32-*` happens on the shared build box (never here).
#   * The .wasm artifacts and the napi .so are fetched back with scp.
#   * Node loads the .so locally, because a native addon loads in a Node process
#     and nowhere else. This is not a local cargo build.
#
# Usage:
#   xtask/m5-conformance.sh              # all targets
#   xtask/m5-conformance.sh --keep DIR   # keep artifacts and db dirs in DIR
#   xtask/m5-conformance.sh --targets native,wasmtime-wasip1
#   DXDB_ALLOW_MISSING_WASMTIME=1 ...    # opt out of target 1/2 if not installed
#
set -uo pipefail

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$REPO_ROOT" || exit 1

CRATE=dxdb-wasi
BIN=dxdb-conformance
NODE_CRATE=dxdb-node
WORK=${DXDB_M5_WORK:-}
TARGETS=${DXDB_M5_TARGETS:-native,wasmtime-wasip1,wasmtime-wasip2,node-wasi,node-napi}
KEEP=0

while [ $# -gt 0 ]; do
  case "$1" in
    --keep)   KEEP=1; WORK=${2:?--keep needs a directory}; shift 2 ;;
    --targets) TARGETS=$2; shift 2 ;;
    -h|--help)
      sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [ -z "$WORK" ]; then
  WORK=$(mktemp -d "${TMPDIR:-/tmp}/dxdb-m5.XXXXXX") || exit 2
fi
mkdir -p "$WORK" || exit 2

LOG="$WORK/m5-conformance.log"
: > "$LOG"

# ---------------------------------------------------------------- reporting --
RED=''; GREEN=''; YELLOW=''; DIM=''; RESET=''
if [ -t 1 ]; then RED=$'\033[31m'; GREEN=$'\033[32m'; YELLOW=$'\033[33m'; DIM=$'\033[2m'; RESET=$'\033[0m'; fi

FAILED_TARGETS=()
PASSED_TARGETS=()
SKIPPED_TARGETS=()

report_pass() { printf '%sPASS%s  %-18s %s\n' "$GREEN" "$RESET" "$1" "$2"; PASSED_TARGETS+=("$1"); }
report_fail() { printf '%sFAIL%s  %-18s %s\n' "$RED" "$RESET" "$1" "$2"; FAILED_TARGETS+=("$1"); }
report_skip() { printf '%sSKIP%s  %-18s %s\n' "$YELLOW" "$RESET" "$1" "$2"; SKIPPED_TARGETS+=("$1"); }
note()        { printf '%s      %s%s\n' "$DIM" "$1" "$RESET"; }

have() { command -v "$1" >/dev/null 2>&1; }

# ------------------------------------------------------------------- tooling --
# Route selection mirrors the Makefile: the WireGuard 6PN path when it works, Fly's
# API proxy when it does not. Degrading to the proxy is not a fallback that hides a
# problem -- it is how this repo reaches the box while the tunnel is down.
remote_cargo() {
  local route
  route=$(DXDB_FORCE_ROUTE=${DXDB_FORCE_ROUTE:-} xtask/remote-route.sh 2>/dev/null || true)
  case "$route" in
    proxy:*)
      cargo remote-3000 -H fly-builder-proxy -p "${route#proxy:}" -d 1.95.0 "$@" ;;
    *)
      cargo remote-3000 -r fly -d 1.95.0 "$@" ;;
  esac
}

# Finds the remote build directory for this project, so artifacts can be fetched.
# cargo-remote logs it; the log is the only supported way to learn the path.
remote_build_dir() {
  grep -o '/data/builds/[0-9a-f]*' "$1" 2>/dev/null | head -1
}

fetch() { # fetch <remote-path> <local-path> <logfile>
  scp -q root@fly-builder-proxy:"$1" "$2" 2>>"$LOG" && return 0
  scp -q root@fly-builder:"$1" "$2" 2>>"$LOG" && return 0
  return 1
}

# A `command -v wasmtime` hit is not proof it works: mise installs a shim that
# exists on PATH but exits nonzero with "installed but not activated". So every
# candidate is executed before it is accepted.
wasmtime_bin() {
  local candidate
  if have wasmtime; then
    candidate=$(command -v wasmtime)
    if "$candidate" --version >/dev/null 2>&1; then
      printf '%s\n' "$candidate"; return 0
    fi
  fi
  candidate=$(ls -d "$HOME"/.local/share/mise/installs/wasmtime/*/wasmtime-v* 2>/dev/null | sort -V | tail -1)
  if [ -n "$candidate" ] && [ -x "$candidate/wasmtime" ] && "$candidate/wasmtime" --version >/dev/null 2>&1; then
    printf '%s\n' "$candidate/wasmtime"; return 0
  fi
  return 1
}

# ------------------------------------------------------------------- targets --
# Each target function must: run the suite, print its own one-line result, and
# return 0 only when every case passed.

run_native() {
  local dir="$WORK/db-native"
  rm -rf "$dir"; mkdir -p "$dir"
  local out exit_code
  out=$(cargo run -q --package "$CRATE" --bin "$BIN" -- --dir "$dir" 2>&1); exit_code=$?
  printf '%s\n' "$out" >> "$LOG"
  if [ $exit_code -eq 0 ] && printf '%s' "$out" | grep -qE '^([0-9]+)/\1 passed'; then
    report_pass native "$(printf '%s' "$out" | tail -1)"
    return 0
  fi
  report_fail native "exit $exit_code: $(printf '%s' "$out" | tail -2 | tr '\n' ' ')"
  return 1
}

build_wasm() { # build_wasm <target> <logfile>
  remote_cargo build --package "$CRATE" --bin "$BIN" --target "$1" > "$2" 2>&1
}

fetch_wasm() { # fetch_wasm <target> <dest> <logfile>
  local dir
  dir=$(remote_build_dir "$3")
  [ -n "$dir" ] || return 1
  fetch "$dir/target/$1/debug/$BIN.wasm" "$2" "$3"
}

run_wasmtime() { # run_wasmtime <label> <target> <artifact-name>
  local label=$1 target=$2 artifact=$3
  local log="$WORK/build-$target.log" wasm="$WORK/$artifact.wasm"

  if ! have wasm-tools && ! have cargo; then
    report_fail "$label" "no toolchain to build with"
    return 1
  fi
  build_wasm "$target" "$log" || {
    report_fail "$label" "build failed: $(grep -m1 -E '^error' "$log" | cut -c1-120)"
    return 1
  }
  fetch_wasm "$target" "$wasm" "$log" || {
    report_fail "$label" "could not fetch $BIN.wasm (see $log)"
    return 1
  }

  local wt
  if ! wt=$(wasmtime_bin); then
    if [ "${DXDB_ALLOW_MISSING_WASMTIME:-0}" = "1" ]; then
      report_skip "$label" "wasmtime not installed (DXDB_ALLOW_MISSING_WASMTIME=1)"
      return 0
    fi
    report_fail "$label" "wasmtime not installed and not opted out of"
    return 1
  fi

  # The preopen. Guest sees /data and nothing else -- not /, not the host's /tmp.
  # DB_DIR is the guest-side path on purpose; passing the host path here is the
  # trap that yields ENOENT from a directory that demonstrably exists.
  local dbdir="$WORK/db-$target"
  rm -rf "$dbdir"; mkdir -p "$dbdir"

  local out exit_code
  out=$("$wt" run --dir "$dbdir::/data" "$wasm" --dir /data 2>&1); exit_code=$?
  printf '%s\n' "$out" >> "$LOG"
  if [ $exit_code -eq 0 ] && printf '%s' "$out" | grep -qE '^([0-9]+)/\1 passed'; then
    report_pass "$label" "$(printf '%s' "$out" | tail -1)  [wasmtime $("$wt" --version | awk '{print $2}')]"
    return 0
  fi
  report_fail "$label" "exit $exit_code: $(printf '%s' "$out" | grep -m1 FAIL || printf '%s' "$out" | tail -1)"
  return 1
}

run_node_wasi() {
  local target=wasm32-wasip1
  local log="$WORK/build-node-wasi.log" wasm="$WORK/$target.wasm"

  build_wasm "$target" "$log" || {
    report_fail node-wasi "build failed: $(grep -m1 -E '^error' "$log" | cut -c1-120)"
    return 1
  }
  fetch_wasm "$target" "$wasm" "$log" || {
    report_fail node-wasi "could not fetch $BIN.wasm"
    return 1
  }

  local dbdir="$WORK/db-node-wasi"
  rm -rf "$dbdir"; mkdir -p "$dbdir"

  local out exit_code
  out=$(node "$REPO_ROOT/xtask/m5-node-wasi.mjs" "$wasm" /data "$dbdir" 2>&1); exit_code=$?
  printf '%s\n' "$out" >> "$LOG"
  if [ $exit_code -eq 0 ] && printf '%s' "$out" | grep -qE '^([0-9]+)/\1 passed'; then
    report_pass node-wasi "$(printf '%s' "$out" | tail -1)  [node $(node --version)]"
    return 0
  fi
  report_fail node-wasi "exit $exit_code: $(printf '%s' "$out" | grep -m1 FAIL || printf '%s' "$out" | tail -1)"
  return 1
}

run_node_napi() {
  local log="$WORK/build-node-napi.log" addon="$WORK/dxdb_node.node"

  remote_cargo build --package "$NODE_CRATE" --features node-addon --release > "$log" 2>&1 || {
    report_fail node-napi "build failed: $(grep -m1 -E '^error' "$log" | cut -c1-140)"
    return 1
  }
  local dir
  dir=$(remote_build_dir "$log")
  # cargo replaces `-` with `_` in artifact names: dxdb-node -> libdxdb_node.so.
  local artifact
  artifact="lib$(printf '%s' "$NODE_CRATE" | tr '-' '_').so"
  if [ -z "$dir" ] || ! fetch "$dir/target/release/$artifact" "$addon" "$log"; then
    report_fail node-napi "could not fetch the addon (see $log)"
    return 1
  fi

  local dbdir="$WORK/db-node-napi"
  rm -rf "$dbdir"; mkdir -p "$dbdir"

  local out exit_code
  out=$(node "$REPO_ROOT/xtask/m5-node-napi.mjs" "$addon" "$dbdir" 2>&1); exit_code=$?
  printf '%s\n' "$out" >> "$LOG"
  if [ $exit_code -eq 0 ] && printf '%s' "$out" | grep -qE '^([0-9]+)/\1 passed'; then
    report_pass node-napi "$(printf '%s' "$out" | tail -1)  [node $(node --version)]"
    return 0
  fi
  report_fail node-napi "exit $exit_code: $(printf '%s' "$out" | grep -m1 FAIL || printf '%s' "$out" | tail -1)"
  return 1
}

# --------------------------------------------------------------------- main --
printf 'M5 conformance: %s\n' "$WORK"
printf '  node %s   wasmtime %s\n' \
  "$(node --version 2>/dev/null || echo 'missing')" \
  "$( { wt=$(wasmtime_bin) && "$wt" --version | awk '{print $2}'; } 2>/dev/null || echo 'missing')"

IFS=',' read -r -a wanted <<< "$TARGETS"
for t in "${wanted[@]}"; do
  case "$t" in
    native)          run_native ;;
    wasmtime-wasip1) run_wasmtime wasmtime-wasip1 wasm32-wasip1 wasm32-wasip1 ;;
    wasmtime-wasip2) run_wasmtime wasmtime-wasip2 wasm32-wasip2 wasm32-wasip2 ;;
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
  [ $KEEP -eq 1 ] && printf '  kept: %s\n' "$WORK"
  exit 1
fi

if [ ${#PASSED_TARGETS[@]} -eq 0 ]; then
  printf '%sM5 conformance FAILED%s: no target ran\n' "$RED" "$RESET"
  exit 1
fi

printf '%sM5 conformance OK%s: %d passed, 0 failed, %d skipped\n' \
  "$GREEN" "$RESET" "${#PASSED_TARGETS[@]}" "${#SKIPPED_TARGETS[@]}"
printf '  log: %s\n' "$LOG"
if [ $KEEP -eq 1 ]; then
  printf '  kept: %s\n' "$WORK"
else
  rm -rf "$WORK"
fi
exit 0
