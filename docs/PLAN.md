# dxdb: a Rust port of RxDB

Status: draft for review. Nothing here is committed code yet.
Date: 2026-10-25
Upstream: `pubkey/rxdb` at `3b763ee17d62bc96cfae556ea0ccf92f9891c8c1` (2026-10-25), version 17.6.0, Apache-2.0.

## 1. What we are building

dxdb is a Rust reimplementation of RxDB: a local-first, NoSQL, reactive document database
with multi-instance safety, a swappable storage layer, and replication as a first-class
protocol. Every public behaviour that RxDB defines in TypeScript gets a Rust equivalent that
is bit-compatible where compatibility is observable (schemas, revisions, index strings,
replication checkpoints, conflict resolution) and idiomatic where it is not (ownership,
errors, async traits, streams).

Two programs of work, in order:

1. **The port.** Behavioural parity with the Apache-2.0 upstream: core, open-source plugins,
   open-source storages. Verified by running RxDB's own semantics against ours.
2. **The clean room.** Independently designed replacements for the paid features. These are
   specified from observable behaviour and public documentation only, never from the closed
   source. Details in [CLEANROOM.md](./CLEANROOM.md).

The name `dxdb` is free on crates.io today (`rxdb` is also unclaimed, but the port should not
squat the upstream name).

## 2. Scale of the thing being ported

Measured on the pinned upstream commit, not estimated.

| Surface | Size |
| --- | --- |
| TypeScript source, all of `src/` | 275 files, 54,448 lines |
| Core only (`src/*.ts` + `src/types/`) | 10,952 + 3,010 lines |
| Plugin code (`src/plugins/**`) | 37,698 lines |
| Plugin directories | 52 |
| Test files (77 `*.test.ts`) | 55,062 lines |
| Docs (`docs-src/docs`, incl. subfolders) | 205 pages, 96 at top level |
| Distinct module paths tests import | 72 (47 of them plugin entry points) |

The core is 14k lines out of 54k, so 70% of the code is plugins. The 14k is where the semantics
live: schema generation, query normalisation, the query planner, the storage wrapper stack, the
document cache, the change-event buffer, revision handling, conflict categorisation, hooks.
Everything else hangs off it as plugins, which is good news for a port because plugin behaviour is
mostly expressed through a handful of traits.

The 55k lines of tests are the real asset. They are the executable specification, and they are
Apache-2.0 along with the source, so we can read them and encode them as Rust integration
tests. That is a port activity, not a clean-room activity, and it is the cheapest possible
route to correctness.

Two files carry most of the storage contract and deserve naming now, because they are the
acceptance oracle for every storage crate in this workspace:

- `test/unit/rx-storage-implementations.test.ts` (4,115 lines): the conformance suite every
  `RxStorage` must pass.
- `test/unit/rx-storage-query-correctness.test.ts` (2,663 lines): Mango semantics per storage.

Port those two first, into a reusable `dxdb-testkit`, and every storage after that is mostly
plumbing.

## 3. Non-obvious decisions up front

These are the four choices that determine whether the port is a weekend toy or a real thing.

### 3.1 Documents are ordered JSON, not `serde_json::Value`

RxDB sorts on index strings built from a deterministic encoding of field values
(`src/custom-index.ts:164` `getIndexableStringMonad`, `:274` `getNumberIndexString`), and it
compares whole index strings with JavaScript `<` (`src/plugins/storage-memory/memory-helper.ts:337`
`compareDocsWithIndex`). Two consequences:

- **Key order matters** to anything that hashes a document (schema hash, schema identity,
  `_meta`, encryption AAD). `serde_json` preserves insertion order only with the
  `preserve_order` feature. That feature must be on, and document storage must never round-trip
  through a `BTreeMap`.
- **Number formatting matters** to anything that hashes or serialises a document. Probe result
  on this machine: JavaScript `String(1e21)` is `"1e+21"` and `JSON.stringify(1e21)` is
  `"1e+21"`, while Rust `f64`'s `Display` gives `1000000000000000000000` and
  `serde_json::to_string` gives `1e+21` for the same value but `1.0` where JS gives `1`.
  We need a small `js_number` module implementing ECMAScript `Number::toString` (shortest
  round-trip, exponent thresholds at 1e21 and 1e-7) and use it everywhere a number becomes a
  string. This is not optional: revision-compatible data written by a JS client and read by
  dxdb (or the reverse) breaks otherwise.

### 3.2 String comparison is UTF-16 code unit order, not UTF-8 byte order

Smoking gun from the probe, run on this machine:

```
JS:    '\u{1F600}' < '\uFFFF'  ->  true    (surrogate pair 0xD83D < 0xFFFF)
Rust:  '\u{1F600}' < '\uFFFF'  ->  false   (UTF-8 byte order)
```

RxDB's memory storage, its index strings, and its documentation sorting all assume JS string
order. `localeCompare` is also used for some sort orders (`src/plugins/utils/utils-object.ts:207`,
`:220`), which is a third order again and depends on ICU collation. The port needs:

- A `JsString` comparison helper that compares by UTF-16 code units, used by every index and
  sort path.
- A deliberate decision, documented, about `localeCompare`: either vendor a collation table for
  the locales RxDB tests, or declare index-string ordering (code-unit) as the only supported
  order and treat `localeCompare`-based sorts as a compatibility shim.

### 3.3 Async traits: no `async_trait` in the hot paths

`RxStorageInstance` (`src/types/rx-storage.interface.d.ts:116`) has 8 async methods plus a
`changeStream(): Observable<...>`. Every plugin wraps some subset of them. On native, `dyn` +
boxed futures is acceptable; on `wasm32` the object overhead is measurable. (Two of the eight are
optional upstream: `getChangedDocumentsSince` has a documented fallback.) Plan:

- Native: `trait RxStorageInstance` with `-> impl Future` returns where the concrete type is
  known, and a `#[cfg_attr]`-gated `dyn`-safe mirror where it is not.
- Trait objects where the plugin stack genuinely needs them (the wrapped-storage pattern:
  key compression, encryption, sharding, worker all wrap a storage). Use `dynosaur` (0.3.1) to
  generate the dyn-compatible twin automatically rather than hand-writing two traits.
- Streams: `futures::Stream` for change streams and query results. Do not leak RxJS.
  JS consumers get an adapter that turns the stream into an `Observable` at the boundary, in
  the same way `wasm-bindgen` adapts a `Promise`.

### 3.4 WASM is a first-class target with a capability matrix, not a single build

"Full WASM support" is not one build. It is a matrix, and the plan is explicit about which
cells are shipping in which milestone. The probe results below were produced on this machine
today; see section 8 for the commands.

| Target | Status here | Use |
| --- | --- | --- |
| `wasm32-unknown-unknown` | stable, builds on 1.95.0 and 1.99.0 | browser + any JS host via `wasm-bindgen` |
| `wasm32-wasip1` | stable, builds on 1.95.0 | WASI 0.1 hosts (Node `node:wasi`, Wasmtime) |
| `wasm32-wasip1-threads` | stable, builds on 1.95.0, shared memory confirmed | threaded WASI |
| `wasm32-wasip2` | stable, builds on 1.95.0 | WASI 0.2 component model |
| `wasm32-wasip3` | nightly only, installed, builds and emits a real component (`wasm-tools component wit` shows `wasi:clocks@0.3.0` imports) | WASI 0.3 native async. Do not ship; track |
| `wasm64-unknown-unknown` | Tier 3, `rustup target add` refused, `build-std` on nightly succeeded and emitted `(memory i64 16)` | >4 GiB datasets. Experimental only |
| `wasm32v1-none` | installed; our probe failed with "can't find crate for std" | MVP-only embeddings. Optional |

Feature-level findings, also probed (details and commands in section 8):

- `multivalue`, `reference-types`, `tail-call`, `simd128`, `relaxed-simd`, `extended-const`,
  `bulk-memory`, `nontrapping-fptoint` compile on **stable** 1.99 with no warning.
- `exception-handling`, `gc`, `wide-arithmetic`, `atomics` compile on stable but rustc warns
  "not stably supported; its behavior can change in the future". Treat as allowed-but-fragile,
  pin the toolchain, and gate in CI.
- `multimemory` and `relaxed-atomics` are "unknown and unstable" on 1.99. Do not use; revisit
  when they stabilise.
- Threads (`atomics` + shared memory) require either `wasm32-wasip1-threads` or
  `-Z build-std` on nightly, because the prebuilt `wasm32-unknown-unknown` std is not compiled
  with atomics. The probe confirms both paths produce a correctly linked artifact; the plain
  stable path fails with `--shared-memory is disallowed by std-....o because it was not compiled
  with 'atomics' or 'bulk-memory' features`.

Consequence for the plan: **`wasm32-unknown-unknown` is the primary supported WASM target and
must build on stable.** Threads and the component model are supported configurations behind
features, documented with their toolchain requirements. Nothing in the core may hard-require a
nightly-only feature.

WebAssembly 3.0 finished on 2025-09-17 and is the current live standard; it folds in 64-bit
address space, multiple memories, GC, typed references, tail calls, exception handling, relaxed
SIMD, and a deterministic profile. Browser availability lags the spec: WasmGC and tail calls
went Baseline "newly available" on 2024-12-11, exception references on 2025-05-29 (Chrome 137,
Firefox 131, Safari 18.4). So "use the latest WASM release features" means: **use them where
they buy something measurable, keep them behind capability detection, and never make a mobile
Safari user's build fail because we wanted GC.**

## 4. Architecture

### 4.1 Crate layout

A workspace, one crate per concern, matching the upstream split so the feature map stays
legible.

```
dxdb/
  crates/
    dxdb-core/            # createDatabase, collection, document, query, schema, hooks
    dxdb-schema/          # JSON-Schema handling, meta field injection, indexes, hashing
    dxdb-query/           # Mango selector parsing, normalisation, planner, index strings
    dxdb-reactivity/      # change-event buffer, event reduce, live query windows
    dxdb-storage/         # RxStorage / RxStorageInstance traits, conflict categorisation
    dxdb-replication/     # the replication protocol: checkpoints, downstream, upstream, meta
    dxdb-attachments/     # attachment data model + digest
    dxdb-js/              # wasm-bindgen surface, JS-side types, Observable adapter
    dxdb-node/            # napi-rs surface for Node.js
    dxdb-wasi/            # WASI host binding (component model) + CLI
    storages/
      dxdb-memory/        # reference storage, indexed in-memory, the parity oracle
      dxdb-indexeddb/     # browser, via web-sys + idb
      dxdb-opfs/          # browser, OPFS via web-sys
      dxdb-sqlite/        # sqlite-wasm-rs on wasm, rusqlite on native
      dxdb-filesystem/    # redb on native, OPFS-backed on wasm
      dxdb-localstorage/  # localStorage / any kv
      dxdb-remote/        # message-channel transport, mirrors storage-remote
    plugins/              # one crate each, mirroring src/plugins/*
  testdata/               # vectors exported from upstream RxDB
  xtask/                  # remote build + vector sync + conformance runner
```

The two rules that keep this sane:

- **`dxdb-core` never depends on a storage crate.** It depends on `dxdb-storage` traits.
  The parity oracle is `dxdb-memory`, and it is the only storage allowed in `dxdb-core`'s
  dev-dependencies.
- **Every crate builds for `wasm32-unknown-unknown` on stable.** CI enforces it, including
  for crates that only make sense elsewhere, because a dependency that breaks wasm deep in the
  tree is expensive to discover late.

### 4.2 The storage trait is the spine

Upstream's `RxStorage` / `RxStorageInstance` is the single most important interface in the
system. Port it faithfully, then let everything else be a wrapper:

- `bulk_write(rows, context)` is per-document atomic, no cross-document transaction, partial
  success with a per-row error list. This is load-bearing: the retry and conflict logic above it
  assumes it.
- `find_documents_by_id(ids, with_deleted)`, `query(prepared)`, `count(prepared)`,
  `get_attachment_data(doc_id, att_id, digest)`.
- `get_changed_documents_since(limit, checkpoint)` (optional, with a fallback defined upstream).
- `change_stream()`.
- `cleanup(minimum_deleted_time) -> bool` where `false` means "more work remains, call again".
- `close()`, `remove()`.

The wrapper pattern (compression wraps storage, encryption wraps storage, sharding wraps
storage, worker proxies storage) is how we get composition. Each wrapper is a crate that
implements the traits over an inner storage. This is also the clean-room boundary for the paid
features: the clean-room wrappers plug into the same trait and never touch core.

### 4.3 Semantics that must match exactly

Collected from the source, because these are the things a "spiritual port" gets wrong and then
silently corrupts data.

**Revisions are `{height}-{databaseInstanceToken}`** (`src/plugins/utils/utils-revision.ts:59`).
Not a content hash, not a UUID, not a vector clock. Height is 1 for a new doc, `previous + 1`
after. `parseRevision` accepts multi-digit heights. Anything that compares heights uses
`get_height_of_revision` without allocating.

**Index strings** are built per field type and concatenated, with the primary key padded in
(`src/custom-index.ts`). Numbers are zero-padded around a schema-declared min/max
(`getNumberIndexString`, `:274`), so the encoding depends on the schema's bounds. Port the
encoding exactly, and keep the boundary clamping behaviour (a value outside the schema range
clamps rather than producing a broken index).

**Sorting is by index string, code-unit order**, with the primary key appended if absent
(`normalizeMangoQuery`). A `desc` anywhere in the sort forces a re-sort because storages do not
support descending indexes (`src/query-planner.ts:48`).

**The Mango operator set is deliberately small** (`src/types/rx-query.d.ts:43`):
`$eq $gt $gte $lt $lt $lte $ne $in $nin $regex $options $exists $type $mod $not $size $elemMatch`
plus `$and $or $nor` and a `$not` object form. Not mingo's full surface. Unknown-CRUD and
`$or`/`$nor` pushdown limitations are documented upstream and must be matched, not improved,
because query plans are part of the wire behaviour.

**Logical operators usable in the planner** are only `$eq $gt $gte $lt $lte`
(`src/query-planner.ts:239`). The planner emits `startKeys`/`endKeys`/`selectorSatisfiedByIndex`
(`:244`) and storages decide whether to trust it. `INDEX_MIN` is `Number.MIN_SAFE_INTEGER`, not
`-Infinity`, specifically because a query plan must survive `JSON.stringify`
(`src/query-planner.ts:25`, with the explaining comment at `:20`).

**Bulk write categorisation** (`src/rx-storage-helper.ts:205` `categorizeBulkWriteRows`) is where
insert / update / conflict / same-state-fork decisions are made. This function is the heart of
RxDB's consistency story and should be ported as a single well-tested pure function with the
same inputs and outputs, not reinvented as scattered logic.

**Schema defaulting** (`src/rx-schema-helper.ts`) injects `_rev`, `_deleted`, `_meta`,
`_attachments` into properties and `required`, and prepends `_deleted` to every index unless it
is already first (`:271`). Schema hash caching exists and the hash is part of schema identity.

**Replication protocol** (`src/replication-protocol/*`, 2,121 lines) is checkpoint-stack based:
downstream pulls `(documents, checkpoint)`, upstream pushes
`{assumedMasterState, newDocumentState}` and receives back the master states for conflicts only.
Conflicts are resolved by a user-supplied handler whose default drops the fork and keeps the
master (`default-conflict-handler.ts`). The meta instance persists per-replication checkpoints
(`meta-instance.ts`). Getting this wrong breaks interop with every RxDB server implementation,
so it gets its own conformance suite (section 7).

**Hooks and plugin injection**: upstream mutates five prototypes at plugin add time
(`src/plugin.ts:35`) and exposes 24 named hooks (`src/hooks.ts`). Rust has no prototype
mutation. Re-expression:

- Plugin registration returns a `PluginHandle` and a registered set, ordered.
- Hooks become typed registration on a `Hooks` registry owned by the database, with explicit
  ordering and the same names (`preCreateRxDatabase`, `createRxCollection`, `preCreateRxSchema`,
  ...).
- Entity methods that plugins add (the `RxDocument` ORM-method pattern) become extension traits
  plus a type-indexed map on the document handle. Generic `OrmMethods` in TypeScript maps to a
  trait bound in Rust.

## 5. The WASM strategy

**Toolchain truth.** The shared build box runs `1.95.0` and that pin governs this workspace
(7.4). Everything the plan needs from stable Rust must therefore work on 1.95.0, which was
verified per-feature in section 8. Nightly is for the WASI 0.3 component, `build-std` variants,
and the exception-handling experiment only. If a feature needs nightly, it does not ship as a
default.

### 5.1 Three bindings, one core

The core crates know nothing about JS, JS promises, or `wasm-bindgen`. Three thin binding
crates adapt it:

| Binding | Target | Consumers | Notes |
| --- | --- | --- | --- |
| `dxdb-js` | `wasm32-unknown-unknown` | browsers, Deno, Bun, edge workers, Electron renderer | `wasm-bindgen` 0.2.129, `tsify-next` 0.5.6 for typed options, `wasm-bindgen-futures` for promises, a generated `#\[wasm_bindgen\]` Observable bridge |
| `dxdb-node` | native node via napi, plus `wasm32-wasip1` as the fallback | Node servers, CLIs | napi where native speed matters, WASI where install-free matters |
| `dxdb-wasi` | `wasm32-wasip2` (0.2 component), `wasm32-wasip3` nightly | Wasmtime/WasmEdge hosts, serverless, component composition | `wit-bindgen` 0.63.0, exports a WIT world |

### 5.2 How we actually use newer WASM features

Not "because they exist". Each one is tied to a measured problem:

- **`simd128` (stable):** JSON scanning (finding `_rev`/`_deleted` without full parse), index
  string construction over byte slices, attachment digests (Sha256 is already fast; the win is in
  scanning). Benchmarked against scalar before adoption, per operation.
- **`exception-handling` (warning on stable, but really nightly):** maps to Rust panics with
  `panic="unwind"`, giving catchable errors across the boundary instead of trapping the module.
  Today an uncaught Rust error aborts and takes the JS caller's whole database instance with it.
  Verified caveat: real `try`/`catch`/`throw` instructions only appear with
  `-C panic=unwind` **plus nightly `-Z build-std=std,panic_unwind`**; stable fails with "the
  crate `panic_unwind` does not have the panic strategy `unwind`". So this is a nightly
  configuration. Default stays `panic="abort"` with a trap-based error path, EH behind a feature
  flag and a pinned toolchain. See the probe log in section 8.
- **`tail-call` (stable):** the replication loop and recursive schema traversal. Cheap to enable,
  no downside on engines that support it (Baseline since 2024-12-11).
- **`relaxed-simd` (stable):** only in the SIMD kernels, gated, because results must be
  deterministic for index strings. Never let a relaxed instruction touch anything that becomes
  an index string or a hash. The W3C 3.0 "deterministic profile" is the right mental model:
  relaxed instructions are allowed where a wildcard result is legal within a set.
- **`atomics` / threads:** needed for the worker and shared-worker storage equivalents and for
  parallel scan. Two paths, both verified above: `wasm32-wasip1-threads` on stable, or
  `-Z build-std` on nightly for `wasm32-unknown-unknown`. `wasm-bindgen-rayon` 1.3.0 still
  works but was last published 2024-12-21; prefer our own small thread pool adapter so we
  are not stuck on a stale bridge.
- **`gc`:** not for our own data (Rust uses linear memory). It matters for the JS glue: strong
  `externref` handles instead of the table-based `JsValue` indirection that `wasm-bindgen`
  uses today. Track it, do not build on it yet, and measure the glue cost before touching it.
- **`wide-arithmetic`:** 128-bit multiply-add for hashing and checksums. Keep behind a flag,
  measure.
- **64-bit memory / `wasm64`:** the out-of-memory answer for large local datasets. Tier 3,
  build-std, no `rustup target add`. Experimental target `wasm64-unknown-unknown`, and on the
  web a 64-bit memory is capped at 16 GiB anyway.
- **Multiple memories (`multimemory`):** unstable/unknown on 1.99. Ignored, but the design keeps
  open the option of separating a document heap from an index heap, because Wasm 3.0 makes that
  a linker concern rather than an architecture rewrite.
- **JSPI:** lets synchronous wasm call async JS APIs, which is exactly the OPFS
  `createSyncAccessHandle` story. Node 22 here accepts `--experimental-wasm-jspi`, Chrome 137+
  shipped it. Not a dependency, but it is the fallback path for storages that are only
  synchronous in a worker.

### 5.3 Wasm budget

Targets, enforced in CI as size budgets (the same idea as upstream's `bundle-size` job):

| Artifact | Budget | Notes |
| --- | --- | --- |
| `dxdb-js` core only, no storages | 250 KiB brotli | schema + query + reactivity |
| `dxdb-js` + memory storage | 320 KiB brotli | the "just works" bundle |
| `dxdb-js` + OPFS storage | 400 KiB brotli | the browser production bundle |
| `dxdb-js` + SQLite storage | 900 KiB brotli | wasm sqlite is most of it |

Budgets are aspirational until milestone M4 measures them; they get revised with numbers, not
vibes. `opt-level="s"`, `lto="fat"`, `codegen-units=1`, `panic="abort"` as the baseline release
profile, then per-crate overrides for the hot paths.

### 5.4 Storage availability on wasm

The honest matrix, because this is where "WASM support" claims usually fall apart:

| Storage | native | browser | worker | WASI |
| --- | --- | --- | --- | --- |
| memory | yes | yes | yes | yes |
| filesystem (redb) | yes | no | no | yes (`preopens`) |
| OPFS | no | yes (async), yes (sync, worker only) | yes | no |
| IndexedDB | no | yes | yes | no |
| SQLite (`sqlite-wasm-rs` 0.6.2) | yes (`rusqlite` 0.41) | yes via SAH-pool VFS, **worker only** | yes | yes |
| localStorage | no | yes | no | no |
| remote | yes | yes | yes | yes |

Note the OPFS constraint, which is upstream's too: `createSyncAccessHandle` exists only in a
dedicated worker, so the fast OPFS path is inherently a worker story. That is why the worker
storage and OPFS storage land together.

## 6. Milestones

Sequenced so each milestone is independently demonstrable and each one de-risks the next.
Every milestone has an exit gate that is a command, not a vibe.

### M0: Skeleton and probes (week 1)

- Workspace, CI matrix (native, `wasm32-unknown-unknown`, `wasm32-wasip1`,
  `wasm32-wasip2`), `clippy -D warnings`, `rustfmt --check`.
- Three throwaway probes promoted into permanent tests: `js_number` formatting table,
  UTF-16 comparison table, `regress` regex semantics table (already validated: JS-style flags
  are a `Flags` struct, not inline `(?i)`; `\p{Letter}` needs `unicode: true`).
- Remote build wiring (section 7.4) so nobody's local disk fills up.
- Gate: `cargo test --workspace` green on host and on `wasm32-unknown-unknown` (via
  `wasm-bindgen-test` in a headless browser).

### M1: Schema and query (weeks 2-4)

`dxdb-schema` + `dxdb-query`. Schema defaulting, meta injection, index builder, schema hash,
Mango parsing, normalisation, planner, index strings, all comparison semantics.

- Gate: upstream's `test/unit/rx-query.test.ts`, `rx-schema.test.ts`, and
  `rx-query-helper.test.ts` translated to Rust, plus a fuzz harness that generates random
  queries and random documents and compares match results against a recorded JS oracle
  (`test/query-correctness-fuzzing.test.ts` exists upstream; we reuse its generator ideas,
  not its code).

### M2: Database, collection, document (weeks 4-7)

`dxdb-core` on top of `dxdb-memory`. CRUD, bulk operations, revisions, conflict
categorisation, the document cache, change events, hooks, live queries, the write queue.

- Gate: upstream's `test/unit/rx-database.test.ts`, `rx-collection.test.ts`,
  `rx-document.test.ts`, `rx-query.test.ts`, `rx-storage-helper.test.ts`, plus the full
  `rx-storage-implementations.test.ts` and `rx-storage-query-correctness.test.ts` suites running
  against `dxdb-memory`. Live-query correctness measured by the fuzz harness from M1 extended to
  change events.

### M3: Replication protocol (weeks 7-9)

`dxdb-replication`. Checkpoints, downstream, upstream, meta instance, conflict handlers,
replication state machine, pause/resume/cancel, retry.

- Gate: `src/plugins/test-utils/replication-base-test-suite.ts` (454 lines, the shared suite
  every replication plugin runs) ported once and run against in-memory + filesystem endpoints.
  Plus a two-process test: dxdb -> HTTP -> dxdb, asserting document convergence and checkpoint
  equality.

### M4: Browser storages and the JS bundle (weeks 9-12)

`dxdb-js`, `dxdb-indexeddb`, `dxdb-opfs` (with the worker path), size budgets, the Observable
bridge, a TypeScript declaration file that a real app can compile against.

- Gate: a demo app, built from the repo, that runs the same conformance suite in a browser via
  Playwright, on Chromium, Firefox, and WebKit. Bundle size within budget.

### M5: WASI, node, and the component (weeks 12-14)

`dxdb-wasi` with a WIT world and a `wasm32-wasip2` component, `dxdb-node` via napi, filesystem
storage on redb, and the `wasm32-wasip3` nightly build tracked but not shipped.

- Gate: the conformance suite runs under Wasmtime from a `.wasm` component with a preopened
  directory, and under Node from both the napi build and the WASI build.

### M6: Parity push (weeks 14-20)

The long tail of open-source plugins, in order of what unblocks users: attachments,
query-builder, update, key-compression, migration-schema, migration-storage, local-documents,
leader-election, cleanup, json-dump/import-export, backup, crdt, state, pipeline, dev-mode,
the validator plugins, the replication adapters (couchdb, graphql, websocket, http, webrtc),
and the reactivity adapters.

- Gate: the full ported test corpus. This is where the 55k lines of upstream tests pay off.

### M7: Clean room (parallel from week 6, heavy from week 20)

Section 8 and [CLEANROOM.md](./CLEANROOM.md). Runs in parallel as a separate track with its
own repository boundary, so that clean-room authors never see the closed source and the port
authors stay focused.

## 7. Verification strategy

The whole plan is worthless without a way to prove parity. Five layers, cheapest first.

### 7.1 Ported upstream tests

Upstream's tests are Apache-2.0. Translate them mechanically where possible: the patterns are
`insert -> query -> assert`, `bulkWrite -> expect no errors`, `replicate -> waitUntil`. Build a
small `dxdb-testkit` that mirrors upstream's `test-utils` ergonomics (the `humans` collection
schema, `randomToken`, `waitUntil`, `isFastMode`) so translations stay one-to-one and reviewable
against the original.

### 7.2 Exported vectors (the cross-language oracle)

The cleverest trick available: **run upstream RxDB in Node and export expected outputs as JSON.**

- Index strings for every schema/field/value combination in upstream's test fixtures.
- Schema hashes for every fixture schema.
- Normalised queries for a generated query corpus.
- `categorizeBulkWriteRows` outputs for generated write sequences.
- Replication transcripts: a scripted sequence of pull/push results, and the resulting
  checkpoints and resolved document states.

Vectors live in `testdata/vectors/*.json`, generated by `xtask vectors`, committed, and diffed
in CI. A drift in either implementation shows up as a data diff, not a mystery test failure.
This is what makes "the Rust port is behaviourally identical" a claim we can assert.

### 7.3 Fuzzing and property tests

- `proptest` for the selector matcher: generate documents and selectors, assert our matcher
  agrees with the JS oracle vectors.
- Cross-implementation differential fuzzing for the storage contract: random operation
  sequences against memory storage, assert the invariants upstream guarantees (per-document
  atomicity, monotonic revisions, no lost writes, `count` equals `query().len()`).
- A `cargo-fuzz` target on the schema/index path, since that parses untrusted JSON.

### 7.4 Build discipline: never compile locally, never disturb other builds

Rust builds here are remote, via `cargo-remote-3000` against the shared Fly.io build server.
That is a hard rule for this project, not a preference: a debug `target/` for this workspace
will be tens of gigabytes and local disk is already at 46% with 482 GB free, which is fine
today and will not be fine on a fresh `cargo clean && cargo build --workspace --all-targets`
over a 20-crate workspace with `wasm-bindgen` and SQLite in the tree.

**Use the shared box, share it politely.**

```bash
# the only way builds happen in this repo (see the Makefile below)
cargo remote-3000 -r fly -d 1.95.0 check  --workspace --all-targets
cargo remote-3000 -r fly -d 1.95.0 clippy --workspace --all-targets -- --deny warnings
cargo remote-3000 -r fly -d 1.95.0 test   --workspace
```

Rules, learned the hard way and recorded here so nobody re-learns them:

- **`-d` is mandatory.** Without it cargo-remote runs `rustup default stable` and installs
  stable, silently un-pinning and re-pointing every project on the machine.
- **dxdb builds alongside, never over.** The box is shared: 8 cores, `[build] jobs = 4`
  in the shared cargo config, one `target/` per project under `/data/builds/<hash>`. We do not
  raise `CARGO_BUILD_JOBS`, we do not `cargo clean` anything that is not ours, and we do not
  touch `/data/cargo` or `/data/rustup` outside `rustup target add`.
- **Check before you build, check before you stop.** Run `make busy` before a big upload, so a
  full-volume rsync does not collide with someone else's compile, and before
  `flyctl machine stop`, because stopping mid-compile destroys another project's build.
  **Do not use `pgrep -f "rustc|cargo"` for this.** `-f` matches the pattern against every
  command line, including the `pgrep` and the `sh -c` that carry it, so a naive guard reports
  "busy" permanently and any count taken that way is inflated. A measurement during this
  session read "19 rustc processes"; the true number was 2 `cargo check` processes, both from
  another project. Match executable names (`ps -eo comm=`, `pgrep -x`) instead.
- **Disk is the shared resource, and it is currently exhausted.** The volume went from 95% to
  **100% full (599 MB free of 98 GB)** during the session that wrote this plan, with 15-16 build
  trees, 89 GB of `/data/builds`, and another project's `cargo check --workspace` resident for
  the whole session. Nothing was stopped or pruned that was not ours, because doing so would
  have destroyed someone else's compile. That is the correct behaviour, and it means dxdb has to plan for "no disk today".
  Consequences:
  - A workspace with SQLite and `wasm-bindgen` is 2-4 GB per build tree. Before the first real
    build, confirm headroom (`ssh fly-builder 'df -h /data'`) rather than assuming it.
  - Prefer `check` and `clippy` for iteration. A full `build` or `test` with a cold dependency
    graph is the expensive case; do it deliberately, not casually.
  - Never `-c` the whole target tree back (`-c=release/my-binary` for one artifact, or nothing).
  - Prune **only** our own tree, and only `/data/builds/<hash>` whose `Cargo.toml` is ours.
    Verify with `ssh fly-builder 'grep -l dxdb /data/builds/*/Cargo.toml'` before deleting
    anything.
  - If the box cannot host us, the answer is a **separate build space** (the project rule is
    "set up a separate user account or build space on the server if necessary"), not raising
    `CARGO_BUILD_JOBS` on a shared box or deleting a neighbour's tree.
- **Never stop the machine while it is busy.** A `flyctl machine stop` while another project's
  `cargo check` is running destroys that work and cannot be undone. The stop step only applies when
  `pgrep` shows nothing running, and when the stop is ours to make (we started it, or everyone
  sharing it has finished). Leaving a machine running is a billing cost; stopping it mid-build is
  a data loss. The billing cost is the cheaper mistake.
- **WASM targets need provisioning once.** The box currently has only `wasm32-unknown-unknown`
  and `x86_64` (verified). `wasm32-wasip1` and `wasm32-wasip2` get added to `/data/rustup`, which
  is persistent and shared, so it is a one-time cost, and it must match the pinned toolchain:
  `cargo remote-3000 -r fly -d 1.95.0 build` will need those targets present. Coordinate the
  `rustup target add` with whoever else is on the box, since it mutates shared state.
- **`--` before cargo flags** when using `-p`, because cargo-remote parses its own flags after
  the cargo subcommand and `-p` means `--ssh-port` to it.
- **Long builds run detached with a log**, per the repo conventions:
  `setsid nohup sh -c '... > /tmp/dxdb-remote.log 2>&1; echo EXIT=$? >> /tmp/dxdb-remote.log' &`.
- **Scale the machine to zero when the last build finishes.** Billing is per second. The
  documented flow is `machine status` -> `pgrep` -> `stop`, and the pgrep is not optional.
- **`-d` also MUTATES shared state: it runs `rustup default <toolchain>`.** The box has
  exactly one toolchain installed today, `1.95.0`. Asking for `-d 1.99.0` downloads it and makes
  it the default for everybody else's next build on that machine. So the toolchain is a
  coordination point, not a per-project preference:
  - **Default to `-d 1.95.0`**, the pin the shared cargo config was written against. Our code
    must stay 1.95-compatible anyway; wasm feature support we rely on is available there.
  - Move the box to a newer toolchain only after messaging the other agents/projects that share
    it, and only by agreement. Record the change and its date in `docs/TOOLCHAIN.md`.
  - Per-project `rust-toolchain.toml` is not a substitute: cargo-remote ignores it in favour of
    `-d`.
- **Verify the remote path before trusting it.** It was verified for this plan: a trivial crate
  built and tested on the box with `-d 1.95.0`, one test passed, `Cargo.lock` came home, exit 0,
  and the box was left exactly as found. Re-run that probe whenever the remote config changes.

Project wiring that makes it hard to do the wrong thing:

```make
# Makefile -- the only supported way to compile this workspace
TOOLCHAIN := 1.95.0            # matches the shared pin; see docs/TOOLCHAIN.md before changing
CARGO     := cargo remote-3000 -r fly -d $(TOOLCHAIN)
MACHINE   := 87477e0cd01748
APP       := open-surrealdb-ds-builder
# Match by executable name. `pgrep -f rustc` also matches the pgrep's own command
# line, so a -f guard reports "busy" forever and a -f count is inflated. Keep the
# predicate inline in the recipe: putting it in a make variable loses the quoting.
BUSY      := 'ps -eo comm= | grep -qxE "rustc|cargo" && echo BUSY || echo IDLE'

.PHONY: check clippy test wasm remote-up busy headroom
check:
	$(CARGO) check --workspace --all-targets
clippy:
	$(CARGO) clippy --workspace --all-targets -- --deny warnings
test:
	$(CARGO) test --workspace
wasm:
	$(CARGO) check --workspace --target wasm32-unknown-unknown
remote-up:
	flyctl machine start $(MACHINE) -a $(APP)
busy:
	@ssh fly-builder $(BUSY)
headroom:
	@ssh fly-builder 'df -h /data | tail -1'
```

Three things about that Makefile are the result of getting them wrong while writing this plan,
so they are worth reading before anyone edits it:

1. **`busy` asks the box and prints one word.** An earlier version piped the whole pipeline
   through `ssh` and tested its exit status. Because `ssh` returns the remote shell's status and
   the quoting put the pipeline in the wrong shell, it ran `cargo --help` instead. A guard you
   cannot trust is worse than no guard.
2. **There is no automatic stop target, on purpose.** A `remote-down` that stops the machine is
   one typing mistake away from killing a neighbour's compile, and the mistake is silent. So the
   documented flow is: `make busy`, then human judgement, then `flyctl machine stop` by hand.
   The stop is the deliberate action, not the convenient one. If you do script it in your own
   environment, gate it on `$$(ssh fly-builder '<BUSY>')` printing `IDLE`, never on an exit code.
3. **`headroom` exists because the volume reached 0 bytes free** (100%, showing `0` available)
   while another project was mid-compile. Check it before any build that has a cold dependency
   graph.

Plus a `.cargo-remote-3000.toml` in the repo root pinning the remote name and excludes, so no
one has to remember flags.

### 7.5 Performance gates

Upstream measures storage performance with `test/performance.test.ts` and a
`performance-validation` job. We mirror the shape: a criterion bench per storage, a JSON result
file per run, and a CI gate that fails on a regression over a threshold. Numbers are recorded in
`docs/BENCHMARKS.md` so the Rust-advantage claims are evidence, not assertion.

## 8. Probes already run (evidence for the claims above)

All on this machine, 2026-10-25. Toolchains used: rustc 1.95.0 (the shared build-server pin),
rustc 1.99.0 stable, and rustc 1.101.0-nightly. Where 1.95.0 and 1.99.0 could differ, the 1.95.0
result is the one that counts.

**Feature stability, per-feature on `wasm32-unknown-unknown`.** Run twice, on rustc 1.99.0 and on
**1.95.0**, the toolchain the shared build box actually has. Identical results, which matters,
because 1.95.0 is what will compile this workspace:

```
stable           multivalue, tail-call, reference-types, simd128, relaxed-simd,
                 extended-const, bulk-memory, nontrapping-fptoint
unstable (warn)  exception-handling, gc, wide-arithmetic, atomics
unknown+unstable multimemory, relaxed-atomics
```

**Target build results:**

```
                         1.95.0   1.99.0   nightly   notes
wasm32-unknown-unknown   OK       OK       OK        primary target
wasm32-wasip1            OK       OK       OK
wasm32-wasip1-threads    OK       OK       OK        shared memory confirmed
wasm32-wasip2            OK       OK       OK        component model
wasm32-wasip3            n/a      n/a      OK        22 KB real component, imports wasi:clocks@0.3.0
wasm64-unknown-unknown   refused  refused  OK*       * rustup refuses (Tier 3); -Z build-std works,
                                                      emits (memory i64 16)
wasm32v1-none            -        failed   -         can't find crate for std
threads, plain wasm32    failed   failed   OK*       * only with -Z build-std;
                                                      stable: std not compiled with atomics
                                                      nightly: (memory 0 16 16 shared)
```

The important line is the first column: everything the plan ships as a default builds on the
pinned 1.95.0. `wasm32-wasip3`, `wasm64`, and threaded `wasm32-unknown-unknown` do not, which is
exactly why they are opt-in.

**Crate feasibility (not just "does it compile", does it behave):** a probe crate with
`wasm-bindgen` 0.2.129, `jsonschema` 0.60 (draft-07), `regress` 0.12, `sha2` 0.11,
`serde_json` with `preserve_order` built cleanly for host and `wasm32-unknown-unknown` on both
1.99.0 and **1.95.0**, and its four tests pass on both (3.7-3.9 MB wasm before the size budget's
LTO and `opt-level="s"` are applied). Tests confirmed:

- draft-07 validation accepts and rejects correctly,
- `regress` expresses JS regex semantics including lookbehind, named groups, backreferences,
  and unicode property escapes (which require `Flags { unicode: true }`, not inline `(?u)`),
  and case-insensitivity (which requires `Flags { icase: true }`, not inline `(?i)`),
- `sha2` matches the known `abc` vector,
- `serde_json` with `preserve_order` preserves insertion order through parse and re-serialize.

**Exception handling needs `panic=unwind` and a nightly `build-std`:**

```
stable,   +exception-handling                          -> warns "not stably supported"
stable,   +exception-handling -C panic=unwind          -> error: panic_unwind has strategy abort
nightly,  -C panic=unwind -Z build-std=std,panic_unwind -> builds; wasm has try/catch/throw + a tag
```

Catchable panics across the JS boundary are therefore a nightly configuration, not a stable one.
Section 5.2 records this, and it is why the default profile keeps `panic="abort"`.

**Semantic gaps found by probe** (these become tests in M0, and are the reason section 3 exists):

```
JS   String(1e21)                -> "1e+21"     Rust Display -> "1000000000000000000000"
JS   JSON.stringify(1.0)         -> "1"         Rust serde_json -> "1.0"
JS   Math.round(-0.5)            -> -0          Rust f64::round -> -1
JS   Math.round(-1.5)            -> -1          Rust f64::round -> -2
JS   '\u{1F600}' < '\uFFFF'      -> true        Rust str <     -> false
```

Each one is a data-corruption bug if it reaches a stored document, an index string, or a
revision.

## 9. Rust crates

Chosen, with the reason, so nobody re-litigates this in month two.

| Need | Crate | Version | Why this one |
| --- | --- | --- | --- |
| JS binding | `wasm-bindgen` | 0.2.129 | only real option; MSRV 1.81 |
| Promises | `wasm-bindgen-futures` | 0.4.79 | pairs with the above |
| Web APIs | `web-sys`, `js-sys` | 0.3.106 | IndexedDB, OPFS, crypto, workers |
| JS-shaped typed options | `tsify-next` | 0.5.6 | generates `.d.ts` from Rust types |
| JSON | `serde_json` | 1.0.154 | **with `preserve_order`**, see 3.1 |
| Fast JSON | `simd-json` | 0.18 | optional, SIMD scan only, native + wasm |
| JSON Schema | `jsonschema` | 0.60.1 | draft-07 (RxDB's draft), wasm-clean, `Validator: Send+Sync` on wasm since 0.38 |
| Regex (Mango `$regex`) | `regress` | 0.12 | ECMAScript semantics, not Rust/PCRE. Verified above |
| Regex (fast path) | `regex` | 1.13 | for anchored/index-friendly selectors; must agree with `regress` or not be used |
| Hashing | `sha2` | 0.11.1 | WebCrypto-compatible SHA-256 |
| Encryption | `aes-gcm` 0.11.1 / `chacha20poly1305` 0.11.0 / `argon2` | | clean-room envelope is chacha20poly1305 + argon2; `aes-gcm` only for a WebCrypto-compatible option |
| Async | `futures` 0.3.34; `tokio` 1.54 native-only by policy | | tokio compiles for wasm32 but its timers and IO do not work there, so the core uses `futures` and the platform binding supplies the executor (`wasm-bindgen-futures` in JS, `wasi` poll in WASI) |
| Dyn traits | `dynosaur` | 0.3.1 | auto-generated dyn-compatible trait twins |
| Native kv/storage | `redb` | 4.4.0 | pure Rust, no C, works for filesystem storage |
| SQLite | `rusqlite` 0.41 (native) / `sqlite-wasm-rs` 0.6.2 (wasm) | | rusqlite's `default` features already include `ffi-sqlite-wasm-rs`, so it swaps FFI on wasm32 without a fork; there is also a `wasm32-wasi-vfs` feature for WASI hosts |
| OPFS from SQLite | `sqlite-wasm-vfs` SAH-pool | | worker-only, documented; `rusqlite` defaults to the in-memory VFS on wasm, so OPFS must be installed explicitly |
| Component model | `wit-bindgen` | 0.63.0, `@bytecodealliance/jco` 1.39 | WASI 0.2 today, 0.3 via `async` feature |
| Node | napi-rs | | native Node surface |
| Errors | `thiserror` | 2 | mirrors upstream's error-code catalogue (196 codes) |
| Tracing | `tracing` + `tracing-wasm` | | mirrors upstream's logger plugin hook points |
| Tests | `proptest`, `cargo-fuzz`, `wasm-bindgen-test` 0.3.79, `playwright` (JS side) | | |

Deliberately not chosen as a default: `tantivy` 0.27, for the fulltext clean room. It is a large
dependency with its own wasm story, and it is offered only as an optional native backend there,
never as the baseline (see CLEANROOM 5.6).
`nostr-tools`, `simple-peer`, `graphql` equivalents only appear if and when the matching
replication adapters are ported.

## 10. What "done" means for the port

A checklist, because "port complete" is otherwise unfalsifiable.

- [ ] Every module in upstream `src/` (core + 52 plugin dirs) has a dxdb counterpart or a
      written, reviewed justification for omission.
- [ ] The ported test corpus passes, and each ported test names the upstream file and line it
      came from.
- [ ] The exported vector suite matches upstream byte for byte.
- [ ] A store written by upstream RxDB (Dexie) can be migrated into dxdb's equivalent storage
      and back without semantic loss: same documents, same revisions, same schema hashes.
- [ ] Replication interop: dxdb client <-> upstream RxDB client through the same HTTP endpoint
      converges, and checkpoints survive a restart on both sides.
- [ ] Native, `wasm32-unknown-unknown`, `wasm32-wasip1`, and `wasm32-wasip2` all pass CI on
      stable.
- [ ] WASM size budgets met and recorded.
- [ ] Benchmarks recorded; the Rust numbers are stated honestly, including where they lose.
- [ ] Apache-2.0 LICENSE, a NOTICE that credits RxDB and Netflix/Microsoft per upstream's
      license text, and a README that says plainly this is an unaffiliated port.
- [ ] No `Co-Authored-By` trailers in git history, per repo policy.

## 11. Risks

| Risk | Impact | Mitigation |
| --- | --- | --- |
| JS number/string semantics drift | silent data corruption on interop | M0 probes promoted to permanent tests; `js_number` and `js_string` are core modules, not helpers |
| `localeCompare` / ICU collation | sort order differs from RxDB | decide early, document, keep behind a feature; index-string order is the contract |
| Upstream moves (17.6.0 today) | port chases a moving target | pin a commit per release train, port in labelled tranches, diff upstream changelogs each train |
| Paid features are closed source | clean room is the only route | hard repo separation, reading list, reviewer sign-off; see CLEANROOM.md |
| Shared build server contention (hit 100% disk full during planning) | blocked builds, upset neighbours | check headroom before building, prune only our own verified tree, `check`/`clippy` first, separate build space or account if the box cannot host us, never stop a busy machine |
| Threads/atomics need nightly or `-threads` target | "latest WASM features" claim overreaches | ship stable-only defaults; threads are a documented opt-in configuration |
| Rust ecosystem WASM regressions | build breaks in a dependency | CI builds every crate for wasm on every PR, including non-wasm ones |
| Scope: 52 plugins, 55k test lines | multi-month effort, burnout | milestone gates, port the 12 plugins users actually need first, publish a status matrix |

## 12. Immediate next steps

1. Land this plan and CLEANROOM.md, plus `docs/STATUS.md` (a live parity matrix, one row per
   upstream module, three columns: not started / in progress / ported + tested).
2. M0 scaffolding: workspace, CI, the `Makefile` and `.cargo-remote-3000.toml` from 7.4, and
   the three probe suites as real tests.
3. Provision `wasm32-wasip1`, `wasm32-wasip2` (and coordinate nightly `wasm32-wasip3`) on the
   shared build server, and record the toolchain decision.
4. Build `xtask vectors` and generate the first vector set from upstream RxDB in Node, before
   writing a single line of `dxdb-query`. Vectors first, code second.
5. Start M1 in parallel with M7's reading-list phase, with separate authors.
