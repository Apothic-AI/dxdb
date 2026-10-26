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

/// The index a query would use: the forced index if one is given, else the sort
/// fields, with `_deleted` first — the shape `fillWithDefaultSettings` produces.
///
/// This is not the planner. It is the *identity* of the index a plan would name, so
/// a storage that materialised that index can be handed a plan for it. Choosing a
/// better index than the sort order requires costs information (`getQueryPlan`).
fn planned_index(query: &MangoQuery, primary_path: &str) -> Vec<String> {
    let mut index: Vec<String> = Vec::new();
    if let Some(forced) = &query.index {
        index.extend(forced.iter().cloned());
    } else {
        for part in &query.sort {
            index.push(part.field.clone());
        }
    }
    if index.first().map(String::as_str) != Some("_deleted") {
        index.insert(0, "_deleted".to_string());
    }
    if !index.iter().any(|field| field == primary_path) {
        index.push(primary_path.to_string());
    }
    index
}

/// Builds the `PreparedQuery` a case hands to `query()` / `count()`.
///
/// See the module docs: the plan asserts nothing and carries no bounds, so the
/// storage must do the filtering and the sorting itself. That is the approximation,
/// and it is the conservative direction — it can fail a correct storage for being
/// slow, but it cannot pass an incorrect one.
pub fn build_prepared_query(query: &Value, primary_path: &str) -> PreparedQuery {
    let mango = mango_query(query, primary_path);
    let index = planned_index(&mango, primary_path);
    let plan = QueryPlan {
        // No bounds: a full scan of the named index.
        start_keys: vec![
            PlanKey::index_min();
            index.len()
        ],
        end_keys: vec![PlanKey::index_max(); index.len()],
        selector_satisfied_by_index: false,
        sort_satisfied_by_index: false,
        index,
        inclusive_start: true,
        inclusive_end: true,
    };
    PreparedQuery::new(mango, plan)
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

/// Fills a schema the way `fillWithDefaultSettings` does *for the fields a storage
/// reads*: `_deleted`, `_rev`, `_meta`, `_attachments` in `properties`/`required`,
/// and `_deleted` prepended to every index.
///
/// This is a stopgap with a named owner. `dxdb-schema` (lane B) owns the real
/// `fill_with_default_settings`, including hashing and the meta defaults, and this
/// function is deleted when it lands. It exists now because a storage reads
/// `StorageSchema.indexes` and upstream's fixtures carry *unfilled* indexes; without
/// it, every case would fail for a reason that has nothing to do with the storage.
///
/// Deliberately does **not** hash the schema or inject `default` values: those parts
/// of filling are only observable through `dxdb-schema`, and guessing them here
/// would produce a schema whose hash disagrees with the real one.
pub fn fill_with_default_settings(mut schema: Value) -> Value {
    let Some(object) = schema.as_object_mut() else {
        panic!("fill_with_default_settings() needs an object schema")
    };

    // properties
    let properties = object
        .entry("properties".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(properties) = properties.as_object_mut() else {
        panic!("schema.properties is not an object")
    };
    properties
        .entry("_deleted".to_string())
        .or_insert_with(|| serde_json::json!({ "type": "boolean" }));
    properties
        .entry("_attachments".to_string())
        .or_insert_with(|| serde_json::json!({ "type": "object" }));
    properties
        .entry("_rev".to_string())
        .or_insert_with(|| serde_json::json!({ "type": "string", "minLength": 1, "maxLength": 100 }));
    properties
        .entry("_meta".to_string())
        .or_insert_with(|| serde_json::json!({ "type": "object" }));
    properties
        .entry("_rev".to_string())
        .or_insert_with(|| serde_json::json!({ "type": "string" }));

    // required
    let required = object
        .entry("required".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(required) = required.as_array_mut() else {
        panic!("schema.required is not an array")
    };
    for field in ["_deleted", "_attachments", "_rev", "_meta"] {
        if !required.iter().any(|item| item.as_str() == Some(field)) {
            required.push(Value::String(field.to_string()));
        }
    }

    let primary_path = match object.get("primaryKey") {
        Some(Value::String(path)) => path.clone(),
        Some(Value::Object(map)) => map
            .get("key")
            .and_then(Value::as_str)
            .unwrap_or("id")
            .to_string(),
        _ => panic!("schema has no primaryKey"),
    };

    // indexes: `_deleted` prepended unless already first, primary key appended
    // unless already present, then the mandatory change-feed index.
    let mut indexes: Vec<Value> = match object.remove("indexes") {
        Some(Value::Array(list)) => list,
        Some(Value::Null) | None => Vec::new(),
        Some(other) => panic!("schema.indexes is not an array: {other}"),
    };
    let mut filled: Vec<Value> = Vec::new();
    for index in indexes.drain(..) {
        let mut fields: Vec<String> = match index {
            Value::String(field) => vec![field],
            Value::Array(parts) => parts
                .iter()
                .filter_map(|part| part.as_str().map(str::to_string))
                .collect(),
            other => panic!("schema index entry is neither a string nor an array: {other}"),
        };
        // Upstream's rule: `_deleted` first (before the primary key is appended, so
        // a schema that already begins with the primary key still gets `_deleted`
        // in front).
        if fields.first().map(String::as_str) != Some("_deleted") {
            fields.insert(0, "_deleted".to_string());
        }
        if !fields.iter().any(|field| field == &primary_path) {
            fields.push(primary_path.clone());
        }
        filled.push(Value::Array(
            fields.into_iter().map(Value::String).collect(),
        ));
    }
    let change_feed = Value::Array(vec![
        Value::String("_meta.lwt".to_string()),
        Value::String(primary_path.clone()),
    ]);
    if !filled
        .iter()
        .any(|index| index.as_array().and_then(|parts| parts.first()).and_then(Value::as_str) == Some("_meta.lwt"))
    {
        filled.push(change_feed);
    }
    object.insert("indexes".to_string(), Value::Array(filled));

    schema
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
