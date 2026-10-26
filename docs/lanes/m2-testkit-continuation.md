# Lane continuation: the conformance testkit (M2)

Written for a fresh session. Read this, then `docs/lanes/m2-testkit.md` (the original
brief), then `crates/dxdb-testkit/LANE_REPORT.md` (the lane's own accounting).

Branch `lane/m2-testkit`. Owns `crates/dxdb-testkit/` only.

---

## 1. Merged to master

All lane work up to and including the opfs row is on master. Lane commits, newest first:

| hash | what |
| --- | --- |
| `51e12fa` | merge lane/m2-testkit: the opfs row, and a storage list read from the filesystem |
| `3b941a2` | opfs row + `every_storage_crate_is_accounted_for` reads `crates/storages/` |
| `a8e04ed` | merge lane/m2-testkit: five storage rows and a differential fuzz harness |
| `36154d3` | clippy clean on the fuzz harness |
| `6a4bb8e` | known-violation baseline |
| `f22628a` | five storage rows + differential fuzz harness |
| `c08d9a0` | storage matrix + three fixture bugs in the lane report |
| `ae06095` | real schema filling, storage matrix, three fixture bugs |
| `103bc10` | three real failures (skip/limit constant, `$regex` classes, array `$eq`) |
| `dcd4a7d` | repaired a conflict marker that reached `Cargo.lock` |

`docs/lanes/m2-testkit.md` is the original brief and is still accurate about scope.

Superseded: the older `docs/lanes/m2-testkit-continuation.md` (if any) — this file replaces it.

---

## 2. In progress right now

**One file, committed but never run to completion:**

`crates/dxdb-testkit/tests/operator_coverage.rs` — new, ~470 lines.

- `cargo check -p dxdb-testkit --all-targets` → **exit 0** (verified).
- The test run stayed queued behind the shared build lock for the whole session and never
  produced output. **Its pass/fail state is unknown — it has never executed.** The first
  thing to do is run it (see §3). Treat it as unvalidated code.

Everything else in the worktree is committed and green (last full gate sweep: check,
clippy, test, wasm, gate-storage, gate-query-correctness all exit 0).

### What the file contains

- `DOCUMENTED_OPERATORS: &[&str]` — the 19 operators from PLAN.md 4.3, mirroring
  upstream `src/types/rx-query.d.ts:43`. Single source of truth.
- `fixture_documents()` — 12 documents, unique ids `a1..a6,b1..b4,c1,c2`, chosen so each
  operator has both matching and non-matching docs and so a limit of 2 / skip of 1 both
  land inside the result set. `c1` deliberately omits `opt` (for `$exists: false`), `c2`
  has a string in `score` (for `$type`).
- `cases()` — 19 `OperatorCase` entries, one per operator. `$options` rides with `$regex`
  (it is the flag set, not a standalone selector).
- `run_case()` — per operator, asserts four clause invariants:
  1. `count(no limit) == query().len()`
  2. `query(limit=k) == all[..k]`
  3. `query(skip=1) == all[1..]`
  4. `query(skip=1, limit=2) == all[1..][..2]`
  plus two fixture-sanity assertions (the selector must match *some* but *not all*
  documents, or the clauses are vacuous).
- `every_operator_has_sort_limit_and_skip_against_the_control()` — runs against the
  reference scaffold (the control).
- `every_operator_works_on_every_storage()` — memory, indexeddb, opfs, localstorage.
  Catches per storage so one storage's bug does not hide the rest.
- `every_documented_operator_is_covered_with_every_clause()` — fails if an operator in
  `DOCUMENTED_OPERATORS` has no case, and fails in reverse if a case exists for an
  undocumented operator.
- `fixture_is_large_enough_for_the_clauses_to_bite()` — >= 10 docs, unique primaries.

---

## 3. The exact next task

```bash
cd /home/bitnom/Code/dxdb-worktrees/m2-testkit
git merge master                       # lane was 11 commits behind at reset
/tmp/runall.sh test -- -p dxdb-testkit --test operator_coverage -- --nocapture
```

`/tmp/runall.sh` is a scratch helper that resolves the build route then runs cargo under
the lock. **It will not exist in a fresh session — recreate it:**

```bash
#!/bin/sh
cd /home/bitnom/Code/dxdb-worktrees/m2-testkit
ROUTE=$(xtask/remote-route.sh 2>/dev/null)
case "$ROUTE" in
  proxy:*) CARGO="cargo remote-3000 -H fly-builder-proxy -p ${ROUTE#proxy:} -d 1.95.0" ;;
  *)       CARGO="cargo remote-3000 -r fly -d 1.95.0" ;;
esac
exec xtask/with-build-lock.sh $CARGO "$@"
```

Never `flock` a `make` target yourself, and never build locally.

### Then, expected outcomes

The 19 operator cases are new assertions against real storages. Some **will** fail; that is
the point of the exercise. Triage each failure as one of:

- **a bug in `operator_coverage.rs`** (my fixture or my invariant is wrong) → fix it here;
- **a bug in a storage** → do not fix it (other lanes own those crates). Record it in
  `KNOWN_INVARIANT_VIOLATIONS`-style fashion or in `LANE_REPORT.md`, and tell the leader.
  `dxdb-localstorage` already has a known `run_query` bug (see §4) and will very likely
  fail several of these.

Then:

```bash
make check && make clippy && make test && make wasm
cargo fmt -p dxdb-testkit          # NEVER `cargo fmt --all`
git add crates/dxdb-testkit/       # NEVER `git add -A`
```

Commit. Update `crates/dxdb-testkit/LANE_REPORT.md` §5a with the operator matrix.

### Alternative next task if operator coverage lands cleanly

The ported suite's `Static` cases (8, `PortStatus::NeedsQueryCrate`) are now unblockable:
`dxdb-query` is merged and exports `prepare_query`, `get_query_plan`,
`get_sort_comparator`. Wiring them flips 8 `NeedsQueryCrate` rows to `Exercised` and lets
the 25 `PartiallyExercised` correctness groups recompute their own oracle instead of taking
expected ids as given. Files: `crates/dxdb-testkit/src/harness.rs` (the one function
`build_correctness_query`), `src/suite.rs`, `src/decl/mod.rs` (status enum).

---

## 4. Not written down in the repo — traps and decisions

### The three fixture bugs, and why they matter more than they look

Each of these made a **correct** storage look broken. Every future fixture must avoid them.

1. **The RNG was `thread_local`.** Every `#[test]` runs on a fresh thread, so every test
   started from the same seed and drew the **same** "random" database name.
   `dxdb-memory` keys its registry on the collection key process-wide, so two tests shared
   one collection and the second's inserts conflicted with the first's tombstones — 8
   failures that read as storage bugs. The generator is now a `static` behind a `Mutex`.
   There is a test `random_tokens_differ_across_threads` pinning it.

2. **`cleanup(minimumDeletedTime)` is a DURATION, not a deadline.** Upstream calls
   `cleanup(0)` to purge everything and `cleanup(1000*60*6)` to purge only six-minute-old
   tombstones (`rx-storage-implementations.test.ts:3441`, `:3451`), looping
   `while (!await cleanup(...))`. Passing an absolute timestamp made cleanup purge too much
   or too little depending on the fix attempt.

3. **The fixture `lwt` was frozen in 2023**, then overcorrected into the future.
   - `1_700_000_000_000` is 2023-11-14, so every tombstone was 1,076 days old and a
     6-minute retention window correctly purged it.
   - Fixing that to `base + n` where `base = now()` put `lwt` *milliseconds in the future*,
     so `cleanup(0)` (cutoff = exactly `now()`) correctly refused to purge.
   - Final: `base_lwt() = now().floor() - 60_000` (60 s in the past), `lwt(n) = base + n`,
     ascending. Satisfies ordering, recency and never-future at once. Pinned by
     `suite_fixture_timestamps_are_recent`.

### `fill_with_default_settings` must call `dxdb-schema`, never be hand-rolled

The testkit used to inject `_meta` as a bare `{"type": "object"}` with no `lwt`, while
`fillWithDefaultSettings` prepends `_meta.lwt` to essentially every index. The result was a
schema whose own index could not be resolved:

```
CI1: index field `_meta.lwt` is not in the schema
```

`dxdb-indexeddb` failed 43 of 49 cases on that **before any storage code ran**. The function
in `src/harness.rs` is now a one-line delegation to `dxdb_schema::fill_with_default_settings`.
Never reintroduce a local fill.

Two regression tests guard the class: `fixture_schema_declares_the_meta_lwt_its_indexes_reference`
and `every_index_field_resolves_against_the_filled_properties`. Note the latter had to walk
schema `properties` differently from document paths — a schema level is
`{"type": "object", "properties": {...}}`, so the next segment is a key of the **nested**
`properties`, not of the level itself. `get_object_path` is for documents, not schemas.

### `_deleted` handling: two builders, and the asymmetry

- A **bare storage does not filter tombstones.** Upstream says so verbatim
  (`rx-storage-implementations.test.ts:2172`, *"must still be 2 because the storage itself
  also counts deleted docs"*).
- The **query-correctness suite injects `_deleted: false` itself**
  (`rx-storage-query-correctness.test.ts:212`).

So `harness.rs` has two builders and they must stay separate:
`build_prepared_query` / `build_prepared_query_for_schema` (no injection — the
implementations suite) and `build_correctness_query` (injects — the correctness suite).
One function with a flag produced a genuinely wrong assertion once.

### A query plan must name an index the schema actually has

An earlier `planned_index` synthesised an index name from the sort fields
(`_deleted,value,key`), which no schema declared, so storages correctly returned
`UnknownIndex` and three `dxdb-memory` cases failed. It now picks a materialised index, or
falls back to the change-feed index `['_meta.lwt', primary]`. Note the change-feed index is
**legitimately not `_deleted`-prefixed** — upstream appends it after the prefixing loop
(`rx-schema-helper.ts:283`) because it must enumerate tombstones.

### Known storage bugs found by this lane (report, do not fix)

| storage | bug | evidence |
| --- | --- | --- |
| `dxdb-localstorage` | `run_query` returns no documents for **any** selector, including `{}`, when the plan names a maintained index. `find_documents_by_id` works. Encoded lower bound resolves to boolean `false` because `_deleted` is the first index field. | conformance `impl-query-03` (`:1626`), `impl-count-01` (`:2123`); fuzz fails 24/24 seeds. Both instruments agree. |
| `dxdb-sqlite` | **not a storage**: no `dxdb-storage` dependency, no `RxStorage` impl. It is a `SqliteDriver` trait + rusqlite driver. Verified by lane G's `xtask/m5-conformance.sh` instead. | matrix row `Unavailable::StorageMissing` |

`dxdb-filesystem`'s attachment-payload bug (an unchanged attachment's payload did not
survive a later write) was reported and **has since been fixed** by its lane; filesystem is
48/48.

### The known-violation baseline

`KNOWN_INVARIANT_VIOLATIONS` in `tests/differential_fuzz.rs` names `localstorage` with its
reason. The fuzz test asserts **both directions**: a new violation fails the build, and a
listed storage that starts *passing* also fails (so the entry cannot rot into a permanent
excuse). If localstorage's bug is fixed, that test will go red until the entry is removed —
that is intentional.

### Test-file naming is load-bearing

`make gate-storage` runs `--test storage_implementations`; `make gate-query-correctness`
runs `--test query_correctness`. Those two filenames are fixed by the Makefile.

### Build-box traps hit repeatedly

- `-p` is claimed by `cargo-remote-3000` as `--ssh-port`. The Makefile passes `--` before
  it. A hand-written `cargo remote-3000 test -p x` fails; use `/tmp/runall.sh` (above).
- `git add -A` in this worktree staged **145 files belonging to five other lanes**. Always
  `git add crates/dxdb-testkit/`.
- `Cargo.lock` conflict markers have reached a commit once. If it happens, fix it and verify
  with a TOML parse + "every dependency resolves", not by eye.
- The Fly box restarts and loses `/root/.cargo`; `make remote-repair` fixes it. Route
  failures are handled by `xtask/remote-route.sh` which only the Makefile and
  `/tmp/runall.sh` use.

---

## 5. Blocked / questions for the leader

**Nothing blocking.**

One open preference question, low stakes: `dxdb-localstorage`'s fuzz failures are recorded
in `KNOWN_INVARIANT_VIOLATIONS` rather than failing `make test` for the whole workspace.
The alternative is that this lane owns a red gate until localstorage is fixed. Current
choice keeps other lanes unblocked while keeping the finding visible and self-removing.

One correction worth recording because it cost a cycle: the leader reported the matrix was
missing sqlite; it was not (the row builder is `sqlite_row()`, not a `storage: "…"` literal).
The real gap was opfs, now added and **runnable** via `MemoryFiles` — not marked unavailable.
The lesson applied: `every_storage_crate_is_accounted_for` now reads `crates/storages/` from
the filesystem instead of a hardcoded list, verified by adding a probe crate
(`dxdb-probe`) and watching the test fail, then removing it.

### Operator coverage matrix as measured (pre-fill)

The 19 documented operators vs the ported correctness suite's 89 queries. This is what
`operator_coverage.rs` exists to fill; the columns are the *existing* ported coverage.

| operator | cases | with sort | with limit | with skip | gap |
| --- | ---: | ---: | ---: | ---: | --- |
| `$eq` | 15 | 15 | 1 | 1 | |
| `$gt` | 19 | 17 | 2 | 2 | |
| `$gte` | 12 | 9 | 0 | 0 | no limit, no skip |
| `$lt` | 19 | 18 | 2 | 1 | |
| `$lte` | 10 | 9 | 1 | 1 | |
| `$ne` | 2 | 2 | 0 | 0 | no limit, no skip |
| `$in` | 10 | 6 | 1 | 0 | no skip |
| `$nin` | 4 | 3 | 0 | 0 | no limit, no skip |
| `$regex` | 3 | 3 | 0 | 0 | no limit, no skip |
| `$options` | 2 | 2 | 0 | 0 | no limit, no skip |
| `$exists` | 0 | 0 | 0 | 0 | **no case at all** |
| `$type` | 2 | 2 | 0 | 0 | no limit, no skip |
| `$mod` | 0 | 0 | 0 | 0 | **no case at all** |
| `$not` | 0 | 0 | 0 | 0 | **no case at all** |
| `$size` | 1 | 1 | 0 | 0 | no limit, no skip |
| `$elemMatch` | 6 | 6 | 0 | 0 | no limit, no skip |
| `$and` | 1 | 1 | 0 | 0 | no limit, no skip |
| `$or` | 5 | 5 | 0 | 0 | no limit, no skip |
| `$nor` | 0 | 0 | 0 | 0 | **no case at all** |

4 operators have no case; 10 more have neither a limit nor a skip. `operator_coverage.rs`
supplies all 19 with sort + limit + skip against four storages.

---

## Current storage matrix (for reference)

| storage | declared | exercised | passed | failed | not run |
| --- | ---: | ---: | ---: | ---: | --- |
| reference-scaffold | 48 | 48 | 48 | 0 | 0 |
| memory | 48 | 48 | 48 | 0 | 0 |
| indexeddb | 48 | 48 | 48 | 0 | 0 |
| opfs | 48 | 48 | 48 | 0 | 0 |
| filesystem | 48 | 48 | 48 | 0 | 0 |
| localstorage | 48 | 48 | 46 | 2 | 0 |
| sqlite | 48 | 0 | 0 | 0 | 48 (`StorageMissing`) |

Fuzz (24 seeds × 60 ops, same seeds everywhere): scaffold, memory, indexeddb, opfs,
filesystem all satisfy every invariant; localstorage violates all 24 seeds (known).

"Declared 48" is `PortStatus::Exercised` implementations cases. The declaration table holds
90 rows total: 65 implementations (48 exercised, 8 `NeedsQueryCrate`, 6 `NeedsMultiInstance`,
2 `NeedsKeyCompressionWrapper`, 1 `NeedsEncryptionWrapper`) and 25 correctness groups
(all `PartiallyExercised` — storage half runs, oracle half needs `dxdb-query`).
