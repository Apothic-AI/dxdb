# Lane brief: M5 WASI, Node, filesystem, and SQLite storages

You own `crates/dxdb-wasi/`, `crates/dxdb-node/`,
`crates/storages/dxdb-filesystem/`, `crates/storages/dxdb-sqlite/`.

## Read first

1. `docs/PLAN.md` sections 3.4 (the WASM capability matrix), 5.1-5.2 (bindings and
   feature use), 5.4 (storage availability), 6 M5, 7.4 (build discipline).
2. `docs/GATES.md`, `docs/WORKTREES.md`, `docs/TOOLCHAIN.md`.

Upstream at `~/Code/rxdb` (pin `3b763ee17`).

## The capability matrix is the spec

PLAN.md 3.4, verified by probe on the pinned toolchain:

| Target | Status on 1.95.0 | Your use |
| --- | --- | --- |
| `wasm32-unknown-unknown` | OK | primary, must stay green |
| `wasm32-wasip1` | OK | WASI 0.1 hosts (Node `node:wasi`, Wasmtime) |
| `wasm32-wasip1-threads` | OK, shared memory confirmed | threaded WASI |
| `wasm32-wasip2` | OK | WASI 0.2 component model |
| `wasm32-wasip3` | **nightly only** | WASI 0.3 native async. **Do not ship; track it.** |

`wasm32-wasip1` and `wasm32-wasip2` targets are not installed on the shared box
yet. Provisioning them mutates a shared, persistent volume
(`/data/rustup`), so **coordinate before running `rustup target add`**: report that
you need them rather than doing it silently, and never change the box's default
toolchain.

## What to build

### `dxdb-wasi`

- A WIT world, exported via `wit-bindgen` 0.63.0, building a `wasm32-wasip2`
  component.
- A CLI entry point so the conformance suite can run from a `.wasm` under
  Wasmtime with a preopened directory. That is M5's gate.
- Track `wasm32-wasip3` (nightly, `async` feature) but do not make it a
  dependency of anything that ships.

### `dxdb-node`

- napi-rs surface for Node.
- `wasm32-wasip1` as the install-free fallback.
- M5's gate is running the conformance suite "under Node from both the napi build
  and the WASI build", so both paths need to be real.

### `dxdb-filesystem`

- **`redb` on native** (`4.4.0`, pure Rust, MVCC, crash safe). PLAN.md 5.4 and
  CLEANROOM 5.4: multi-process safety comes from the engine, not from hand-rolled
  locking, and there is a documented JSON-file export path for portability.
- OPFS-backed on wasm is lane F's territory; coordinate rather than duplicating.
- `multiInstance` safety is part of the ported conformance suite. Make it pass.

### `dxdb-sqlite`

- `rusqlite` 0.41 on native; `sqlite-wasm-rs` 0.6.2 on wasm. `rusqlite`'s default
  features already include `ffi-sqlite-wasm-rs`, so it swaps FFI on wasm32 without
  a fork; there is also a `wasm32-wasi-vfs` feature for WASI hosts.
- OPFS from SQLite needs the `sqlite-wasm-vfs` SAH-pool, which is **worker-only**.
  `rusqlite` defaults to the in-memory VFS on wasm, so OPFS must be installed
  explicitly — document that.
- Your own schema DDL, prepared statements, and a `SqliteDriver` adapter trait with
  implementations for both. Per CLEANROOM 5.3 this is the clean-room crate's
  design; here you are porting the open-source `storage-sqlite` behaviour against
  that shape.

## Rules

- No `unsafe` beyond what the bindings require, and each use gets a comment saying
  why it is unavoidable and what the invariant is.
- The `wasm32-unknown-unknown` build must stay green even for crates that only make
  sense elsewhere. PLAN.md 4.1: "CI enforces it, including for crates that only
  make sense elsewhere, because a dependency that breaks wasm deep in the tree is
  expensive to discover late."
- Do not add a dependency to `[workspace.dependencies]` (leader-owned). Add it to
  your crate.
- Disk: SQLite plus `wasm-bindgen` is 2-4 GB per build tree. Check `make headroom`
  before a cold build; the box is shared.

## Your gate

```bash
make check && make clippy && make test && make wasm
```

plus:

- conformance suite under Wasmtime from the `.wasm` component with a preopened
  directory,
- conformance suite under Node from the napi build,
- conformance suite under Node from the WASI build.

Report all three with exit statuses and the Wasmtime/Node versions.

## Reporting

Report gates with exit statuses, which WASI targets you built and which needed
provisioning, and any capability you had to gate. If `wasm32-wasip1`/`wasip2` are
missing on the box, say so explicitly and do not provision them yourself.
