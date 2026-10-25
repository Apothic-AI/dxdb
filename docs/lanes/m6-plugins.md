# Lane brief: M6 open-source plugin parity

You own `crates/plugins/`. One crate per upstream plugin, mirroring
`src/plugins/*`.

## Read first

1. `docs/PLAN.md` section 6 M6 — it names the order and the reason for it.
2. `docs/PLAN.md` 4.3 last bullet: hooks and plugin injection, and why Rust cannot
   reproduce prototype mutation.
3. `docs/GATES.md`, `docs/WORKTREES.md`.

Upstream at `~/Code/rxdb` (pin `3b763ee17`). 52 plugin directories, 37,698 lines.

## First: add the workspace member glob

`Cargo.toml` deliberately has **no** `crates/plugins/*` glob yet, because cargo
fails hard when a member glob matches nothing. Adding it is a leader-owned edit, so
either request it or make the single-line change yourself **as your first commit**
with the message `workspace: add the plugins member glob` — nothing else in that
file. Everything after that needs no workspace edit at all.

## Build order

PLAN.md M6, in order of what unblocks users:

1. `attachments` (data model; the crate may already exist from lane E — coordinate)
2. `query-builder`
3. `update`
4. `key-compression`
5. `migration-schema`
6. `migration-storage`
7. `local-documents`
8. `leader-election`
9. `cleanup`
10. `json-dump` / import-export
11. `backup`
12. `crdt`
13. `state`
14. `pipeline`
15. `dev-mode`
16. the validator plugins (`validate-ajv`, `validate-ajv-standalone`,
    `validate-is-my-json-valid`, `validate-z-schema`)
17. the replication adapters (`couchdb`, `graphql`, `websocket`, `webrtc`, plus the
    others once `dxdb-replication` is stable)
18. the reactivity adapters

Do not skip ahead. Each crate lands with its gate green before the next starts, so
the lane always has a working state.

## The hook model (PLAN.md 4.3)

Upstream mutates five prototypes at plugin add time (`src/plugin.ts:35`) and
exposes 24 named hooks (`src/hooks.ts`). Rust has no prototype mutation.
Re-expression is fixed by the plan:

- Plugin registration returns a `PluginHandle` and a registered set, ordered.
- Hooks are typed registrations on a `Hooks` registry owned by the database, with
  explicit ordering and **the same hook names** (`preCreateRxDatabase`,
  `createRxCollection`, `preCreateRxSchema`, ...).
- Entity methods that plugins add (the `RxDocument` ORM-method pattern) become
  extension traits plus a type-indexed map on the document handle. TypeScript's
  generic `OrmMethods` maps to a trait bound.

Do not invent a different plugin model because it is nicer; the names and ordering
are observable and the port is supposed to be a port.

## Per-plugin discipline

For each plugin:

1. Read the upstream plugin's source and its tests.
2. Port behaviour, not structure, where Rust forces a difference — and note the
   difference in the crate's module docs.
3. Port its tests. Each ported test names the upstream file and line.
4. Gate green before moving on.

## Deliberate omissions

Some plugins cannot be ported meaningfully because they exist to bind to a JS
ecosystem: `react`, `flutter`, `electron`, `webmcp`, and the JS framework
reactivity adapters. `docs/PLAN.md` section 10 requires that **every** upstream
module have "a dxdb counterpart or a written, reviewed justification for
omission". So for each omission, write the justification into the crate-level docs
of `crates/plugins/README.md` (create it) with the module name, the reason, and
what a Rust user does instead. An omitted plugin with no justification is an
incomplete milestone, not a shortcut.

Also note: `test-utils` is not a plugin to port — its ergonomics live in
`dxdb-testkit` (lane D).

## Rules

- Each crate builds for `wasm32-unknown-unknown`. Some genuinely cannot (anything
  filesystem- or network-specific); those get a feature gate, not a broken build.
- No `unsafe`.
- Do not depend on a concrete storage crate; depend on `dxdb-storage` traits.
- One crate per plugin. Do not merge plugins into a `dxdb-plugins` monolith.

## Your gate

```bash
make check && make clippy && make test && make wasm
```

plus the ported test corpus for each plugin. Report per-plugin: ported test count,
gate status, and any behavioural difference from upstream with its justification.

## Reporting

Report the sequence you completed, the source-line totals ported per plugin, the
gates with exit statuses, and the omissions list with justifications. If you run
out of lane capacity, report where you stopped mid-sequence, not a rounded summary.
