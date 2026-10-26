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

| Artifact | Budget (brotli) | Measured | Date | Status |
| --- | --- | --- | --- | --- |
| `dxdb-js` core only, no storages | 250 KiB | 5.3 KiB floor / 13.6 KiB with async plumbing | 2026-10-25 | measured, budget barely engaged |
| `dxdb-js` + memory storage | 320 KiB | not yet measured | — | pending M4 |
| `dxdb-js` + OPFS storage | 400 KiB | not yet measured | — | pending M4 |
| `dxdb-js` + SQLite storage | 900 KiB | not yet measured | — | pending M4 |

Full method and the three-engine verification are in
`docs/lanes/m4-browser-plan.md` section 6.

A budget that is exceeded gets revised **with the measured number and a reason**,
or the bundle gets smaller. It does not get quietly ignored.

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
| memory | pending M2 | | | | | |
| filesystem (redb) | 96,816 docs/s | 96,772 ids/s | 218 queries/s | 1,066 queries/s | 42,553/s | 2026-10-26 |
| indexeddb | pending M4 | | | | | |
| opfs | pending M4 | | | | | |
| sqlite | 256,524 docs/s | 188,041 ids/s | 160 queries/s | 534 queries/s | 9,099/s | 2026-10-26 |

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
