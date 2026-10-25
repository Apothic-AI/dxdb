# Worktree and lane layout

The work is too large for one agent session and too coupled for unstructured
parallelism. This document defines how lanes are separated on disk, what each
lane may touch, and the rules that keep parallel work from corrupting itself.

## Layout

Worktrees live outside the repository, one per lane:

```
/home/bitnom/Code/
  dxdb/                     # master; never edited by a lane
  dxdb-worktrees/
    m0-foundation/          # lane A
    m1-schema-query/        # lane B
    m1-vectors/             # lane C (oracle vectors)
    m2-core/                # lane D
    m3-replication/         # lane E
    m4-browser/             # lane F
    m5-wasi-node/           # lane G
    m6-plugins/             # lane H
    m7-cleanroom/           # lane I
```

Each lane is a git worktree on its own branch (`lane/<name>`). A lane commits
and pushes its branch. The leader merges to `master`. A lane never merges.

Why worktrees and not clones: the shared build box keys its per-project build
directory on the absolute local project path
(`cargo-remote-3000`'s `remote_build_root` hashes `project_root`), so a clone and
a worktree are equivalent in remote cost. A worktree is equivalent on the remote
side and cheaper locally, and it keeps one object store.

## File ownership: disjoint by construction

Parallel agents corrupt work by editing the same file, not by disagreeing about
architecture. Each lane therefore owns a disjoint set of paths, declared up
front. The leader owns the shared contract files, and **no lane edits them**:

Leader-owned (change only via the leader, by request):

```
Cargo.toml                 # workspace: members, workspace.dependencies, profile
Makefile                   # build discipline
rust-toolchain.toml
.cargo-remote-3000.toml
.github/workflows/
LICENSE, NOTICE, README.md
docs/PLAN.md, docs/CLEANROOM.md, docs/TOOLCHAIN.md, docs/WORKTREES.md
docs/STATUS.md
```

Lane-owned paths:

| Lane | Branch | Owns |
| --- | --- | --- |
| A `m0-foundation` | `lane/m0-foundation` | `crates/dxdb-jsnum/`, `crates/dxdb-jsstr/`, `crates/dxdb-json/`, `xtask/` (except `xtask/vectors/`) |
| B `m1-schema-query` | `lane/m1-schema-query` | `crates/dxdb-schema/`, `crates/dxdb-query/` |
| C `m1-vectors` | `lane/m1-vectors` | `xtask/vectors/`, `testdata/vectors/` |
| D `m2-core` | `lane/m2-core` | `crates/dxdb-core/`, `crates/dxdb-storage/`, `crates/dxdb-reactivity/`, `crates/storages/dxdb-memory/`, `crates/dxdb-testkit/` |
| E `m3-replication` | `lane/m3-replication` | `crates/dxdb-replication/`, `crates/dxdb-attachments/` |
| F `m4-browser` | `lane/m4-browser` | `crates/dxdb-js/`, `crates/storages/dxdb-indexeddb/`, `crates/storages/dxdb-opfs/`, `crates/storages/dxdb-localstorage/`, `crates/storages/dxdb-remote/`, `demo/` |
| G `m5-wasi-node` | `lane/m5-wasi-node` | `crates/dxdb-wasi/`, `crates/dxdb-node/`, `crates/storages/dxdb-filesystem/`, `crates/storages/dxdb-sqlite/` |
| H `m6-plugins` | `lane/m6-plugins` | `crates/plugins/` |
| I `m7-cleanroom` | `lane/m7-cleanroom` | `crates/cleanroom/`, `specs/` |

Adding a crate means the leader adds the path to the workspace `members` glob
(it already covers `crates/*`, `crates/storages/*`, `crates/plugins/*`), so a
lane can create its own crate directories without touching `Cargo.toml`. That is
deliberate: it removes the most common parallel conflicts. A lane that needs a
new *shared* dependency adds it to its own crate's `Cargo.toml`; adding to
`[workspace.dependencies]` is a leader action.

## Remote builds: the shared-box rules still apply, per lane

Each worktree is a distinct project path, therefore a distinct remote build
directory (~2-4 GB per tree with SQLite and `wasm-bindgen` in it). With nine
lanes that is a real disk risk. Rules:

- **One remote compile at a time per box.** `make check`, `make test`,
  `make clippy`, and `make wasm` all take `flock /tmp/dxdb-remote-build.lock`.
  Concurrent invocations from different worktrees queue instead of thrashing
  the box's 4 build jobs, and the failure mode is waiting, not corruption.
- **Check headroom before the first cold build in a new worktree**:
  `make headroom`. Under ~10% free means ask before building, per PLAN.md 7.4.
- **Prune only proven-ours trees**: `make prune-ours` greps for `dxdb` in each
  tree's `Cargo.toml` before removing anything.
- **Never stop the machine while a remote build is running.** `make busy` first.
- **Iterate with `check`/`clippy`.** Run `test` deliberately, not in a loop.

## Merge protocol

1. A lane works on `lane/<name>`, commits in small units, and pushes.
2. When a lane reaches its gate (the command in its brief), it reports the exact
   command, the exit status, and the evidence.
3. The leader reviews the diff, runs the gate itself on `master` after merge,
   and records the outcome in `docs/STATUS.md`.
4. Cross-lane dependencies are declared explicitly in each brief. A lane never
   reaches into another lane's directory: it consumes published crate APIs only.

## Merge conflicts: the expected kinds and their resolution

Parallel lanes on one Rust workspace produce exactly three classes of conflict.
Knowing them in advance makes each one mechanical.

**1. `Cargo.lock`.** Guaranteed, because every lane adds dependencies. It is a
generated file, so never hand-merge it. Resolve by taking either side and letting
the build regenerate it:

```bash
git checkout --theirs Cargo.lock   # or --ours; it does not matter
make check                          # the shared box regenerates and returns a correct lock
git add Cargo.lock
```

`make check` copies the regenerated `Cargo.lock` home from the build server, so the
remote build is what fixes it. A hand-resolved lock file is a source of silent
version drift and is never worth the effort.

**2. `Cargo.toml`.** Should not happen: it is leader-owned, and the member globs
mean a lane adds crates without editing it. The two exceptions are the one-line
`crates/plugins/*` and `crates/cleanroom/*` glob additions, each made once, by the
lane that creates the first crate in the group. If two lanes touch this file, the
leader resolves it; a lane does not.

**3. Additive collisions in shared docs.** Two lanes appending to `docs/STATUS.md`
or a lane appending to `docs/GATES.md`. `docs/STATUS.md` is generated by
`make status` from the upstream checkout, so its row set never conflicts — only the
human-maintained State/Evidence columns can, and those are leader-edited. If a lane
sees a conflict in a file it does not own, it stops and reports rather than
guessing, because guessing at a shared file is how the plan stops matching the code.

## Disk and contention budget for eight lanes

`cargo-remote-3000` keys its per-project build directory on the absolute local
project path, so each of the eight worktrees gets its own cold dependency graph on
the shared box. A workspace with SQLite and `wasm-bindgen` is 2-4 GB per tree, so
the fleet can occupy 16-32 GB. The volume had 136 GB free when the fleet started.

Consequences, checked rather than assumed:

- The `flock` in the Makefile serialises compiles, so eight lanes do not thrash the
  box's four build jobs. The cost is queueing, which is the correct cost.
- `make headroom` before a lane's first cold build. Under ~10% free means report and
  wait, not proceed.
- `make prune-ours` removes only trees whose `Cargo.toml` mentions dxdb, after a
  lane merges and no longer needs its tree.

## The rule that keeps this honest

A lane reporting "done" is a claim, not evidence. The leader's merge gate is
the command in the lane brief, run by the leader on the merged result. If the
gate is not green on `master`, the lane is not done, whatever the lane says.
