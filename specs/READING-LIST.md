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

Further rows are added when each spec lands. A row is incomplete (and blocks
review) until both read columns are filled.

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
