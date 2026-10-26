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

# Every remote compile goes through this wrapper rather than a bare `flock`.
#
# It is reentrant, because a bare `flock $(LOCK) make check` deadlocked this fleet
# for 75 minutes: the outer flock took the lock, then `make check` ran a nested
# `flock $(LOCK) cargo ...` and waited forever on its own ancestor. flock is not
# reentrant and a blocking wait has no timeout, so one lane's mistake stalled all
# nine while they all looked busy.
#
# Reentrancy is by an inherited marker, so any depth of nesting works. A timeout
# makes a genuinely stuck lock fail loudly. See xtask/with-build-lock.sh for the
# full reasoning.
LOCKED := xtask/with-build-lock.sh

.PHONY: help check clippy test wasm fmt fmt-check vectors vectors-check status busy headroom lock remote-up remote-status remote-repair prune-ours gate-storage gate-query-correctness size audit

help:
	@echo "check clippy test wasm fmt fmt-check vectors vectors-check status busy headroom lock remote-up remote-status remote-repair prune-ours gate-storage gate-query-correctness size audit"

check:
	@$(LOCKED) $(CARGO) check --workspace --all-targets

clippy:
	@$(LOCKED) $(CARGO) clippy --workspace --all-targets -- --deny warnings

test:
	@$(LOCKED) $(CARGO) test --workspace

wasm:
	@$(LOCKED) $(CARGO) check --workspace --target wasm32-unknown-unknown

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

# Repair the shared build box after a machine restart. See docs/TOOLCHAIN.md.
#
# The machine's entrypoint is `sleep infinity` and there is no systemd (pid1 is
# Fly's init), so nothing starts our sshd on port 2222 after a restart. The
# platform sshd on 22 comes up and rejects our key, so the only way in is
# `flyctl machine exec`, which is what this runs. It also restores the
# /root/.cargo and /root/.rustup symlinks, which live on the ephemeral rootfs
# and vanish on every restart even though the toolchains survive on the volume.
#
# Symptom it fixes: all remote builds fail with "connection refused" on 2222, or
# later with "cargo: command not found".
remote-repair:
	flyctl machine exec -a $(APP) --no-container $(MACHINE) /data/bin/ensure-ready

# Prune only build trees that are provably ours (Cargo.toml mentions dxdb).
# Never touch another project's tree, and never clean /data/cargo or /data/rustup.
prune-ours:
	@ssh fly-builder 'for f in $$(grep -l dxdb /data/builds/*/Cargo.toml 2>/dev/null); do d=$$(dirname $$f); echo "removing $$d"; rm -rf "$$d"; done; df -h /data | tail -1'

# Acceptance gates named in docs/GATES.md. These are the commands a lane cites.
# `--` before the package selector: cargo-remote-3000 parses its own flags after
# the cargo subcommand and reads `-p` as --ssh-port, so `test -p X` fails before
# cargo runs. PLAN.md 7.4 records the rule ("`--` before cargo flags when using
# -p"); these two recipes were written without it and were dead targets.
gate-storage:
	@$(LOCKED) $(CARGO) test -- -p dxdb-testkit --test storage_implementations

gate-query-correctness:
	@$(LOCKED) $(CARGO) test -- -p dxdb-testkit --test query_correctness

vectors-check:
	@node xtask/vectors/generate.mjs --check

# The upstream checkout location. Overridable so CI can clone a shallow copy
# instead of depending on a developer's ~/Code/rxdb existing.
RXDB_UPSTREAM ?= $(HOME)/Code/rxdb

status:
	@RXDB_UPSTREAM="$(RXDB_UPSTREAM)" python3 xtask/status.py

# The PLAN.md section 10 completion audit. Every item maps to evidence that is
# present or absent; nothing is inferred. --strict exits nonzero if anything is
# unmet or partial, which is what makes "port complete" falsifiable.
audit:
	@python3 xtask/audit.py

# Enforce a WASM size budget from PLAN.md 5.3, e.g.
#   make size BUDGET_WASM=target/wasm32-unknown-unknown/release/dxdb_js.wasm BUDGET_KIB=250
size:
	@xtask/size-budget.sh "$(BUDGET_WASM)" "$(BUDGET_KIB)"
