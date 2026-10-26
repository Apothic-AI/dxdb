# Lane D (m2-core) continuation — post-M2

`git status` clean. Branch `lane/m2-core`, everything pushed. M2 is met (audit 11/11,
module coverage 80/80); the lane's remaining work is the integration gap in §2.

## 1. Completed and pushed

Branch `lane/m2-core`. Merged to master by the leader as it went. My last pushed commit is
`628bbb3`.

| Commit | What |
| --- | --- |
| `d136a7a` | `dxdb-storage`: trait surface six lanes compile against — `DocumentData`, revisions, `ChangeEvent`/`EventBulk`/`Checkpoint`, `BulkWriteRow`/`Response`/`WriteError`, `MangoQuery`/`QueryPlan`/`PreparedQuery`, `RxStorageInstance`+`DynRxStorageInstance` (dynosaur), `RxStorage`+`DynRxStorage`, `categorize_bulk_write_rows`, `Spawner` (D3), `Clock`/`now` (D4) |
| `4b1d3e1` | `categorize_bulk_write_rows` verified against a private 600-case upstream oracle; four real port bugs fixed |
| `898ff70` | `dxdb-memory`: index-string encoder vs all 900 upstream vectors (later deleted, see `fa21d45`) |
| `fa21d45` | deleted that local encoder once `dxdb-query` published; oracle re-pointed at lane B's, 896/808/89 cases still pass |
| `f8e9666` | `dxdb-storage` consumes lane C's committed `bulk-write-categorization` family |
| `a4c4e6e` | **`dxdb-core`**: database/collection/document/hooks/write-queue/cache + 30 ported tests |
| `49301e5` | **`dxdb-reactivity`** event reduce, verified against **all 262144** upstream decisions |
| `453cb7d` | `dxdb-reactivity`: change-event buffer, query cache, 16 live-query integration tests |
| `47c37b9` | `dxdb-storage`: two-oracle framing + migration onto `dxdb-vectors` |
| `6c5267f` | earlier continuation doc (superseded by this file) |
| `628bbb3` | **differential fuzz harness** for live queries (PLAN.md 7.3) |

Last full gate on this branch:

- `make check` **0**
- `make test` **0** — 931 tests workspace-wide
- `make wasm` **0**
- `make clippy` **2** — **not my crates**: `crates/cleanroom/dxdb-sharding/tests/acceptance.rs:86`,
  unused `fn update`, `-D dead-code`. Cleanroom lane's file; reported, not fixed here.

My crate test inventory: `dxdb-storage` 47 + `bulk_write_categorization_vectors` +
`categorize_oracle` 4, `dxdb-memory` lib 8 + `index_strings_oracle` 6 +
`storage_implementations` 50, `dxdb-core` 29 + `core_behaviour` 30,
`dxdb-reactivity` 17 + `event_reduce_table` 5 + `live_query` 16 + `live_query_fuzz` 6.

## 2. What is left in this lane's scope

### 2.1 The one real gap: run the testkit suite against `dxdb-memory`

`crates/dxdb-testkit/tests/storage_implementations.rs` currently runs the ported
`rx-storage-implementations` suite against **`ReferenceStorage`**, the testkit's own scaffold,
not against a real storage. Its own module doc says so:

> The storage under test is `ReferenceStorage`, the testkit's own scaffold. That is deliberate
> and documented in its module: it exists so this gate is not vacuous before `dxdb-memory` lands.
> **When lane D's `dxdb-memory` is available, add a second test module here that runs the same
> suite against it** — the suite functions are generic over `RxStorage`, so that is a one-line
> addition per storage, not a rewrite.

`dxdb-memory` is available. That is the gap.

The suite is already generic — `crates/dxdb-testkit/src/suite.rs` (2480 lines, ~50 cases), each
`pub async fn …<S: RxStorage>(storage: &S)`. So the work is:

1. Add `dxdb-memory` as a **dev-dependency** of `crates/dxdb-testkit/Cargo.toml`
   (`dxdb-memory = { path = "../storages/dxdb-memory" }`). Note the path: memory lives under
   `crates/storages/`, not `crates/`.
2. In `crates/dxdb-testkit/tests/storage_implementations.rs`, add a second module that drives the
   same `suite::*` functions with `MemoryStorage`.

   **Blocking detail to check first:** the suite functions take `&S where S: RxStorage`, and
   `dxdb-memory`'s factory is `MemoryStorage` (a unit-ish struct implementing `RxStorage`) — so
   `&MemoryStorage` should work directly. But `ReferenceStorage::new()` in the existing module
   suggests the suite may also want a way to *make* instances per case; read
   `tests/support/reference_storage.rs` before writing the second module, because if the suite
   takes a factory trait rather than `RxStorage`, `dxdb-memory` needs whatever that trait needs.
   I have not read that file — that is the first thing the next session should do.
3. Confirm the Makefile target actually runs it. `make gate-storage` is
   `$(CARGO) test -- -p dxdb-testkit --test storage_implementations`. The `--` before `-p` passes
   everything after it to the **test binary**, not to cargo, so the target may not run the suite
   at all. It was green in my runs, but `make test` also runs these suites, so green there proves
   the suite passes, not that the gate target selects it. **Report the Makefile bug to the leader
   — the Makefile is leader-owned, do not edit it.**
4. The same file's sibling `crates/dxdb-testkit/tests/query_correctness.rs` (31 tests) already
   runs; check whether it also uses `ReferenceStorage` and needs the same second module.

Files touched: `crates/dxdb-testkit/Cargo.toml`,
`crates/dxdb-testkit/tests/storage_implementations.rs` (and possibly `query_correctness.rs`),
possibly `crates/dxdb-testkit/tests/support/reference_storage.rs` read-only.

**`dxdb-testkit` is not my crate** (split to its own lane early on). Treat the above as a request
to that lane's owner or an explicit leader assignment; do not edit it silently.

### 2.2 My duplicate suite, to delete once §2.1 is green

`crates/storages/dxdb-memory/tests/storage_implementations.rs` (50 tests) is my own port of the
same upstream file. It is a duplicate of the testkit's. Recommended: delete it once the testkit
suite runs against `dxdb-memory` and is green, keeping only any of my extras the shared suite
lacks (attachments payload rules, `cleanup` contract, multi-instance sharing, code-unit index
ordering — check each against `suite.rs`). **Do not delete before the replacement is proven.**

### 2.3 Remaining `dxdb-vectors` migration (not mine, but mine contributed to the audit)

Six crates still hand-roll `testdata/vectors` readers: `dxdb-query` ×3, `dxdb-replication`,
`dxdb-schema`, `plugins/query-builder`. Mine are migrated (`dxdb-storage`). Two of my files read
**crate-local fixtures** rather than `testdata/vectors`
(`crates/dxdb-storage/tests/categorize_oracle.rs`,
`crates/storages/dxdb-memory/tests/index_strings_oracle.rs`); `dxdb-vectors` does not model
private-fixture shapes, so moving those would widen that crate's scope. Ask the leader.

## 3. Things learned that are not written down in the repo

### Upstream behaviours that look like bugs and are not

- **`getFrom`'s pointer is inclusive.** Upstream calls `getFrom(latest + 1)` (`rx-query.ts:750`).
  Last-seen replays one event (harmless); last-seen **+2 skips one, which is silently stale**.
  When unsure, pass the lower pointer. Documented in `change_event_buffer.rs`; you will hit it.
- **`hasLimit` is `!!limit`**, so `limit: 0` counts as *no* limit in event-reduce. Required for
  the recorded decision table to stay valid.
- **A resurrection event carries `previousDocumentData: null`**, not the tombstone. An `INSERT`
  claiming a previous document makes event-reduce misbehave.
- **A row failing its attachment check (510) still emits its event.** Only the 409 conflict
  branch `continue`s upstream. "Emits an event" and "touches attachments" are *different*
  questions: a 510 row emits its event but skips every attachment side effect.
- **Attachment payload rule is narrower than it looks**: only a *new* attachment needs `data`. A
  changed digest on an existing id with no payload is accepted upstream.
- **`categorizeBulkWriteRows` snapshots documents once**, before the loop, so two rows for one
  document both see the pre-batch state and the second conflicts.
- **One `now()` per `bulkWrite`** (`rx-storage-helper.ts:630`): all rows of a batch share an
  `lwt`. A per-row clock changes checkpoint ordering.
- **Revisions are assigned by the collection wrapper**, not the storage and not `bulkInsert`
  (`rx-storage-helper.ts:612`). Inserts get `1-{token}`; updates `previous_height + 1`.
- **A delete is a tombstone revision, never a physical delete.**
- **`getSchemaByObjectPath` is not "descend into properties then take the field".** Upstream
  rewrites `a.b` to `properties.a.properties.b` and does one flat lookup, so the leaf is under
  `properties` too. This is what makes `_meta.lwt` resolve at all.
- **`INDEX_MIN` is the number `MIN_SAFE_INTEGER`, `INDEX_MAX` the string `U+FFFF`.** They match by
  value and only on the type they belong to. On a string field `INDEX_MIN` is "not a string" and
  takes the `\x00` arm.
- **`undefined` is not a sentinel** in the bound encoders: it matches none of the `===` tests and
  falls to the final `else` of every type chain. Reading it as "unbounded" is wrong for numbers
  and booleans. `null` and `undefined` are *different branches*, and the vector records them
  differently (explicit `null` vs an absent `bound` key) — `Option<Value>` conflates them.

### Repo traps

- **`dxdb-memory` keys shared state on `(database, collection, schema version)`.** Two tests
  reusing a database name share one store, so the second test's insert conflicts against the
  first's document. This produced 16 false failures in `dxdb-core`. Use a unique database name per
  test (a counter is enough; upstream uses `randomToken(10)`).
- **The dynosaur dyn mirror requires the trait in scope.** `no method named find_documents_by_id
  on &DynRxStorageInstance` is a missing `use dxdb_storage::RxStorageInstance`, *not* a gap in the
  generated mirror. This was misdiagnosed once already and cost real time.
- **`make test` takes no package argument**, and the `CARGO` variable carries route flags. Running
  `cargo remote-3000 … test -p <crate>` directly misparses `-p` as `--ssh-port`. To run one crate,
  either rely on `make test` and grep, or use the Makefile's own invocation shape. If you need a
  single suite's output, temporarily make a test fail with the data in the panic message (that is
  how the fuzz corpus numbers were captured — see `corpus_report` in `live_query_fuzz.rs`).
- **Never wrap a `make` target in your own `flock`** (a 75-minute fleet stall came from that).
  **Never `cargo fmt --all`** — use `cargo fmt -p <crate>`.
- **`DynRxStorage::new_box(..)` already returns `Box<Self>`** — do not `Box::new` it again.
- Transport failures ("connection refused"/"No route to host" on 2222) are the build box, not your
  code: tell the leader, use `make remote-repair` / `xtask/remote-route.sh`. Never build locally,
  never skip a gate.

### Decisions made in this lane, with reasons

- **`dxdb-storage` is the bottom of the graph** (D1). It owns `DocumentData` and the plan ABI and
  never depends on `dxdb-query`, `dxdb-core`, or a storage crate.
- **Three deliberate storage-trait divergences**, documented in `dxdb-storage`'s module docs: no
  `internals`/`options` members (`as_any` downcast), `getAttachmentData` returns `Vec<u8>` not a
  `Blob`, `changeStream` returns `futures::Stream` not an `Observable`.
- **`dxdb-memory` deliberately does not override `get_changed_documents_since`.** Upstream's
  memory storage does not implement it either; the trait default *is* the documented fallback.
  Faster-than-upstream on the reference implementation means less-tested-than-upstream. I wrote
  one, then deleted it.
- **`dxdb-memory` does not reproduce lazy index materialisation.** Upstream defers index writes to
  an idle callback so a `bulkWrite` can resolve before its rows are searchable. Rust has no DOM
  scheduler; reproducing it would let a read miss an already-resolved write.
- **Event-reduce is conservative by construction**: an unrecognised state row, an unloadable
  table, or an unimplemented action all yield `RunFullQueryAgain`. Three places I chose *not* to
  copy upstream exactly, all towards re-running: `alwaysWrong` does not clear the result array
  (empty result = silently wrong answer); `replaceExisting` on a delete removes instead of storing
  `null`; unknown table rows re-run.
- **`rx-change-event.ts` has no module.** `dxdb-storage::ChangeEvent` already carries the same
  four facts; event-reduce-js's only difference is `doc`/`previous` naming plus `doc: null` for a
  delete, and that case is one function (`apply::inserted_document`).
- **The event-reduce oracle is the complete 2^18 decision table**, not samples, because upstream
  decides with an opaque packed BDD. `testdata/event-reduce-action-table.json` records the action
  for every state combination, generated by driving the library's own BDD (command in the file's
  `$comment`).
- **Two categorizeBulkWriteRows oracles, both kept.** The committed 47-case family is regenerated
  every `make vectors` run and catches **upstream drift**; my frozen 600-case corpus reaches
  combinations nobody wrote down but can never detect drift. Neither is provisional.
- **Fuzz harness numbers are pinned** in `corpus_report`: 96 runs (8 seeds × 12 shapes × 40
  steps), 3840 events, fast path 94.8%, `doNothing` 50.8% of fast, re-run 5.2%, 0 divergences.
  A deliberate corpus change should update those numbers in the same commit.

## 4. Blocked / questions for the leader

1. **Who owns §2.1?** `dxdb-testkit` was split to its own lane before I could create it. Running
   the suite against `dxdb-memory` needs either that lane's owner or an explicit assignment to me.
   This is the only thing standing between M2's stated gate and it actually being run against a
   real storage.
2. **`make gate-storage` / `make gate-query-correctness` look malformed** (`test -- -p dxdb-testkit
   --test …`). The `--` passes the rest to the test binary. Worth fixing before anyone cites those
   targets as evidence. Makefile is leader-owned.
3. **`make clippy` is red on master via `crates/cleanroom/dxdb-sharding/tests/acceptance.rs:86`**
   (unused `fn update`). Not mine, but no lane can report a green clippy until it is fixed.
4. **My duplicate `dxdb-memory/tests/storage_implementations.rs`** (50 tests) vs the testkit suite:
   confirm the deletion order (§2.2).
5. **Should `dxdb-vectors` model crate-local private fixtures**, so my two remaining hand-rolled
   readers (`categorize_oracle.rs`, `index_strings_oracle.rs`) can migrate too? (§2.3)

## Where things are

```
crates/dxdb-storage/            5009 lines
  src/categorize.rs             the pure function, 20 unit tests
  src/traits.rs                 RxStorageInstance/RxStorage + dynosaur mirrors
  tests/bulk_write_categorization_vectors.rs   47-case family via dxdb-vectors
  tests/categorize_oracle.rs    frozen 600-case corpus
  testdata/cases-{1,2,3}.json
crates/dxdb-core/               4138 lines, 11 modules
  tests/core_behaviour.rs       30 ported upstream tests
crates/dxdb-reactivity/         2630 lines
  testdata/event-reduce-action-table.json      the 2^18 table
  tests/event_reduce_table.rs   the equivalence proof
  tests/live_query.rs           16 integration tests
  tests/live_query_fuzz.rs      6 tests, the differential harness
crates/storages/dxdb-memory/    3672 lines
  src/lib.rs                    the engine
  tests/storage_implementations.rs   50 tests (candidate for deletion, §2.2)
  tests/index_strings_oracle.rs      encoder oracle via dxdb-query
crates/dxdb-testkit/            NOT MINE — see §2.1
  src/suite.rs                  ~50 generic suite fns, S: RxStorage
  tests/storage_implementations.rs   runs ReferenceStorage today
  tests/support/reference_storage.rs  read this first
```
