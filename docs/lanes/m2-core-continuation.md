# Lane D (m2-core) continuation

Written at context reset, after M2 was completed. Everything below is committed and pushed;
`git status` is clean on `lane/m2-core`.

## 1. Completed and pushed

Branch `lane/m2-core`, all pushed. Merged to master by the leader at various points; the
last merge I know of is `4abf943` plus my own `47c37b9`.

| Commit | What |
| --- | --- |
| `d136a7a` | `dxdb-storage`: the trait surface six lanes compile against — `DocumentData`, revisions, `ChangeEvent`/`EventBulk`/`Checkpoint`, `BulkWriteRow`/`Response`/`WriteError`, `MangoQuery`/`QueryPlan`/`PreparedQuery`, `RxStorageInstance` + `DynRxStorageInstance` (dynosaur), `RxStorage` + `DynRxStorage`, `categorize_bulk_write_rows`, `Spawner` (D3), `Clock`/`now` (D4) |
| `4b1d3e1` | `categorize_bulk_write_rows` verified against a private 600-case upstream oracle; four real port bugs fixed |
| `898ff70` | `dxdb-memory`: index-string encoder, verified against all 900 upstream vectors (later deleted — see below) |
| `fa21d45` | deleted that local encoder now `dxdb-query` is published; oracle re-pointed at lane B's encoder, all 896/808/89 cases still pass |
| `f8e9666` | `dxdb-storage` consumes lane C's committed `bulk-write-categorization` family |
| `a4c4e6e` | **`dxdb-core`**: database/collection/document/hooks/write-queue/cache + 30 ported tests |
| `49301e5` | **`dxdb-reactivity`** event reduce, verified against **all 262144** upstream decisions |
| `453cb7d` | `dxdb-reactivity`: change-event buffer, query cache, 16 live-query integration tests |
| `47c37b9` | `dxdb-storage`: two-oracle framing + migration onto `dxdb-vectors` |

Last full-gate result (all from the commit above):

- `make check` **0**
- `make test` **0** — 703 tests workspace-wide, of which mine: `dxdb-storage` 47,
  `storage_implementations` 50, `index_strings_oracle` 6, `dxdb_memory` lib 8,
  `dxdb_core` 29, `core_behaviour` 30, `dxdb_reactivity` 17, `event_reduce_table` 5,
  `live_query` 16, `bulk_write_categorization_vectors` (in `dxdb-storage`) included in 47
- `make wasm` **0**
- `make clippy` **2** — **not my crates**: `crates/cleanroom/dxdb-sharding/tests/acceptance.rs:86`
  has an unused `fn update`, `-D dead-code`. That is the cleanroom lane's file. Reported to the
  leader; do not fix it from this lane.

## 2. In progress in the worktree

**Nothing.** Working tree clean, everything committed and pushed.

## 3. Exact next task

**Run the real M2 acceptance gates against `dxdb-memory`.** The lane brief named two Tier-1
suites as the acceptance oracle, and until now I was running my own ported copy in
`crates/storages/dxdb-memory/tests/storage_implementations.rs` (50 tests). The testkit lane has
since published the shared suite, so my copy is now a duplicate of theirs:

```
crates/dxdb-testkit/tests/storage_implementations.rs
crates/dxdb-testkit/tests/query_correctness.rs
```

Steps, in order:

1. `make gate-storage` and `make gate-query-correctness`. Both currently invoke
   `$(CARGO) test -- -p dxdb-testkit --test …` (note: the `--` before `-p` looks wrong; if the
   target fails with an argument error, run
   `xtask/with-build-lock.sh cargo remote-3000 -r fly -d 1.95.0 test -p dxdb-testkit --test storage_implementations`
   and report the Makefile bug to the leader rather than editing the Makefile — it is
   leader-owned).
2. Whatever fails is almost certainly a **gap in `dxdb-memory` that my own 50 tests did not
   cover**, because my copy was written from the same upstream file by the same agent. Treat
   every failure as a real storage bug until proven otherwise, and fix it in
   `crates/storages/dxdb-memory/src/lib.rs`.
3. Once green, decide with the leader whether to delete my duplicate
   `crates/storages/dxdb-memory/tests/storage_implementations.rs` (recommended: delete, and keep
   only the app-specific extras — attachments, cleanup, multi-instance — if the shared suite
   lacks them). Do not delete it before the gate is green.
4. Then M2 is genuinely done and the lane's remaining work is whatever the leader assigns.

Files that task will touch: `crates/storages/dxdb-memory/src/lib.rs` (fixes), possibly
`crates/storages/dxdb-memory/tests/storage_implementations.rs` (deletion), and nothing else.

## 4. Things learned that are not written down elsewhere

### Upstream behaviours that look like bugs and are not

- **`getFrom`'s pointer is inclusive.** Upstream calls `getFrom(latest + 1)`
  (`rx-query.ts:750`). Passing the last-seen counter replays one event (harmless); passing
  last-seen **+2 skips one, which makes a live query silently stale**. When unsure, pass the
  lower pointer. This is documented in `change_event_buffer.rs`, but you will hit it again.
- **`hasLimit` is `!!limit`**, so a query with `limit: 0` counts as *no* limit in
  event-reduce. Matching it is required for the recorded decision table to stay valid.
- **A resurrection event carries `previousDocumentData: null`.** Not the tombstone. An `INSERT`
  that also claims a previous document makes event-reduce misbehave.
- **A row that fails its attachment check (510) still emits its event.** Only the 409 conflict
  branch `continue`s upstream. And "emits an event" vs "touches attachments" are *different*
  questions: a 510 row emits its event but skips every attachment side effect.
- **The attachment payload rule is narrower than it looks**: only a *new* attachment needs
  `data`. A changed digest on an existing id with no payload is accepted upstream.
- **`categorizeBulkWriteRows` reads the document snapshot once**, before the loop, so two rows
  for the same document in one batch both see the pre-batch state and the second conflicts.
- **One `now()` per `bulkWrite`** (`rx-storage-helper.ts:630`): every row of a batch shares one
  `_meta.lwt`. A per-row clock changes checkpoint ordering.
- **Revisions are assigned by the *collection* wrapper, not the storage** and not
  `bulkInsert` (`rx-storage-helper.ts:612`). Inserts get `1-{token}`; updates get
  `previous_height + 1`.
- **A delete is a tombstone revision, never a physical delete.**
- **`getSchemaByObjectPath` is not "descend into properties then take the field".** Upstream
  rewrites the dotted path to `properties.a.properties.b` and does one flat lookup, so the leaf
  is under `properties` too. This is what makes `_meta.lwt` resolve at all.
- **`INDEX_MIN` is the number `MIN_SAFE_INTEGER`, `INDEX_MAX` is the string `U+FFFF`.** They
  match by value and only on the type they belong to. On a string field `INDEX_MIN` is "not a
  string" and takes the `\x00` arm.
- **`undefined` is not a sentinel** in the bound encoders: it matches none of the `===` tests and
  falls to the final `else` of every type chain. Reading it as "unbounded" is wrong for numbers
  and booleans. `null` and `undefined` are *different branches*, and lane C's vector records them
  differently (explicit `null` vs an absent `bound` key) — flattening both into `Option<Value>`
  conflates them.

### Traps in this repo

- **`dxdb-memory` keys shared state on `(database, collection, schema version)`.** Two tests
  using the same database name share one store, so the second test's insert conflicts against the
  first's document. This produced 16 false failures in `dxdb-core`. Always use a unique database
  name per test (I use a counter; upstream uses `randomToken(10)`).
- **The `dynosaur` dyn mirror requires the trait in scope.** `no method named
  find_documents_by_id on &DynRxStorageInstance` is a missing `use dxdb_storage::RxStorageInstance`,
  *not* a gap in the generated mirror. This cost real time and was misdiagnosed once already.
- **`make test` takes no package argument**, and the `CARGO` variable carries route flags. To run
  one crate: `xtask/with-build-lock.sh cargo remote-3000 -r fly -d 1.95.0 test -p <crate>`.
- **Never wrap a `make` target in your own `flock`** (the Makefile lock is reentrant now, but the
  habit caused a 75-minute fleet stall). **Never `cargo fmt --all`** — use `cargo fmt -p <crate>`.
- **`DynRxStorage::new_box(..)` already returns `Box<Self>`** — do not `Box::new` it again.
- **`Cargo.lock` in this worktree was stale** (84 packages, missing `dxdb-query`/`dxdb-schema`);
  fixed in `648556d`. If a build shows a lockfile diff that only adds packages, it is the
  regeneration catching up, not your change.
- Build-box outages are transport, not code: "connection refused"/"No route to host" on port 2222
  means tell the leader (`make remote-repair` / `xtask/remote-route.sh` handles it). Never build
  locally, never skip the gate.

### Decisions made in this lane, and why

- **`dxdb-storage` is the bottom of the graph** (D1). It defines `DocumentData` and the plan ABI.
  It never depends on `dxdb-query`, `dxdb-core`, or any storage crate.
- **Three deliberate divergences from upstream in the storage trait**, all documented in
  `dxdb-storage`'s module docs: no `internals`/`options` trait members (`as_any` downcast
  instead), `getAttachmentData` returns `Vec<u8>` not a `Blob`, `changeStream` returns
  `futures::Stream` not an `Observable`.
- **`dxdb-memory` deliberately does not override `get_changed_documents_since`.** Upstream's
  memory storage does not implement it either; the trait's default body *is* the documented
  fallback. A native version would be faster than upstream and less tested than upstream, which is
  the wrong trade for the reference implementation. I wrote one, then deleted it.
- **`dxdb-memory` does not reproduce lazy index materialisation.** Upstream defers index writes to
  an idle callback, so a `bulkWrite` can resolve before its rows are searchable. Rust has no DOM
  scheduler and reproducing it would let a read miss an already-resolved write.
- **Event-reduce is conservative by construction**: an unrecognised state row, an unloadable
  table, or an unimplemented action all yield `RunFullQueryAgain`. Three places where I chose
  *not* to reproduce upstream exactly, all towards re-running: `alwaysWrong` does not clear the
  result array (an empty result is a silently wrong answer); `replaceExisting` on a delete removes
  rather than storing `null`; unknown table rows re-run.
- **`rx-change-event.ts` has no module here.** `dxdb-storage::ChangeEvent` already carries the
  same four facts; event-reduce-js's only difference is `doc`/`previous` naming plus `doc: null`
  for a delete, and that case is one function (`apply::inserted_document`). A second type would be
  two truths about one event.
- **The event-reduce oracle is the complete 2^18 decision table**, not samples, because upstream
  decides with an opaque packed BDD. `testdata/event-reduce-action-table.json` records the action
  for every state combination, generated by driving the library's own BDD (`/tmp/er/table.cjs`, the
  command is quoted in the file's `$comment`). It is committed because it is derived from upstream
  output, not from our source.
- **Two categorizeBulkWriteRows oracles, both kept.** The committed 47-case family is regenerated
  every `make vectors` run and catches **upstream drift**; my frozen 600-case corpus reaches
  combinations nobody wrote down but can never detect drift. Neither is provisional.

## 5. Blocked on / questions

1. **`make gate-storage` and `make gate-query-correctness` have a suspicious command**:
   `$(CARGO) test -- -p dxdb-testkit --test storage_implementations`. The `--` before `-p` passes
   everything after it to the *test binary*, not to cargo. Worth confirming with the leader before
   relying on those targets; the Makefile is leader-owned.
2. **`make clippy` is red on master via `crates/cleanroom/dxdb-sharding`.** Not mine; needs the
   cleanroom lane or the leader. Until it is fixed, no lane can report a green `make clippy`.
3. **My duplicate `dxdb-memory/tests/storage_implementations.rs` (50 tests) vs the shared
   `dxdb-testkit` suite.** I recommend deleting mine once the shared gate is green, but I did not
   want to remove a passing suite before the replacement was proven. The leader should confirm the
   order.
4. **Remaining hand-rolled `testdata/vectors` readers** (for the vectors lane's audit):
   `dxdb-query` ×3, `dxdb-replication`, `dxdb-schema`, `plugins/query-builder`. Two more of mine
   read *crate-local fixtures* rather than `testdata/vectors`
   (`dxdb-storage/tests/categorize_oracle.rs`, `dxdb-memory/tests/index_strings_oracle.rs`);
   `dxdb-vectors` does not model private-fixture shapes, so moving those would widen that crate's
   scope. Ask the leader whether that boundary should widen.

## Where things are

```
crates/dxdb-storage/            traits, document, revisions, categorize (5009 lines)
  src/categorize.rs             the pure function, 20 unit tests
  src/traits.rs                 RxStorageInstance/RxStorage + dynosaur mirrors
  tests/categorize_oracle.rs            frozen 600-case corpus
  tests/bulk_write_categorization_vectors.rs   committed 47-case family via dxdb-vectors
  testdata/cases-{1,2,3}.json   the frozen corpus
crates/dxdb-core/               database/collection/document/hooks/queue/cache (4138 lines)
  11 modules; tests/core_behaviour.rs is the 30 ported upstream tests
crates/dxdb-reactivity/         event reduce + buffer + cache (2630 lines)
  testdata/event-reduce-action-table.json   the 2^18 table
  tests/event_reduce_table.rs   is the equivalence proof
  tests/live_query.rs           16 integration + differential harness
crates/storages/dxdb-memory/    the reference storage (3672 lines)
  src/lib.rs                    the engine
  tests/storage_implementations.rs   50 ported tests (candidate for deletion, see §5.3)
  tests/index_strings_oracle.rs      encoder oracle via dxdb-query
```
