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
| `dxdb-js` core only, no storages | 250 KiB | not yet measured | — | pending M4 |
| `dxdb-js` + memory storage | 320 KiB | not yet measured | — | pending M4 |
| `dxdb-js` + OPFS storage | 400 KiB | not yet measured | — | pending M4 |
| `dxdb-js` + SQLite storage | 900 KiB | not yet measured | — | pending M4 |

A budget that is exceeded gets revised **with the measured number and a reason**,
or the bundle gets smaller. It does not get quietly ignored.

## Storage performance

Mirroring upstream's `test/performance.test.ts` shape: per-storage criterion
benches over the same operation mix, so the numbers are comparable across dxdb
storages and against upstream.

| Storage | insert/s | bulk insert/s | find-by-id/s | query/s | count/s | Date |
| --- | --- | --- | --- | --- | --- | --- |
| memory | pending M2 | | | | | |
| filesystem (redb) | pending M5 | | | | | |
| indexeddb | pending M4 | | | | | |
| opfs | pending M4 | | | | | |
| sqlite | pending M5 | | | | | |

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
