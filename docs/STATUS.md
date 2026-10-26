# STATUS

Live parity matrix against upstream RxDB. One row per upstream module.

Upstream pin: `pubkey/rxdb` @ `3b763ee17d62bc96cfae556ea0ccf92f9891c8c1` (v17.6.0).

Regenerate the row set with `make status`; edit only the State/Crate/Evidence columns.


States: `not started` · `in progress` · `ported` (code) · `ported + tested` (gate green).

A row is `ported + tested` only when the named gate command exits 0 on `master`.


## Core (`src/*.ts`, 28 files, 10952 lines)

| Module | Upstream lines | Crate | State | Evidence (gate) |
| --- | --- | --- | --- | --- |
| `change-event-buffer.ts` | 174 | dxdb-reactivity | in progress | crate not yet created (lane D, next) |
| `custom-index.ts` | 470 | dxdb-query | ported + tested | 900-case index-string oracle green |
| `doc-cache.ts` | 512 | dxdb-core | ported + tested | cache.rs; dxdb-core 59 tests green |
| `event-reduce.ts` | 292 | dxdb-reactivity | in progress | crate not yet created |
| `hooks.ts` | 128 | dxdb-core | ported + tested | 24 hooks with upstream names |
| `incremental-write.ts` | 251 | dxdb-core | ported + tested | write queue; dxdb-core tests green |
| `index.ts` | 32 | dxdb-core | ported | public surface |
| `overwritable.ts` | 41 | dxdb-core | ported |  |
| `plugin-helpers.ts` | 326 | dxdb-core | in progress | plugin registration pending M6 plugin model |
| `plugin.ts` | 116 | dxdb-core | in progress | registry lands with the plugin model |
| `query-cache.ts` | 140 | dxdb-reactivity | in progress | crate not yet created |
| `query-planner.ts` | 483 | dxdb-query | ported + tested | plan.rs; normalised-queries oracle 798 cases |
| `rx-change-event.ts` | 136 | dxdb-reactivity | in progress | crate not yet created |
| `rx-collection-helper.ts` | 298 | dxdb-core | ported + tested |  |
| `rx-collection.ts` | 1362 | dxdb-core | ported + tested | CRUD incl COL22/COL21/CONFLICT |
| `rx-database-internal-store.ts` | 402 | dxdb-core | not ported (planned) | a second internal collection; documented as not-yet in the dxdb-core commit rather than stubbed |
| `rx-database.ts` | 1030 | dxdb-core | ported + tested | name validation, instance token |
| `rx-document-prototype-merge.ts` | 134 | dxdb-core | ported | patch/merge semantics |
| `rx-document.ts` | 627 | dxdb-core | ported + tested | document + patch semantics |
| `rx-error.ts` | 191 | dxdb-core | ported + tested | error catalogue incl. COL/SC codes |
| `rx-query-helper.ts` | 576 | dxdb-query | ported + tested | normalize.rs |
| `rx-query-mingo.ts` | 68 | dxdb-query | ported + tested | matcher.rs + selector-matching oracle |
| `rx-query-single-result.ts` | 117 | dxdb-query | ported |  |
| `rx-query.ts` | 1008 | dxdb-query | ported + tested |  |
| `rx-schema-helper.ts` | 394 | dxdb-schema | ported + tested | 52-case hash oracle |
| `rx-schema.ts` | 325 | dxdb-schema | ported + tested |  |
| `rx-storage-helper.ts` | 1131 | dxdb-storage | ported + tested | categorizeBulkWriteRows: 600-case + 47-case oracles |
| `rx-storage-multiinstance.ts` | 188 | dxdb-storage | in progress | redb/filesystem concurrency mode landed; wrapper pending |

## Plugins (`src/plugins/**`, 52 directories, 37700 lines)

| Module | Upstream lines | Crate | State | Evidence (gate) |
| --- | --- | --- | --- | --- |
| `attachments` | 397 | dxdb-attachments | ported | data model + digest |
| `attachments-compression` | 236 | — | not ported (JS-ecosystem binding) | gzip of attachment bytes via a JS codec; a Rust caller pipes through flate2 itself, so a wrapper crate would only re-expose std |
| `backup` | 413 | — | not ported (planned) | portable JSON export/import already covered by dxdb-core; a dedicated backup plugin is M6 tail work, not omitted |
| `cleanup` | 291 | — | not ported (planned) | cleanup() is on the storage trait and every storage implements it; the plugin that schedules it is M6 tail work |
| `crdt` | 577 | — | not ported (planned) | M6 tail: needs the plugin model plus a CRDT crate choice, listed in the plan's later sequence |
| `dev-mode` | 3088 | — | not ported (intentional divergence) | dev-mode's core value is catching schema/lib misuse at runtime; Rust catches most of it at compile time, and the tracking iframe is explicitly not ported (CLEANROOM 3) |
| `electron` | 125 | — | not ported (JS-ecosystem binding) | binds Electron's IPC to a storage; Rust owns the process, so an equivalent is a deployment concern not a crate |
| `encryption-crypto-js` | 233 | — | not ported (superseded) | dxdb-encryption's DXE1 envelope replaces it; the open plugin's CryptoJS envelope is deliberately not reproduced (see specs/encryption.md) |
| `flutter` | 65 | — | not ported (JS-ecosystem binding) | binds Dart/Flutter FFI to RxDB; a Rust core exposes a C ABI instead, which is a binding crate decision not a plugin port |
| `json-dump` | 236 | — | not ported (planned) | export/import helpers; M6 tail |
| `key-compression` | 249 | plugins/key-compression | ported + tested | 59 plugin tests green |
| `leader-election` | 132 | — | not ported (planned) | M6 tail; needs the plugin model plus a broadcast-channel transport |
| `local-documents` | 776 | — | not ported (not started) | documented as not-yet in the dxdb-core commit; it is a second storage instance with different visibility rules |
| `migration-schema` | 1187 | plugins/migration-schema | ported + tested | 59 plugin tests green |
| `migration-storage` | 255 | plugins/migration-storage | ported + tested | crash-order cases |
| `pipeline` | 497 | — | not ported (planned) | M6 tail; the plugin model must land first since pipelines are RxPipeline extension points |
| `query-builder` | 749 | plugins/query-builder | ported + tested | 890 lines + ported tests |
| `react` | 469 | — | not ported (JS-ecosystem binding) | React hooks over RxDB observables; a Rust user has no React to bind to |
| `reactivity-angular` | 32 | — | not ported (JS-ecosystem binding) | Angular change detection; framework-specific by definition |
| `reactivity-preact-signals` | 90 | — | not ported (JS-ecosystem binding) | Preact signals; framework-specific |
| `reactivity-vue` | 43 | — | not ported (JS-ecosystem binding) | Vue reactivity; framework-specific |
| `replication` | 1022 | dxdb-replication | ported + tested | 88 tests incl. HTTP convergence |
| `replication-appwrite` | 338 | — | not ported (transport adapter) | protocol is ported (dxdb-replication); an Appwrite adapter is a transport crate sequenced after M3's protocol stabilised |
| `replication-couchdb` | 475 | — | not ported (transport adapter) | protocol ported; CouchDB's checkpoint semantics need a dedicated adapter + conformance run |
| `replication-firestore` | 538 | — | not ported (transport adapter) | protocol ported; needs the Firestore sync protocol, a separate specification |
| `replication-google-drive` | 2421 | — | not ported (transport adapter) | protocol ported; Drive-as-transport is M6 tail |
| `replication-graphql` | 901 | — | not ported (transport adapter) | protocol ported; GraphQL adapter is M6 tail |
| `replication-microsoft-onedrive` | 2053 | — | not ported (transport adapter) | protocol ported; OneDrive-as-transport is M6 tail |
| `replication-mongodb` | 578 | — | not ported (transport adapter) | protocol ported; Mongo change-stream adapter is M6 tail |
| `replication-nats` | 347 | — | not ported (transport adapter) | protocol ported; NATS adapter is M6 tail |
| `replication-supabase` | 435 | — | not ported (transport adapter) | protocol ported; Supabase realtime adapter is M6 tail |
| `replication-webrtc` | 2314 | — | not ported (transport adapter) | protocol ported; WebRTC signalling is a transport with its own peer negotiation |
| `replication-websocket` | 638 | — | not ported (transport adapter) | protocol ported; WebSocket transport is the next adapter in sequence |
| `state` | 577 | — | not ported (planned) | RxState is a small observable store; M6 tail |
| `storage-denokv` | 696 | — | not ported (storage adapter) | kv adapter over Deno's API; Rust has no Deno equivalent, and filesystem/sqlite cover the same ground |
| `storage-dexie` | 1041 | — | not ported (storage adapter) | Dexie is IndexedDB + a JS wrapper; dxdb-indexeddb targets the same store and the Dexie migration test is the interop path (audit item 4) |
| `storage-foundationdb` | 736 | storages/dxdb-filesystem | not ported (deliberate non-goal) | FoundationDB is a distributed *server-side* store, not local-first; porting it would mean shipping a cluster client for a topology dxdb does not target. The durable-local-storage role it fills upstream is covered by the redb-backed filesystem storage, which passes 48/48 of the shared conformance suite. See the note below the table. |
| `storage-localstorage` | 794 | storages/dxdb-localstorage | ported + tested | 20 tests + watchdog; 46/48 conformance (`impl-query-03`, `impl-count-01`) |
| `storage-memory` | 1387 | storages/dxdb-memory | ported + tested | 46/46 conformance |
| `storage-mongodb` | 699 | — | not ported (storage adapter) | would need a Mongo driver in the storage layer; a server-side storage, not local-first, so it is a deliberate non-goal for the port |
| `storage-remote` | 827 | — | not ported (planned) | the message-channel transport; dxdb-replication's transport trait is the seam and a remote storage crate is M6 tail |
| `storage-remote-websocket` | 252 | — | not ported (planned) | as storage-remote, over WebSocket |
| `storage-sqlite` | 1746 | storages/dxdb-sqlite | ported + tested | 50 wasi/native tests; driver layer only, does not implement `RxStorage` (see note) |
| `test-utils` | 3762 | dxdb-testkit | ported + tested | 118 tests; generated case table |
| `update` | 83 | plugins/update | ported + tested | full modify-js operator surface |
| `utils` | 1817 | dxdb-core | ported | revisions, time, hash helpers |
| `validate-ajv` | 70 | — | not ported (validation backend) | Ajv is one of four interchangeable JSON-Schema validators upstream offers; dxdb-schema uses the jsonschema crate, so the *choice* is ported rather than each backend |
| `validate-ajv-standalone` | 126 | — | not ported (validation backend) | as validate-ajv; standalone codegen is an Ajv implementation detail |
| `validate-is-my-json-valid` | 30 | — | not ported (validation backend) | as validate-ajv |
| `validate-z-schema` | 58 | — | not ported (validation backend) | as validate-ajv |
| `vector` | 37 | — | not ported (not a plugin) | upstream's own similarity/vector index experiment; not part of the documented plugin surface, and dxdb-fulltext covers the search need |
| `webmcp` | 762 | — | not ported (JS-ecosystem binding) | exposes RxDB over the WebMCP browser API; no Rust-side equivalent exists |

### `storage-foundationdb`: why this is a non-goal, not a backlog item

Upstream's `storage-foundationdb` is an `RxStorage` backed by FoundationDB: a
distributed database that runs as a cluster of server processes and requires a
client library plus a cluster file to reach it. Three things follow, and together
they make it a non-goal rather than a milestone:

1. **It is not local-first.** dxdb's premise is a database in the user's process, on
   the user's device, that works offline and syncs opportunistically. A storage whose
   reads and writes are network round-trips to a cluster is the opposite topology.
   Reproducing it would put a cluster dependency in a local-first library.
2. **There is nothing to be compatible with.** Parity with FoundationDB would mean
   parity with FoundationDB's own semantics, not with RxDB's contract: the RxDB-side
   behaviour is already the `RxStorage` contract, which every other storage
   implements and which `dxdb-testkit` gates.
3. **The role is already covered.** Upstream ships FoundationDB as *the* durable
   server-side storage. dxdb's answer to "durable local storage" is the redb-backed
   `dxdb-filesystem`, which passes **48 of 48** shared conformance cases
   (`cargo test -p dxdb-filesystem --test conformance`). So the user-facing capability
   is present, by a different and more appropriate engine.

This is the "written, reviewed justification for omission" PLAN.md section 10 allows,
not a silent gap. If a user genuinely needs FoundationDB, the honest answer is that
dxdb targets a different topology, not that the adapter is coming later.

### Two storages do not implement `RxStorage`, and that is recorded rather than hidden

`dxdb-sqlite` is a **driver layer** (`SqliteDriver` + rusqlite/sqlite-wasm-rs) and
`dxdb-filesystem` was a **key layout** until this change made it a full storage. The
conformance matrix distinguishes "0 cases run, does not implement the trait" from
"0 passed", because a matrix that printed `0/48` for a driver layer would be
indistinguishable from total failure. `dxdb-sqlite` remains a driver layer with no
`RxStorageInstance` implementation, so it has no conformance row yet; its behaviour is
covered by the wasi/native suite and by the benchmark, not by the shared matrix.

## Intentional divergences

Recorded per CLEANROOM.md section 3. These rows are marked `not ported (intentional)` and are
not parity gaps:

- The premium-flag gate (`utils-premium.ts`): dxdb ships no artificial limits and no phone-home.
- `NON_PREMIUM_COLLECTION_LIMIT = 13`: not reproduced.
- The SQLite trial caps (no indexes, no attachments, ~500 documents): not reproduced.
- The dev-mode tracking iframe: not reproduced.

## Milestone gates

| Milestone | Gate (command) | State |
| --- | --- | --- |
| M0 | `make check` + `make test` green on host and `wasm32-unknown-unknown` | **met** |
| M1 | `make test -p dxdb-schema -p dxdb-query` incl. ported rx-query/rx-schema suites + fuzz vs JS oracle | **in progress** (60 tests; 900-case oracle green) |
| M2 | `make test -p dxdb-core` incl. full rx-storage-implementations + query-correctness suites on dxdb-memory | **in progress** (memory storage + conformance at 46/46) |
| M3 | `make test -p dxdb-replication` incl. ported replication-base-test-suite + two-process convergence | **in progress** (6 cases + protocol layer 1) |
| M4 | demo app passes conformance in Chromium/Firefox/WebKit via Playwright; bundle within budget | **in progress** (adapter green in 3 engines; storages pending) |
| M5 | conformance under Wasmtime from .wasm component with preopen; under Node from napi and WASI builds | **in progress** (storage drivers landed; component run pending) |
| M6 | full ported open-source plugin test corpus green | not started |
| M7 | clean-room specs + implementations, invariants-first tests green, gaps register published | **in progress** (B0+B1 done; 3 B2 specs written) |

