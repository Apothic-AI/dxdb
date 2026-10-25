# Lane brief: M1 schema and query

You are implementing `dxdb-schema` and `dxdb-query`. This brief is self-contained;
read the two planning docs it names before writing code, then work.

## Read first

1. `docs/PLAN.md` — sections 3 (non-obvious decisions), 4.3 (semantics that must
   match exactly), 6 M1, 7 (verification). These are not background: they are the
   acceptance criteria.
2. `docs/WORKTREES.md` — you own `crates/dxdb-schema/` and `crates/dxdb-query/`.
   Do not edit `Cargo.toml`, the Makefile, or anything else in the leader-owned
   list.
3. `docs/GATES.md` — what "done" means.

Upstream reference is checked out read-only at `~/Code/rxdb` (pin `3b763ee17`,
v17.6.0). Read it. It is Apache-2.0 and porting it is the task.

## What to build

### `dxdb-schema`

Port from `src/rx-schema-helper.ts` and `src/rx-schema.ts`:

- **Meta field injection.** `_rev`, `_deleted`, `_meta`, `_attachments` are added
  to `properties` and `required`, and `_deleted` is prepended to every index
  unless it is already first (`rx-schema-helper.ts:271`). Get the ordering
  exactly right: it is observable in index strings.
- **Index builder.** The index string every schema produces is part of the
  wire behaviour.
- **Schema hash**, computed over ordered JSON bytes with JavaScript number
  formatting. Use `dxdb-json::canonical_bytes`, not `serde_json::to_string`.
  Caching exists upstream and the hash is part of schema identity.
- **JSON Schema validation**, draft-07 (that is RxDB's draft), via the
  `jsonschema` crate. PLAN.md 8 verified it is wasm-clean.

### `dxdb-query`

Port from `src/custom-index.ts`, `src/query-planner.ts`, `src/rx-query-helper.ts`,
`src/rx-query-mingo.ts`, `src/rx-query.ts`:

- **Index strings** (`src/custom-index.ts`, `getIndexableStringMonad` at `:164`,
  `getNumberIndexString` at `:274`). Numbers are zero-padded around a
  schema-declared min/max, so the encoding depends on the schema's bounds. Port
  the boundary clamping behaviour: a value outside the declared range clamps
  rather than producing a broken index.
- **The Mango operator set**, deliberately small (`src/types/rx-query.d.ts:43`):
  `$eq $gt $gte $lt $lte $ne $in $nin $regex $options $exists $type $mod $not`
  `$size $elemMatch` plus `$and $or $nor` and the `$not` object form. This is
  **not** mingo's full surface. Match the documented limitations of unknown-CRUD
  and `$or`/`$nor` pushdown **rather than improving on them**: query plans are
  wire behaviour.
- **`$regex` via the `regress` crate**, which is ECMAScript semantics. Not
  `regex`'s syntax. PLAN.md 8 verified: JS-style flags are a `Flags` struct, not
  inline `(?i)`; `\p{Letter}` needs `unicode: true`.
- **The planner** (`src/query-planner.ts`). Only `$eq $gt $gte $lt $lte` are
  usable as logical operators (`:239`). It emits `startKeys`/`endKeys`/
  `selectorSatisfiedByIndex` (`:244`). `INDEX_MIN` is `Number.MIN_SAFE_INTEGER`,
  **not** `-Infinity`, specifically so a query plan survives `JSON.stringify`
  (comment at `:20`, constant at `:25`).
- **Normalisation and sorting.** Sort is by index string in **code-unit order**
  (`dxdb_jsstr`), with the primary key appended if absent. A `desc` anywhere in
  the sort forces a re-sort, because storages do not support descending indexes
  (`src/query-planner.ts:48`).

## Non-negotiable rules

- **Never use `f64::to_string` or `serde_json`'s float writer** for anything that
  becomes an index string, a hash, or stored bytes. Use `dxdb-jsnum`.
  (`docs/PLAN.md` 3.1: JS `String(1e21)` is `1e+21`, Rust `Display` is
  `1000000000000000000000`.)
- **Never compare strings with Rust's `str` ordering** on any sort or index path.
  Use `dxdb_jsstr::compare_js`. (`docs/PLAN.md` 3.2: `'\u{1F600}' < '\uFFFF'` is
  true in JS, false in Rust.)
- Never round-trip a document through a `BTreeMap` or a `HashMap` where key order
  can be observed.
- Every crate must build for `wasm32-unknown-unknown`. Run `make wasm`.
- No `unsafe`. No `async_trait` on hot paths (see PLAN.md 3.3 for the plan).

## Verification: your gate

```bash
make check     # cargo check --workspace --all-targets, remotely
make clippy    # -D warnings
make test      # workspace tests, remotely
make wasm      # wasm32-unknown-unknown
```

All four must exit 0. In addition:

1. **Ported upstream tests.** Translate `test/unit/rx-query.test.ts`,
   `test/unit/rx-schema.test.ts`, and `test/unit/rx-query-helper.test.ts` to Rust.
   Each ported test names the upstream file and line it came from in a comment.
   Put module-specific tests in the crate; put shared fixtures in
   `crates/dxdb-testkit/` **only by requesting it** — that crate belongs to lane D.
2. **Vector tests.** Read `testdata/vectors/*.json` for the families that exist
   (`jsnum.json` today). Consume them; do not edit them. If you need a new vector
   family, request it from lane C in your report rather than writing a generator
   into `xtask/vectors/` (that path is lane C's).
3. **Fuzz/property test.** A `proptest` harness generating documents and selectors,
   asserting the matcher agrees with oracle vectors where they exist, and asserting
   internal invariants (e.g. `$eq` implies `$gte` and `$lte`; `$in` of one equals
   `$eq`).

## Reporting

When the gate is green, report: the exact commands and exit statuses, the count of
ported upstream tests, any behaviour you could not match and why, and any vector
family you need from lane C. Do not report "done" without the command output.

## Scope discipline

Do not implement storage, core, replication, or any plugin. `dxdb-query` and
`dxdb-schema` have no dependency on a storage crate and must not gain one.
