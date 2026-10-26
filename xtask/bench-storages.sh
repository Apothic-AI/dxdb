#!/usr/bin/env bash
#
# xtask/bench-storages.sh -- run the storage benchmarks and capture the metadata
# that docs/BENCHMARKS.md requires.
#
# BENCHMARKS.md's method section requires every recorded number to carry: the box,
# the toolchain, whether the box was idle at run start, the date, and the command
# that produced it. This script collects those rather than leaving them to be
# remembered, because a number without them is not reproducible and a number that is
# not reproducible does not get recorded.
#
# Why the idle check matters and is not optional: the build box is shared. A run
# taken while another project compiles is not a measurement of the storage, and
# BENCHMARKS.md says so explicitly. This script records `idle_at_start` truthfully
# and warns loudly when it is false -- it does not refuse to run, because a
# non-idle run is still useful for spotting a large regression, but it marks the
# artifact so nobody later cites it as a baseline.
#
# Usage:
#   xtask/bench-storages.sh                 # both storages, 5 runs each
#   xtask/bench-storages.sh --runs 3        # fewer runs (faster, noisier)
#   xtask/bench-storages.sh --only sqlite
#   xtask/bench-storages.sh --out DIR       # where to put the JSON artifacts
#
# Artifacts: <out>/<storage>.json plus <out>/metadata.json summarising the run.
#
set -uo pipefail

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$REPO_ROOT" || exit 1

RUNS=5
ONLY=""
OUT=${DXDB_BENCH_OUT:-bench-results}

while [ $# -gt 0 ]; do
  case "$1" in
    --runs) RUNS=$2; shift 2 ;;
    --only) ONLY=$2; shift 2 ;;
    --out)  OUT=$2;  shift 2 ;;
    -h|--help) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

# The per-project build directory on the box. cargo-remote keys it on the absolute
# local project path; this is that value for this worktree.
REMOTE_BUILD_DIR=${DXDB_BENCH_REMOTE_DIR:-/data/builds/88b6632c01641f9a}

mkdir -p "$OUT" || exit 2

# ------------------------------------------------------------------ metadata --
# The route mirrors the Makefile so this works through the API-proxy fallback.
ssh_box() {
  local route
  route=$(DXDB_FORCE_ROUTE=${DXDB_FORCE_ROUTE:-} xtask/remote-route.sh 2>/dev/null || true)
  case "$route" in
    proxy:*) ssh -o ConnectTimeout=10 fly-builder-proxy "$@" ;;
    *)       ssh -o ConnectTimeout=10 fly-builder "$@" ;;
  esac
}

# Pushes the working tree to the box's build directory for this project.
#
# cargo-remote does the same thing, but it also *builds* whatever command it is
# given; here the sync and the run are separate so the build and the measurement can
# be reasoned about independently. target/ is excluded so a 2 GB tree is not copied.
sync_tree() {
  local route
  route=$(DXDB_FORCE_ROUTE=${DXDB_FORCE_ROUTE:-} xtask/remote-route.sh 2>/dev/null || true)
  case "$route" in
    proxy:*)
      rsync -az --delete --exclude 'target/' --exclude '.git/' --exclude 'graft/' \
        --exclude 'node_modules/' \
        -e "ssh -p ${route#proxy:}" \
        ./ root@127.0.0.1:"$REMOTE_BUILD_DIR/" >/dev/null 2>&1 ;;
    *)
      rsync -az --delete --exclude 'target/' --exclude '.git/' --exclude 'graft/' \
        --exclude 'node_modules/' \
        -e "ssh -p 2222" \
        ./ root@fly-builder:"$REMOTE_BUILD_DIR/" >/dev/null 2>&1 ;;
  esac
}

# The same predicate the Makefile's `busy` target uses: match executable names,
# because `pgrep -f rustc` also matches the pgrep's own command line.
IDLE_STATE=$(ssh_box 'ps -eo comm= | grep -qxE "rustc|cargo" && echo BUSY || echo IDLE' 2>/dev/null || echo UNKNOWN)
LOADAVG=$(ssh_box 'cat /proc/loadavg' 2>/dev/null || echo "unknown")
NPROC=$(ssh_box 'nproc' 2>/dev/null || echo "unknown")
TOOLCHAIN=$(ssh_box 'source ~/.cargo/env 2>/dev/null; rustc --version' 2>/dev/null || echo "unknown")
DATE=$(date -u +%Y-%m-%d)

IDLE_AT_START=false
[ "$IDLE_STATE" = "IDLE" ] && IDLE_AT_START=true
IDLE_EVIDENCE="make busy -> ${IDLE_STATE}; /proc/loadavg '${LOADAVG}'; nproc ${NPROC}"
CPU=$(ssh_box 'grep -m1 "model name" /proc/cpuinfo | cut -d: -f2 | sed "s/^ *//"' 2>/dev/null || echo unknown)

cat <<EOF
bench-storages
  date:       $DATE
  toolchain:  $TOOLCHAIN
  box:        fly open-surrealdb-ds-builder 87477e0cd01748 (${NPROC} cores, ${CPU}, shared)
  runs on:    the box (not locally)
  idle:       $IDLE_STATE  (load: $LOADAVG)
  runs:       $RUNS
  output:     $OUT
EOF

if [ "$IDLE_STATE" != "IDLE" ]; then
  printf '\nWARNING: the box was not idle. These numbers are marked idle_at_start=false\n'
  printf 'and must not be recorded in docs/BENCHMARKS.md as a baseline.\n\n'
fi

export DXDB_BENCH_IDLE=$([ "$IDLE_AT_START" = true ] && echo yes || echo no)
export DXDB_BENCH_IDLE_EVIDENCE="$IDLE_EVIDENCE"
export DXDB_BENCH_DATE="$DATE"

# ---------------------------------------------------------------- execution --
FAILED=0
# The benchmark RUNS ON THE BOX, not here.
#
# This is the correction to a real methodology error: the first version of this
# script collected the box's idle state and load average for the metadata, then ran
# `cargo run` locally. The artifact therefore claimed the Fly box while describing a
# 13th-gen Intel laptop, which is not reproducible and cannot be compared to any
# other run of this file. Metadata is only honest if it describes the machine the
# numbers came from, so the build and the run both happen remotely and only the JSON
# comes back.
REMOTE_DIR=""
remote_run() { # remote_run <bin> <label>
  local bin=$1 label=$2
  local remote_out="/data/bench-results/$label.json"
  local cmd="cd $REMOTE_BUILD_DIR && \\
/root/.cargo/bin/cargo run --release --package dxdb-bench --bin $bin -- --runs $RUNS --out $remote_out"

  echo "--- $bin (on the box) ---"
  # Sync the current tree first: /data/builds/<hash> may hold an older revision, and
  # a benchmark of stale source is not a benchmark of this code.
  sync_tree || { echo "FAILED: could not sync sources to the box" >&2; FAILED=1; return 1; }
  # `mkdir -p` first: the build root exists but the results dir does not.
  ssh_box "mkdir -p /data/bench-results" || { FAILED=1; return 1; }
  # DXDB_BENCH_* are forwarded so the artifact carries the box's own idle state.
  if ! ssh_box "export DXDB_BENCH_IDLE='$([ "$IDLE_AT_START" = true ] && echo yes || echo no)'; \
      export DXDB_BENCH_IDLE_EVIDENCE='$(printf '%s' "$IDLE_EVIDENCE" | sed "s/'/\\x27/g")'; \
      export DXDB_BENCH_DATE='$DATE'; \
      export DXDB_BENCH_COMMAND='$(printf '%s' "$cmd" | sed "s/'/\\x27/g")'; \
      $cmd" 2>&1; then
    echo "FAILED: $bin" >&2
    FAILED=1
    return 1
  fi
  # Fetch the artifact back. `ssh_box cat` rather than scp: it works identically
  # over the direct route and the API-proxy fallback, so there is one path instead of
  # two and no dependency on which port the proxy happened to bind.
  if ! ssh_box "cat $remote_out" > "$OUT/$label.json" 2>/dev/null; then
    echo "FAILED: could not fetch $remote_out" >&2
    FAILED=1
    return 1
  fi
  [ -s "$OUT/$label.json" ] || { echo "FAILED: empty artifact for $label" >&2; FAILED=1; return 1; }
  return 0
}

case "$ONLY" in
  "")        remote_run bench-filesystem filesystem; remote_run bench-sqlite sqlite ;;
  filesystem) remote_run bench-filesystem filesystem ;;
  sqlite)     remote_run bench-sqlite sqlite ;;
  *) echo "unknown --only value: $ONLY" >&2; exit 2 ;;
esac

# A compact metadata sidecar so a CI diff of this directory shows both when the run
# happened and whether it was trustworthy.
cat > "$OUT/metadata.json" <<EOF
{
  "date": "$DATE",
  "toolchain": "$TOOLCHAIN",
  "box": "fly open-surrealdb-ds-builder 87477e0cd01748 (${NPROC} cores, ${CPU}, shared)",
  "ran_on_remote": true,
  "idle_state": "$IDLE_STATE",
  "idle_at_start": $IDLE_AT_START,
  "idle_evidence": "$(printf '%s' "$IDLE_EVIDENCE" | sed 's/"/\\"/g')",
  "runs": $RUNS,
  "loadavg": "$(printf '%s' "$LOADAVG" | sed 's/"/\\"/g')"
}
EOF

echo
if [ $FAILED -ne 0 ]; then
  echo "bench-storages FAILED: see above" >&2
  exit 1
fi
echo "bench-storages OK: artifacts in $OUT"
ls -1 "$OUT"
if [ "$IDLE_AT_START" != true ]; then
  echo
  echo "NOTE: idle_at_start=false. Do not record these numbers as a baseline."
fi
exit 0
