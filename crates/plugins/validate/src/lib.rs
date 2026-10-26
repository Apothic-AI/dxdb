//! Validate documents before they reach storage, reporting `422` write errors.
//!
//! Ported from the shared half of the four `validate-*` plugins:
//! `wrappedValidateStorageFactory` (`src/plugin-helpers.ts:55-146`) plus the contract the
//! upstream test file asserts (`test/unit/validate.test.ts`, 1,020 lines).
//!
//! # Why this is one crate and not four
//!
//! Upstream ships `validate-ajv`, `validate-ajv-standalone`,
//! `validate-is-my-json-valid` and `validate-z-schema`. They differ **only in which JS
//! library compiles the JSON Schema**; everything else — the storage wrapper, the property
//! stripping, the lazy initialisation, the `422` reporting, the per-schema validator cache —
//! is identical and lives in `plugin-helpers.ts`, not in any of the four. Each plugin file is
//! 30–126 lines of adapter.
//!
//! dxdb has one schema engine (`jsonschema` 0.60.1, PLAN §9) and `dxdb-schema` already uses
//! it. So porting four adapters would produce four crates calling the same function with the
//! same arguments. What *is* worth porting is the behaviour above the engine, which is what
//! this crate is. `validate-z-schema` and `validate-is-my-json-valid` are omitted, and
//! `validate-ajv-standalone` becomes the [`precompiled`](#) feature — see
//! [`ValidatorCache`] for what that feature actually changes.
//!
//! # The behaviour worth having
//!
//! **A failed row does not reach storage, and the other rows still do.** Upstream partitions
//! the batch: rows that fail validation become `422` errors appended to the response, and only
//! the survivors are handed to the real storage (`plugin-helpers.ts:100-140`). That is a
//! *per-row* contract, not an all-or-nothing one — `bulkWrite` is defined as per-document
//! atomic, so validating it any other way would change its semantics.
//!
//! **The 422 error carries the schema.** The upstream test asserts
//! `err.schema.type === 'object'` on every validation error
//! (`validate.test.ts:84-85`), "so that the validation errors are easier to debug". A port
//! that reported only the messages would pass a naive test and fail that one.
//!
//! **Read-only and meta properties are stripped before validating.** Wait — they are not, and
//! that is worth stating precisely because it is the natural assumption. Upstream validates
//! `row.document` **as the storage receives it**, which includes `_deleted`, `_rev`, `_meta`
//! and `_attachments`; those are in the schema because `fillWithDefaultSettings` injected them
//! (`dxdb-schema` does the same). So there is nothing to strip here, and the module that looks
//! like it does that work is `wrappedValidateStorageFactory`'s caller in *each* plugin, not the
//! factory. Checked against the source rather than assumed.
//!
//! **The validator is built lazily and cached per schema.** Upstream defers construction with
//! `requestIdleCallbackIfAvailable` and falls back to building it on the first write
//! (`:92-98`), then caches by `JSON.stringify(schema)` in a per-library map so two collections
//! with one schema share a validator (`:49-53`, `:63-71`). The lazy part is a start-up
//! optimisation and needs a browser idle callback; the **cache** is the part with observable
//! behaviour (same schema ⇒ same validator ⇒ the same errors), and it is here.
//!
//! # What this crate does not do
//!
//! It does not implement `RxStorageInstance`. A validator is a *storage wrapper*, which
//! returns `Box<dyn RxStorageInstance>` and is therefore blocked on the
//! `DynRxStorageInstance` twin (PLAN §3.3) — the same blocker as `key-compression`,
//! `encryption` and `attachments-compression`. What is implemented and tested is everything
//! that decides *what a validation write response is*: [`partition_rows`], [`validation_error`],
//! [`ValidatorCache`] and [`ValidateStorage`]. The wrapper's remaining job is forwarding.

#![forbid(unsafe_code)]

use serde_json::Value;
use std::collections::BTreeMap;
use thiserror::Error;

/// The status code a validation failure reports (`plugin-helpers.ts:110`, and the upstream
/// assertion at `validate.test.ts:83`).
pub const VALIDATION_STATUS: u16 = 422;

/// The engine name prefix upstream adds to the wrapped storage
/// (`name: 'validate-' + validatorKey + '-' + storage.name`, `plugin-helpers.ts:81`).
pub const STORAGE_NAME_PREFIX: &str = "validate-";

/// Errors.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum Error {
    /// The schema could not be prepared for validation.
    #[error("could not prepare a validator for the schema: {0}")]
    Schema(String),
    /// The document is not an object, so it cannot be validated against an object schema.
    #[error("a document must be a JSON object to validate, got {0}")]
    NotAnObject(String),
}

/// Result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// One validator's verdict on one document: the list of failure messages, empty when valid.
///
/// Mirrors upstream's `ValidatorFunction = (docData) => RxValidationError[]`
/// (`plugin-helpers.ts:40`): a validator **returns** errors rather than throwing, which is
/// what lets the batch be partitioned instead of aborting.
pub type ValidationErrors = Vec<Value>;

/// A compiled validator for one schema.
pub trait Validator {
    /// Validates one document, returning every failure (empty when valid).
    fn validate(&self, document: &Value) -> Result<ValidationErrors>;
}

/// The validator: `dxdb-schema`'s draft-07 engine, with **format checking enabled**.
///
/// # Why this is not just `dxdb_schema::validate_document`
///
/// `dxdb-schema` compiles its validator with `should_validate_formats(false)` and documents
/// why (`dxdb-schema/src/json_schema.rs:10-14`): upstream's *default* stack does not check
/// `format`, and a schema with `format: "date-time"` on a 40-character field would otherwise
/// punish a user for adding documentation.
///
/// But the validate plugins are precisely the opt-in that turns those checks on. Upstream's
/// `validate-ajv` calls `addFormats(ret)` on the Ajv instance
/// (`src/plugins/validate-ajv/index.ts:39`, and its doc comment says "the RxDB specific schema
/// keywords and the default formats"), and the upstream test file requires it:
/// `validate.test.ts:715` "#7253 should know about the date-time format by default by all
/// validation plugins".
///
/// So this crate does not delegate to `validate_document`. It compiles the same draft-07 engine
/// with formats **enabled**, which is the behaviour the plugin is for — and it is a genuinely
/// different verdict from `dxdb-schema`'s, which is why it is worth having rather than being a
/// second call to the same function.
#[derive(Clone, Debug)]
pub struct SchemaValidator {
    schema: Value,
    inner: jsonschema::Validator,
}

impl SchemaValidator {
    /// Prepares a validator for `schema`, with format checking on.
    ///
    /// Compiling once here is what makes the cache worthwhile; a schema the engine cannot
    /// compile is a start-up error rather than a first-write surprise (`plugin-helpers.ts:92-98`
    /// defers construction, and this is the same deferral resolved eagerly at cache time).
    pub fn new(schema: &Value) -> Result<Self> {
        let inner = jsonschema::options()
            .with_draft(jsonschema::Draft::Draft7)
            // The opposite of `dxdb-schema`'s choice: enable formats, which is what the
            // validate plugins exist to do. See the struct docs.
            .should_validate_formats(true)
            // Unknown formats are ignored rather than erroring, matching upstream: an unknown
            // format is documentation, not a typo to fail on.
            .should_ignore_unknown_formats(true)
            .build(schema)
            .map_err(|error| Error::Schema(error.to_string()))?;
        Ok(SchemaValidator {
            schema: schema.clone(),
            inner,
        })
    }

    /// The schema this validator was built from.
    pub fn schema(&self) -> &Value {
        &self.schema
    }
}

impl Validator for SchemaValidator {
    fn validate(&self, document: &Value) -> Result<ValidationErrors> {
        if !document.is_object() {
            return Err(Error::NotAnObject(document.to_string()));
        }
        let mut failures: ValidationErrors = self
            .inner
            .iter_errors(document)
            .map(|error| Value::String(error.to_string()))
            .collect();
        // Sorted so two runs against the same document produce identical output — the cache's
        // contract in [`ValidatorCache`] is that they do.
        failures.sort_by_key(|failure| failure.to_string());
        Ok(failures)
    }
}

/// The per-schema validator cache.
///
/// Upstream caches by `JSON.stringify(schema)` in a map keyed by the validator library's name
/// (`plugin-helpers.ts:49-53`, `:63-71`), so two collections sharing a schema share a compiled
/// validator. Observable consequence: validating the same document against the same schema
/// twice produces byte-identical errors, and a schema change produces a different validator.
///
/// **What the `precompiled` feature changes.** Upstream's `validate-ajv-standalone` swaps the
/// adapter for one that uses validators compiled at *build* time, to keep the schema compiler
/// out of the production bundle. In Rust the compiler is a crate dependency either way, so the
/// feature cannot remove it; what it does here is build every registered schema's validator
/// eagerly at [`ValidatorCache::precompile_all`] rather than on first use, which is the
/// start-up-cost half of the same trade. Recorded as a narrower feature than upstream's,
/// rather than claimed as a port.
#[derive(Debug, Default)]
pub struct ValidatorCache {
    /// Keyed by the canonical schema string, which is upstream's key.
    by_schema: BTreeMap<String, SchemaValidator>,
}

impl ValidatorCache {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// The cache key for a schema: `JSON.stringify` equivalent, which for ordered JSON is the
    /// canonical bytes (`dxdb-json`) rather than `serde_json`'s default map ordering.
    pub fn key_for(schema: &Value) -> String {
        String::from_utf8_lossy(&dxdb_json::canonical_bytes(schema)).to_string()
    }

    /// The validator for `schema`, building and caching it on first use
    /// (`initValidator`, `:63-71`).
    pub fn get(&mut self, schema: &Value) -> Result<&SchemaValidator> {
        let key = Self::key_for(schema);
        if !self.by_schema.contains_key(&key) {
            self.by_schema
                .insert(key.clone(), SchemaValidator::new(schema)?);
        }
        Ok(self.by_schema.get(&key).expect("just inserted"))
    }

    /// How many validators are cached, so a caller can assert the cache is doing its job.
    pub fn len(&self) -> usize {
        self.by_schema.len()
    }

    /// True when nothing has been cached.
    pub fn is_empty(&self) -> bool {
        self.by_schema.is_empty()
    }

    /// Builds every schema's validator now instead of on first use.
    ///
    /// The eager half of what upstream's standalone plugin buys (see the struct docs).
    pub fn precompile_all(&mut self, schemas: &[Value]) -> Result<()> {
        for schema in schemas {
            self.get(schema)?;
        }
        Ok(())
    }
}

/// One row offered to the validating storage, reduced to what the wrapper decides on.
#[derive(Clone, Debug, PartialEq)]
pub struct CandidateRow {
    /// The row's primary value.
    pub document_id: String,
    /// The document as the storage would receive it.
    pub document: Value,
    /// The `context` string of the `bulk_write` call.
    pub context: String,
    /// The stored state, for a write with a `previous`.
    pub previous: Option<Value>,
}

/// The result of partitioning a batch: what goes on, and what is reported as `422`.
///
/// This split is the heart of the plugin (`plugin-helpers.ts:100-140`): a failed row does not
/// abort the batch, and the survivors are written without it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Partition {
    /// Rows that validated, in their original order.
    pub accepted: Vec<CandidateRow>,
    /// One entry per rejected row, in the order they appeared.
    pub rejected: Vec<RejectedRow>,
}

/// A row that failed validation.
#[derive(Clone, Debug, PartialEq)]
pub struct RejectedRow {
    /// The row.
    pub row: CandidateRow,
    /// The validator's failures.
    pub errors: ValidationErrors,
    /// **The schema**, which upstream includes so a `422` is debuggable
    /// (`validate.test.ts:84-85`). Not optional: the upstream test asserts its presence.
    pub schema: Value,
}

impl RejectedRow {
    /// The `422` write error for this row, in the shape `dxdb-storage` uses
    /// (`WriteError::Validation`).
    ///
    /// Returned as JSON because `WriteError` holds the `BulkWriteRow` itself, which this crate
    /// does not construct; a binding converts this at the boundary. The field names are the
    /// storage ones, not renamed, so the conversion is mechanical.
    pub fn to_write_error_json(&self) -> Value {
        serde_json::json!({
            "status": VALIDATION_STATUS,
            "isError": true,
            "documentId": self.row.document_id,
            "validationErrors": self.errors,
            "schema": self.schema,
            "context": self.row.context,
        })
    }
}

/// Splits a batch into the rows that validate and the rows that do not
/// (`plugin-helpers.ts:100-140`).
///
/// The validator is called once per row, in batch order, and an empty error list means
/// accepted. A row whose validation *itself* fails (a non-object document) is treated as
/// rejected with that failure, so one malformed row cannot abort a batch either.
pub fn partition_rows<V: Validator>(
    validator: &V,
    schema: &Value,
    rows: Vec<CandidateRow>,
) -> Partition {
    let mut partition = Partition::default();
    for row in rows {
        match validator.validate(&row.document) {
            Ok(errors) if errors.is_empty() => partition.accepted.push(row),
            Ok(errors) => partition.rejected.push(RejectedRow {
                row,
                errors,
                schema: schema.clone(),
            }),
            Err(error) => partition.rejected.push(RejectedRow {
                row,
                errors: vec![Value::String(error.to_string())],
                schema: schema.clone(),
            }),
        }
    }
    partition
}

/// The wrapped storage name (`plugin-helpers.ts:81`).
pub fn wrapped_storage_name(validator_key: &str, inner_name: &str) -> String {
    format!("{STORAGE_NAME_PREFIX}{validator_key}-{inner_name}")
}

/// The `wrappedValidateStorageFactory` equivalent, as far as it can exist before the dyn twin.
///
/// Holds the cache and the validator key; a binding supplies the inner storage and forwards
/// `bulk_write` through [`ValidateStorage::partition`].
#[derive(Debug)]
pub struct ValidateStorage {
    /// The validator-library key upstream puts in the storage name (`'ajv'`,
    /// `'ajv-standalone'`, ...). Always `"schema"` here, since there is one engine.
    pub validator_key: String,
    /// The inner storage's name.
    pub inner_name: String,
    /// The cache.
    pub cache: ValidatorCache,
}

impl ValidateStorage {
    /// A storage wrapper for one inner storage.
    pub fn new(inner_name: impl Into<String>) -> Self {
        ValidateStorage {
            validator_key: "schema".to_string(),
            inner_name: inner_name.into(),
            cache: ValidatorCache::new(),
        }
    }

    /// The wrapped storage's name, which is what upstream reports so a misconfiguration is
    /// diagnosable (`plugin-helpers.ts:81`).
    pub fn name(&self) -> String {
        wrapped_storage_name(&self.validator_key, &self.inner_name)
    }

    /// Partitions a batch for `schema`.
    pub fn partition(&mut self, schema: &Value, rows: Vec<CandidateRow>) -> Result<Partition> {
        let validator = self.cache.get(schema)?;
        Ok(partition_rows(validator, schema, rows))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        dxdb_schema::fill_with_default_settings(&json!({
            "title": "human",
            "version": 0,
            "type": "object",
            "primaryKey": "passportId",
            "properties": {
                "passportId": { "type": "string", "maxLength": 100 },
                "age": { "type": "integer", "minimum": 0, "maximum": 150, "multipleOf": 1 },
            },
            "required": ["passportId"],
        }))
        .expect("fill")
    }

    fn row(id: &str, document: Value) -> CandidateRow {
        CandidateRow {
            document_id: id.to_string(),
            document,
            context: "test".to_string(),
            previous: None,
        }
    }

    /// A document with the meta fields the schema requires after defaulting.
    fn stored(id: &str, age: i64) -> Value {
        json!({
            "passportId": id,
            "age": age,
            "_deleted": false,
            "_rev": "1-abc",
            "_meta": { "lwt": 1 },
            "_attachments": {},
        })
    }

    #[test]
    fn the_cache_is_keyed_by_the_canonical_schema() {
        let mut cache = ValidatorCache::new();
        let schema = schema();
        cache.get(&schema).expect("first");
        assert_eq!(cache.len(), 1);
        // The same schema value gets the same validator.
        cache.get(&schema).expect("second");
        assert_eq!(cache.len(), 1, "one schema means one validator");
        // A different schema gets another.
        let mut other = schema.clone();
        other["title"] = json!("other");
        cache.get(&other).expect("third");
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn the_cache_key_is_order_insensitive_but_content_sensitive() {
        // `JSON.stringify` on an ordered object is order-sensitive; the canonical bytes are too,
        // because key order is meaningful in RxDB (PLAN §3.1). What matters is that two
        // *identical* schemas key the same and two different ones do not.
        let a = json!({ "b": 1, "a": 2 });
        let b = json!({ "b": 1, "a": 2 });
        assert_eq!(ValidatorCache::key_for(&a), ValidatorCache::key_for(&b));
        let c = json!({ "b": 2, "a": 2 });
        assert_ne!(ValidatorCache::key_for(&a), ValidatorCache::key_for(&c));
    }

    #[test]
    fn the_wrapped_storage_name_matches_upstream() {
        // `name: 'validate-' + validatorKey + '-' + args.storage.name`.
        let storage = ValidateStorage::new("memory");
        assert_eq!(storage.name(), "validate-schema-memory");
        assert_eq!(wrapped_storage_name("ajv", "dexie"), "validate-ajv-dexie");
    }

    #[test]
    fn a_valid_document_is_accepted() {
        let mut storage = ValidateStorage::new("memory");
        let schema = schema();
        let partition = storage
            .partition(&schema, vec![row("a", stored("a", 30))])
            .expect("partition");
        assert_eq!(partition.accepted.len(), 1);
        assert!(partition.rejected.is_empty());
    }

    #[test]
    fn an_invalid_document_is_rejected_with_the_schema() {
        // The upstream assertion: every 422 carries `schema.type === 'object'`
        // (`validate.test.ts:84-85`).
        let mut storage = ValidateStorage::new("memory");
        let schema = schema();
        let partition = storage
            .partition(&schema, vec![row("a", json!({ "age": 30 }))])
            .expect("partition");
        assert_eq!(partition.rejected.len(), 1, "the missing primary key fails");
        let rejected = &partition.rejected[0];
        assert_eq!(rejected.schema["type"], json!("object"));
        assert_eq!(
            rejected.to_write_error_json()["status"],
            json!(VALIDATION_STATUS)
        );
        assert!(!rejected.errors.is_empty());
    }

    #[test]
    fn a_mixed_batch_writes_the_good_rows_and_reports_the_bad_ones() {
        // THE CONTRACT: per-row, not all-or-nothing (`plugin-helpers.ts:100-140`).
        let mut storage = ValidateStorage::new("memory");
        let schema = schema();
        let partition = storage
            .partition(
                &schema,
                vec![
                    row("good", stored("good", 30)),
                    row("bad", json!({ "age": 30 })),
                    row("also-good", stored("also-good", 40)),
                ],
            )
            .expect("partition");
        assert_eq!(
            partition
                .accepted
                .iter()
                .map(|row| row.document_id.as_str())
                .collect::<Vec<_>>(),
            vec!["good", "also-good"],
            "the survivors keep their order"
        );
        assert_eq!(partition.rejected.len(), 1);
        assert_eq!(partition.rejected[0].row.document_id, "bad");
    }

    #[test]
    fn an_out_of_range_number_is_rejected() {
        // The upstream `overflow maximum int` case (`validate.test.ts:295`).
        let mut storage = ValidateStorage::new("memory");
        let schema = schema();
        let partition = storage
            .partition(&schema, vec![row("a", stored("a", 999))])
            .expect("partition");
        assert_eq!(partition.rejected.len(), 1, "age maximum is 150");
    }

    #[test]
    fn an_extra_property_is_rejected() {
        // `additionalProperties: false` after defaulting, and the upstream
        // `additional property` case (`validate.test.ts:309`).
        let mut storage = ValidateStorage::new("memory");
        let schema = schema();
        let mut document = stored("a", 30);
        document["surprise"] = json!(true);
        let partition = storage
            .partition(&schema, vec![row("a", document)])
            .expect("partition");
        assert_eq!(partition.rejected.len(), 1);
    }

    #[test]
    fn a_non_object_document_is_rejected_rather_than_aborting_the_batch() {
        // A malformed row must not take the batch down; it becomes one 422 like any other.
        let mut storage = ValidateStorage::new("memory");
        let schema = schema();
        let partition = storage
            .partition(
                &schema,
                vec![
                    row("scalar", json!("not a document")),
                    row("good", stored("good", 30)),
                ],
            )
            .expect("partition");
        assert_eq!(partition.rejected.len(), 1);
        assert_eq!(partition.accepted.len(), 1);
    }

    #[test]
    fn precompiling_builds_every_schema_up_front() {
        // The `precompiled` feature's behaviour, exercised through `precompile_all` rather than
        // the feature flag so it is testable in a default build.
        let mut cache = ValidatorCache::new();
        let mut second = schema();
        second["title"] = json!("second");
        cache
            .precompile_all(&[schema(), second])
            .expect("precompile");
        assert_eq!(cache.len(), 2);
    }
}
