# Benchmarks

PLAN.md section 7.5 and CLEANROOM.md section 6 both make the same demand in
different words: **a performance claim without a number from this file does not
ship.** "Faster than X" is not a sentence dxdb is allowed to write until the
measurement below exists for the case being described.

This file records measurements. It does not record aspirations; the targets live
in PLAN.md (5.3 for WASM sizes) and CLEANROOM.md (5.x for the clean-room targets)
and are reproduced here only so a measured number has something to be compared
against.

## Method

- Benchmarks are criterion benches, one per storage, producing a JSON result file
  per run so CI can compare runs rather than prose.
- Runs happen on the shared Fly build box. That box is shared, so benchmark numbers
  are only comparable between runs taken when it was otherwise idle. Each result
  records whether the box was idle.
- Distances from upstream are only meaningful against upstream measured on the
  **same box in the same session**. An upstream number copied from a blog post is
  not a comparison, it is a coincidence.

### Recording a run

```
box:        fly open-surrealdb-ds-builder 87477e0cd01748 (8 cores, shared)
toolchain:  1.95.0
idle:       yes | no (see `make busy` at run start)
date:       YYYY-MM-DD
artifact:   <command that produced the number>
```

## WASM size budgets (PLAN.md 5.3)

Budgets are aspirational until measured; they get revised with numbers, not vibes.
Measure the release profile already in `Cargo.toml` (`opt-level="s"`, `lto="fat"`,
`codegen-units=1`, `panic="abort"`), then brotli.

### Status of the four budgets

Two columns per row on purpose: the binding floor is not the budget test, and the
engine-linked figure is. Both are measured; the second is the one that decides whether
PLAN.md 5.3 was a real target.

| Artifact | Budget (brotli) | Binding floor only | **With engine linked** | Status |
| --- | ---: | ---: | ---: | --- |
| `dxdb-js` core only | 250 KiB | 6,735 B | n/a (no engine by definition) | within, 95.1% unused |
| `dxdb-js` + memory | 320 KiB | 6,759 B | **131,775 B** | **within, 59.8% unused** |
| `dxdb-js` + OPFS | 400 KiB | 14,194 B | not measured: OPFS is wasm32-only and needs a worker to run, but its size does not depend on the engine | within, 96.5% unused |
| `dxdb-js` + SQLite | 900 KiB | 283,011 B | **402,134 B** | **within, 55.1% unused** |

Adding `dxdb-core` and `dxdb-reactivity` on top of schema+query costs a further 1,378 B
(133,153 B total), so the memory budget holds with the *entire* engine present.

### Measurement 2026-10-26: all four budget rows, and the caveat that dominates them

```
box:        fly open-surrealdb-ds-builder 87477e0cd01748
            8 cores, AMD EPYC, shared
toolchain:  rustc 1.95.0 (59807616e 2026-04-14), workspace release profile
idle:       yes  (make busy -> IDLE)
date:       2026-10-26
artifact:   xtask/wasm-size-budgets.sh --out <dir>
            (builds remotely with the release profile, post-processes with the
             wasm-bindgen CLI 0.2.129, brotli -q 11, round trip verified)
ran on:     the box; only the artifact is fetched back for compression

| Bundle | Brotli q11 | Budget | Δ vs budget |
| --- | ---: | ---: | ---: |
| `dxdb-js` + smoke surface (base fixture) | **6,735 B** (6.6 KiB) | 250 KiB (256,000 B) | **−243.4 KiB (within by 95.1%)** |
| + `dxdb-memory` | **6,759 B** (6.6 KiB) | 320 KiB (327,680 B) | **−313.4 KiB (within by 97.9%)** |
| + `dxdb-opfs` | **14,194 B** (13.9 KiB) | 400 KiB (409,600 B) | **−386.1 KiB (within by 96.5%)** |
| + `dxdb-sqlite` | **283,011 B** (276.4 KiB) | 900 KiB (921,600 B) | **−623.6 KiB (within by 69.2%)** |

All three measured rows are **within budget**. That is the good news, and it is also
where the honest reporting has to start.

#### Measurement 2026-10-26: the engine-linked bundles (the budget test)

The rows above link a *storage* to the binding floor, which says how small the binding
layer is. They are not the budget test, because `dxdb-js` links exactly one workspace
crate (`dxdb-storage`): no schema validator, no query planner, no index-string encoder.
These two rows link the engine, which is the first measurement where PLAN.md 5.3's
numbers are a real constraint.

| Bundle | Brotli q11 | Budget | Δ vs budget |
| --- | ---: | ---: | ---: |
| `dxdb-js` + schema + query + memory | **131,775 B** (128.7 KiB) | 320 KiB (327,680 B) | **−191.3 KiB (within by 59.8%)** |
| `dxdb-js` + schema + query + sqlite | **402,134 B** (392.7 KiB) | 900 KiB (921,600 B) | **−507.3 KiB (within by 55.1%)** |
| `dxdb-js` + core + reactivity + schema + query + memory | **133,153 B** (130.0 KiB) | 320 KiB (327,680 B) | **−190.0 KiB (within by 59.4%)** |

**Both budgets hold with the engine linked.** That is the answer to "does PLAN.md 5.3
survive contact", and it survives with roughly 59% and 55% of the respective budgets
unused. `dxdb-core` and `dxdb-reactivity` cost only ~1.4 KiB brotli on top of
schema+query (133,153 − 131,775), so the three-crate guess in the earlier caveat would
have been wrong about where the weight is.

##### Where the weight actually is (attribution, not guesswork)

Measured by linking one dependency at a time, each with a real code path so dead-code
elimination cannot drop it:

| Fixture | Brotli q11 | Marginal cost |
| --- | ---: | ---: |
| binding floor (`dxdb-js` + `dxdb-storage`) | 6,735 B | — |
| + `regress` alone (ECMAScript regex) | 86,511 B | **+79,776 B** |
| + `dxdb-schema` alone | 37,864 B | **+31,129 B** |
| + schema + query + memory | 131,775 B | +93,911 B over query's dependencies |
| + core + reactivity on top | 133,153 B | **+1,378 B** |

The dominant term is **`regress`, the ECMAScript-regex engine, at ~80 KiB brotli** —
62% of the whole engine+memory bundle's cost over the binding floor (131,775 − 6,735 =
125,040 B). It is pulled in by `dxdb-query` for Mango `$regex`, and PLAN.md 9 chose it
deliberately because RxDB's `$regex` is ECMAScript and `regex`'s semantics differ. So
this is a known, accepted cost rather than an accident — but it is worth naming, because
if the 320 KiB budget were ever exceeded, `regress` would be the first place to look and
a lazily-loaded `$regex` path would be the first remedy.

`dxdb-schema` is ~31 KiB, of which the `jsonschema` crate is the bulk; our own schema
code is a fraction. `dxdb-core` + `dxdb-reactivity` are negligible at this granularity.
SQLite adds ~270 KiB on top of the engine (402,134 − 131,775), i.e. the embedded C
database remains the single biggest line item in the largest bundle.

#### The caveat that this measurement retires

The paragraph below was true when it was written and is **no longer true**: the
engine-linked rows above now contain `dxdb-schema`, `dxdb-query`, `dxdb-core` and
`dxdb-reactivity`, so "within budget" is a statement about a bundle that contains the
engine. It is kept for the record because the reasoning it records — that a bundle
without the engine proves nothing about the budget — is why the measurement above
exists.



**These bundles do not contain the engine, so "within budget" does not yet mean the
budget is achievable.** `crates/dxdb-js` today links exactly one workspace crate,
`dxdb-storage`, plus `wasm-bindgen`/`wasm-bindgen-futures`. It does **not** link:

| Crate | Source lines | In the measured bundle? |
| --- | ---: | --- |
| `dxdb-core` | 3,189 | no |
| `dxdb-schema` | 1,947 | no |
| `dxdb-query` | 4,484 | no |
| `dxdb-reactivity` | 1,662 | no |

`dxdb-js`'s own module docs say the same: the exported database/collection surface
"arrives once `dxdb-storage` publishes the traits". Those traits now exist, so the
wiring is *possible* and simply not done. Until it is, the memory row's 6,759 B is the
cost of the binding layer plus a storage, not the cost of a usable database — the
schema, planner and reactivity code that a real bundle needs is 11,282 source lines
away. The 250 KiB budget is therefore still barely engaged, exactly as the 2026-10-25
row said, and remains unproven rather than proven.

**What the numbers do establish**, which is worth having:

- **`wasm-bindgen` + the async plumbing is ~2.6% of the 250 KiB core budget.** The
  binding layer is not the size risk; the engine is.
- **SQLite costs ~276 KiB brotli on top of the binding floor** (283,011 − 6,735),
  which is **30.7% of its own 900 KiB budget** before a single byte of engine code. That
  is the useful constraint: the SQLite bundle has ~637 KiB left for the engine, and the
  engine is at least `dxdb-core` + `dxdb-schema` + `dxdb-query`. If those three land
  above 637 KiB brotli, the 900 KiB budget is exceeded and the SQLite row is the one
  that breaks first. That is a prediction this file can now be held to.
- **OPFS costs 7,459 B more than the base fixture** (14,194 − 6,735), i.e. 2.2× the
  binding floor and 1.1% of its own 400 KiB budget. That was measured rather than
  predicted, and it lands where the design said it would: OPFS is *our* code plus
  web-sys bindings and a file layout, with no embedded C database, so it sits near the
  memory row (6,759 B) rather than anywhere near SQLite (283,011 B). The 400 KiB budget
  has 386 KiB spare — more than any other row — which means the OPFS row will not be
  the one that constrains the engine.
- **Adding a storage to the binding floor is nearly free when the storage is small**
  (memory added 26 B brotli; the memory storage's own code is already in the tree at
  46/48 conformance) and expensive when it embeds a C database (sqlite, +276 KiB).

#### Reproducing

The fixture entry point in `crates/dxdb-bench/src/lib.rs` exists to stop a specific
failure: a `cdylib` whose storage code is never referenced gets dead-code-eliminated, so
the artifact would measure `dxdb-js` alone while the row claims to include a storage.
`dxdb_size_fixture_probe` reaches through the storage features so the linker cannot drop
them, which is why the raw sizes differ at all (41,372 → 42,679 → 881,224 before
bindgen). A number measured without that root would be flattering and wrong.

### Measurement 2026-10-25: the wasm-bindgen floor

Method: `crates/dxdb-js` with the real dependency set (`wasm-bindgen =0.2.129`,
`wasm-bindgen-futures 0.4.79`), workspace release profile, `wasm32-unknown-unknown`,
shared Fly box with `-d 1.95.0`, bindings from the matching `wasm-bindgen` CLI
0.2.129 (mismatched versions trap at instantiation), `brotli -q 11`, round-trip
verified with `brotli -d | cmp`.

| Variant | Raw | Brotli q11 | JS glue (brotli) | Total |
| --- | ---: | ---: | ---: | ---: |
| Fixed floor (`abi_version` + `echo`), no futures | 13,554 B | 5,373 B | 1,957 B | 7,330 B |
| With async plumbing, as in `crates/dxdb-js` today | 43,226 B | 13,927 B | 3,264 B | 17,191 B |

**What this establishes:** `wasm-bindgen` plus the async plumbing is ~5% of the 250 KiB
core budget, so the binding layer is not the size risk. The budget has to be spent on
schema parsing, the query planner, index-string construction, and reactivity.

**What it does not establish:** whether the 250 KiB target is achievable, because none
of that code exists yet. The number to watch is the first measurement that includes
`dxdb-schema` and `dxdb-query`; that is when this budget becomes a real constraint
rather than a formality.

## Storage performance

Mirroring upstream's `test/performance.test.ts` shape: per-storage benches over the
same operation mix, so the numbers are comparable across dxdb storages and against
upstream. The mix is defined once in `crates/dxdb-bench/src/scenario.rs` and both
bench binaries consume it, which is what makes the two rows below comparable to each
other at all.

### Measurement 2026-10-26: filesystem and sqlite, native

```
box:        fly open-surrealdb-ds-builder 87477e0cd01748
            8 cores, AMD EPYC, shared
toolchain:  1.95.0 (rustc 59807616e 2026-04-14), release profile
idle:       yes  (make busy -> IDLE; /proc/loadavg 0.26 0.36 0.36; nproc 8)
date:       2026-10-26
artifact:   xtask/bench-storages.sh --runs 5 --out <dir>
            -> <dir>/{filesystem,sqlite}.json + <dir>/metadata.json
ran on:     the box (the script rsyncs, builds and runs remotely, fetches JSON back)
```

Scenario: 3000 documents inserted in 6 batches of 500, 50 serial inserts, 4
parallel selector queries, document 290 B in upstream's `averageSchemaData()` shape.
5 independent runs per operation; medians below, min/max in the artifacts.

| Operation | filesystem (redb) | sqlite (rusqlite) | sqlite ÷ filesystem |
| --- | ---: | ---: | ---: |
| `bulk_insert` (3000 docs, 6 txns) | 30.99 ms — 96,816 docs/s | 11.69 ms — 256,524 docs/s | 0.38× |
| `insert_serial` (50 docs, 50 txns) | 25.31 ms — 1,976 docs/s | 2.91 ms — 17,209 docs/s | 0.11× |
| `find_by_id_bulk` (3000 ids) | 31.00 ms — 96,772 ids/s | 15.95 ms — 188,041 ids/s | 0.51× |
| `find_by_id_serial` (50 ids) | 0.50 ms — 99,463 ids/s | 0.28 ms — 178,699 ids/s | 0.56× |
| `query_sorted` (3050 rows, sorted) | 4.58 ms | 6.24 ms | 1.36× |
| `query_parallel` (4 selectors) | 4.66 ms — 858 q/s | 7.72 ms — 518 q/s | 1.65× |
| `count` (4 selector counts) | 3.75 ms — 1,066 q/s | 7.49 ms — 534 q/s | 1.99× |
| `changed_since` (limit 100) | 0.02 ms | 0.11 ms | 4.68× |

"A ÷ B = 0.38×" means sqlite took 38% of the time filesystem took, i.e. sqlite was
faster there. The column is deliberately one direction so it reads the same for
every row.

### What these numbers do not measure, stated before anyone quotes them

- **`query_sorted`, `query_parallel` and `count` are scan-and-filter on both
  storages, not index range scans.** The planner that would emit index strings lives
  in `dxdb-query`, which is not published, so the filesystem bench scans the document
  prefix and sorts in Rust, and the sqlite bench runs `SELECT data ... ` and sorts in
  Rust too (the same Rust sort on both, so the comparison measures storage, not two
  different sort implementations). This is the same position as upstream's own trial
  SQLite storage, which scans and filters in JavaScript
  (`sqlite-storage-instance.ts:322`). A real index scan should make both much faster
  and will change these rows.
- **Neither number includes the `RxStorageInstance` layer.**
  `categorizeBulkWriteRows`, per-row conflict detection, JSON validation and change
  event emission all sit above this and are not in the path. A real storage will be
  slower than these numbers, and the honest reading is "this is the engine, not the
  storage".
- **The two storages are not configured for equal durability, and the difference is
  the engine's, not a choice.** redb in `MultiWriter` mode **refuses**
  `Durability::None` — a commit held only in memory would be invisible to the other
  processes that mode exists for — so every filesystem commit fsyncs. sqlite runs
  WAL with `synchronous=NORMAL`, which does not fsync per commit. Measured directly
  by re-running sqlite with `--synchronous FULL`:

  | | `insert_serial` median | docs/s |
  | --- | ---: | ---: |
  | sqlite, WAL + NORMAL (default) | 2.91 ms | 17,209 |
  | sqlite, `synchronous=FULL` | 45.99 ms | 1,079 |
  | filesystem, redb MultiWriter | 25.31 ms | 1,976 |

  So the 8.7× gap on `insert_serial` is mostly a **durability-policy** difference,
  not an engine-speed difference: fsync-per-commit sqlite is 1.8× *slower* than redb.
  Anyone quoting 0.11× without this paragraph is quoting the wrong thing.
- **`changed_since` is dxdb-only.** Upstream's perf test has no
  `getChangedDocumentsSince` case. It is measured here because the `chg` table exists
  to make it cheap, and 0.02 ms versus 0.11 ms says the redb key layout does what it
  was designed for.
- **No upstream comparison.** Upstream was **not** measured on this box in this
  session, so no dxdb-to-upstream ratio is stated anywhere in this file. Doing that
  needs Node and upstream `rxdb` running on the same box in the same idle window;
  until then the comparison is unmeasured, not "pending" and not implied.

### Reading the two rows against each other

The shape is consistent across every write operation: sqlite wins on writes and
point lookups, filesystem wins on scans and the change feed. That is what the designs
predict. redb's per-commit fsync cost is paid on every `insert_serial`, while
sqlite's WAL amortises it; conversely redb's ordered key layout makes a scan a
sequential byte read, while sqlite's `ORDER BY`/full-column read pays for the TEXT
column. Neither is "faster" without naming the operation and the durability setting.

| Storage | bulk insert | find-by-id | query (scan) | count (scan) | changed_since | Date |
| --- | --- | --- | --- | --- | --- | --- |
| memory | 23,915 docs/s | 168,585 ids/s | 47 queries/s | 1,149 queries/s | 61/s | 2026-10-26 |
| filesystem (redb) | 96,816 docs/s | 96,772 ids/s | 218 queries/s | 1,066 queries/s | 42,553/s | 2026-10-26 |
| indexeddb | not measurable on the build box | | | | | — |
| opfs | not measurable on the build box | | | | | — |
| sqlite | 256,524 docs/s | 188,041 ids/s | 160 queries/s | 534 queries/s | 9,099/s | 2026-10-26 |

### Measurement 2026-10-26: memory, the reference implementation

```
box:        fly open-surrealdb-ds-builder 87477e0cd01748, 8 cores, AMD EPYC, shared
toolchain:  1.95.0 (rustc 59807616e 2026-04-14), release profile
idle:       yes  (make busy -> IDLE; /proc/loadavg 0.00 0.08 0.29)
date:       2026-10-26
artifact:   xtask/bench-storages.sh --only memory --runs 5 --out <dir>
            -> <dir>/memory.json + <dir>/metadata.json
ran on:     the box (script rsyncs, builds and runs remotely)
```

| Operation | memory | filesystem | sqlite |
| --- | ---: | ---: | ---: |
| `bulk_insert` (3000 docs, 6 batches) | 125.44 ms — 23,915/s | 30.99 ms — 96,816/s | 11.69 ms — 256,524/s |
| `insert_serial` (50 docs) | 2.57 ms — 19,435/s | 25.31 ms — 1,976/s | 2.91 ms — 17,209/s |
| `find_by_id_bulk` (3000 ids) | 17.80 ms — 168,585/s | 31.00 ms — 96,772/s | 15.95 ms — 188,041/s |
| `find_by_id_serial` (50 ids) | 0.25 ms — 198,020/s | 0.50 ms — 99,463/s | 0.28 ms — 178,699/s |
| `query_sorted` (3050 rows) | 21.13 ms — 47/s | 4.58 ms — 218/s | 6.24 ms — 160/s |
| `query_parallel` (4 selectors) | 22.58 ms — 177/s | 4.66 ms — 858/s | 7.72 ms — 518/s |
| `count` (4 counts) | 3.48 ms — 1,149/s | 3.75 ms — 1,066/s | 7.49 ms — 534/s |
| `changed_since` (limit 100) | 16.37 ms | 0.02 ms | 0.11 ms |

**This row is not the same measurement as the other two, and reading it as "memory is
slowest" would be wrong.** The filesystem and sqlite benches drive a *driver API*
(`put_document`, `StatementId::InsertDocument`); this one drives the real
`RxStorageInstance` trait, because that is what `dxdb-memory` is. So its numbers include
`categorize_bulk_write_rows`, document cloning, `Matcher` evaluation, event emission and
change-feed dispatch — the full storage contract. The other two skip all of that.

Two consequences, both visible in the table and both worth stating rather than
explaining away:

- **`bulk_insert` looks far worse for memory (23,915/s vs 96,816/s) and that is not a
  memory-storage weakness.** It is the cost of the contract layer: every row goes through
  the categoriser and produces change events. A filesystem `bulk_write` written against
  the trait (rather than against the driver) would pay the same cost. The honest
  comparison for "which engine is faster" is driver-to-driver, and that comparison does
  not include memory at all.
- **`query_sorted` and `changed_since` look far worse (47/s, 16.37 ms) and here the
  ordering is real.** Memory's `query` sorts an owned `Vec` through the query crate's
  comparator and its `changed_since` runs the trait's documented fallback (a full
  ordered query, because upstream's memory storage does not implement a native change
  feed either). filesystem's `changed_since` rides the `chg` table and is 800× faster at
  0.02 ms. That is a genuine advantage of the redb key layout, and it is the one place
  in this table where the specialised storage beats the reference implementation by an
  order of magnitude on a contract operation.

**What the memory row is for:** it is the baseline a *new* storage should meet on the
operations where memory is competitive — point lookups and count — because memory has no
I/O to hide behind, and it is the ceiling a new storage should beat on the operations
where memory is structurally weak (`changed_since`, where an ordered index wins).

### Why indexeddb and opfs are "not measurable on the build box"

These are not unfinished rows; they are rows that cannot be filled by this harness at
all, and the distinction matters because "pending" implies someone will get to it.

- **`indexeddb`** needs a browser `IndexedDB` implementation. The crate exists
  (`crates/storages/dxdb-indexeddb`, 46/48 conformance) and has a `MemoryKv`-style
  backing for native tests, but that backing is in-memory: benchmarking it would measure
  the backing map, not IndexedDB, and the number would be meaningless as an IndexedDB
  figure. A real measurement needs the `demo/` browser harness driving Chromium over the
  same operation mix, reporting the same JSON shape.
- **`opfs`** needs a dedicated worker and `FileSystemSyncAccessHandle`, which exists
  only in a browser worker context. The crate now exists and passes 49/49 conformance
  (`crates/storages/dxdb-opfs`), but it runs against a `FileStore`, and the only backing
  available natively is an in-memory one: benchmarking that would measure a hash map, not
  OPFS. Its *size* is measurable and measured (14,194 B brotli, well within the 400 KiB
  budget); its *throughput* is not.

Both are therefore recorded as **not measurable on the build box; requires the `demo/`
browser harness**, which is lane F's territory. Recording a native proxy number instead
would be worse than an empty cell: it would look comparable to the rows above it while
measuring something else.

## Upstream's published claims, as targets rather than facts

CLEANROOM.md section 6: "Their published numbers (3-4x, 30-50% reads, 25% writes,
200ms startup) are targets to benchmark against on our own hardware, not facts to
repeat."

| Claim | Upstream subject | dxdb target | Measured |
| --- | --- | --- | --- |
| 3-4x over IndexedDB | OPFS storage | to be measured on our box | pending |
| 30-50% read gain on slow IndexedDB | sharding | to be measured | pending |
| 25% write gain | sharding | to be measured | pending |
| ~200 ms startup saving, >4 collections | localstorage meta cache | to be measured | pending |

## Where dxdb is expected to lose

Recorded in advance so that a later measurement cannot be quietly omitted. These
are the cases where the Rust port is *not* obviously ahead:

- **Small documents, hot loop, native.** V8's JIT is extremely good at the shapes
  RxDB actually uses. A Rust implementation wins on memory and predictability, not
  necessarily on raw throughput for tiny payloads.
- **JSON parse of already-validated data.** `serde_json` is fast, but so is
  `JSON.parse`, and JS avoids a copy the Rust boundary may introduce.
- **Startup on a warm cache in a browser.** The wasm module has to instantiate.

An honest benchmark table includes these rows once they are measured.
