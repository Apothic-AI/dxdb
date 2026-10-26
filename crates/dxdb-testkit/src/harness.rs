//! The harness: opening instances, building `PreparedQuery`s, and running the
//! assertions upstream's suites make.
//!
//! # The one thing this module refuses to do
//!
//! Upstream's suites call `prepareQuery(schema, query)` — the query planner — and
//! its output is what a storage consumes. `dxdb-query` (lane B) owns the planner
//! and is not in this workspace yet. So this module provides
//! [`build_prepared_query`], which is an **explicit, documented approximation**: it
//! normalises the pieces the *fixtures* need (selector as-is, sort with the primary
//! key appended, skip/limit) and produces a plan that asserts nothing
//! (`selector_satisfied_by_index: false`, `sort_satisfied_by_index: false`, no
//! bounds), which forces a storage to filter and sort for itself.
//!
//! That is a deliberate design, not a shortcut:
//!
//! * It is the *weakest* plan a storage can be handed, so a case that passes with it
//!   proves the storage's own filtering and ordering are correct — the harder half
//!   of the contract.
//! * It is honest about what it is. A storage that passed these cases and then
//!   failed with a real planner plan would be a planner bug, not a storage bug, and
//!   the harness has no way to tell the two apart, so it does not claim to.
//! * The moment `dxdb-query` exists, [`build_prepared_query`] is replaced by a call
//!   to it and every case that used it starts testing the plan too. That is one
//!   function body, not 90 rewrites.
//!
//! The `selectorSatisfiedByIndex` assertions from upstream's correctness suite are
//! therefore *declared but not evaluated* here, and the declaration table says so
//! per group (`PortStatus::NeedsQueryCrate`).

use serde_json::{Map, Value};

use dxdb_storage::{
    BulkWriteResponse, CountResult, DocumentData, DynRxStorageInstance, EventBulk,
    InstanceCreationParams, MangoQuery, PlanKey, PreparedQuery, QueryPlan, RxStorage,
    RxStorageInstance, SortDirection, SortPart, StorageError, StorageSchema,
};

use crate::fixtures::write::doc;

/// What a case needs to open an instance, with the defaults the suites use.
#[derive(Clone, Debug)]
pub struct InstanceSpec {
    /// `databaseInstanceToken`: identifies the owning database. Random per case
    /// upstream, which is why two cases never interfere.
    pub database_instance_token: String,
    /// Database name.
    pub database_name: String,
    /// Collection name.
    pub collection_name: String,
    /// The filled schema, as JSON.
    pub schema: Value,
    /// Storage-specific options; `{}` in every ported case.
    pub options: Value,
    /// `multiInstance`.
    pub multi_instance: bool,
    /// `devMode: true` — upstream's suites always set it, so a storage that is
    /// stricter in dev mode must be tested in it.
    pub dev_mode: bool,
}

impl InstanceSpec {
    /// A spec with fresh random token database and collection names, and the
    /// given schema. This is the shape every ported case starts from.
    pub fn new(schema: Value) -> Self {
        InstanceSpec {
            database_instance_token: crate::util::random_token(10),
            database_name: crate::util::random_token(12),
            collection_name: crate::util::random_token(12),
            schema,
            options: Value::Object(Map::new()),
            multi_instance: false,
            dev_mode: true,
        }
    }

    /// The same spec but with a caller-chosen database and collection name, for the
    /// cases that deliberately share a name (`multiInstance`, and "should not mix up
    /// documents stored in a different database name").
    pub fn named(schema: Value, database_name: &str, collection_name: &str) -> Self {
        let mut spec = Self::new(schema);
        spec.database_name = database_name.to_string();
        spec.collection_name = collection_name.to_string();
        spec
    }

    /// Sets `multi_instance`.
    pub fn multi_instance(mut self, value: bool) -> Self {
        self.multi_instance = value;
        self
    }

    /// Sets the collection name, keeping the rest.
    pub fn collection(mut self, name: &str) -> Self {
        self.collection_name = name.to_string();
        self
    }

    /// Sets the database name, keeping the rest.
    pub fn database(mut self, name: &str) -> Self {
        self.database_name = name.to_string();
        self
    }

    /// The `StorageSchema` a storage reads.
    pub fn storage_schema(&self) -> StorageSchema {
        StorageSchema::from_value(&self.schema)
            .unwrap_or_else(|err| panic!("fixture schema is not a storage schema: {err}"))
    }

    /// The `InstanceCreationParams` the trait takes.
    pub fn params(&self) -> InstanceCreationParams {
        InstanceCreationParams {
            database_instance_token: self.database_instance_token.clone(),
            database_name: self.database_name.clone(),
            collection_name: self.collection_name.clone(),
            schema: self.storage_schema(),
            options: self.options.clone(),
            multi_instance: self.multi_instance,
            dev_mode: self.dev_mode,
        }
    }
}

/// A handle that bundles an instance with the schema facts a case needs, so a case
/// body is assertions rather than bookkeeping.
pub struct Harness {
    /// The open instance.
    ///
    /// `DynRxStorageInstance` is dynosaur's generated twin of
    /// [`dxdb_storage::RxStorageInstance`]: it *implements* that trait by delegating,
    /// which is why every call below reaches the concrete storage's method through
    /// the same trait method the concrete type implements. That is what lets the suite
    /// be written once against the trait and run against any storage.
    pub instance: Box<DynRxStorageInstance<'static>>,
    /// The primary key path of the schema.
    pub primary_path: String,
    /// The collection name, for a failure message that names what failed.
    pub collection_name: String,
}

impl Harness {
    /// Opens an instance for `spec`.
    ///
    /// # Errors
    ///
    /// Propagates whatever `create_storage_instance` returns. A case that expects a
    /// refusal calls `create_storage_instance` directly instead, because *that* is
    /// the assertion.
    pub async fn open<S: RxStorage>(
        storage: &S,
        spec: &InstanceSpec,
    ) -> Result<Harness, StorageError> {
        let schema = spec.storage_schema();
        let primary_path = schema.primary_path.clone();
        let collection_name = spec.collection_name.clone();
        let instance = storage.create_storage_instance(spec.params()).await?;
        Ok(Harness {
            instance,
            primary_path,
            collection_name,
        })
    }

    /// Writes rows and asserts there were no per-row errors, the way upstream's
    /// every write does with `assert.deepStrictEqual(response.error, [])`.
    pub async fn write_ok(&self, rows: Vec<dxdb_storage::BulkWriteRow>) -> BulkWriteResponse {
        let response = self
            .instance.as_ref()
            .bulk_write(rows, "testkit".to_string())
            .await
            .unwrap_or_else(|err| panic!("bulk_write failed outright: {err}"));
        assert!(
            response.is_ok(),
            "bulk_write returned per-row errors: {:?}",
            response.error
        );
        response
    }

    /// Writes rows and returns the response without asserting, for the cases whose
    /// subject *is* the error list.
    pub async fn write(&self, rows: Vec<dxdb_storage::BulkWriteRow>) -> BulkWriteResponse {
        self.instance.as_ref()
            .bulk_write(rows, "testkit".to_string())
            .await
            .expect("bulk_write failed outright")
    }

    /// `findDocumentsById`.
    pub async fn find(&self, ids: &[&str], with_deleted: bool) -> Vec<DocumentData> {
        self.instance.as_ref()
            .find_documents_by_id(
                ids.iter().map(|id| (*id).to_string()).collect(),
                with_deleted,
            )
            .await
            .expect("find_documents_by_id failed")
    }

    /// Builds a prepared query against **this instance's** schema.
    ///
    /// The right way for a case to build a query: the plan picks an index the schema
    /// actually has, so a correct storage cannot fail it with `UnknownIndex`.
    pub fn prepare(&self, query: &Value) -> PreparedQuery {
        build_prepared_query_for_schema(query, self.instance.schema())
    }

    /// Runs a prepared query.
    pub async fn query(&self, prepared: PreparedQuery) -> Vec<DocumentData> {
        self.instance.as_ref()
            .query(prepared)
            .await
            .expect("query failed")
    }

    /// Runs a prepared query and returns the primary values in order — the form
    /// every expected-result assertion is written in.
    pub async fn query_ids(&self, prepared: PreparedQuery) -> Vec<String> {
        let primary = self.primary_path.clone();
        self.query(prepared)
            .await
            .iter()
            .map(|document| {
                document
                    .id(&primary)
                    .unwrap_or_else(|err| panic!("a returned document has no primary value: {err}"))
                    .to_string()
            })
            .collect()
    }

    /// `count()`.
    pub async fn count(&self, prepared: PreparedQuery) -> CountResult {
        self.instance.as_ref().count(prepared).await.expect("count failed")
    }

    /// `close()`.
    pub async fn close(self) {
        self.instance.as_ref().close().await.expect("close failed");
    }

    /// `remove()`.
    pub async fn remove(self) {
        self.instance.as_ref().remove().await.expect("remove failed");
    }
}

/// Builds a `MangoQuery` from the fixture JSON.
///
/// Normalisation here is limited to what the *fixtures* need, and each step is
/// named:
///
/// * `selector` is passed through. Upstream's fixtures already use operator form
///   (`{age: {$gt: 20}}`), so there is nothing to rewrite.
///
///   **No `_deleted` filtering is added here.** That distinction is load-bearing and it is
///   the one thing this harness got wrong first: a bare storage does *not* filter
///   tombstones. Upstream spells it out in the implementations suite —
///   `rx-storage-implementations.test.ts:2172`, "must still be 2 because the storage
///   itself also counts deleted docs" — and the correctness suite compensates by
///   *injecting* `_deleted: false` into the selector before calling the storage
///   (`rx-storage-query-correctness.test.ts:212`). So:
///
///   * [`mango_query`] + [`build_prepared_query`] = what `prepareQuery` produces, used by
///     the implementations suite, which expects tombstones to be included;
///   * [`build_correctness_query`] = the same plus the `_deleted: false` injection the
///     correctness suite performs, used by that suite because its expected-id lists omit
///     tombstones.
/// * `sort` is taken in order. **When it is empty**, the primary key is appended —
///   `normalizeMangoQuery` always appends the primary key if absent, and a storage
///   is entitled to rely on a total order. Three fixtures omit `sort` entirely and
///   upstream's expected-id lists for those are order-sensitive, which only works
///   because of this append.
/// * `skip` defaults to `0`, `limit` stays optional (`None` means unlimited; an
///   explicit `0` stays `0`, which is a real zero).
/// * `index` is read as a list.
pub fn mango_query(query: &Value, primary_path: &str) -> MangoQuery {
    let selector = query
        .get("selector")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));

    let mut sort: Vec<SortPart> = Vec::new();
    if let Some(parts) = query.get("sort").and_then(Value::as_array) {
        for part in parts {
            let Some(object) = part.as_object() else { continue };
            for (field, direction) in object {
                let direction = direction
                    .as_str()
                    .and_then(SortDirection::parse)
                    .unwrap_or_else(|| {
                        panic!("fixture sort direction is neither asc nor desc: {direction}")
                    });
                sort.push(SortPart {
                    field: field.clone(),
                    direction,
                });
            }
        }
    }
    if !sort.iter().any(|part| part.field == primary_path) {
        sort.push(SortPart::asc(primary_path));
    }

    let skip = query.get("skip").and_then(Value::as_u64).unwrap_or(0) as usize;
    let limit = query.get("limit").and_then(Value::as_u64).map(|n| n as usize);
    let index = query.get("index").and_then(Value::as_array).map(|parts| {
        parts
            .iter()
            .filter_map(|part| part.as_str().map(str::to_string))
            .collect::<Vec<String>>()
    });

    MangoQuery {
        selector,
        sort,
        skip,
        limit,
        index,
    }
}

/// Chooses an index **the schema actually materialised** for this query.
///
/// This replaces an earlier version that synthesized an index name from the sort fields
/// (`_deleted` + sort parts + primary key). That produced plans naming indexes the schema
/// never had — `UnknownIndex("_deleted,value,key")` — which failed three `dxdb-memory`
/// cases and would have failed every storage that correctly refuses an index it does not
/// have. The storage was right; the harness was asking for something imaginary.
///
/// The rule now, matching what a real planner guarantees:
///
/// 1. If the query forces an `index`, use it **only if the schema has it**. A query that
///    names a missing index is a bug in the query, and upstream's planner returns a plan
///    for a schema index instead of erroring, so falling through to (2) is faithful.
/// 2. Otherwise pick the schema index whose field list is a prefix-compatible match for the
///    sort: every sort field must appear, in order, at the start of the index. That is the
///    only condition under which an index can satisfy a sort.
/// 3. Otherwise the change-feed index, which every filled schema has, and which is a
///    valid total order (`_meta.lwt`, primary) — the storage will re-sort, which is
///    correct and is what `sort_satisfied_by_index: false` tells it to do.
///
/// Every branch returns an index from [`StorageSchema::indexes`], so a plan can never name
/// an index the storage does not have.
fn planned_index(query: &MangoQuery, schema: &StorageSchema) -> Vec<String> {
    let primary = &schema.primary_path;

    // (1) A forced index, if the schema has it.
    if let Some(forced) = &query.index {
        let mut wanted = forced.clone();
        // Upstream prepends `_deleted` to a forced index in the correctness suite; accept
        // both spellings so a fixture written either way resolves.
        let mut candidates = vec![wanted.clone()];
        if wanted.first().map(String::as_str) != Some("_deleted") {
            wanted.insert(0, "_deleted".to_string());
            candidates.push(wanted);
        }
        for candidate in candidates {
            if let Some(actual) = schema.indexes.iter().find(|index| **index == candidate) {
                return actual.clone();
            }
        }
    }

    // (2) An index that can satisfy the sort: the sort fields must be a prefix of it.
    let sort_fields: Vec<&str> = query.sort.iter().map(|part| part.field.as_str()).collect();
    for index in &schema.indexes {
        // Skip the change-feed index here; it is the fallback in (3) and would otherwise
        // match any query sorted by `_meta.lwt` before a user index gets a chance.
        if index.first().map(String::as_str) == Some("_meta.lwt") {
            continue;
        }
        // The index begins with `_deleted` and ends with the primary key; compare only the
        // user field run against the sort.
        let user_run: Vec<&str> = index
            .iter()
            .map(String::as_str)
            .filter(|field| *field != "_deleted")
            .collect();
        let mut matched = 0;
        for field in &sort_fields {
            if field == primary {
                // The primary key is appended by normalisation, so it never needs to
                // appear in the compared prefix.
                break;
            }
            if user_run.get(matched).copied() == Some(*field) {
                matched += 1;
            } else {
                break;
            }
        }
        if matched == sort_fields.iter().filter(|field| **field != primary).count() {
            return index.clone();
        }
    }

    // (3) The change-feed index: always present after filling, and a valid total order.
    if let Some(change_feed) = schema.change_feed_index() {
        return change_feed.clone();
    }

    // (4) Nothing to offer. Fall back to a `_deleted`-prefixed primary-key index, which
    // filling guarantees, and which a storage can always scan.
    vec!["_deleted".to_string(), primary.clone()]
}

/// Builds the `PreparedQuery` a case hands to `query()` / `count()`.
///
/// See the module docs: the plan asserts nothing and carries no bounds, so the
/// storage must do the filtering and the sorting itself. That is the approximation,
/// and it is the conservative direction — it can fail a correct storage for being
/// slow, but it cannot pass an incorrect one.
///
/// Prefer [`build_prepared_query_for_schema`] wherever a `StorageSchema` is in hand: this
/// form cannot see which indexes exist, so it must guess, and a guess that names an index
/// the storage does not have fails against a *correct* storage (`UnknownIndex`). It is kept
/// for call sites that only have a primary path.
pub fn build_prepared_query(query: &Value, primary_path: &str) -> PreparedQuery {
    let mango = mango_query(query, primary_path);
    // Without a schema, the only index that is guaranteed to exist is the change-feed
    // index, so use it rather than synthesizing one from the sort.
    let index = vec!["_meta.lwt".to_string(), primary_path.to_string()];
    PreparedQuery::new(mango, unrestricted_plan(index))
}

/// Builds a `PreparedQuery` using the indexes `schema` actually materialised.
///
/// This is the form a suite should use. It picks an index the storage has — see
/// [`planned_index`] for the rule — so the plan can never name an index that does not
/// exist, which is how three `dxdb-memory` cases were failing against a correct storage.
pub fn build_prepared_query_for_schema(query: &Value, schema: &StorageSchema) -> PreparedQuery {
    let mango = mango_query(query, &schema.primary_path);
    let index = planned_index(&mango, schema);
    PreparedQuery::new(mango, unrestricted_plan(index))
}

/// A plan that bounds nothing and claims nothing: the storage filters and sorts.
fn unrestricted_plan(index: Vec<String>) -> QueryPlan {
    QueryPlan {
        start_keys: vec![PlanKey::index_min(); index.len()],
        end_keys: vec![PlanKey::index_max(); index.len()],
        selector_satisfied_by_index: false,
        sort_satisfied_by_index: false,
        index,
        inclusive_start: true,
        inclusive_end: true,
    }
}

/// Builds the `PreparedQuery` the **query-correctness suite** hands to a storage.
///
/// Identical to [`build_prepared_query`] except that it injects `_deleted: false` into the
/// selector, which is what that suite does at
/// `rx-storage-query-correctness.test.ts:211-212`:
///
/// ```text
/// if (!queryForStorage.selector) queryForStorage.selector = {};
/// (queryForStorage.selector as any)._deleted = false;
/// ```
///
/// It is an *assignment*, not a default, so a fixture that sets `_deleted` itself — the
/// "$eq operator" group's `null` field and the `$or` group do not, but a future one could
/// — would still be overwritten, exactly as upstream overwrites it.
///
/// This is a separate function rather than a flag because the two suites' expectations are
/// opposite (see [`mango_query`]): the implementations suite asserts that a tombstone *is*
/// counted and *is* found, and the correctness suite's expected-id lists never contain one.
/// One function with a flag invites exactly the mix-up that produced a wrong assertion here.
pub fn build_correctness_query(query: &Value, primary_path: &str) -> PreparedQuery {
    let mut with_tombstone_filter = query.clone();
    if let Some(object) = with_tombstone_filter.as_object_mut() {
        if object.get("selector").is_none() {
            object.insert("selector".to_string(), Value::Object(Map::new()));
        }
        if let Some(selector) = object.get_mut("selector").and_then(Value::as_object_mut) {
            selector.insert("_deleted".to_string(), Value::Bool(false));
        }
        // Upstream also unshifts `_deleted` onto a forced index:
        // `if (queryForStorage.index) queryForStorage.index.unshift('_deleted')`.
        if let Some(index) = object.get_mut("index").and_then(Value::as_array_mut) {
            index.insert(0, Value::String("_deleted".to_string()));
        }
    } else {
        panic!("fixture query is not an object: {query}");
    }
    build_prepared_query(&with_tombstone_filter, primary_path)
}

/// Builds a `PreparedQuery` for an unfiltered scan of the whole collection —
/// upstream's `{ selector: {}, sort: [{ key: 'asc' }] }`.
pub fn full_scan(primary_path: &str) -> PreparedQuery {
    build_prepared_query(
        &serde_json::json!({
            "selector": {},
            "sort": [{ primary_path: "asc" }],
        }),
        primary_path,
    )
}

/// Collects events from a change stream until `predicate` is satisfied.
///
/// A `futures::Stream` replaces upstream's `Observable` (D2): there is no
/// subscription to forget, and iteration is explicit. `max_bulks` bounds the wait so
/// a storage that never emits fails the case instead of hanging the suite.
///
/// # Errors
///
/// Returns whatever the predicate returns when the bound is reached.
pub async fn collect_until<S, F>(
    stream: &mut S,
    max_bulks: usize,
    mut predicate: F,
) -> Result<Vec<EventBulk>, String>
where
    S: futures::Stream<Item = EventBulk> + Unpin,
    F: FnMut(&[EventBulk]) -> bool,
{
    use futures::StreamExt;
    let mut collected = Vec::new();
    for _ in 0..max_bulks {
        if predicate(&collected) {
            return Ok(collected);
        }
        match stream.next().await {
            Some(bulk) => collected.push(bulk),
            None => {
                return Err(format!(
                    "change stream ended after {} bulk(s) without satisfying the predicate",
                    collected.len()
                ))
            }
        }
    }
    if predicate(&collected) {
        Ok(collected)
    } else {
        Err(format!(
            "change stream produced {max_bulks} bulk(s) without satisfying the predicate; saw {:?}",
            collected
                .iter()
                .map(|bulk| bulk.events.len())
                .collect::<Vec<_>>()
        ))
    }
}

/// Flattens bulks into their events — upstream's `flattenEvents`.
pub fn flatten_events(bulks: &[EventBulk]) -> Vec<&dxdb_storage::ChangeEvent> {
    bulks.iter().flat_map(|bulk| bulk.events.iter()).collect()
}

/// The operation spellings of every event, in order — what upstream asserts against
/// `['INSERT', 'INSERT', ...]`.
pub fn operations(bulks: &[EventBulk]) -> Vec<&'static str> {
    flatten_events(bulks)
        .iter()
        .map(|event| event.operation.as_str())
        .collect()
}

/// Fills a schema the way a real database does, by calling `dxdb-schema`.
///
/// This used to be a local stopgap that injected `_deleted`, `_rev`, `_meta` and
/// `_attachments` itself, on the stated reasoning that `dxdb-schema` was another lane's
/// crate and guessing at hashing would produce a schema whose hash disagreed with the
/// real one. That reasoning was right at the time and became wrong the moment
/// `dxdb-schema` landed, because the guess was **not** conservative — it injected `_meta`
/// as a bare `{"type": "object"}` with no `lwt`, while the fixtures' indexes reference
/// `_meta.lwt` (PLAN.md 4.3 prepends it to essentially every index as the change-feed
/// sort key). The result was a schema whose own index could not be resolved:
///
/// ```text
/// CI1: index field `_meta.lwt` is not in the schema
/// ```
///
/// which failed 43 of 49 `dxdb-indexeddb` cases before any storage code ran. A fixture
/// that builds its own schema and gets it wrong makes every storage look broken, so this
/// now defers to the crate that owns schema defaulting.
///
/// # What is *not* delegated, and why
///
/// Nothing. The whole body is a call to [`dxdb_schema::fill_with_default_settings`],
/// including the meta defaults and the schema hash, because every part of filling is
/// observable: the hash is part of schema identity, `_meta.lwt` is referenced by the
/// indexes, and `_deleted` is referenced by every index's first key. A partial fill is a
/// schema that is not the one a database would produce, and a suite running against a
/// schema no database would produce is testing schema validation instead of storage
/// behaviour.
///
/// # Panics
///
/// Panics on a schema `dxdb-schema` rejects. These are test fixtures written by hand, so
/// a rejection is a bug in the fixture and should fail loudly at the point it is used
/// rather than being threaded through as a `Result` that every case would have to unwrap.
pub fn fill_with_default_settings(schema: Value) -> Value {
    dxdb_schema::fill_with_default_settings(&schema).unwrap_or_else(|err| {
        panic!("fixture schema is not one dxdb-schema accepts: {err}\n  schema: {schema}")
    })
}

/// A one-line summary of a document, for a failure message.
pub fn describe(document: &DocumentData) -> String {
    document.to_js_json()
}

/// The primary values of a document list, in order.
pub fn ids(documents: &[DocumentData], primary_path: &str) -> Vec<String> {
    documents
        .iter()
        .map(|document| {
            document
                .id(primary_path)
                .unwrap_or_else(|err| panic!("document has no primary value: {err}"))
                .to_string()
        })
        .collect()
}

/// Wraps a JSON payload as a [`DocumentData`], for a case that needs one without
/// going through the write helpers.
pub fn document(value: Value) -> DocumentData {
    doc(value)
}
