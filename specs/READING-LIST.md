# Reading list and provenance record

CLEANROOM.md section 2 makes the clean-room claim a *process* claim, and section
9 lists contamination as the top risk. A process claim is only auditable if the
artifacts say who read what. This file is that record.

Two roles, separated by knowledge:

| Role | May read | Writes |
| --- | --- | --- |
| Specification author | public docs, the Apache-2.0 source at `~/Code/rxdb`, public issue tracker, blog posts, RFCs and standards | `specs/<feature>.md` only |
| Implementer | the spec, dxdb's own code, generic prior art (crate docs, RFCs, papers, standards) | `crates/cleanroom/<crate>/` only |

One lane holds both roles in sequence (see the brief), so the record is per
*crate*: which spec it was written from, and what the implementer had read.

## Denied material, for the whole lane

Never read, in any form: `rxdb-premium` source, its minified bundles, its
`.d.ts`, its source maps, its encrypted payloads, its installer, its
`package.json` scripts, its npm tarball. Not to "check one enum". Not to confirm
a name. Not to see whether a behaviour exists. The gap register exists so that
running out of public information has a legitimate outlet.

If any of that material is read by someone on this lane, the matching crate is
handed to an uncontaminated implementer and the incident is recorded here. That
has not happened.

## Per-crate provenance

| Crate | Spec | Spec author read | Implementer read | Implementer has seen premium source? |
| --- | --- | --- | --- | --- |
| `dxdb-logger` | [`logger.md`](./logger.md) | `docs-src/docs/logger.md`; Apache-2.0 `src/types/rx-storage.interface.d.ts`, `src/rx-storage-helper.ts`; Sentry span interface docs; OTel database span conventions | `specs/logger.md`, `specs/READING-LIST.md`, dxdb workspace crates, `tracing`/`serde`/`dynosaur` crate docs | **No** |
| `dxdb-encryption` | [`encryption.md`](./encryption.md) | `docs-src/docs/encryption.md`; Apache-2.0 `src/plugins/encryption-crypto-js/index.ts`, `src/rx-schema.ts` (schema hash), `src/plugins/utils/utils-revision.ts`, `src/types/rx-database-internal-store.d.ts`, `src/rx-database-internal-store.ts`; RFC 9106, RFC 8439, draft-irtf-cfrg-xchacha-03, OWASP Password Storage Cheat Sheet; RustCrypto `argon2`/`chacha20poly1305` crate docs | `specs/encryption.md`, `specs/READING-LIST.md`, `crates/dxdb-storage/src/{traits,document,write,query,changes,schema}.rs`, `crates/dxdb-json/src/lib.rs`, and the crate docs for `argon2` 0.6.0, `chacha20poly1305` 0.11.0, `aes-gcm` 0.11.0, `sha2` 0.11.1, `zeroize` 1.9.1 | **No** |
| `dxdb-sharding` | [`sharding.md`](./sharding.md) | `docs-src/docs/rx-storage-sharding.md`; Apache-2.0 `src/types/rx-storage.interface.d.ts`; the landed `crates/dxdb-storage/src/traits.rs` (query/count/changed-documents contracts); PLAN.md 3.2 and 4.2 | `specs/sharding.md`, `specs/READING-LIST.md`, `crates/dxdb-storage/src/*`, `crates/dxdb-query/src/{sort,normalize}.rs`, `crates/dxdb-jsstr/src/lib.rs`, `crates/dxdb-schema/src/lib.rs` (`fill_with_default_settings`), `crates/dxdb-testkit/src/harness.rs`, `crates/storages/dxdb-memory/src/lib.rs` | **No** |
| `dxdb-worker` | [`worker.md`](./worker.md) | `docs-src/docs/rx-storage-worker.md`, `docs-src/docs/rx-storage-shared-worker.md`; Apache-2.0 `src/plugins/storage-remote/*` (the transport this builds on); MDN `Worker` / `SharedWorker` / `MessagePort`; PLAN.md 3.4 and 5.2 | *not yet assigned — implementation not started* | **No** |
| `dxdb-meta-cache` | [`meta-cache.md`](./meta-cache.md) | `docs-src/docs/rx-storage-localstorage-meta-optimizer.md`; Apache-2.0 `src/rx-database-internal-store.ts`, `src/rx-database.ts`, `src/types/rx-database-internal-store.d.ts`, `src/rx-schema.ts`; the Web Storage API specification | `specs/meta-cache.md`, `specs/READING-LIST.md`, `crates/dxdb-storage/src/*`, `crates/storages/dxdb-localstorage/src/{lib,kv,web}.rs` (the `Kv` seam), the Web Storage API spec | **No** |

A row is incomplete (and blocks implementation review) until the implementer's read
column is filled. The three B2 rows are intentionally incomplete: their specs landed
before their crates did, which is the sequence CLEANROOM.md section 2 requires.

## Sources, per crate, with the literal pages used

### `dxdb-logger` (B1)

Public documentation:

- `~/Code/rxdb/docs-src/docs/logger.md` — the whole page. It is the only public
  statement of the feature: it wraps "any RxStorage", logs "all operations to the
  storage layer", has a `settings` object with `prefix`, `times`,
  `metaStorageInstances` and per-operation booleans, and takes
  `onOperationStart` / `onOperationEnd` / `onOperationError` callbacks. The page
  names APM targets including Sentry.
- `~/Code/rxdb/docs-src/docs/releases/15.0.0.md:110` — "Added Logger Plugin",
  which dates the feature and repeats "detect performance problems and errors".
- `docs/CLEANROOM.md` sections 4 (row 17, the logger's documented line), 5.13
  (the design brief: tap the storage trait wrappers, emit through `tracing`,
  Sentry compatibility means a documented field set), 6 (verification), 9 (risk
  register).

Apache-2.0 source, read only for the trait surface the wrapper must sit on:

- `~/Code/rxdb/src/types/rx-storage.interface.d.ts:47-66` — `RxStorage`:
  `name`, `rxdbVersion`, `createStorageInstance`.
- the same file, `:164` `bulkWrite`, `:180` `findDocumentsById`, `:208` `query`,
  `:220` `count`, `:227` `getAttachmentData`, `:246`
  `getChangedDocumentsSince?` (optional, with an upstream fallback),
  `:288` `cleanup`, `:308` `close`, `:314` `remove`, plus
  `:152` `underlyingPersistentStorage` and `:277` `changeStream`.
- `~/Code/rxdb/src/rx-storage-helper.ts:51` — `INTERNAL_STORAGE_NAME =
  '_rxdb_internal'`, the collection name RxDB's own meta storage instance uses
  (`src/rx-database.ts:769`), which is the is-it-meta-storage test.
- `~/Code/rxdb/src/rx-storage-helper.ts:825` and `:936-960` — the documented
  fallback when `getChangedDocumentsSince` is absent, which is why the wrapper
  must not invent the operation.

Standards and prior art (generic, not vendor-specific):

- Sentry Span interface, <https://develop.sentry.dev/sdk/foundations/envelopes/event-payloads/span/>
  — required `span_id` (8 bytes, hex), `trace_id` (16 bytes, hex),
  `start_timestamp`, `timestamp`; recommended `op`; optional `description`,
  `status` (with the 15 documented states), `tags`, `data`, `origin`,
  `parent_span_id`. Timestamps are RFC 3339 or Unix seconds.
- OpenTelemetry database client span conventions,
  <https://opentelemetry.io/docs/specs/semconv/db/database-spans/> — `db.system`,
  `db.operation.name`, span naming from operation plus target.
- The Rust `tracing` crate documentation, for `Span::current`, `idle`/`enter`
  guards, `Span::none`, and the no-subscriber no-op path.
- The `dynosaur` crate documentation — `#[dynosaur::dynosaur(DynX = dyn(box) X)]`
  generates the boxed-twin type, which is how an async trait gets a wrapper that
  can hold `Arc<dyn ...>` without `async_trait`.

Inside dxdb, read by the implementer:

- `crates/dxdb-storage/src/lib.rs` (a scaffold with no traits at the time of
  writing, which is what put `SpecEnvelope` / `SpecTraitsAbsent` in
  `docs/CLEANROOM-GAPS.md`).
- `crates/dxdb-json/src/lib.rs` for the ordered-JSON encoder (`to_js_json`,
  `canonical_bytes`) and `Cargo.toml` for the dependency set already in the
  lockfile.

### `dxdb-encryption` (B2)

Public documentation:

- `~/Code/rxdb/docs-src/docs/encryption.md` — the whole 277-line page. Read for: the
  `encrypted: string[]` schema field; per-field transparency; "Encrypted fields cannot
  be used as operators in queries"; password handling ("RxDB does not define how you
  should store or retrieve the encryption password", "Opening an existing database with
  a different password will throw an error"); "it is not possible to change the
  password of a database" and the two documented workarounds; `attachments.encrypted`;
  encryption-inside-a-worker guidance; the nested-path rule ("You cannot also encrypt a
  child path of an already-encrypted parent"); the 10x/5x performance claims.

Apache-2.0 source, read because it is the readable sibling of the paid module:

- `~/Code/rxdb/src/plugins/encryption-crypto-js/index.ts` (233 lines) — the whole file.
  Read for: the call shape `AES.encrypt(value, password)`; `MINIMUM_PASSWORD_LENGTH = 8`;
  the `EN1`/`EN2`/`EN3` error codes; how the schema is rewritten so encrypted fields
  become `{type: "string"}`; the attachment branch. **Read in order to document what we
  deliberately do not reproduce** (G-030); nothing is copied from it.
- `~/Code/rxdb/src/rx-schema.ts:113` — `RxSchema.hash` is
  `hashFunction(JSON.stringify(this.jsonSchema))`, which is what makes the schema hash
  usable as a collection identity for AAD.
- `~/Code/rxdb/src/plugins/utils/utils-revision.ts` — `parseRevision` shows `_rev` is
  `"<height>-<hash>"`, a public format, which is why G-036 can describe the residual
  replay gap honestly instead of claiming more than AAD binding can deliver.
- `~/Code/rxdb/src/types/rx-database-internal-store.d.ts:6,17,28` — `InternalStoreDocType`
  and the collection document's `schemaHash` field, which is where the salt and verifier
  live.
- `~/Code/rxdb/src/rx-database-internal-store.ts:152-200` — the storage-token
  bootstrap and its `passwordHash`.

Standards (generic prior art, not vendor-specific):

- RFC 9106, <https://www.rfc-editor.org/rfc/rfc9106.html> — Argon2. §3.1 input
  parameters (16-byte salt recommended, `m >= 8p`, version `0x13`), §4 parameter choice
  (FIRST RECOMMENDED `t=1, p=4, m=2^21`; SECOND RECOMMENDED `t=3, p=4, m=2^16`; step 3
  "choose Argon2id"; step 8 tag length), §1 on Argon2id vs i vs d.
- RFC 8439 — ChaCha20 and Poly1305, the AEAD that XChaCha20-Poly1305 wraps.
- draft-irtf-cfrg-xchacha-03 §2 — the 192-bit-nonce construction (HChaCha20 subkey from
  the first 16 nonce bytes, RFC 8439 AEAD with the remaining 8).
- OWASP Password Storage Cheat Sheet — Argon2id minimum `m = 19 MiB, t = 2, p = 1`,
  cited only to explain why we do not adopt the RustCrypto `argon2` crate's defaults.
- NIST SP 800-63B — the length-based, no-composition-rules direction behind G-032.

Inside dxdb: `crates/dxdb-json/src/lib.rs` (`to_js_json`, so encrypted plaintext is
serialised with JS number and string semantics as PLAN.md 3.1 requires),
`crates/dxdb-storage/src/{traits,revision,document}.rs` for the document and revision
shapes.

### `dxdb-sharding` (B2)

- `~/Code/rxdb/docs-src/docs/rx-storage-sharding.md` — the whole 108-line page. Read
  for: wrapper-over-any-storage; `sharding.shards` and `sharding.mode`
  (`'collection' | 'database'`); "10 Shards is a good value to start with"; "Changing
  the value of shards is not possible on an already existing database state, you will
  lose access to your data"; the 30-50% read / 25% write claims and the page's own
  qualifier that it helps "**some** RxStorage implementations".
- `~/Code/rxdb/src/types/rx-storage.interface.d.ts` — the operation set the wrapper must
  preserve.
- inside dxdb, `crates/dxdb-storage/src/traits.rs` — the landed `count` contract
  ("must equal the length of the equivalent `query()` — including with a `limit`"),
  which is what makes G-040 and INV-2 derivable rather than invented.
- PLAN.md 3.2 (UTF-16 code-unit ordering, the reason a naive merge is silently wrong)
  and 4.2 (per-document atomicity, no cross-document transaction).

### `dxdb-meta-cache` (B2)

- `~/Code/rxdb/docs-src/docs/rx-storage-localstorage-meta-optimizer.md` — the whole
  50-line page. Read for: `localStorage` holding "the plain key-value metadata that RxDB
  needs to create databases and collections"; "automatically detect if an RxDB internal
  storage instance is created, and replace that with a localstorage based instance";
  browser-only; "about 200 milliseconds"; "more than 4 RxCollections".
- `~/Code/rxdb/src/rx-database-internal-store.ts` — what the internal store actually
  contains at bootstrap (`ensureStorageTokenDocumentExists`, the `passwordHash` field,
  the storage-token document), which is what a snapshot would have to cache.
- `~/Code/rxdb/src/types/rx-database-internal-store.d.ts:6,28` — the internal document
  type and the collection document's fields.
- `~/Code/rxdb/src/rx-storage-helper.ts:51` — `INTERNAL_STORAGE_NAME = '_rxdb_internal'`,
  the is-it-the-internal-instance test.
- `~/Code/rxdb/src/rx-schema.ts:113` — the schema hash, which a collection document
  carries and which a stale snapshot must not resurrect.
- The Web Storage API specification — `setItem` atomicity per key and the synchronous
  access model, both relied on in `specs/meta-cache.md` 3.1 and section 7.

### `dxdb-worker` (B3)

- `~/Code/rxdb/docs-src/docs/rx-storage-worker.md` — the whole page. Read for:
  storage runs inside a WebWorker or a Node Worker Thread; `exposeWorkerRxStorage` on
  the worker side; `getRxStorageWorker({ workerInput, workerOptions })` on the main side;
  "any valid `worker.js` JavaScript file can be used both, for normal Workers and
  SharedWorkers"; the pre-built-bundle section (which we deliberately do not reproduce,
  G-051).
- `~/Code/rxdb/docs-src/docs/rx-storage-shared-worker.md` — the whole page. Read for:
  "the SharedWorker is created exactly once, even when there are multiple browser tabs
  opened"; the stated consequence, "multiple performance optimizations can be done
  because the storage itself does not have to handle multiple opened database
  connections"; passing a `SharedWorker` instance instead of a URL; and the
  `multiInstance: false` advice when the database only ever lives in the worker.
- `~/Code/rxdb/src/plugins/storage-remote/` (Apache-2.0) — the open-source message-channel
  transport that CLEANROOM.md 5.7 names as the thing to build on: `rx-storage-remote.ts`,
  `storage-remote-types.ts`, `message-channel-cache.ts`. Read for the shape of a
  request/response envelope over a `MessagePort`; the paid worker plugin's protocol is a
  different, undocumented thing (G-053).
- PLAN.md 3.4 and 5.2 — the WASM target matrix and the atomics/threads constraint that
  makes G-048 the operative decision.
- MDN, `Worker`, `SharedWorker`, `MessagePort`, `structuredClone` — the platform contract
  the adapters implement.

Never opened during B0 and B1: anything under an `rxdb-premium` path on this
machine or elsewhere.

## What the implementer of each crate is *not* allowed to add

- Constants, magic numbers, index layouts, KDF parameters or string formats that
  are not derivable from the sources above. Choosing our own value and recording
  it in the gap register is the correct move.
- Any use of `PREMIUM_FLAG_HASH`, `RXDB_UTILS_GLOBAL.premium`, `hasPremiumFlag`,
  or `NON_PREMIUM_COLLECTION_LIMIT` (all of which are Apache-2.0 and therefore
  readable, and which we deliberately do not reproduce; see
  `docs/CLEANROOM-GAPS.md` G-006).
- A limit, a trial cap, a telemetry ping, or a network call of any kind.
