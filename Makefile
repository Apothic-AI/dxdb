# Build discipline: the only supported way to compile this workspace.
#
# Everything here goes to the shared Fly build server via cargo-remote-3000.
# Read docs/PLAN.md section 7.4 and docs/TOOLCHAIN.md before editing.
#
#   -d is mandatory. Without it cargo-remote runs `rustup default stable`, which
#   silently re-points every project sharing the box. The box has exactly one
#   toolchain, 1.95.0.
#
# The box is shared with other projects: 8 cores, [build] jobs = 4 in the
# shared cargo config. Do not raise CARGO_BUILD_JOBS, do not clean anything that
# is not ours, do not stop a busy machine.

TOOLCHAIN := 1.95.0
CARGO     := cargo remote-3000 -r fly -d $(TOOLCHAIN)
MACHINE   := 87477e0cd01748
APP       := open-surrealdb-ds-builder

# Match by executable name. `pgrep -f rustc` also matches the pgrep's own
# command line, so a -f guard reports "busy" forever and a -f count is
# inflated. Keep the predicate inline: a make variable loses the quoting.
BUSY := 'ps -eo comm= | grep -qxE "rustc|cargo" && echo BUSY || echo IDLE'

# Concurrent remote compiles from this repo are serialised with a lock file so
# worktrees do not thrash the shared box's 4 build jobs. See docs/WORKTREES.md.
LOCK := /tmp/dxdb-remote-build.lock

.PHONY: help check clippy test wasm fmt fmt-check vectors vectors-check status busy headroom lock remote-up remote-status prune-ours gate-storage gate-query-correctness

help:
	@echo "check clippy test wasm fmt fmt-check vectors vectors-check status busy headroom lock remote-up remote-status prune-ours gate-storage gate-query-correctness"

check:
	@flock $(LOCK) $(CARGO) check --workspace --all-targets

clippy:
	@flock $(LOCK) $(CARGO) clippy --workspace --all-targets -- --deny warnings

test:
	@flock $(LOCK) $(CARGO) test --workspace

wasm:
	@flock $(LOCK) $(CARGO) check --workspace --target wasm32-unknown-unknown

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

# Regenerate the cross-language oracle vectors from upstream RxDB in Node.
vectors:
	node xtask/vectors/generate.mjs

busy:
	@ssh fly-builder $(BUSY)

headroom:
	@ssh fly-builder 'df -h /data | tail -1'

lock:
	@flock -n $(LOCK) echo "build lock acquired" || echo "LOCKED: another dxdb remote build is running"

remote-up:
	flyctl machine start $(MACHINE) -a $(APP)

remote-status:
	flyctl machine list -a $(APP)

# Prune only build trees that are provably ours (Cargo.toml mentions dxdb).
# Never touch another project's tree, and never clean /data/cargo or /data/rustup.
prune-ours:
	@ssh fly-builder 'for f in $$(grep -l dxdb /data/builds/*/Cargo.toml 2>/dev/null); do d=$$(dirname $$f); echo "removing $$d"; rm -rf "$$d"; done; df -h /data | tail -1'

# Acceptance gates named in docs/GATES.md. These are the commands a lane cites.
gate-storage:
	@flock $(LOCK) $(CARGO) test -p dxdb-testkit --test storage_implementations

gate-query-correctness:
	@flock $(LOCK) $(CARGO) test -p dxdb-testkit --test query_correctness

vectors-check:
	@node xtask/vectors/generate.mjs --check

status:
	@python3 xtask/status.py
