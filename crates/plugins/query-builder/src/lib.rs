//! The chainable Mango query builder (`RxDBQueryBuilderPlugin`).
//!
//! # Status
//!
//! Scaffold only. The port is gated on `dxdb-core` and `dxdb-query` publishing
//! their query surface (lane D / lane B), because every behaviour here is
//! defined in terms of `RxQuery` and `MangoQuery`. Nothing in this crate is
//! invented ahead of those APIs; `docs/lanes/m6-port-plan.md` records what
//! lands once they exist.
//!
//! # What this port is, and what it is not
//!
//! Upstream, `query-builder` is a *prototype plugin*
//! (`src/plugins/query-builder/index.ts:54`): it bolts seventeen chainable
//! methods onto `RxQuery.prototype` and rewrites the query on every call.
//! Rust has no prototype mutation, so this crate ports the *builder* — the
//! thing that owns a mutable query, validates paths, and projects itself back
//! to a `MangoQuery` — and the plan (`docs/lanes/m6-port-plan.md`, "Query
//! builder: builder first, methods second") records why the chainable form
//! arrives only after the builder's behaviour is pinned against upstream's
//! `test/unit/query-builder.test.ts`.
//!
//! The behavioural rules that must survive the port, none of which are
//! implemented yet:
//!
//! - `where`, `equals`, `eq`, `or`, `nor`, `and`, `mod`, `exists`,
//!   `elemMatch` and `sort` are explicit methods
//!   (`src/plugins/query-builder/mquery/nosql-query-builder.ts:67-321`), and
//!   `.eq()` is `$eq` so it can coexist with other operators on one field
//!   (`:106`, covered by `test/unit/query-builder.test.ts:113`).
//! - `limit`, `skip`, `maxScan`, `batchSize`, `comment` write to the options
//!   bag (`:459`), while `gt`, `gte`, `lt`, `lte`, `ne`, `in`, `nin`, `regex`
//!   and `size` are generated $(operator)s (`:473`).
//! - `merge` must refuse `__proto__`, `constructor` and `prototype` keys
//!   (`mquery-utils.ts:10`). In Rust the equivalent hazard is a JSON key that
//!   later consumers treat as a path, so the guard becomes a validation rule
//!   rather than a silent skip; the divergence is recorded in the plan.
//! - `sort` accepts either mquery's `{ field: 1 | -1 }` map or a Mango sort
//!   array and normalises to `[{ field: 'asc' | 'desc' }]`
//!   (`nosql-query-builder.ts:275`, `:414`).
//! - `toJSON()` emits `{ selector }` plus `skip`, `limit` and `sort` only when
//!   they are set, and reports the current path so the *next* chained call
//!   resumes from it (`:389`, and the `queryBuilderPath` bookkeeping at
//!   `src/plugins/query-builder/index.ts:13`).
//! - `findByIds` is not a chainable op and must refuse to build
//!   (`src/plugins/query-builder/index.ts:42`, error `QU17`).

#![forbid(unsafe_code)]
