#!/usr/bin/env bash
# Build dxdb-js for wasm32-unknown-unknown and generate the JS/TS binding.
#
# Two rules this script exists to enforce:
#
#  1. Compilation goes through the workspace Makefile / cargo-remote-3000 to the
#     shared Fly build server. Nothing compiles locally (docs/PLAN.md 7.4,
#     docs/WORKTREES.md). The remote invocation goes through cargo-remote to the
#     shared box. The Makefile owns the build lock (reentrantly, via
#     xtask/with-build-lock.sh); this script deliberately takes no lock of its own.
#  2. The `wasm-bindgen` CLI version must equal the `wasm-bindgen` crate version.
#     A mismatch does not fail at build time; it traps at instantiation with a
#     message about versions that gives no hint where to look. So we check first.
#
# Usage: ./scripts/build-wasm.sh [--no-remote]
#        --no-remote  reuse an existing target/ artifact (debugging only; it does
#                     not skip the CLI version check or wasm-bindgen generation)

set -euo pipefail

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_DIR="$(cd "${DEMO_DIR}/.." && pwd)"
PKG_DIR="${DEMO_DIR}/pkg"
ARTIFACT="target/wasm32-unknown-unknown/release/dxdb_js.wasm"
TOOLCHAIN="1.95.0"

cd "${REPO_DIR}"

CRATE_VERSION="$(sed -n 's/^wasm-bindgen = "=\(.*\)"$/\1/p' crates/dxdb-js/Cargo.toml)"
if [ -z "${CRATE_VERSION}" ]; then
  echo "error: could not read the pinned wasm-bindgen version from crates/dxdb-js/Cargo.toml" >&2
  exit 1
fi

if ! command -v wasm-bindgen >/dev/null 2>&1; then
  echo "error: wasm-bindgen CLI not on PATH." >&2
  echo "  install the exact version: https://github.com/rustwasm/wasm-bindgen/releases/tag/${CRATE_VERSION}" >&2
  exit 1
fi

CLI_VERSION="$(wasm-bindgen --version | awk '{print $2}')"
if [ "${CLI_VERSION}" != "${CRATE_VERSION}" ]; then
  echo "error: wasm-bindgen CLI ${CLI_VERSION} != crate ${CRATE_VERSION}" >&2
  echo "  the generated glue and the module's custom sections would disagree," >&2
  echo "  and the module would trap at instantiation with a confusing message." >&2
  echo "  fix: install wasm-bindgen ${CRATE_VERSION} and put it first on PATH." >&2
  exit 1
fi

if [ "${1:-}" != "--no-remote" ]; then
  echo "== building ${ARTIFACT} on the shared Fly box =="
  # Two things this invocation must get right:
  #
  # 1. `--copy-back` takes a **directory relative to target/**, not a file. It
  #    appends a trailing slash, so a file path fails with "Not a directory".
  # 2. `-p dxdb-js` must come after the cargo subcommand, because cargo-remote
  #    parses a leading `-p` as its own `--ssh-port` and dies with "invalid digit
  #    found in string".
  #
  # No outer `flock`: the Makefile targets take the reentrant build lock
  # themselves, and wrapping them is what deadlocked the fleet. See
  # xtask/with-build-lock.sh.
  cargo remote-3000 -r fly -d "${TOOLCHAIN}" \
    --copy-back=wasm32-unknown-unknown/release \
    build --release --target wasm32-unknown-unknown -p dxdb-js
fi

if [ ! -f "${ARTIFACT}" ]; then
  echo "error: ${ARTIFACT} missing after build" >&2
  exit 1
fi

echo "== wasm-bindgen ${CLI_VERSION} (--target web) =="
rm -rf "${PKG_DIR}"
# `--remove-name-section` drops the `name` custom section (every Rust symbol
# name). Measured on the tranche-1 baseline: -1,174 B brotli. It costs readable
# stack traces in a trap, which is an acceptable trade for a shipped bundle and
# the flag is trivially dropped for a debug build.
wasm-bindgen "${ARTIFACT}" \
  --target web \
  --out-dir "${PKG_DIR}" \
  --out-name dxdb_js \
  --remove-name-section

# The hand-written .d.ts a real app compiles against is authored in this lane and
# will live at pkg/dxdb_js.d.ts once the surface is real. Until then the
# generated one is what the page uses; renaming the file here would be a lie.

echo "== sizes =="
RAW_BYTES="$(stat -c %s "${PKG_DIR}/dxdb_js_bg.wasm")"
# `--quality=11` and `--best` are the same setting and brotli rejects both
# together ("quality already set") while still writing to stdout, which yields a
# plausible-looking but wrong byte count. Use exactly one spelling.
BROTLI_BYTES="$(brotli -q 11 -c "${PKG_DIR}/dxdb_js_bg.wasm" | wc -c)"
GZIP_BYTES="$(gzip -9 -c "${PKG_DIR}/dxdb_js_bg.wasm" | wc -c)"
printf 'raw            %8d bytes  %7.1f KiB\n' "${RAW_BYTES}" "$(echo "${RAW_BYTES}/1024" | bc -l)"
printf 'brotli q11     %8d bytes  %7.1f KiB\n' "${BROTLI_BYTES}" "$(echo "${BROTLI_BYTES}/1024" | bc -l)"
printf 'gzip -9        %8d bytes  %7.1f KiB  (reference only)\n' "${GZIP_BYTES}" "$(echo "${GZIP_BYTES}/1024" | bc -l)"

# PLAN.md 5.3: dxdb-js core only, no storages = 250 KiB brotli.
BUDGET_BYTES=$((250 * 1024))
printf 'core budget    %8d bytes  %7.1f KiB\n' "${BUDGET_BYTES}" "$(echo "${BUDGET_BYTES}/1024" | bc -l)"
if [ "${BROTLI_BYTES}" -gt "${BUDGET_BYTES}" ]; then
  printf 'core bundle    OVER BUDGET by %d bytes\n' "$((BROTLI_BYTES - BUDGET_BYTES))"
else
  printf 'core bundle    under budget, %d bytes headroom\n' "$((BUDGET_BYTES - BROTLI_BYTES))"
fi

echo "== wrote ${PKG_DIR} =="
