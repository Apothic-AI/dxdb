# Lane brief: the conformance testkit

You own `crates/dxdb-testkit/` and nothing else. This lane exists because the
conformance suites are the single highest-value artifact in the whole port and they
were sitting on the critical path behind four other crates.

## Why this is the most valuable work in the repo

PLAN.md section 2, verbatim: the 55k lines of upstream tests "are the real asset.
They are the executable specification". And specifically:

> Two files carry most of the storage contract and deserve naming now, because they
> are the acceptance oracle for every storage crate in this workspace:
> `test/unit/rx-storage-implementations.test.ts` (4,115 lines) and
> `test/unit/rx-storage-query-correctness.test.ts` (2,663 lines).
> Port those two first, into a reusable `dxdb-testkit`, and every storage after that
> is mostly plumbing.

That is your job. Every storage crate — memory, filesystem, sqlite, indexeddb, opfs,
localstorage, remote, and every clean-room wrapper — is measured by whether it passes
what you write. A testkit that is wrong, or shallow, or that quietly skips the hard
cases, makes every downstream claim about dxdb false. So this lane is judged on
fidelity, not on test count.

## Read first

1. `docs/PLAN.md` sections 2, 4.2 (the storage trait contract), 7.1 (ported tests), 7.3.
2. `docs/GATES.md` tier 1 and tier 3.
3. `docs/DECISIONS.md` — D1 (document type lives in `dxdb-storage`), D3 (Spawner),
   D5 (code-unit ordering). These bind your API.
4. `docs/WORKTREES.md`.

Upstream is at `~/Code/rxdb` (pin `3b763ee17`, v17.6.0). Read it. It is Apache-2.0 and
porting it is the task.

## The block you are working behind, and what to do about it

`crates/dxdb-storage/` is being written right now by lane D (branch `lane/m2-core`).
It defines the traits and types you will test. You must not edit it, and you must not
invent a competing trait surface.

So structure your work in two layers, which is the right structure anyway:

**Layer 1 — the harness and fixtures, which need no traits.**
Port upstream's `src/plugins/test-utils/` ergonomics so translations stay one-to-one
and reviewable against the original: the `humans` collection schema and its
`HumanDocumentType`, `randomToken`, `waitUntil`, `isFastMode`, `randomString`,
`clone`, `nextTick`, and the schema-object fixtures. These are plain Rust utilities
over `serde_json::Value` and need nothing from `dxdb-storage`.

Also write the **test declaration layer**: a table-driven description of what the
suite asserts, independent of the traits. Each case is a struct
`{ id, upstream_file, upstream_line, title, kind }` where `kind` is one of
`Insert`, `BulkWrite`, `FindByIds`, `Query`, `Count`, `ChangedSince`, `Cleanup`,
`Conflict`, `Revision`, `Attachment`, `ChangeStream`. That table is the contract: it
lets lane D implement against a named list rather than a 4,000-line file, and it lets
you report honest coverage ("413 declared, 388 exercised") rather than a vague claim.

**Layer 2 — the executable suites, behind a trait bound.**
Write the suites as generic functions over `dxdb-storage`'s traits as soon as those
traits exist. Until then, keep them as `#[ignore]`-gated or feature-gated definitions
so the crate compiles and the declaration table is real.

Check `lane/m2-core` for the traits; when they land, merge that branch in and wire
layer 2 up. Do not wait idly: layer 1 is several thousand lines of real work.

## Fidelity rules — these are what "done" means

1. **Every ported test names its upstream file and line.** No exceptions. A test with
   no provenance is a test nobody can check against the original.
2. **Do not skip the hard cases.** The upstream suites are valuable precisely because
   they cover same-state forks, deleted-document queries, `_deleted` index prefixing,
   boundary clamping, multi-digit revisions, per-document atomicity with a partial
   error list, and `cleanup` returning `false` for "more work remains". If a case is
   hard to express in Rust, express it anyway or write down precisely why not.
3. **Match upstream's assertions, not upstream's style.** Where upstream asserts on an
   error message string, assert on the error *kind* plus the fields that matter, and
   note the divergence in a comment. Where upstream asserts an ordering, assert the
   ordering exactly — that is wire behaviour.
4. **The query-correctness suite is about Mango semantics per storage.** Every selector
   operator in the small upstream set, with both matching and non-matching documents,
   in both directions. `docs/PLAN.md` 4.3 lists the operator set; do not test operators
   upstream does not have.
5. **Do not import a concrete storage crate here.** The testkit is generic. It may
   depend on `dxdb-storage` and on nothing else in the workspace.

## Your gate

```bash
make check && make clippy && make test && make wasm
```

Plus, once the traits are wired: the two gate targets the Makefile already defines,

```bash
make gate-storage            # cargo test -p dxdb-testkit --test storage_implementations
make gate-query-correctness  # cargo test -p dxdb-testkit --test query_correctness
```

which means your integration test files must be named `storage_implementations.rs` and
`query_correctness.rs`. Those targets are how the leader verifies every storage lane,
so the names are fixed.

## Constraints

- No `unsafe`.
- No `tokio`. Tests that need concurrency use `futures::executor` or the `Spawner`
  from D3 with its sequential default.
- Builds go to the shared Fly box via the Makefile only. Never local. The Makefile
  takes an flock; if the box is busy, wait.
- Pipe long output to `/tmp/testkit-*.log`.
- Commit in small units on branch `lane/m2-testkit`. Never merge to master.
- Never edit leader-owned files or another lane's crate. If you need a trait change,
  report it; lane D owns that file.

## Reporting

Report: the declaration table size by `kind`, how many cases are exercised versus
declared per suite, the ported upstream line coverage (your own honest tally against
4,115 and 2,663), each gate's exit status, and every case you could not port with the
reason. An honest "388 of 413 exercised, here are the 25 reasons" is a good report. A
rounded-up "ported both suites" is not.
