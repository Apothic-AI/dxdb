# Lane report: the conformance testkit (layer 1 → 2)

Branch `lane/m2-testkit`. Scope: `crates/dxdb-testkit/` only.

This is the lane's own report, kept beside the code so the numbers are the ones the
code produces rather than the ones a chat message remembered. Regenerate it with
`cargo run -p dxdb-testkit --example coverage`.

---

## 1. What the lane was asked for

The brief gave two layers:

- **Layer 1**, needing nothing from `dxdb-storage`: port upstream's
  `src/plugins/test-utils/` ergonomics, and build the table-driven case
  declaration.
- **Layer 2**, behind the storage traits: the executable suites, in files named
  `storage_implementations.rs` and `query_correctness.rs` because `make
  gate-storage` and `make gate-query-correctness` name those paths.

`dxdb-storage` landed mid-lane (merged from `master`), so both layers are done.

---

## 2. Declaration table by kind

90 cases: 65 from `rx-storage-implementations.test.ts`, 25 groups from
`rx-storage-query-correctness.test.ts` (which hold **89 queries** between them).

"Exercised" means a storage regression fails the case today. "Fully exercised"
means every assertion upstream makes in that case runs; the difference is the
correctness groups, explained in §4.

| kind | declared | exercised | fully exercised |
| --- | ---: | ---: | ---: |
| `Insert` | 1 | 1 | 1 |
| `BulkWrite` | 12 | 7 | 7 |
| `FindByIds` | 6 | 6 | 6 |
| `Query` | 36 | 33 | 8 |
| `Count` | 1 | 1 | 1 |
| `ChangedSince` | 3 | 3 | 3 |
| `Cleanup` | 7 | 7 | 7 |
| `Conflict` | 4 | 4 | 4 |
| `Revision` | 3 | 3 | 3 |
| `Attachment` | 5 | 5 | 5 |
| `ChangeStream` | 4 | 3 | 3 |
| `Static` | 8 | 0 | 0 |
| **total** | **90** | **73** | **48** |

Status breakdown: `Exercised` 48, `PartiallyExercised` 25, `NeedsQueryCrate` 8,
`NeedsMultiInstance` 6, `NeedsKeyCompressionWrapper` 2, `NeedsEncryptionWrapper` 1.

### Two deviations from the brief's vocabulary, both stated rather than silent

**`Static` is a twelfth kind.** The brief names eleven. Upstream's
`.prepareQuery()`, `.getSortComparator()` and `.getQueryMatcher()` blocks are 590
lines inside the 4,115 that are pure functions of a schema and a query with no
storage instance involved. Dropping them would understate the contract; calling
them `Query` would imply a storage implements them, which is false. `Static` says
what they are: the in-memory halves of the query contract, provided by
`dxdb-query`. They are `NeedsQueryCrate`.

**`Insert` had no natural home.** RxDB has no storage-level `insert()` — every
write is `bulkWrite`, even for one row — so no upstream `describe` block is named
"insert". Rather than invent a case or leave the kind empty, `should write the
document` (the single-row degenerate case) is classified `Insert`. The reason is
recorded on the row itself.

---

## 3. Cases declared vs exercised, per suite

| suite | declared | exercised | fully exercised |
| --- | ---: | ---: | ---: |
| implementations | 65 | 48 | 48 |
| query correctness | 25 | 25 | 0 |

The correctness count is **groups**, not queries. All 25 groups and all 89 queries
are extracted, committed and asserted against; what varies is which of upstream's
four per-query assertions can run.

---

## 4. What "partially exercised" means, exactly

Upstream's correctness harness asserts four things per query
(`rx-storage-query-correctness.test.ts:204-340`):

| # | assertion | needs | runs |
| --- | --- | --- | --- |
| 1 | in-memory statics: `getQueryMatcher` + `getSortComparator` vs expected ids | `dxdb-query` | no |
| 2 | planner's `selectorSatisfiedByIndex` vs the fixture's value | `dxdb-query` | no |
| 3 | `RxStorageInstance.query()` vs `expectedResultDocIds` | nothing extra | **yes** |
| 4 | `count()` vs expected length, when the query has no skip/limit | nothing extra | **yes** |

3 and 4 are the storage half and they are the half this crate owns. So the groups
are `PartiallyExercised`: a storage that returns the wrong documents fails them
today, but their *oracle* is taken as given rather than recomputed, which means a
storage bug and a wrong fixture would be indistinguishable. That caveat is in the
`query_correctness.rs` module docs and in every failure message. It disappears when
`dxdb-query` lands, not before.

Upstream also runs (5) `RxCollection.find()` / `findByIds()` and (6) a
remove-and-reinsert live-query cycle. Both sit above the storage layer and need
`dxdb-core`; they are accounted `DeclaredNotPorted` in the line tally rather than
silently absent.

---

## 5. Line coverage: the honest tally

Two numbers per suite, because for this work they differ a lot and the larger one
is the easier one to inflate.

| suite | upstream | ported + running | ported, pending a dep | declared not ported | fixture data | scaffolding |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| implementations | 4,115 | 3,223 | 332 | 434 | 0 | 126 |
| query correctness | 2,663 | 138 | 75 | 98 | 2,287 | 65 |

Every row sums exactly to the upstream line count: the tallies are ranges measured
from the upstream files, and `line_tallies_partition_their_files` fails if any range
is missing or overlaps, and a separate cross-check asserts that no case outside
`Exercised`/`PartiallyExercised` sits inside a range labelled "ported and exercised" —
that check found one real overclaim (the key-compression case at `:728`) when the
report was first written.

Read that carefully, because the interesting part is where the numbers are *not*
flattering:

- **The implementations suite is a real port.** 3,223 of 4,115 lines are upstream
  case bodies that run today — 78%. Another 332 lines are the `Static` blocks that
  need `dxdb-query`; 434 are the multi-instance and wrapper cases declared with a
  reason; 126 are imports and mocha scaffolding with no Rust equivalent.
- **The correctness suite is not 2,663 ported lines.** 2,287 of its lines are group
  *data* — schemas, documents, 89 query literals with expected ids. All of it is
  extracted, committed and asserted against, and a wrong expected-id list fails a
  case (there is a test that perturbs one and asserts the failure). But it is data
  this crate did not write, so it is counted separately and not claimed as code. The
  code this crate wrote for that suite is 213 lines, of which 138 run: the harness's
  query and count assertions plus the group runner.

So the honest summary is: **the implementations suite is 78% ported and running;
the correctness suite's *data* is 100% extracted and asserted, but the code behind
it is 138 running lines, not 2,663.** A report that said "both suites ported" would
be true of the data and false of the code.

---

## 6. Gates

Run from the worktree root on the merged result.

| gate | command | exit |
| --- | --- | ---: |
| check | `make check` | 0 |
| clippy | `make clippy` (`--deny warnings`) | 0 |
| test | `make test` | 0 |
| wasm | `make wasm` (`wasm32-unknown-unknown`) | 0 |
| storage | `make gate-storage` | 0 |
| query correctness | `make gate-query-correctness` | 0 |

Test counts: 37 tests in `tests/declaration.rs` (the testkit's own honesty checks),
50 in `tests/storage_implementations.rs` (48 ported cases + 2 anti-vacuity checks),
28 in `tests/query_correctness.rs` (25 groups + 3 meta-checks).

### The gates are not vacuous

`dxdb-memory` is still a stub, so without something else the two gate targets would
run zero assertions and exit 0 — the "green for the wrong reason" failure this repo
has already been bitten by once. `tests/support/reference_storage.rs` is a
deliberately small storage that exists so the gates execute. Its module docs say
plainly that a real storage passing this suite is not thereby correct.

Three checks keep the suite from being decorative:

- `every_exercised_case_is_covered_by_a_test_in_this_file` — the `#[test]` list is
  checked against the declaration table, so a case cannot become `Exercised` and
  silently not run.
- `the_suite_catches_a_wrong_storage` — a storage that accepts every write and keeps
  nothing must fail both the insert and the conflict case.
- `the_suite_catches_a_wrong_expectation` — a group whose expected order is reversed
  must fail the comparison.

---

## 7. Cases that could not be ported, and why

All 42 unexercised-or-partial cases, by reason. Nothing here is skipped silently;
each has a row in the declaration table carrying the same reason.

### Summary

| reason | cases | what it is |
| --- | ---: | --- |
| `Exercised` (fully) | 48 | every assertion upstream makes in the case runs |
| `PartiallyExercised` | 25 | the storage half runs; the oracle half needs `dxdb-query` |
| `NeedsQueryCrate` | 8 | `.prepareQuery()` / `.getSortComparator()` / `.getQueryMatcher()` |
| `NeedsMultiInstance` | 6 | needs two instances sharing one collection |
| `NeedsKeyCompressionWrapper` | 2 | needs the key-compression wrapper to exist |
| `NeedsEncryptionWrapper` | 1 | needs the encryption wrapper to exist |
| **total** | **90** | |

### `NeedsQueryCrate` — 8 cases (the `Static` blocks)

`.prepareQuery()` ×2 (`:1180`, `:1197`), `.getSortComparator()` ×3 (`:1216`,
`:1275`, `:1324`), `.getQueryMatcher()` ×3 (`:1383`, `:1424`, `:1457`).
`dxdb-query` is lane B's crate and is not in the workspace. The harness therefore
supplies the weakest possible `PreparedQuery` (no bounds, neither satisfaction flag
set), which forces a storage to filter and sort for itself — the harder half of the
contract. These 8 cases unblock by merging lane B, not by rewriting a case.

### `NeedsMultiInstance` — 6 cases

`:3757`, `:3789`, `:3862`, `:3890`, `:3971`, `:4026`. These need a layer that shares
state between two instances on one collection and forwards change events between
them — upstream's `rx-storage-multiinstance.ts`. No such layer exists in this
workspace yet. Note that the *schema-version-isolation* case (`:4046`) does run,
because two instances on one collection with the flag off is enough to prove the
collection key includes the version.

### `NeedsKeyCompressionWrapper` — 2 cases

`:218` (asserting the refusal) and `:728` (a write against a compressed schema).
The refusal is the wrapper stack's property, not a bare storage's. The reference
scaffold does refuse `keyCompression: true`, so the *behaviour* is exercised in
`tests/support/reference_storage.rs`, but upstream's case needs the wrapper to exist
to be a port of upstream.

### `NeedsEncryptionWrapper` — 1 case

`:244`, the same shape as the key-compression refusal, for `encrypted` fields.

### Partially exercised but not fully — 25 groups

Assertions 1 and 2 of each group need `dxdb-query`; see §4.

---

## 7a. What the reference scaffold cannot evaluate

This is the honest answer to "where does the suite look complete and is not". The
scaffold in `tests/support/reference_storage.rs` is what the gates run against while
`dxdb-memory` is a stub, so anywhere it is weaker than a real storage is a place the
suite is measuring the scaffold rather than the contract.

**Operators.** The correctness fixtures use exactly 15 operators:
`$and $elemMatch $eq $gt $gte $in $lt $lte $ne $nin $options $or $regex $size $type`.
The scaffold implements all 15, and `fixture_operators_are_in_the_documented_set`
asserts that the fixture set is a subset of both PLAN.md 4.3's list and the
scaffold's supported list — so a new fixture using an unimplemented operator fails
loudly instead of quietly matching nothing.

**`$regex`.** Three patterns are used: `^bar[13]$`, `^bar4$`, `^ops$`. The scaffold
originally refused character classes, which made `$elemMatch/$size` fail — and that
was the *right* failure, because a matcher that silently answered "no match" for a
pattern it could not evaluate would turn the case into a false pass. Character
classes and ranges are now implemented for this reason. Backtracking constructs
(`+ * ? |` groups, escapes, `.`) still panic rather than guess; none are used, and
adding a regex engine to a test scaffold is not a dependency a conformance suite
should want.

**Array-field equality.** `$eq`/`$in` against an array field match by *membership*
(`{skills: 'a'}` matches `{skills: ['a','b']}`), which upstream's
"$in for array fields named skills" group asserts. The scaffold originally compared
whole arrays and reported both documents as non-matching; that was a scaffold bug,
now fixed, and it is the reason that group failed before.

**What the scaffold still does not do**, and which therefore means these behaviours
are proven by *no* case today:

| not implemented | consequence |
| --- | --- |
| indexes | `count()` always reports `Slow`; no case asserts `Fast`, so the `CountMode` contract is untested |
| multi-instance | 6 cases declared as blocked rather than faked |
| encryption / key compression | 3 cases declared as blocked |
| attachment payload digests | the scaffold verifies a digest but does not compute one; `sha256_hex` in the crate is NIST-vector-tested instead |
| `SortDirection::Desc` via index | the scaffold always re-sorts, so a storage that claims `sort_satisfied_by_index` on a descending sort is not caught |
| unknown-index rejection | the scaffold never returns `StorageError::UnknownIndex`, so that error path is untested |

Those six rows are the gaps. None of them are hidden by a passing test; the third
column is what a reader should treat as "not yet measured by this lane".

## 8. Bugs this lane found in its own work

Recorded because each was a place where the testkit would have demanded behaviour
upstream does not have, and a storage author would have chased a phantom.

1. **`count()` and tombstones.** The first version of `count_correct_amount`
   asserted that `count()` excludes deleted documents. Upstream says the opposite in
   as many words: "*must still be 2 because the storage itself also counts deleted
   docs*" (`:2172`). Fixed, and the case now asserts both halves — the storage does
   not filter, and with `_deleted: false` injected by the caller it does.

2. **An empty selector must not filter `_deleted`.** Consequence of (1). The
   correctness suite injects `_deleted: false` itself (`:212`), which is why the
   crate has **two** query builders, `build_prepared_query` and
   `build_correctness_query`, rather than one with a flag. One function with a flag
   is what produced the mix-up.

3. **Change streams subscribe from now.** A first draft replayed history from the
   start, so a stream opened before a write saw an older event first and made
   "should not emit an empty eventBulk when the write had only errors" fail against a
   correct storage.

4. **`remove()` after `close()` must error while `close()` after `remove()` must
   not.** Two separate upstream cases (`:3676`, `:3693`), and the asymmetry is the
   contract.

5. **`fillWithDefaultSettings` is not idempotent.** Upstream appends the change-feed
   index `['_meta.lwt', primary]` on every call, *after* the `_deleted`-prefixing loop
   (`rx-schema-helper.ts:283`), so a second fill produces
   `['_deleted', '_meta.lwt', primary]` plus a fresh unprefixed copy. A downstream
   lane that re-fills a stored schema gets a corrupt index list and a change feed that
   silently misses the newest documents. The testkit pins this as documented behaviour
   with a test, so it is caught if it changes rather than rediscovered.

6. **A hardcoded fixture constant was wrong, not the extraction.** A meta-test
   asserted that 5 correctness queries carry a `skip` or a `limit`; the extraction
   found 3. The extraction was right: upstream has exactly three (`:873` with
   `limit: 50`, and the two in "issue: wrong results using skip and limit"). The
   guess had been written before the extraction existed. The constant is now 3, and
   the verification command is recorded next to it so the next person checks rather
   than re-guesses.

7. **The change-feed index must not be `_deleted`-prefixed.** My first fixture test
   asserted every filled index starts with `_deleted`; the change-feed index is the
   one deliberate exception, because a `_deleted` prefix would exclude exactly the
   tombstones it exists to find.

---

## 9. Divergences from upstream, all deliberate

| divergence | why |
| --- | --- |
| randomness is a seedable generator (`DXDB_TESTKIT_SEED`) not `Math.random` | a conformance failure must replay; a fixed default means it does by default |
| waiting yields to the executor instead of `setTimeout` | `wasm32-unknown-unknown` has no thread to sleep on; upstream's retry *order* is preserved exactly |
| `wait_until` bounds by attempts, not milliseconds | the default sleeper has no clock; a bound must still exist or a wrong assertion hangs the suite |
| `localeCompare`-based sorts are not reproduced | D5: code-unit order is the contract |
| error assertions use kind + fields, not message strings | the brief's fidelity rule 3; the divergence is noted at each assertion |
| parallel writes/reads are issued sequentially | D3 makes the sequential executor the default; the invariant under test does not depend on interleaving, and each such case says so in a comment |

---

## 10. What the next lane needs to do

One thing, and it is small: **when `dxdb-memory` exists, add a second module to
`tests/storage_implementations.rs` that runs the same suite against it.** The case
functions are generic over `RxStorage`, so that is a `mod` and a loop, not a
rewrite. `tests/support/reference_storage.rs` should then be deleted, or kept only
as the anti-vacuity fixture it was written to be.

The second is `dxdb-query` (lane B). Merging it turns 25 `PartiallyExercised`
groups into `Exercised` and 8 `NeedsQueryCrate` cases into running cases, which
takes the fully-exercised count from 48 to 81 without touching this crate's source:
`build_correctness_query` is the single function that changes.
