# Lane F plan: browser storages, the JS binding, and the demo harness

**Status:** Tranche 1 complete. Implementation is **gated** on lane D publishing
`dxdb-storage`'s traits and `dxdb-memory`. This document is the contract this
lane will implement against the moment those land, plus the one thing that could
be measured without them: the wasm size baseline.

Everything below is written against a pinned upstream RxDB at `3b763ee17` and the
plan sections named inline (`docs/PLAN.md`).

---

## 1. What Tranche 1 produced

| Deliverable | Where | State |
| --- | --- | --- |
| `dxdb-js` crate with the wasm-bindgen ABI pinned | `crates/dxdb-js/` | compiles, exports a 3-function smoke surface |
| Measured wasm baseline | this file, §6 | **10,008 B brotli** shipped; **5,373 B** fixed floor |
| Runnable 3-engine Playwright harness | `demo/` | passes on Chromium, Firefox, WebKit |
| Remote build path for the wasm artifact | `demo/scripts/build-wasm.sh` | `make`-equivalent, flock-guarded, no local compile |
| `make check` | — | exit **0** |

The storages (`dxdb-indexeddb`, `dxdb-opfs`, `dxdb-localstorage`, `dxdb-remote`)
are **not** created yet. Section 3 explains exactly what is missing and why
writing them now would produce code that has to be thrown away.

---

## 2. The wasm-bindgen surface

The public surface is fixed here so the `.d.ts` and the demo can be written
against it before the traits exist. Function names are the ones a JS RxDB user
already knows, because a rename is a migration for every downstream app.

### 2.1 Wasm-level functions (the `#[wasm_bindgen]` exports)

All of these live in `crates/dxdb-js`. Types are stated as they appear in the
hand-written `.d.ts`; the Rust signature is the same shape with `JsValue` where
an opaquely-typed object crosses.

```
// --- lifecycle ---------------------------------------------------------------
createDatabase(options: DatabaseOptions): Promise<DatabaseHandle>
// Rejects on schema/hash mismatch, unsupported storage, or a name collision.

// --- collection access -------------------------------------------------------
database.collection(name: string): Promise<CollectionHandle>
database.close(): Promise<void>            // idempotent; resolves after streams complete
database.remove(): Promise<void>           // destroys backing storage

// --- collection --------------------------------------------------------------
collection.insert(doc: DocumentData): Promise<DocumentData>
collection.bulkInsert(docs: DocumentData[]): Promise<BulkWriteResult>
collection.upsert(doc: DocumentData): Promise<DocumentData>
collection.bulkUpsert(docs: DocumentData[]): Promise<BulkWriteResult>
collection.find(ids: string | string[]): Promise<DocumentData[]>
collection.findOne(query: MangoQuery): Promise<DocumentData | null>
collection.find(query: MangoQuery): Promise<DocumentData[]>
collection.count(query: MangoQuery): Promise<number>
collection.removeDoc(id: string): Promise<DocumentData>
collection.bulkRemove(ids: string[]): Promise<BulkWriteResult>
collection.checkpoint(): Promise<Checkpoint>

// --- reactivity --------------------------------------------------------------
collection.changes(options?: ChangeStreamOptions): Observable<EventBulk>
collection.find$(query: MangoQuery): Observable<DocumentData[]>
collection.count$(query: MangoQuery): Observable<number>
collection.findOne$(query: MangoQuery): Observable<DocumentData | null>
```

**Every one of those that returns `Promise<T>` is `wasm-bindgen-futures`-backed**,
not a callback, because `wasm-bindgen` already adapts a Rust `Future` to a JS
`Promise` and doing it twice is how you get a promise that never settles.

**Types.** `DocumentData` is a plain JS object (`JsValue`), not a
`serde_json::Value` round-trip: PLAN.md 3.1 makes documents ordered JSON, and
`serde_json` with `preserve_order` inside wasm means two allocations per document
crossing the boundary. The ordered-JSON crate is `dxdb-json` and it already
exists (lane A).

`DatabaseOptions` and `MangoQuery` are **`tsify-next`**-derived structs (PLAN.md
5.1 pins `tsify-next` 0.5.6) so the `.d.ts` for options is generated from the
same Rust type that deserializes them. Actually-used options:

```ts
interface DatabaseOptions {
  name: string;
  storage: StorageName;          // 'memory' | 'indexeddb' | 'opfs' | 'localstorage' | 'remote'
  storageOptions?: StorageOptions;  // discriminated on `storage`, see §4
  multiInstance?: boolean;       // default true in a browser
  ignoreDuplicate?: boolean;
  hashFunction?: 'sha256';       // schema hash, PLAN.md 4.3
}
```

### 2.2 The Observable adapter

PLAN.md 3.3 is explicit: streams are `futures::Stream` in Rust, and *"JS
consumers get an adapter that turns the stream into an `Observable` at the
boundary, in the same way `wasm-bindgen` adapts a `Promise`."* No RxJS in the
dependency graph — the adapter is a hand-written JS object that satisfies the
Observable contract subset an RxJS `from()` or `subscribe()` accepts.

#### The contract, stated as assertions

```
observable.subscribe(next, error, complete) -> Subscription
```

1. **`subscribe` returns a subscription object with `unsubscribe()`.**
   `unsubscribe()` is **idempotent**: a second call is a no-op, not an error.
2. **Zero or more `next(value)` calls**, serially, never concurrently. A slow
   `next` must not be re-entered: if the Rust stream yields while the previous
   `next` is still running, the value is queued, not delivered.
3. **At most one terminal call.** Either `complete()` or `error(err)`, never
   both, never twice. After a terminal call the subscription is closed.
4. **No callback after `unsubscribe()`, on any of the three paths.** This is the
   one that is easy to get subtly wrong: the JS subscription drops its references
   to all three callbacks *synchronously* inside `unsubscribe()`, **and** signals
   the Rust side to drop the stream. Dropping only the JS references still leaves
   the Rust pump running, which leaks and, worse, can call a callback the JS
   wrapper has already nulled out → `TypeError: next is not a function`. Both
   halves are required.
5. **`error` is delivered for a Rust-stream error, and the subscription is
   closed.** A JS exception thrown *by the consumer's* `next` callback is **not**
   converted into an `error` on the same subscription — it is rethrown
   asynchronously (matching RxJS), so a consumer bug does not look like a
   database failure.
6. **Unsubscribing before the first value is legal** and delivers nothing.
7. **Completion is delivered exactly once for a finite stream.** For a change
   stream (infinite by nature) completion happens only on `collection.close()`
   or `database.close()`.
8. **`subscribe` with only `next`** (a shorthand object form) is supported;
   missing callbacks are no-ops.

#### Encoding on the wire (why a plain `Object` table is not enough)

The adapter is a `#[wasm_bindgen]` struct holding:

- a JS-side `Subscription` object created by Rust via a `js_sys` closure set,
- a `futures::Stream` driven by `wasm_bindgen_futures::spawn_local`,
- an `Arc<AtomicBool>` "closed" flag checked **before every call into JS** so a
  value already in flight at the moment of `unsubscribe()` is dropped rather
  than delivered.

`spawn_local` is the only executor in the browser target (no `tokio`, per this
lane's constraints), which means the pump is a microtask loop and point 2 above
is satisfied by construction: one `next` per microtask, awaited before the next
poll.

The `.d.ts` for it, with no `any`:

```ts
export interface Subscription {
  unsubscribe(): void;
  readonly closed: boolean;
}

export interface Observer<T> {
  next?: (value: T) => void;
  error?: (err: Error) => void;
  complete?: () => void;
}

export interface Observable<T> {
  subscribe(
    nextOrObserver: ((value: T) => void) | Observer<T>,
    error?: (err: Error) => void,
    complete?: () => void,
  ): Subscription;
}
```

`Observable<T>` is structural, so an RxJS `from(ourObservable)` works without
this crate depending on RxJS. That is the whole point of the boundary.

### 2.3 What is deliberately *not* exported

- No `RxStorage` / `RxStorageInstance` trait objects. They are a Rust-side
  composition mechanism (PLAN.md 4.2); exposing them across wasm-bindgen would
  make every wrapper a vtable crossing.
- No RxJS operators, no `Subject`, no `BehaviorSubject`.
- No `serde_json::Value`-shaped document type.
- No thread or `SharedArrayBuffer` entry point. Threads need
  `wasm32-wasip1-threads` or nightly `-Z build-std` (PLAN.md 3.4) and are not a
  default; the storage APIs are single-threaded.

---

## 3. Dependencies and the unblock condition

This lane cannot implement against imagined traits. Concretely, blocked on:

| Needed from lane D | Why |
| --- | --- |
| `dxdb_storage::RxStorage` / `RxStorageInstance` (the async-method trait, PLAN.md 3.3) | every storage crate implements it; `dynosaur` twin included |
| `dxdb_storage::RxStorageInstanceCreationParams` + `ensure…AreCorrect` | the constructor every storage shares |
| `categorizeBulkWriteRows` (PLAN.md 4.3) | `bulkWrite` is *defined* by this pure function; reimplementing it in this lane would fork the conflict semantics |
| `dxdb_memory::MemoryStorage` | the parity oracle: every ported test is written against it first, and the conformance suite's expected values come from it |
| `dxdb_testkit` storage conformance suite | the M4 acceptance criterion itself |

Writing the four storage crates before these exist means inventing trait method
names, signatures, and error types, then rewriting all four when the real ones
land — for zero deliverable value, since nothing can run until the suite exists
anyway. So Tranche 1 fixes the *contract* (§2, §4, §5) and does not guess the
trait.

**Unblock condition:** `crates/dxdb-storage/src/lib.rs` exports the
`RxStorageInstance` trait, `crates/storages/dxdb-memory` implements it, and
`dxdb-testkit`'s `storage_implementations` test target exists (`make gate-storage`
becomes runnable). At that point the order of work is:

1. `dxdb-localstorage` — smallest, synchronous, no worker story; proves the trait
   adoption.
2. `dxdb-indexeddb` — the production default; needs the batched-cursor reader.
3. `dxdb-opfs` — worker path first (§5), main-thread fallback second.
4. `dxdb-remote` — transport over a `MessageChannel`-shaped interface; needed
   before the worker OPFS storage can be driven from the main thread, so it is
   effectively a dependency of step 3.
5. `dxdb-js` real surface + hand-written `.d.ts`.
6. `demo/` conformance suite turned on, three engines.

Step 4 before step 3 is a change from the lane brief's ordering and is forced by
PLAN.md 5.4 ("that is why the worker storage and OPFS storage land together"): the
worker path *is* a remote storage with a `Worker` message channel.

---

## 4. Metadata shape per storage

Upstream shapes read for this section: `src/plugins/storage-localstorage/`
(718 + 44 + 32 lines) and `src/plugins/storage-remote/` (118 + 88 + 285 + 298 +
33 lines). `storage-indexeddb` and `storage-opfs` are premium and are not in the
OSS checkout; only their **public docs shape** was read
(`docs-src/docs/rx-storage-indexeddb.md`, `docs-src/docs/rx-storage-opfs.md`).
Where this document and upstream differ, it is called out.

### 4.1 `localstorage`

Upstream uses four key prefixes, all suffixed
`{databaseName}--{collectionName}--{schemaVersion}`
(`rx-storage-instance-localstorage.ts:131-134`):

| Upstream key | Holds |
| --- | --- |
| `RxDB-ls-doc-<…>` + `-` + docId | one JSON-stringified document per key |
| `RxDB-ls-idx-<…>` + indexName | `[indexString, docId][]` for that index |
| `RxDB-ls-changes-<…>` | the **last** `EventBulk` only, for cross-tab broadcast |
| `RxDB-ls-attachment-<…>` + `-` + `docId\|\|attId` | a `data:` URL |

`indexName` is `index.join('|')` (`:678`). `CLEANUP_INDEX` is
`['_deleted', '_meta.lwt']` (`:680`) and the cleanup sweep reads it with a
`boundGT` lower bound rather than scanning all docs (`:513-…`).

Important upstream property that must be preserved: **`localStorage` has no
transactions**, so after `categorizeBulkWriteRows` the writes are all synchronous
(`:201-205`, with the comment saying exactly that). Interleaving an `await`
between the conflict check and the write would be a correctness bug.

dxdb's shape: same four groups, same suffix so an existing dataset is readable,
but the change broadcast moves to `BroadcastChannel` where available and falls
back to the `storage` event. Metadata to keep:

```rust
struct LocalstorageMeta {
    database_name: String,
    collection_name: String,
    schema_version: u32,
    schema_hash: String,       // PLAN.md 4.3: hash is part of schema identity
    primary_path: String,
    indexes: Vec<IndexMeta>,   // index_id, index_name, index: Vec<String>
}
```

### 4.2 `indexeddb`

Not readable from the OSS checkout. From the public docs, the two facts that
constrain the design:

- *"it uses custom index strings instead of the native IndexedDB indexes,
  batches cursors for faster bulk reads"* → custom index strings, as upstream
  does everywhere else; the OS-level IndexedDB index is **not** used for
  ordering, so an index string is the sort key and `getAll` over explicit
  `IDBKeyRange` bounds is the read primitive.
- `batchSize` (default 300) is a documented user-facing option, which sets the
  cursor batch size — so it is part of `StorageOptions`, not a constant.

Stores: `docs` (primary-path keyed), `indexes` (one record per `IndexMeta`), and
`meta`. A write-ahead log is mentioned in the docs; dxdb's shape instead relies
on IndexedDB's own per-transaction atomicity, and per-document atomicity of
`bulkWrite` (PLAN.md 4.2) maps onto one readwrite transaction per row.

### 4.3 `opfs`

From the public docs: the worker variant is `getRxStorageOPFS()`, the main-thread
variant is `getRxStorageOPFSMainThread()`, and the docs note the worker returns
**strings** rather than structured objects because string transfer is faster
(`usesRxDatabaseInWorker` exists to disable that decode). That is a real design
constraint: the OPFS instance's return values are JSON strings at the worker
boundary.

On-disk shape: one file per document (`<collection>/docs/<docId>.json`) plus one
file per index (`<collection>/indexes/<indexName>.idx`) plus `meta.json`, because
the sync-access-handle API is byte-oriented and random access, so a single-file
layout would need a bespoke page manager — that is `dxdb-sqlite`'s job, not this
storage's.

### 4.4 `remote`

Upstream's message protocol is the thing to port exactly
(`storage-remote-types.ts`):

```ts
MessageToRemote   { connectionId, requestId, method, version, params }
MessageFromRemote { connectionId, answerTo, method, error?, return? }
```

`method` is a member of `keyof RxStorageInstance | 'create' | 'custom'` — i.e. the
transport is method-name dispatch, not a hand-built RPC enum, which is what keeps
it in lockstep with the trait. `return` **may be a JSON string** and the receiver
`JSON.parse`s it if so (`rx-storage-remote.ts:150-168`): the comment cites the
cost of `postMessage` on complex objects. `version` is sent on every request to
catch a stale worker build, and `changeStream` is the one *push* method
(`:197`) — every other method is request/answer.

dxdb shape: identical message types, with `custom` kept (it is how a user passes
app-level messages through the channel) and `version` checked against the
workspace version. `RemoteMessageChannel` is the trait a `Worker`,
`BroadcastChannel`, websocket, or postMessage bridge implements:

```rust
trait RemoteMessageChannel {
    fn send(&self, msg: MessageToRemote);
    fn messages(&self) -> Stream<MessageFromRemote>;
    fn close(&self) -> Future<Output = ()>;
}
```

Mode (`one` | `storage` | `database` | `collection`) determines channel reuse and
must be a `StorageOptions` field with the same default, `storage`.

---

## 5. OPFS: worker vs main thread — the decision and its evidence

**Decision: the fenced path is a dedicated worker with
`FileSystemSyncAccessHandle`; the main-thread path uses the async
`FileSystemFileHandle.createWritable()`; the storage detects which one it is in
and reports it.**

### The API evidence

- MDN, `FileSystemFileHandle.createSyncAccessHandle()`: *"This method is only
  available in dedicated workers"* / not exposed in the main thread, iframes, or
  SharedWorkers.
- Upstream's own OPFS docs say it without hedging: *"the most performant
  synchronous methods like `read()` and `write()` of the OPFS API are **only
  available inside of a WebWorker** … not exposed in the main thread, only in a
  Worker"*, and they ship a separate `getRxStorageOPFSMainThread()` for exactly
  that reason.
- PLAN.md 5.4 records the same constraint and draws the consequence: the worker
  storage and OPFS storage land together.

### We measured it in all three engines rather than citing the doc

`demo/harness/capabilities.js` probes
`FileSystemFileHandle.prototype.createSyncAccessHandle` **twice**: once on the
main thread and once inside a dedicated worker
(`harness/sync-handle-worker.js`). The spec asserts the main-thread answer is
`false` and, where OPFS exists at all, the worker answer is `true`. Results are
attached to every run as `capabilities.json`.

Recorded results, 2026-10-25, one run per engine — **all three agree, which is the
first time this plan can say that without quoting a doc**.

| Probe | Chromium 156.0.8078.4 | Firefox 157.0 | WebKit 27.2 |
| --- | --- | --- | --- |
| `navigator.storage.getDirectory` (OPFS) | present | present | present |
| `createSyncAccessHandle` **main thread** | **absent** | **absent** | **absent** |
| `createSyncAccessHandle` **dedicated worker** | **present** | **present** | **present** |
| `createWritable` main thread | present | present | present |

The difference between rows 2 and 3 is the decision, measured in every engine we
ship to. Reproduce with `node demo/scripts/collect-capabilities.mjs`.

### What the fallback costs, and the JSPI note

- **Worker path**: sync `read`/`write`, no `await` per operation; the price is
  that every value crosses a `postMessage` boundary, which is why the worker
  variant returns JSON strings (§4.3).
- **Main-thread path**: `createWritable()` per write, so each write is an
  `await`. The docs' own numbers say it can *appear faster* for bulk inserts
  because it avoids the message round-trip — so "main thread is the slow
  fallback" is not a defensible claim, and the plan does not make it. It is a
  different cost curve, and `registry["opfs-main-thread"]` exists as its own
  conformance target for that reason.

**JSPI** is the documented escape hatch that would let the *single* synchronous
implementation run on the main thread (PLAN.md 5.2: Chrome 137+, Node 22 behind a
flag). It is not a dependency and not used. It is noted here because the OPFS
instance must therefore be written so that the synchronous core is
*reusable* behind a JSPI shim later — i.e. the sync operations take an explicit
`&mut dyn SyncFile` rather than calling `web_sys` globals inline.

---

## 6. Measured wasm baseline

**Method.** A crate at `crates/dxdb-js` with the real dependency set
(`wasm-bindgen =0.2.129`, `wasm-bindgen-futures 0.4.79`), built with the
workspace release profile (`opt-level="s"`, `lto="fat"`, `codegen-units=1`,
`panic="abort"`), for `wasm32-unknown-unknown`, on the shared Fly box with
`-d 1.95.0`. Bindings generated by the `wasm-bindgen` CLI **0.2.129** (versions
must match exactly or the module traps at instantiation). Compressed with
`brotli -q 11`, and the round-trip verified (`brotli -d | cmp`).

Two measurements, because they answer different questions:

| Variant | Raw | **Brotli q11** | JS glue (brotli) | Total |
| --- | ---: | ---: | ---: | ---: |
| **A. Fixed floor** — `abi_version()` + `echo(&str)`, no futures, `--remove-name-section` | 13,554 B | **5,373 B** | 1,957 B | 7,330 B |
| **B. Shipped today** — as in `crates/dxdb-js`, incl. async + `wasm-bindgen-futures`, `--remove-name-section` | 27,446 B | **10,008 B** | 3,264 B | 13,272 B |
| C. Reference: B *without* `--remove-name-section` | 43,226 B | 13,927 B | 3,264 B | 17,191 B |

The measurement harness is a footgun worth recording: `brotli --quality=11 --best`
is rejected ("quality already set") **while still writing output**, so piping it
to `wc -c` produced a plausible-looking 2,103 B early on. Every number above was
re-measured with `brotli -q 11 -c` and the round trip verified
(`brotli -d | cmp` against the original), and the build script now uses exactly
one quality spelling for that reason.

### Against the 250 KiB core budget

```
budget (PLAN.md 5.3)   256,000 B   250.0 KiB
measured baseline       10,008 B     9.8 KiB   (shipped today, stripped)
headroom               245,992 B   240.2 KiB   96.1% of budget unused

fixed floor              5,373 B     5.3 KiB   (wasm-bindgen alone)
```

**The baseline is 3.9% of the core budget.** That is the useful number: it says
`wasm-bindgen` itself is not the risk. The 250 KiB has to be spent on schema
parsing, the query planner, index-string construction, and reactivity — roughly
240 KiB of *our* code and its dependencies. What this tranche cannot tell you is
whether the plan's target is real, because none of that code exists yet. It does
establish:

- the ABI and the CLI pairing are pinned and reproducible through the Makefile path;
- the measurement harness is automatable (it is, `scripts/build-wasm.sh` prints
  the budget comparison and is the thing CI will call);
- async plumbing costs **4,635 B** brotli over the floor, so per-storage async
  does not threaten the budget;

### Rough headroom model for the later budgets

Not measured, derived from B and from the plan's own numbers, and it is a model,
not a claim:

| Artifact | Budget | What is already known |
| --- | ---: | --- |
| core only | 250 KiB | 5.3 KiB of that is the ABI floor |
| + memory | 320 KiB | memory storage is pure Rust, no sys deps → tens of KiB |
| + OPFS | 400 KiB | `web-sys` OPFS bindings are the unknown; measure before believing |
| + SQLite | 900 KiB | docs put wasm sqlite at most of the 900; not this lane's crate |

The `web-sys` cost is the one that could plausibly blow the OPFS budget, because
`web-sys` generates bindings per feature and `idb-sys` adds another layer. **The
first implementation step after unblocking must be a `web-sys` feature-set size
probe**, before writing the indexeddb storage, not after.

---

## 7. The demo harness

`demo/` is a real page served from the repo, loading the real release `.wasm`.

```
demo/
  index.html                   harness page
  app.js                       bootstrap: probe → import+init wasm → boundary → suite → publish
  harness/capabilities.js      per-engine capability probe (main thread + worker)
  harness/sync-handle-worker.js  the worker half of the OPFS evidence
  harness/conformance.js       suite runner; one function to change when testkit lands
  scripts/build-wasm.sh        remote build + wasm-bindgen + size gate
  scripts/serve.mjs            static server: application/wasm, no-store
  tests/harness.spec.js        Playwright spec
  playwright.config.js         chromium + firefox + webkit
```

Two deliberate choices worth knowing:

- **A missing wasm module is a reported failure, not a blank page.** `app.js`
  dynamic-imports `pkg/`, so the capability record still gets written when the
  build is absent, and `body[data-status]` always reaches a terminal state.
- **The conformance test is `test.skip`ped with its reason inline**, not deleted.
  A green run today means "the harness works". The day `dxdb-testkit` lands, the
  assertion below the skip becomes reachable and names exactly what to wire.

Suite results are reported per storage, and `summarize()` treats "all skipped" as
`skipped`, never `passed`, so tranche 1 cannot read as a storage pass.

---

## 8. Tranche 1 verification

| Check | Command | Result |
| --- | --- | --- |
| workspace compiles incl. `dxdb-js` | `make check` | **exit 0** |
| full suite, 3 engines | `npx playwright test` | **exit 0** — 15 passed, 3 skipped |
| Chromium harness | `--project=chromium` | 5 passed (exit 0) |
| Firefox harness | `--project=firefox` | 5 passed (exit 0) |
| WebKit harness | `--project=webkit` | 5 passed (exit 0) |
| brotli baseline | `scripts/build-wasm.sh` | **10,008 B** shipped / 5,373 B floor |

Playwright 1.64.1. Browsers, from each run's `navigator.userAgent`:

| Engine | Version | UA platform |
| --- | --- | --- |
| Chromium | 156.0.8078.4 | Linux x86_64 |
| Firefox | 157.0 | Linux x86_64 |
| WebKit | 27.2 | Macintosh (WebKit's fixed reporting string) |

The 3 skips are the conformance test in each engine, which is `test.skip`ped with
its blocking reason (§7).

Every engine reported the same wasm feature set, which is a stronger result than
expected this early: `simd128`, `multivalue`, and `bulk-memory` all validate in
Chromium, Firefox **and** WebKit. That means the core may use all three without a
capability-detection fallback build — which is what PLAN.md 3.4 assumed and had
not yet measured in WebKit.

---

## 9. Tranche 2: the storage traits, and the two things I was asked to verify

`dxdb-storage` merged. Read `crates/dxdb-storage/src/traits.rs` (448 lines) and
the modules it re-exports. Verdict on the two named questions first, because one
of them found a bug.

### 9.1 Checkpoint encoding — **the type is right, the obvious serializer is a trap**

`Checkpoint { id: String, lwt: f64 }` (`changes.rs:91-97`) is the right shape: the
typed pair rather than opaque bytes, so every storage compares it the same way,
and `lwt` as a number so it survives `JSON.stringify`. `beginning()` uses
`META_LWT_MINIMUM = 1.0` and an empty id, which is the lowest possible checkpoint.

The problem is `Checkpoint::to_json` (`changes.rs:113`). Its doc comment says the
value *"must round-trip through `JSON.stringify` and back without drift — hence
`dxdb-jsnum`"*, and then the body builds a `serde_json::Number` with
`Number::from_f64` and never calls `dxdb-jsnum`. That is only correct if the
serializer downstream formats numbers the JavaScript way, and the obvious one does
not: `serde_json`'s own writer emits `1.0` where JavaScript emits `1`.

Measured, not argued — `crates/dxdb-js/tests/checkpoint_contract.rs`, 6 tests:

| Case | Result |
| --- | --- |
| `checkpoint_json_uses_javascript_number_formatting` | pass |
| `checkpoint_roundtrips_through_json` | pass |
| `minimum_lwt_renders_without_a_trailing_zero` | pass |
| **`serde_json_alone_diverges_from_javascript`** | **pass — asserts the divergence** |
| `beginning_checkpoint_is_the_lowest_possible` | pass |
| `malformed_checkpoints_are_rejected` | pass |

The divergence test asserts both halves:
`Checkpoint::new("", 1.0).to_json().to_string()` is `{"id":"","lwt":1.0}` while
`to_js_json(...)` is `{"id":"","lwt":1}`. Both parse to the same `f64`, so the
round trip is *numerically* fine — the drift is in the bytes.

**Where this actually bites:** anywhere a checkpoint is emitted as a string rather
than as a `serde_json::Value` handed to a JS-aware writer. Concretely, the
`dxdb-js` boundary, the replication transcript vectors (which compare bytes), and
any storage that persists a checkpoint as a stringified key. `1.0` vs `1` in a
checkpoint that is compared as text is a real ordering bug, not cosmetics.

**Not fixed here**, because `dxdb-storage` is lane D's crate and this lane does not
edit it. Two options for whoever owns it:

1. Change `to_json` to return the JavaScript-formatted rendering (make the
   `dxdb-jsnum` mention in its doc comment true), or
2. Keep returning a `Value` and add a `to_json_string()` that uses
   `dxdb_json::to_js_json`, so there is no value-typed path for a caller to
   render wrongly.

Option 2 is better: it leaves the typed value (which storages compare) intact and
removes the trap rather than documenting it. Until then this lane's code goes
through `dxdb_json::to_js_json`, and the contract tests fail loudly if a future
`serde_json` makes the divergence disappear.

### 9.2 Async form — **`impl Future` is usable, no dynosaur needed at my call sites**

The trait methods return `-> impl Future`, per PLAN.md 3.3, and `#[dynosaur]`
generates `DynRxStorageInstance`/`DynRxStorage` for the wrapper stack.

For this lane's call sites:

- **`dxdb-js` async exports** call concrete methods on a concrete storage, so
  `impl Future` is fine and dynosaur is not involved. `wasm-bindgen-futures`
  adapts the future to a `Promise` directly.
- **`dxdb-remote`** dispatches by method name over `DynRxStorageInstance`, because
  it forwards whatever trait member the message names. That is exactly what the
  dynosaur twin is for, and it is the one place this lane needs it.
- **`Observable::from_stream`** needs `Stream`, not the trait, so the storage's
  async shape never reaches the adapter.

So: usable, and the mirror is needed in precisely one crate. No workaround
required, and nothing to report as wrong here.

Two related notes from reading the trait, both of which affect how I implement:

- `change_stream()` returns `Pin<Box<dyn Stream<Item = EventBulk>>>` with no
  lifetime parameter, i.e. `'static`. That settles §2.2's open question: the
  adapter gets a `Stream` to drive, and **dropping it is the unsubscribe**, so
  "no callback after unsubscribe" is implementable without a cancellation message.
  The adapter's `from_stream` path is therefore the real one, and the polling note
  in `observable/bind.rs` is a limitation of the *current* pump (it does not drop
  the stream on unsubscribe) rather than of the trait.
- `get_changed_documents_since` has a working default body that builds an exact
  range plan. A storage that overrides it must keep the same exclusivity
  (`inclusive_start: false`) or the checkpoint document is returned twice.

### 9.3 The adapter, implemented and proven

`crates/dxdb-js/src/observable/` splits into:

- `state.rs` — the delivery **policy**, pure Rust, 14 host tests, no
  `wasm-bindgen`. Covers all five hard cases from the brief plus ordering,
  stale-token, re-subscribe, and before-subscribe behaviour.
- `bind.rs` — the `#[wasm_bindgen]` shell (`Observable`, `Subscription`,
  `Emitter`) plus `from_stream`, `wasm32` only.

`Submit` returns a `Delivery` the caller must honour rather than performing the
call itself. That is deliberate: it makes it *impossible* for the shell to
duplicate the policy without visibly throwing a `Delivery` away.

Browser-side, `demo/harness/adapter-cases.js` runs 14 cases in each engine, so the
binding itself is proven, not just the policy. Cases: ordered delivery; error
exactly once; complete suppressed after error; error suppressed after complete;
completion once and closing; no callback after unsubscribe; a **queued** callback
dropped after unsubscribe; unsubscribe from inside a callback; a throwing callback
not wedging the subscription; the observer-object form; missing callbacks as
no-ops and a late subscriber getting a pre-closed subscription; re-subscribe
surviving a stale unsubscribe; synchronous `Emitter` re-entrancy being bounded and
lossless; a 500-value storm arriving in order.

### 9.4 The typed surface

`demo/types/dxdb.d.ts` is the authored surface — no `any` in the public API, with
the one unavoidable boundary typed as `unknown` (`Observable<T = unknown>`) so it
cannot silently flow into a typed variable the way `any` can.

It is compiled, not just written: `demo/types/consumer.ts` consumes every
declaration and carries negative assertions as `@ts-expect-error`, and
`demo/types/assignability.ts` asserts the authored types and the **generated**
`pkg/dxdb_js.d.ts` agree in the direction that matters (generated → authored), so
a Rust signature change this file has not followed is a compile error.

The gate was mutation-tested rather than assumed green:

| Mutation | Result |
| --- | --- |
| remove `closed` from the authored `Subscription` | 3 errors, exit 2 |
| delete a `@ts-expect-error` | 1 error (the now-unasserted line), exit 2 |
| widen authored `subscribe` to `any` | 4 errors incl. 3 implicit-any, exit 2 |
| restored | exit 0 |

`scripts/check-types.mjs` also refuses to run against a stale `pkg/`: if
`dxdb_js.d.ts` is older than the `.wasm` it describes, it fails and says to
rebuild. A `.d.ts` compiled against a stale module is worse than an unchecked one,
because it is wrong confidently.

### 9.5 Still open

1. **`web-sys` / `idb-sys` bundle cost** (§9.1 above is separate). The one number
   that could invalidate the 400 KiB OPFS budget. Probe before writing the
   indexeddb storage.
2. **Cross-tab change events.** Upstream uses the `storage` event and redistributes
   it because it does not fire in the originating tab. `BroadcastChannel` has the
   same gap in Safari private mode; needs measuring per engine.
3. **`from_stream` does not drop the current stream on unsubscribe.** It stops
   *delivery* (the contract holds) but keeps polling an abandoned stream until it
   ends. Fixing it needs the pump to hold a cancellation handle it can trip — cheap
   once a storage's stream is known to be `Unpin`, so it waits for the first real
   stream rather than being guessed at now.
4. **`Emitter::next` is synchronous**, matching RxJS `Subject`. The non-re-entrant
   guarantee belongs to the `from_stream` path (`spawn_local` = one delivery per
   microtask). Worth documenting for users once both paths are public.
