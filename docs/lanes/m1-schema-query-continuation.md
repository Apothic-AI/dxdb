# Lane B (M1 schema and query) — continuation

Written at a context reset, 2026-10-26. Read this, then `docs/DECISIONS.md` (D5 and D8 are
mine), then `docs/lanes/m1-schema-query.md` (the original brief; its "what to build" is all
done). Nothing else is needed to resume.

Worktree: `/home/bitnom/Code/dxdb-worktrees/m1-schema-query`, branch `lane/m1-schema-query`.
Owned paths: `crates/dxdb-schema/` and `crates/dxdb-query/`. Nothing else. `xtask/vectors/`
and `testdata/vectors/` belong to lane C — import their machinery, never write there.

Gates: `make check`, `make clippy`, `make test`, `make wasm`. All four, exit 0. Never build
locally, never wrap `make` in your own `flock` (the Makefile locks, reentrantly).

---

## 1. Merged to master

Both crates are merged and have been for many rounds. Master commits that carry my work
(the leader squashes on merge, so hashes alone are not a complete record — `git log master`
plus the subjects below is):

| commit | what |
| --- | --- |
| `1927add` | first merge: `dxdb-schema` + `dxdb-query` |
| `dc1396e`-era round | index-strings vector family wired (900 cases) |
| `188cf85` | `$not` involution and complement proptest properties |
| `03f969a` | every vector test migrated onto `dxdb-vectors` (killed 3 private readers) |
| `0474123` | **matcher differential fuzz corpus + the two bugs it found** |
| `83e6954` | numeric-boundary corpus for the index-string encoder (15 schemas, 189 values) |
| `83af3aa` | operator-interaction corpus (72 compound selectors × 23 documents) |

Master status at the reset: 158 suites green, 41 crates, audit 11/11, module coverage 80/80.
`dxdb-query` and `dxdb-schema` are feature-complete against the brief.

### Test inventory (all on master, all green)

Integration tests in `crates/dxdb-query/tests/`:

| file | tests | covers |
| --- | --- | --- |
| `index_strings_vectors.rs` | 4 | lane C's 900-case family via `dxdb-vectors` |
| `normalised_queries_vectors.rs` | 5 | 798 cases: normalisation, plan, `planJson` bytes, round trip |
| `selector_matching_vectors.rs` | 4 | 60 match cases **both directions** + 3 refusals |
| `oracle_sentinels.rs` | 4 | D8: `INDEX_MIN`/`INDEX_MAX` survive `to_js_json` → parse |
| `ported_rx_query.rs` | 26 | `test/unit/rx-query.test.ts` port |
| `ported_rx_query_helper.rs` | 8 | `test/unit/rx-query-helper.test.ts` port |
| `properties.rs` | 15 | proptest consistency (incl. `$not` involution, `$not` complement) |
| `matcher_fuzz_differential.rs` | 4 | 240 selectors × 160 docs = 38,400 assertions |
| `index_boundaries.rs` | 7 | 15 schemas × 189 boundary values + clamp/monotonicity |
| `operator_interactions.rs` | 7 | 72 compound selectors × 23 docs = 1,656 assertions |
| `sort_ties.rs` | 8 | **IN PROGRESS — see §2** |

`schema-hashes_vectors.rs` (5 tests) in `crates/dxdb-schema/tests/`. Unit modules:
`dxdb-schema/src/tests.rs` (39), `dxdb-query/src/custom_index/tests.rs` (19),
`dxdb-schema/src/json_schema.rs` (3).

---

## 2. In progress right now

### 2a. `crates/dxdb-query/tests/sort_ties.rs` — 7 of 8 passing

Sort ties and stability (the leader's item 1). Supporting files, both uncommitted:
`tests/support/sort-ties.mjs` (generator) and `tests/support/sort-ties.json` (17 cases,
382 pairwise comparisons, 7 corpora, upstream-generated).

**A real bug was found and fixed in `crates/dxdb-query/src/sort.rs`** — uncommitted. The
comparator decided ties with `serde_json::Value` equality, which says `1 != 1.0`; JavaScript
has one number type, so those documents are *tied* and the primary key must break the tie. The
old code treated them as different values, fell through to a numeric comparison that returned
`Equal`, and **returned `Ordering::Equal` from the whole comparator**, stopping the sort early.
Result: non-deterministic order across input permutations.

Fix, in `sort.rs`:
- new `fn js_equal(a: &Option<Value>, b: &Option<Value>) -> bool` — compares numbers by `f64`,
  everything else by `Value` equality. Used for the per-part tie test.
- the numeric/string fast path now `continue`s when it yields `Ordering::Equal` (covers `-0.0`
  vs `0.0` and equal-comparing string pairs) instead of returning `Equal`.

**Remaining failure: `one_and_one_point_zero_are_tied`.** Assertion at
`sort_ties.rs:342`-ish: `int-first: the primary key must break the tie identically either way`
— got `["int","float"]`, expected `["float","int"]`. The `n`-only tie assertion now passes
(both directions `Equal`), so `js_equal` works; the residual failure means the *normalised*
`s`-only comparator is still not falling through to `id` for this pair. Suspect: `resolve_field`
returns `Some(Value::Number(1))` vs `Some(Value::Number(1.0))` and `js_equal` is not reached
before some other early return, or the corpus' `sortedIds` and the `id` values disagree.
**Debug by asserting `n_only.compare` then `with_pk.compare` in isolation** — the test already
does the former; add the latter. Do not weaken the assertion.

Two other test-side errors were already fixed and are worth not re-introducing:
- `every_normalized_sort_appends_the_primary_key_as_ascending` must allow a **caller-provided**
  `{id: 'desc'}` to survive unchanged; only the *appended* part is forced `asc`.
- `any_desc_part_forces_a_re_sort` is one-directional. `sortSatisfiedByIndex` can be false for
  an *ascending* sort too, because `fillWithDefaultSettings` puts `_deleted` first on every
  index, so a sort not leading with `_deleted` never matches. Asserting `== !has_desc` is wrong.

### 2b. Not started: `$options` / `$regex` flag parity (the leader's item 2)

`crates/dxdb-query/src/matcher.rs`, `RegexFlags::parse` (~line 171). Probed against upstream
mingo 7.2.5 today; **the leader's brief is wrong on one point and the real behaviour is**:

| `$options` | upstream |
| --- | --- |
| `""` `i` `m` `s` `u` `v` `g` `y` `d` | accepted |
| `ims`, `ui` (combined) | accepted |
| **`x`** | **THROWS** `Invalid flags supplied to RegExp constructor 'x'` — there is no `x` flag in ECMAScript. Do not add one. |
| `ii` (duplicate) | THROWS |
| `z` (unknown) | THROWS |

Flag effects confirmed: `s` makes `.` match `\n`; `m` anchors per line; `i` folds case;
`u` enables `\p{Letter}`.

Current implementation gaps, in order of importance:
1. **`d` is missing** — upstream accepts it, we throw. Add `has_indices` (or accept-and-ignore,
   since `d` only affects capture-group indices which Mango never reads; **document which**).
2. **Duplicate flags are not detected** — `ii` is accepted by us and throws upstream. Need a
   seen-flag set in `parse`.
3. `x` must stay rejected (we already reject it — do **not** "add `x`" on the leader's say-so
   without telling them it is not a JS flag).

Then: a generated corpus `tests/support/regex-flags.mjs` + `tests/regex_flags.rs` asserting
per-flag behaviour against upstream, and that each invalid string is an *error* (not a silent
no-match). Do not put it in `crates/dxdb-query/src/matcher.rs` unit tests — the expectations
must come from upstream, so it is a corpus.

---

## 3. Exact next task

1. Fix `one_and_one_point_zero_are_tied` (§2a) — debug in isolation, do not weaken.
2. `make check && make clippy && make test && make wasm`, then commit
   `sort.rs` + `sort_ties.rs` + `support/sort-ties.{mjs,json}` in one commit. Gate first.
3. Add the three `RegexFlags::parse` fixes (§2b), with the `d` decision documented in the doc
   comment.
4. Write `tests/support/regex-flags.mjs` (clone the structure of
   `tests/support/operator-interactions.mjs`: import `xtask/vectors/{harness,vendor}.mjs`,
   install determinism *before* `loadOracle`, emit to `tests/support/`, support `--check`) and
   `tests/regex_flags.rs`. Cover: each accepted flag's observable effect, combined flags,
   duplicate → error, unknown → error, `x` → error.
5. Gates, commit, push `lane/m1-schema-query`, report.

---

## 4. Things not written down elsewhere

### A regression that cost real time — read this

`3cd598b` ("rustfmt the merged tree after the error-vectors merge") **deleted 25,227 lines**
of my work: `matcher_fuzz_differential.rs` (288 lines),
`tests/support/matcher-fuzz.mjs` (277), `tests/support/matcher-fuzz.json` (24,662). The files
remained as 0–1 byte stubs, so the test silently vanished and **the fuzz differential was not
running on master from 03:47 to when it was noticed**. A "rustfmt" commit deleted whole files;
whatever tool did it (`cargo fmt --all` on a tree with a stale file list? an editor?) is still
unexplained.

Restored by `git checkout b8e7382 -- <the three paths>`; verified the corpus is intact
(160 docs, 240 selectors, 38,400 assertions, pin `3b763ee17d62`). **This is uncommitted.**

Two lessons:
- A file that is *empty* passes no test and fails no build. Check `git ls-tree -r -l | awk '$4==0'`
  occasionally; that is how this was found.
- "rustfmt" is not a safe commit message to trust. Diff the stat: a formatter does not delete
  files.

### Upstream behaviours discovered by probing (cost several rounds each — do not re-derive)

1. **`$mod` coerces its operand.** It is `x % b[0] === b[1]`, and JS `%` coerces, so
   `true % 3 === 1`, `null % 3 === 0`, `undefined % 3 === NaN`, `"6" % 3 === 0`. Requiring a
   JSON number made `{flag: {$mod: [3,1]}}` match nothing. Fixed with `js_to_number`.
   **Ordering comparisons deliberately do NOT coerce** (mingo uses `typeOf`): `{$gt: 0}` matches
   only numbers while `{$mod: [1,0]}` matches booleans/null/numeric strings. Keep them separate.
2. **Array-index paths.** mingo's `getValue` does `obj[Number(key)]` for arrays; serde's
   `Value::get` is object-only. `{tags.0: {$exists: true}}` matched *nothing* until fixed in
   both `resolve_path` and `resolve_path_from`.
3. **`$type: 'null'` is `=== null`**, not `isNil`. Only `'undefined'` (and the deprecated
   numeric `6`) are `isNil`. So `$type: 'null'` excludes an absent field while `$eq: null`
   includes it. Mutation-guarded in `operator_interactions.rs`.
4. **`undefined` is indistinguishable from absent** for every operator — verified with a real
   JS `{f: undefined}`. So there are **only two observable states** and `Option<&Value>` is a
   complete model. Do not invent a third; `operator_interactions.rs::undefined_is_indistinguishable_from_absent`
   records the evidence.
5. **The index-string clamp boundary is `floor(minimum)`/`ceil(maximum)`**, not the declared
   values. On `minimum: 0.5, maximum: 10.5` the encodable range is `[0, 11]`, so `0.4` does
   **not** clamp (`0.4 -> "0004"`). Got this wrong in a test; upstream's own output corrected it.
6. **`INDEX_MAX` bound values are sentinels by value**: `BoundValue::from_json` recognises
   `"\u{FFFF}"` as `IndexMax` and `-9007199254740991` as `IndexMin` before anything else, for
   *any* field type, because upstream compares `bound === INDEX_MIN` by identity.
7. **`lower-null` vs `lower-undefined`:** `index_strings::BoundsCase::bound` is
   `Option<serde_json::Value>` and serde maps both an explicit `null` and an absent key to
   `None`. The test distinguishes them by the case's `why` label. **Gap in `dxdb-vectors`,
   reported to the leader, still open** — the proper fix is a `bound_present: bool` or a
   tri-state. Do not silently "fix" the test to assume one.
8. **`_deleted` is prepended to every index, the primary key appended, and `_meta.lwt` always
   added.** Order is observable in index strings and therefore in the schema hash.
9. **A `desc` anywhere forces a re-sort** (`sort_satisfied_by_index = false`), and the
   **appended** primary key is always `asc` even under `desc`.
10. **`Schema::new` does not draft-07-validate.** Upstream does not validate its own schema
    against the draft it declares — `humanCompositePrimarySchemaLiteral` has
    `"required": []` inside `properties.readonlyProps`, which draft-07 forbids, and upstream
    accepts and hashes it. Validation is an explicit separate step. Recorded as a real
    divergence.
11. **`fill_with_default_settings` keeps a non-string index entry verbatim** (e.g. literal
    `10`), because upstream fills and *hashes* that schema and only fails at encode/plan time.
    Rejecting at fill time would change the schema hash of a fixture upstream hashes. The
    rejection belongs in `dxdb_query::index_meta` (`CI1`).

### Decisions and why

- **D8 (`INDEX_MIN` defined once).** My `dxdb-plan` crate was written, then **reverted**:
  lane D had already made `dxdb-memory` depend on `dxdb-query` and deleted their local port, so
  a shared crate would have re-wired three lanes for nothing. The definition lives in
  `dxdb-query` (`custom_index::INDEX_MIN` as `i64`, `plan::INDEX_MAX_STR` as `&str`), and
  `dxdb-storage` still carries its own `f64`/`char` copies at the time of writing.
  **Requirement 3 of D8 is done** (`oracle_sentinels.rs`). Requirement 1 (exactly one
  definition workspace-wide) is *not* enforced by a test — a workspace-grep test was tried and
  removed because it goes red for a reason outside this crate. If revisiting: put that check in
  `xtask` or CI, not in `dxdb-query`.
- **Vector tests all go through `dxdb-vectors`.** Three private serde readers were deleted.
  `dxdb-query` and `dxdb-schema` both have `dxdb-vectors` as a **dev-dependency**. Any new
  vector test must use the typed reader; if an accessor is missing, ask the leader to route it
  to lane C rather than adding a fourth reader.
- **Generators for corpora live in `crates/dxdb-query/tests/support/*.mjs`**, not
  `xtask/vectors/` (lane C's path). They *import* `xtask/vectors/harness.mjs` and
  `xtask/vectors/vendor.mjs` so the oracle is the same pinned bundle. `installDeterminism()`
  **must** run before `loadOracle()` or fixtures are built from real randomness and the corpus
  is unreproducible.
- **`$not` involution** (`$not` of `$not` is identity) was verified against mingo before being
  asserted, because a field-level `$not` re-normalises its operand rather than negating
  structurally. It is exactly identity, including for missing fields and `$exists`.
- **Mutation testing is how these suites are shown to assert.** For each new assertion, break
  the implementation plausibly and confirm the test fails *with the rule named*. Done for
  `$type:'null'`, `$eq:null`, the both-directions selector test (forcing `did_match = true`
  produced 2,294 divergences), and the refusal cases (dropping unknown operators produced the
  exact "silently 0 of 64" failure). Keep doing this; a passing suite proves nothing on its own.

### Traps

- **`cargo remote-3000` with `-p` collides with its own flag parsing.** Use `make` targets.
- **`make test` output interleaves across binaries** (cargo runs them in parallel), so grepping
  for a test name gives the wrong file. Reliable per-suite evidence: the declared `#[test]`
  count in source, plus a green overall run. Do not trust per-suite attribution from a shared log.
- **`cargo fmt --all` is forbidden** (reformats other lanes' crates). Use `cargo fmt -p <crate>`.
  Given §4's deletion incident, do not trust a "rustfmt" commit either.
- **`/tmp` is wiped by build-box restarts.** The oracle harness (a copy of upstream `src/` plus
  `mingo`/`tsx`) has been rebuilt three times at `/tmp/rxdb-oracle2`. If it is gone:
  `cp -r ~/Code/rxdb/src .`, add a `package.json` with `{"type":"module"}`, `npm i mingo
  js-base64 tsx`, done. Useful for probing semantics far faster than a Rust round trip.
- The build box drops often (`No route to host` on 2222, proxy disconnect on 2223). Retry; never
  build locally, never skip a gate.

---

## 5. Blocked / questions for the leader

1. **`matcher-fuzz` deletion (unresolved cause).** Restored from `b8e7382` in the working tree,
   uncommitted. Please confirm what emptied those three files at 13:10 so it does not happen
   again — a formatter should not delete 25k lines, and the loss was invisible for hours.
2. **`dxdb-vectors` gap:** `index_strings::BoundsCase::bound` cannot distinguish `null` from
   absent, so `lower-null` vs `lower-undefined` relies on the `why` label. Needs a
   `bound_present: bool` or tri-state from lane C. Reported earlier; still open.
3. **D8 requirement 1 is unenforced.** Exactly-one-definition is not tested. Worth a CI check
   owned by `xtask` rather than a crate. `dxdb-storage` still defines its own `INDEX_MIN`.
4. **`$options: 'x'` does not exist in ECMAScript and throws upstream.** The item-2 brief lists
   `x` among flags to support; I will not add it without a decision, since accepting it would
   make us accept a query RxDB rejects.
5. **`$options: 'd'`** is accepted upstream and rejected by us. Accept-and-ignore is probably
   right (Mango never reads capture indices), but say so if you want a different call.
