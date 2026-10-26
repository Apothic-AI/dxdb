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
`conformance` 2 (48 cases inside them) + `memory_specific` 6 + `query_correctness` 2,
`dxdb-core` 29 + `core_behaviour` 30,
`dxdb-reactivity` 17 + `event_reduce_table` 5 + `live_query` 16 + `live_query_fuzz` 6.

## 2. What is left in this lane's scope

### 2.1 CLOSED — the testkit suite now runs against `dxdb-memory`

The gap was that the ported `rx-storage-implementations` suite had only ever run against
`ReferenceStorage`, the testkit's own scaffold. Closed, both halves:

| file | what it does | verified |
| --- | --- | --- |
| `crates/storages/dxdb-memory/tests/conformance.rs` | all 48 exercised implementations cases against `MemoryStorage` | **48/48 pass** |
| `crates/storages/dxdb-memory/tests/query_correctness.rs` | all 31 query-correctness groups against `MemoryStorage` | **compiles/passes NOT YET VERIFIED — see §2.4** |

**Where the drivers live, and why not in the testkit.** My earlier plan was to add a second
module inside `crates/dxdb-testkit/tests/`. That was wrong twice over. The testkit is another
lane's crate, and its `run_group` is private to its own test binary, so a second module there
would have meant either duplicating the runner anyway or widening it to `pub` across
binaries — which Rust does not do for test files. Everything needed is already public and
re-exported from the testkit root (`fixtures::correctness::groups()`, `Group`, `InstanceSpec`,
the whole `Harness` surface), so the driver belongs in the crate that wants to prove itself.
That is the same shape `dxdb-filesystem/tests/conformance.rs` and `dxdb-sharding` already use,
and it leaves the shared oracle untouched.

**The count is not a literal.** `conformance.rs` reads its total from `decl::exercised_cases()`
and asserts the suite and the declaration table name the same set. Verified by probe, not by
assumption: `declared=48`, upstream lines `[176, 195, 270 … 4046]`. 48 is also the number the
declaration table calls `Exercised`, so the filter is not vacuous. This matters because a
suite that silently shrank to three cases would pass the gate *more* easily than the full one.

**`query_correctness.rs` was also scaffold-only** — the same gap, which the earlier brief
suspected and did not confirm. It now has a `dxdb-memory` driver too.

**The Makefile is NOT broken.** My earlier brief asked the leader to fix
`make gate-storage` / `make gate-query-correctness`, suspecting the `--` made them dead
targets. Both were run and both work: `make gate-storage` → exit 0, "running 50 tests", and
`Running tests/storage_implementations.rs` in the log; `make gate-query-correctness` → exit 0,
"running 31 tests". The `-p`-read-as-`--ssh-port` hazard is real for `cargo remote-3000`, but
the Makefile already guards it correctly. **No leader action needed. Do not file this.**

### 2.2 CLOSED — the duplicate suite is deleted

`crates/storages/dxdb-memory/tests/storage_implementations.rs` (50 tests, 2040 lines) is gone,
deleted in the same commit that proved its replacement green. Keeping it would have meant two
oracles, and when two oracles disagree, deciding which is right means re-reading upstream —
`storage_matrix.rs` also stops meaning anything if a storage passes its own suite and fails
the shared one.

**Five of its 50 tests assert something the shared suite genuinely cannot, and were kept** in
`crates/storages/dxdb-memory/tests/memory_specific.rs`. Each was checked against `suite.rs` by
literal search rather than assumed:

| kept test | why the shared suite misses it |
| --- | --- |
| `refuses_key_compression_without_the_wrapper` | upstream runs the whole suite *with* and *without* a key-compression wrapper; neither run asserts a bare storage must **reject** the schema |
| `refuses_encrypted_fields_without_the_wrapper` | same shape for the encryption plugin |
| `an_unknown_index_is_an_error` | upstream never plans a query against an index the schema does not declare; an empty result instead of an error is a silently wrong answer |
| `orders_index_strings_by_code_unit_not_by_bytes` | upstream's sort case is ASCII and its umlaut case is a round-trip check. Neither separates UTF-16 code-unit order from UTF-8 byte order, and that pair (`"\u{FFFF}"` vs an astral char) is the only place a Rust port can silently differ. This is D5's one observable divergence |
| `schema_versions_do_not_share_state`, `database_names_do_not_share_state` | the shared suite uses a fresh random database name per case, so it never opens two — these assertions pass whether or not the engine keys state correctly |

Confirmed absent by search, not by impression: `suite.rs` contains zero occurrences of
`Unsupported`, `UnknownIndex`, `keyCompression`, `encrypted`, or `u{FFFF}`.

**A bug in my own new code, caught by the gate:** I wrote `close()` then `remove()` in the two
isolation tests and got `Closed`. `remove` implies closed, not the reverse — the ordering the
shared suite asserts in `remove_after_close_throws`. Fixed before commit.

### 2.3 Remaining `dxdb-vectors` migration (not mine, but mine contributed to the audit)

Six crates still hand-roll `testdata/vectors` readers: `dxdb-query` ×3, `dxdb-replication`,
`dxdb-schema`, `plugins/query-builder`. Mine are migrated (`dxdb-storage`). Two of my files read
**crate-local fixtures** rather than `testdata/vectors`
(`crates/dxdb-storage/tests/categorize_oracle.rs`,
`crates/storages/dxdb-memory/tests/index_strings_oracle.rs`); `dxdb-vectors` does not model
private-fixture shapes, so moving those would widen that crate's scope. Ask the leader.
### 2.4 OPEN — one file is written but NOT verified

`crates/storages/dxdb-memory/tests/query_correctness.rs` was written during a **hard
maintenance stop on the build box** and has never been compiled. Treat it as unproven.

What *was* done instead of guessing, all by reading source:

- every API it calls was confirmed against the real definition: `query_count(groups: &[Group])
  -> usize` (a slice, not a count), `Group.index: usize`, `asserts_count()`, and
  `fixtures::EXAMPLE_REVISION_1` all exist as used;
- everything it needs is public and re-exported from the testkit root, so it needs no change
  to `dxdb-testkit` and no new manifest entry (`dxdb-memory` is already a dev-dependency there);
- no literal counts: totals come from `correctness::groups()`.

**First action after RESUME:** run it. Two risks were then retired by matching the *compiling*
code rather than by judgement:

- the `for group in groups` loop over `&'static [Group]` is the exact idiom
  `crates/dxdb-testkit/tests/query_correctness.rs` uses at four sites, so it is not a
  move-out-of-borrow;
- `query_count` is called as `dxdb_testkit::fixtures::query_count(groups)`, which is both the
  re-export path in `fixtures/mod.rs` and the form the proven testkit test uses. I had written
  the `correctness_support::` sub-path; it would likely have compiled, but matching proven code
  costs nothing.

If it still fails, the remaining likely cause is a `Vec` vs slice coercion inside the
`catch_unwind` closure. One-line fix. Do not fold the result into any report until it has run.


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

Only one is a real question. Three of the five I carried into this reset turned out to be
non-issues, and one was my own error — the record is corrected here rather than quietly dropped.

1. **Should `dxdb-vectors` model crate-local private fixtures**, so my two remaining hand-rolled
   readers (`categorize_oracle.rs`, `index_strings_oracle.rs`) can migrate too? (§2.3) Still
   open, still not mine to answer unilaterally.

**Resolved, no action needed:**

2. ~~Who owns §2.1~~ — **closed by doing it in my own crate.** The testkit needed no change;
   its public surface was sufficient. See §2.1 for why the driver belongs in the storage crate.
3. ~~`make gate-storage` / `gate-query-correctness` look malformed~~ — **they work.** Both were
   run: exit 0, 50 and 31 tests, correct target selected in the log. The suspicion was wrong.
   Do not spend leader time on it.
4. ~~`make clippy` red via `dxdb-sharding/tests/acceptance.rs:86`~~ — **fixed by another lane.**
   `make clippy` is green (exit 0) on this branch as of commit `c3e495f`. It may be the first
   time this lane could report a green clippy.
5. ~~duplicate suite deletion order~~ — resolved; deleted only after the replacement was
   proven green (§2.2).

## Gate state as of `c3e495f` (all four verified before the maintenance stop)

| gate | result |
| --- | --- |
| `make check` | **0** |
| `make test` | **0** — 152 suites green (was 151: +2 new targets, −1 deleted duplicate) |
| `make clippy` | **0** — first green clippy for this lane |
| `make wasm` | **0** |
| `make gate-storage` | **0** — 50 tests, correct target |
| `make gate-query-correctness` | **0** — 31 tests, correct target |

Not in that table: `tests/query_correctness.rs` in `dxdb-memory`, which was written during the
stop and has never been compiled (§2.4). Re-run everything after RESUME anyway — the box was
resized and restarted.

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
