# STATUS

Live parity matrix against upstream RxDB. One row per upstream module.

Upstream pin: `pubkey/rxdb` @ `3b763ee17d62bc96cfae556ea0ccf92f9891c8c1` (v17.6.0).

Regenerate the row set with `make status`; edit only the State/Crate/Evidence columns.


States: `not started` · `in progress` · `ported` (code) · `ported + tested` (gate green).

A row is `ported + tested` only when the named gate command exits 0 on `master`.


## Core (`src/*.ts`, 28 files, 10952 lines)

| Module | Upstream lines | Crate | State | Evidence (gate) |
| --- | --- | --- | --- | --- |
| `change-event-buffer.ts` | 174 | dxdb-reactivity | not started |  |
| `custom-index.ts` | 470 | dxdb-query | not started |  |
| `doc-cache.ts` | 512 |  | not started |  |
| `event-reduce.ts` | 292 | dxdb-reactivity | not started |  |
| `hooks.ts` | 128 |  | not started |  |
| `incremental-write.ts` | 251 |  | not started |  |
| `index.ts` | 32 |  | not started |  |
| `overwritable.ts` | 41 |  | not started |  |
| `plugin-helpers.ts` | 326 |  | not started |  |
| `plugin.ts` | 116 |  | not started |  |
| `query-cache.ts` | 140 | dxdb-reactivity | not started |  |
| `query-planner.ts` | 483 | dxdb-query | not started |  |
| `rx-change-event.ts` | 136 | dxdb-reactivity | not started |  |
| `rx-collection-helper.ts` | 298 |  | not started |  |
| `rx-collection.ts` | 1362 |  | not started |  |
| `rx-database-internal-store.ts` | 402 |  | not started |  |
| `rx-database.ts` | 1030 |  | not started |  |
| `rx-document-prototype-merge.ts` | 134 |  | not started |  |
| `rx-document.ts` | 627 |  | not started |  |
| `rx-error.ts` | 191 |  | not started |  |
| `rx-query-helper.ts` | 576 | dxdb-query | not started |  |
| `rx-query-mingo.ts` | 68 | dxdb-query | not started |  |
| `rx-query-single-result.ts` | 117 | dxdb-query | not started |  |
| `rx-query.ts` | 1008 | dxdb-query | not started |  |
| `rx-schema-helper.ts` | 394 | dxdb-schema | not started |  |
| `rx-schema.ts` | 325 | dxdb-schema | not started |  |
| `rx-storage-helper.ts` | 1131 | dxdb-storage | not started |  |
| `rx-storage-multiinstance.ts` | 188 | dxdb-storage | not started |  |

## Plugins (`src/plugins/**`, 52 directories, 37700 lines)

| Module | Upstream lines | Crate | State | Evidence (gate) |
| --- | --- | --- | --- | --- |
| `attachments` | 397 |  | not started |  |
| `attachments-compression` | 236 |  | not started |  |
| `backup` | 413 |  | not started |  |
| `cleanup` | 291 |  | not started |  |
| `crdt` | 577 |  | not started |  |
| `dev-mode` | 3088 |  | not started |  |
| `electron` | 125 |  | not started |  |
| `encryption-crypto-js` | 233 |  | not started |  |
| `flutter` | 65 |  | not started |  |
| `json-dump` | 236 |  | not started |  |
| `key-compression` | 249 |  | not started |  |
| `leader-election` | 132 |  | not started |  |
| `local-documents` | 776 |  | not started |  |
| `migration-schema` | 1187 |  | not started |  |
| `migration-storage` | 255 |  | not started |  |
| `pipeline` | 497 |  | not started |  |
| `query-builder` | 749 |  | not started |  |
| `react` | 469 |  | not started |  |
| `reactivity-angular` | 32 |  | not started |  |
| `reactivity-preact-signals` | 90 |  | not started |  |
| `reactivity-vue` | 43 |  | not started |  |
| `replication` | 1022 |  | not started |  |
| `replication-appwrite` | 338 |  | not started |  |
| `replication-couchdb` | 475 |  | not started |  |
| `replication-firestore` | 538 |  | not started |  |
| `replication-google-drive` | 2421 |  | not started |  |
| `replication-graphql` | 901 |  | not started |  |
| `replication-microsoft-onedrive` | 2053 |  | not started |  |
| `replication-mongodb` | 578 |  | not started |  |
| `replication-nats` | 347 |  | not started |  |
| `replication-supabase` | 435 |  | not started |  |
| `replication-webrtc` | 2314 |  | not started |  |
| `replication-websocket` | 638 |  | not started |  |
| `state` | 577 |  | not started |  |
| `storage-denokv` | 696 |  | not started |  |
| `storage-dexie` | 1041 |  | not started |  |
| `storage-foundationdb` | 736 |  | not started |  |
| `storage-localstorage` | 794 |  | not started |  |
| `storage-memory` | 1387 |  | not started |  |
| `storage-mongodb` | 699 |  | not started |  |
| `storage-remote` | 827 |  | not started |  |
| `storage-remote-websocket` | 252 |  | not started |  |
| `storage-sqlite` | 1746 |  | not started |  |
| `test-utils` | 3762 |  | not started |  |
| `update` | 83 |  | not started |  |
| `utils` | 1817 |  | not started |  |
| `validate-ajv` | 70 |  | not started |  |
| `validate-ajv-standalone` | 126 |  | not started |  |
| `validate-is-my-json-valid` | 30 |  | not started |  |
| `validate-z-schema` | 58 |  | not started |  |
| `vector` | 37 |  | not started |  |
| `webmcp` | 762 |  | not started |  |

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
| M0 | `make check` + `make test` green on host and `wasm32-unknown-unknown` | not started |
| M1 | `make test -p dxdb-schema -p dxdb-query` incl. ported rx-query/rx-schema suites + fuzz vs JS oracle | not started |
| M2 | `make test -p dxdb-core` incl. full rx-storage-implementations + query-correctness suites on dxdb-memory | not started |
| M3 | `make test -p dxdb-replication` incl. ported replication-base-test-suite + two-process convergence | not started |
| M4 | demo app passes conformance in Chromium/Firefox/WebKit via Playwright; bundle within budget | not started |
| M5 | conformance under Wasmtime from .wasm component with preopen; under Node from napi and WASI builds | not started |
| M6 | full ported open-source plugin test corpus green | not started |
| M7 | clean-room specs + implementations, invariants-first tests green, gaps register published | not started |

