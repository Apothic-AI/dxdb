//! dxdb core: database, collection, document, hooks.
//!
//! The parity oracle is `dxdb-memory`, and it is the only storage this crate may name in
//! dev-dependencies (PLAN.md 4.1). The rule exists so the core cannot grow a dependency on
//! a storage that would then be unavailable on one of the WASM targets.
//!
//! # Layering
//!
//! ```text
//! dxdb-core ──▶ dxdb-storage   (traits + document type, D1)
//!           ├─▶ dxdb-query     (normalise + plan + match)
//!           ├─▶ dxdb-schema    (defaulting, hashing, defaults)
//!           └─▶ dxdb-json/jsnum/jsstr  (encoding)
//!
//! dxdb-memory is a *dev*-dependency only.
//! ```
//!
//! # What this crate owns
//!
//! | Module | Upstream | Contents |
//! | --- | --- | --- |
//! | [`write`] | `rx-storage-helper.ts:612`, `rx-collection-helper.ts:34` | Revision and `_meta` assignment, insert defaulting |
//! | [`cache`] | `doc-cache.ts`, `incremental-write.ts:237` | One state per id, newest revision wins |
//! | [`queue`] | `incremental-write.ts:48` | Serialised incremental writes per document |
//! | [`hooks`] | `src/hooks.ts` | 24 named hooks, owned registry, explicit ordering (PLAN.md 4.3) |
//! | [`plugin`] | `src/plugin.ts` | `Plugin`/`PluginHandle`/`PluginSet`, overwritables (PLAN.md 4.3) |
//! | [`document`] | `rx-document.ts` | The document value and patch semantics |
//! | [`query`] | `rx-query-helper.ts:345` | Normalise + plan, plus the tombstone filter |
//! | [`collection`] | `rx-collection.ts` | CRUD, bulk operations, `find`/`count` |
//! | [`database`] | `rx-database.ts` | Name validation, collections, instance token |
//!
//! # The three behaviours most likely to be got wrong
//!
//! 1. **One `now()` per `bulkWrite`.** Every row of a batch shares one `_meta.lwt`
//!    (`rx-storage-helper.ts:630`). A per-row clock changes checkpoint ordering.
//! 2. **A collection filters tombstones; a storage does not.** `find()` adds
//!    `_deleted: false`; a raw storage query returns whatever matched.
//! 3. **A resurrection is an `INSERT` event with no previous state**, and a delete is a
//!    tombstone revision, never a physical delete.
//!
//! # Deliberate omissions
//!
//! `localDocuments`, the internal database store, migration state, `collections$` as an
//! observable, and the dev-mode Proxy that detects writes through a stale handle. The
//! stale-handle check exists as [`document::Document::ensure_revision`] instead, because a
//! Rust value cannot report a mutation made through a different handle.

#![forbid(unsafe_code)]

pub mod cache;
pub mod collection;
pub mod database;
pub mod document;
pub mod error;
pub mod hooks;
pub mod plugin;
pub mod query;
pub mod queue;
pub mod write;

pub use cache::{find_newest_of_document_states, revision_height, DocumentCache};
pub use collection::{BulkResult, Collection, BULK_INSERT_CONTEXT, UPDATE_CONTEXT};
pub use database::{CollectionDefinition, Database};
pub use document::{Document, PatchOperation};
pub use error::{CoreError, CoreResult};
pub use hooks::{HookId, HookName, HookOutcome, HookPayload, Hooks, Order, PayloadKind};
pub use plugin::{
    Overwritables, Plugin, PluginError, PluginHandle, PluginSet, Registrations,
};
pub use queue::WriteQueue;
pub use write::{assign_revisions, fill_object_data_before_insert, PreparedRow};
