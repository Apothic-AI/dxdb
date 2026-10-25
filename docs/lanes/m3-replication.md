# Lane brief: M3 replication and attachments

You own `crates/dxdb-replication/` and `crates/dxdb-attachments/`.

## Read first

1. `docs/PLAN.md` sections 4.3 (the replication protocol paragraph), 6 M3, 7.
2. `docs/GATES.md` tier 3.
3. `docs/WORKTREES.md` — you own only those two crate paths.

Upstream at `~/Code/rxdb` (pin `3b763ee17`).

## Why this gets its own lane

PLAN.md 4.3: "Getting this wrong breaks interop with every RxDB server
implementation, so it gets its own conformance suite." The protocol is
checkpoint-stack based and is not something to improvise. Port it.

## What to build

Port `src/replication-protocol/*` (2,121 lines):

- **Downstream**: pulls `(documents, checkpoint)`.
- **Upstream**: pushes `{assumedMasterState, newDocumentState}` and receives back
  the master states **for conflicts only**.
- **Conflict resolution**: a user-supplied handler. The default
  (`default-conflict-handler.ts`) drops the fork and keeps the master. Reproduce
  that default exactly.
- **Meta instance** (`meta-instance.ts`): persists per-replication checkpoints.
- **The state machine**: pause/resume/cancel/retry, and the replication state that
  observably transitions (upstream exposes this through `replicationState`).

Also port `dxdb-attachments`: the attachment data model and digest
(`src/plugins/attachments/`, and the attachment parts of
`src/types/rx-attachment.d.ts`).

## Serialisation rules that are wire behaviour

- Checkpoints are compared and persisted as **ordered JSON with JavaScript number
  formatting**. Use `dxdb-json::canonical_bytes` / `to_js_json`. A checkpoint that
  round-trips through `serde_json`'s float writer stops equalling the JS one.
- Document states crossing the wire use the same encoding.
- Never reorder keys in a checkpoint object.

## Your gate

1. Port `src/plugins/test-utils/replication-base-test-suite.ts` (454 lines, the
   shared suite every replication plugin runs) **once**, and run it against
   in-memory and filesystem endpoints. In-memory exists after lane D lands
   `dxdb-memory`; until then build against `dxdb-storage`'s traits with a test
   double and switch over when `dxdb-memory` is available.
2. A **two-process test**: dxdb -> HTTP -> dxdb, asserting document convergence and
   checkpoint equality. Keep it hermetic: bind an ephemeral port, no external
   network, no fixed port number.
3. Consume `testdata/vectors/replication-transcripts.json` when lane C publishes it
   and assert our checkpoint and resolved-document output matches.

```bash
make check && make clippy && make test && make wasm
```

All exit 0. Every crate builds for `wasm32-unknown-unknown`: the replication
protocol must not depend on native sockets. The HTTP transport belongs to the
caller, behind a trait.

## Constraints

- No `tokio` in this crate. Use `futures`; the caller supplies the executor.
- No `unsafe`.
- Do not depend on a concrete storage crate. Depend on `dxdb-storage` traits.

## Reporting

Report gates with exit statuses, the ported-suite test count, and the two-process
test's convergence assertions. If the ported suite has cases whose upstream
behaviour depends on a JS-only facility, say which and how you handled it.
