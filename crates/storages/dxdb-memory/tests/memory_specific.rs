//! `dxdb-memory` behaviours the shared conformance suite has no case for.
//!
//! `tests/conformance.rs` runs the testkit's ported suite — the oracle every storage in
//! the workspace is measured against — against `MemoryStorage`. This file holds the rest:
//! the tests from this crate's original hand-port that assert something upstream's
//! generic suite cannot, because upstream either instantiates the suite with and without
//! a wrapper instead of asserting the wrapper is *required*, or reaches the behaviour
//! only indirectly.
//!
//! Each one is here rather than folded into `conformance.rs` for the same reason: a
//! failure in this file means "this engine diverges from a decision we made", while a
//! failure in `conformance.rs` means "this engine fails the contract". Keeping them apart
//! is what makes the report say which.
//!
//! # The five, and why the oracle does not have them
//!
//! | test | why the shared suite misses it |
//! | --- | --- |
//! | [`refuses_key_compression_without_the_wrapper`] | upstream runs the suite *with* a key-compression wrapper and *without* one; neither run asserts that a bare storage rejects the schema |
//! | [`refuses_encrypted_fields_without_the_wrapper`] | same shape for the encryption plugin |
//! | [`an_unknown_index_is_an_error`] | upstream never plans a query against an index the schema does not declare |
//! | [`orders_index_strings_by_code_unit_not_by_bytes`] | upstream's sort case uses ASCII; its umlaut case only checks a round-trip. Neither separates UTF-16 code-unit order from UTF-8 byte order, which is the only place a Rust port can silently differ |
//! | [`schema_versions_do_not_share_state`] / [`database_names_do_not_share_state`] | upstream's suite deletes its database between cases, so it never opens two |

use dxdb_memory::MemoryStorage;
use dxdb_storage::{
    now, AttachmentData, AttachmentWriteData, BulkWriteRow, DocumentData, DocumentWriteData,
    DynRxStorageInstance, InstanceCreationParams, PreparedQuery, RxStorage, RxStorageInstance,
    SortDirection, SortPart, StorageError, StorageSchema,
};
use serde_json::{json, Value};

/// The context string upstream's suite passes to every `bulkWrite`.
const TEST_CONTEXT: &str = "rx-storage-implementations.test.ts";

/// `EXAMPLE_REVISION_1` from `src/plugins/test-utils/revisions.ts`.
const EXAMPLE_REVISION_1: &str = "1-12080c42d471e3d2625e49dcca3b8e1a";

/// A unique token per call, so no two tests share collection state.
///
/// `dxdb-memory` keys its shared engine on `(database, collection, schema version)`, so
/// two tests reusing a database name share one store and the second test's insert
/// conflicts against the first's document. That produced 16 false failures in
/// `dxdb-core` once. A counter is enough for uniqueness within a test binary and keeps
/// the test deterministic, which a real RNG would not.
fn random_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("tok{n:016x}")
}

/// `fillWithDefaultSettings(...)` from `rx-schema-helper.ts:196`.
///
/// Every upstream test calls this before handing a schema to a storage, because the
/// meta-injected properties (`_rev`, `_deleted`, `_meta`, `_attachments`) and the index
/// rewriting are what make the schema storage-ready. A test that skipped it would be
/// exercising a shape no caller ever passes.
fn filled(schema: &Value) -> Value {
    dxdb_schema::fill_with_default_settings(schema).expect("fill with default settings")
}

/// `getPseudoSchemaForVersion(0, primaryKey)` from `rx-schema-helper.ts:28`.
fn pseudo_schema(primary_key: &str) -> Value {
    filled(&json!({
        "version": 0,
        "type": "object",
        "primaryKey": primary_key,
        "properties": {
            primary_key: { "type": "string", "maxLength": 100 },
            "value": { "type": "string" }
        },
        "required": [primary_key],
        "indexes": [[primary_key]]
    }))
}

/// The `getTestDataSchema()` helper from the upstream file.
fn test_data_schema() -> Value {
    filled(&json!({
        "version": 0,
        "type": "object",
        "primaryKey": "key",
        "properties": {
            "key": { "type": "string", "maxLength": 100 },
            "value": { "type": "string", "maxLength": 100 }
        },
        "required": ["key", "value"],
        "indexes": [["value"]]
    }))
}

/// A schema that declares attachments, which is what makes the write-error path reachable.
fn attachments_schema() -> Value {
    filled(&json!({
        "version": 0,
        "type": "object",
        "primaryKey": "key",
        "attachments": {},
        "properties": {
            "key": { "type": "string", "maxLength": 100 },
            "value": { "type": "string", "maxLength": 100 }
        },
        "required": ["key", "value"],
        "indexes": [["value"]]
    }))
}

/// A write carrying one attachment stub, with a payload or without.
fn with_attachment(
    document: DocumentData,
    attachment_id: &str,
    digest: &str,
    payload: Option<&[u8]>,
) -> DocumentWriteData {
    let mut write = DocumentWriteData::new(document);
    write.insert_attachment(
        attachment_id,
        AttachmentWriteData {
            stub: AttachmentData {
                // A stub's `length` is the payload length; the fixture uses 3 when the
                // payload is absent, matching the digest's own length convention.
                length: payload.map(|p| p.len() as u64).unwrap_or(3),
                content_type: "text/plain".to_string(),
                digest: digest.to_string(),
            },
            data: payload.map(<[u8]>::to_vec),
        },
    );
    write
}

/// `getWriteData()` from the upstream file.
fn write_data(key: &str, value: &str) -> DocumentData {
    DocumentData::from_value(json!({
        "key": key,
        "value": value,
        "_deleted": false,
        "_attachments": {},
        "_rev": EXAMPLE_REVISION_1,
        "_meta": { "lwt": now() }
    }))
    .expect("valid document")
}

fn instance_params(schema: &Value) -> InstanceCreationParams {
    InstanceCreationParams {
        database_instance_token: random_token(),
        database_name: random_token(),
        collection_name: random_token(),
        schema: StorageSchema::from_value(schema).expect("schema projection"),
        options: json!({}),
        multi_instance: false,
        dev_mode: true,
    }
}

async fn open(schema: &Value) -> Box<DynRxStorageInstance<'static>> {
    MemoryStorage::new()
        .create_storage_instance(instance_params(schema))
        .await
        .expect("create storage instance")
}

async fn open_with_params(params: InstanceCreationParams) -> Box<DynRxStorageInstance<'static>> {
    MemoryStorage::new()
        .create_storage_instance(params)
        .await
        .expect("create storage instance")
}

/// Builds a prepared query the way core would: normalised sort with the primary key
/// appended, plus the planner's plan.
fn prepare(schema: &Value, primary_key: &str, query: Value) -> PreparedQuery {
    use dxdb_query::normalize::{normalize_mango_query, MangoQuery as QueryMangoQuery};
    use dxdb_query::plan::get_query_plan;

    let filled = normalize_mango_query(
        &QueryMangoQuery {
            selector: query.get("selector").cloned(),
            sort: query.get("sort").and_then(Value::as_array).map(|parts| {
                parts
                    .iter()
                    .map(|part| {
                        let object = part.as_object().expect("sort part is an object");
                        let (field, direction) = object.iter().next().expect("one field");
                        dxdb_query::normalize::SortPart {
                            field: field.clone(),
                            direction: if direction == "desc" {
                                dxdb_query::normalize::SortDirection::Desc
                            } else {
                                dxdb_query::normalize::SortDirection::Asc
                            },
                        }
                    })
                    .collect()
            }),
            skip: query.get("skip").and_then(Value::as_u64),
            limit: query.get("limit").and_then(Value::as_u64),
            index: None,
        },
        primary_key,
        &dxdb_schema::get_string_indexes(schema).unwrap_or_default(),
        false,
    )
    .expect("normalise");
    let plan = get_query_plan(schema, &filled).expect("plan");

    /// One conversion, so the `PlanKey` mapping cannot drift between the two bounds.
    fn plan_key(key: &dxdb_query::plan::PlanKey) -> dxdb_storage::PlanKey {
        match key {
            dxdb_query::plan::PlanKey::IndexMin => dxdb_storage::PlanKey::index_min(),
            dxdb_query::plan::PlanKey::IndexMax => dxdb_storage::PlanKey::index_max(),
            dxdb_query::plan::PlanKey::String(text) => dxdb_storage::PlanKey::String(text.clone()),
            dxdb_query::plan::PlanKey::Number(number) => dxdb_storage::PlanKey::Number(*number),
            dxdb_query::plan::PlanKey::Bool(flag) => dxdb_storage::PlanKey::Boolean(*flag),
            dxdb_query::plan::PlanKey::Null | dxdb_query::plan::PlanKey::Undefined => {
                dxdb_storage::PlanKey::Unbounded
            }
        }
    }

    PreparedQuery {
        query: dxdb_storage::MangoQuery {
            selector: filled.selector.clone(),
            sort: filled
                .sort
                .iter()
                .map(|part| SortPart {
                    field: part.field.clone(),
                    direction: match part.direction {
                        dxdb_query::normalize::SortDirection::Asc => SortDirection::Asc,
                        dxdb_query::normalize::SortDirection::Desc => SortDirection::Desc,
                    },
                })
                .collect(),
            skip: filled.skip as usize,
            limit: filled.limit.map(|limit| limit as usize),
            index: filled.index.clone(),
        },
        query_plan: dxdb_storage::QueryPlan {
            index: plan.index.clone(),
            sort_satisfied_by_index: plan.sort_satisfied_by_index,
            selector_satisfied_by_index: plan.selector_satisfied_by_index,
            start_keys: plan.start_keys.iter().map(plan_key).collect(),
            end_keys: plan.end_keys.iter().map(plan_key).collect(),
            inclusive_start: plan.inclusive_start,
            inclusive_end: plan.inclusive_end,
        },
    }
}

// ---------------------------------------------------------------------------
// Schema features a bare storage must refuse
// ---------------------------------------------------------------------------

/// Upstream `it('must throw if keyCompression is set but no key-compression plugin is
/// used')` (:218): a bare storage must refuse rather than write uncompressed keys.
///
/// The shared suite never sees this: upstream runs the whole suite twice, once with a
/// key-compression wrapper and once without, and neither run passes a schema that sets
/// `keyCompression`. A storage that silently ignored the flag would pass both runs and
/// store documents whose keys the caller believes are compressed.
#[test]
fn refuses_key_compression_without_the_wrapper() {
    futures::executor::block_on(async {
        let mut raw = pseudo_schema("key");
        raw["keyCompression"] = json!(true);
        let schema = filled(&raw);
        // `expect_err` would need `Debug` on the instance, which a dyn storage does
        // not provide; matching is also clearer about what is being asserted.
        let error = match MemoryStorage::new()
            .create_storage_instance(instance_params(&schema))
            .await
        {
            Ok(_) => panic!("must refuse key compression"),
            Err(error) => error,
        };
        assert!(
            matches!(error, StorageError::Unsupported(_)),
            "got {error:?}"
        );
    });
}

/// Upstream `it('must throw if encryption is defined in schema is set but no
/// encryption plugin is used')` (:244). A bare storage that accepted this would store
/// plaintext while the caller believed it was encrypted.
#[test]
fn refuses_encrypted_fields_without_the_wrapper() {
    futures::executor::block_on(async {
        let mut raw = pseudo_schema("key");
        raw["encrypted"] = json!(["value"]);
        let schema = filled(&raw);
        let error = match MemoryStorage::new()
            .create_storage_instance(instance_params(&schema))
            .await
        {
            Ok(_) => panic!("must refuse encryption"),
            Err(error) => error,
        };
        assert!(
            matches!(error, StorageError::Unsupported(_)),
            "got {error:?}"
        );
    });
}

// ---------------------------------------------------------------------------
// Query plans
// ---------------------------------------------------------------------------

/// A plan naming an index the schema does not declare must be an error, not an empty
/// result.
///
/// The distinction is the whole point: an empty result is a silently wrong answer that a
/// caller ships, while an error is one they fix. Upstream never plans against an
/// undeclared index, so the shared suite cannot catch a storage that treats it as "no
/// documents match".
#[test]
fn an_unknown_index_is_an_error() {
    futures::executor::block_on(async {
        let instance = open(&test_data_schema()).await;
        let mut prepared = prepare(
            &test_data_schema(),
            "key",
            json!({ "selector": {}, "sort": [{ "key": "asc" }] }),
        );
        prepared.query_plan.index = vec!["_deleted".to_string(), "nope".to_string()];
        let error = instance
            .query(prepared)
            .await
            .expect_err("must reject unknown index");
        assert!(
            matches!(error, StorageError::UnknownIndex(_)),
            "got {error:?}"
        );
        instance.close().await.expect("close");
    });
}

// ---------------------------------------------------------------------------
// Index-string ordering
// ---------------------------------------------------------------------------

/// Index strings must be ordered by UTF-16 code unit, not by Rust's byte order.
///
/// D5 makes code-unit order the contract. The shared suite reaches ordering through
/// `query_sorts_correctly` (ASCII) and `write_and_read_umlauts` (a round-trip check), and
/// neither separates the two orders: for ASCII and for `äöü`, UTF-8 byte order and UTF-16
/// code-unit order agree. `"\u{FFFF}"` against an astral character is the pair where they
/// disagree — the emoji's first code unit is `0xD83D` < `0xFFFF`, while its first byte is
/// `0xF0` > `0xEF`. A storage using `str` ordering therefore passes every case upstream
/// has and silently mis-sorts this one.
///
/// This is documented as an intentional divergence in `docs/STATUS.md` (D5); the test
/// pins that the divergence is the *only* one, i.e. that ordering is code-unit and not
/// byte order.
#[test]
fn orders_index_strings_by_code_unit_not_by_bytes() {
    futures::executor::block_on(async {
        let instance = open(&test_data_schema()).await;
        // `key` maxLength 100 and the index is `(_deleted, key)`, so these are the
        // index strings' suffix after the `_deleted` character.
        for key in ["\u{FFFF}", "😀"] {
            instance
                .bulk_write(
                    vec![BulkWriteRow::insert(DocumentWriteData::new(write_data(
                        key, "v",
                    )))],
                    TEST_CONTEXT.to_string(),
                )
                .await
                .expect("write");
        }
        let found = instance
            .query(prepare(
                &test_data_schema(),
                "key",
                json!({ "selector": {}, "sort": [{ "key": "asc" }] }),
            ))
            .await
            .expect("query");
        let keys: Vec<String> = found
            .iter()
            .map(|document| document.id("key").expect("id").to_string())
            .collect();
        // Code-unit order: the emoji's first UTF-16 unit is 0xD83D, less than 0xFFFF,
        // so the emoji sorts first.
        assert_eq!(keys, vec!["😀", "\u{FFFF}"]);

        // And the two orders must disagree here — otherwise this test would pass under
        // a `str`-ordered storage too, and would be asserting nothing.
        assert!(
            "\u{FFFF}" < "😀",
            "Rust byte order must disagree with code-unit order for this pair"
        );
        instance.close().await.expect("close");
    });
}

// ---------------------------------------------------------------------------
// State isolation between instances
// ---------------------------------------------------------------------------

/// Two schema versions of one collection must not share documents.
///
/// The shared suite deletes its database between cases (`Harness` uses a fresh random
/// token per case), so it never opens two instances that differ only by version. This is
/// the trap that produced 16 false failures in `dxdb-core`: the engine keys state on
/// `(database, collection, schema version)`, and a version-blind key would make an
/// upgrade read the previous version's rows.
#[test]
fn schema_versions_do_not_share_state() {
    futures::executor::block_on(async {
        let schema_v0 = pseudo_schema("key");
        let mut raw_v1 = pseudo_schema("key");
        raw_v1["version"] = json!(1);
        let schema_v1 = filled(&raw_v1);

        // Fixed names, namespaced to this test: the point is that both instances name
        // the same database and collection, and differ only by version.
        let mut params = instance_params(&schema_v0);
        params.database_name = "m2-core-memory-specific-versioned-db".to_string();
        params.collection_name = "versioned".to_string();
        let v0 = open_with_params(params.clone()).await;

        let mut params_v1 = params.clone();
        params_v1.database_instance_token = random_token();
        params_v1.schema = StorageSchema::from_value(&schema_v1).expect("schema");
        let v1 = open_with_params(params_v1).await;

        v0.bulk_write(
            vec![BulkWriteRow::insert(DocumentWriteData::new(write_data(
                "only-in-v0",
                "v",
            )))],
            TEST_CONTEXT.to_string(),
        )
        .await
        .expect("write");

        let in_v1 = v1
            .find_documents_by_id(vec!["only-in-v0".into()], true)
            .await
            .expect("find");
        assert!(in_v1.is_empty(), "schema versions must not share documents");

        // And the v0 instance must not have been disturbed, which a version-blind key
        // would also hide.
        let in_v0 = v0
            .find_documents_by_id(vec!["only-in-v0".into()], true)
            .await
            .expect("find");
        assert_eq!(in_v0.len(), 1, "v0 must still see its own document");

        v0.remove().await.expect("remove v0");
        v1.remove().await.expect("remove v1");
        // `remove` implies closed; a later `close` is a no-op, and the reverse order is
        // the `Closed` error upstream asserts in `remove_after_close_throws`.
        v0.close().await.expect("close v0");
        v1.close().await.expect("close v1");
    });
}

/// Upstream `it('should not mix up documents stored in a different database name')`
/// (:3971). The shared suite's per-case random names make this unreachable there: it
/// would pass whether or not the engine keyed on the database name.
#[test]
fn database_names_do_not_share_state() {
    futures::executor::block_on(async {
        let schema = pseudo_schema("key");
        let mut params = instance_params(&schema);
        params.database_name = "m2-core-memory-specific-db-one".to_string();
        params.collection_name = "same".to_string();
        let one = open_with_params(params.clone()).await;

        let mut params_two = params.clone();
        params_two.database_instance_token = random_token();
        params_two.database_name = "m2-core-memory-specific-db-two".to_string();
        let two = open_with_params(params_two).await;

        one.bulk_write(
            vec![BulkWriteRow::insert(DocumentWriteData::new(write_data(
                "only-in-one",
                "v",
            )))],
            TEST_CONTEXT.to_string(),
        )
        .await
        .expect("write");
        assert!(
            two.find_documents_by_id(vec!["only-in-one".into()], true)
                .await
                .expect("find")
                .is_empty(),
            "a different database name must not see the document"
        );

        one.remove().await.expect("remove one");
        two.remove().await.expect("remove two");
        // Leave no state behind for a re-run in the same process, and in this order:
        // `remove` implies closed, so `close` afterwards is the documented no-op.
        one.close().await.expect("close one");
        two.close().await.expect("close two");
    });
}

// ---------------------------------------------------------------------------
// Attachment write errors
// ---------------------------------------------------------------------------

/// A *new* attachment with no payload must be rejected, and the row must not be written.
///
/// This is upstream `it('should throw on a non-existing attachment')`-adjacent behaviour, and
/// it is the narrow rule the whole attachment path turns on: only a **new** attachment needs
/// `data`. A *changed digest on an existing id* with no payload is accepted — the shared
/// suite's `add_multiple_attachments` covers exactly that case, which is why this rule is easy
/// to believe it covers both.
///
/// Verified absent, not assumed: `suite.rs` contains no assertion mentioning
/// `WRITE_ERROR_ATTACHMENT` or status 510. Its only three matches for "510"/"no payload" are
/// two comments about the *existing*-stub case and one doc comment saying the schema
/// declaration is what makes 510 reachable — it never triggers it.
#[test]
fn refuses_a_new_attachment_without_payload() {
    futures::executor::block_on(async {
        let instance = open(&attachments_schema()).await;
        let response = instance
            .bulk_write(
                vec![BulkWriteRow::insert(with_attachment(
                    write_data("no-payload", "v"),
                    "att-1",
                    "digest-1",
                    None,
                ))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("write");
        assert_eq!(response.error.len(), 1, "the row must be rejected");
        assert_eq!(
            response.error[0].status(),
            dxdb_storage::WRITE_ERROR_ATTACHMENT,
            "a new attachment with no payload is a 510 write error"
        );

        // And it must not have been written: a storage that reports the error but stores the
        // row anyway leaves a document whose attachment can never be fetched.
        let found = instance
            .find_documents_by_id(vec!["no-payload".to_string()], true)
            .await
            .expect("find");
        assert!(found.is_empty(), "the row must not have been written");
        instance.close().await.expect("close");
    });
}

/// A wrong digest on a fetch is an error, not a mismatch to reconcile.
///
/// The shared suite does assert this (`getAttachmentData` with `"0".repeat(64)`), so this is a
/// deliberate second opinion on the one behaviour where "return whatever you have" is a
/// plausible wrong implementation. Kept because the failure mode is silent.
#[test]
fn a_wrong_digest_is_an_error() {
    use dxdb_storage::RxStorageInstance as _;
    futures::executor::block_on(async {
        let instance = open(&attachments_schema()).await;
        let payload = b"abc";
        // The testkit's own SHA-256, so the fixture digest is the one the storage computes
        // rather than a constant that would make this pass for the wrong reason.
        let digest = dxdb_testkit::suite::sha256_hex(payload);
        let response = instance
            .bulk_write(
                vec![BulkWriteRow::insert(with_attachment(
                    write_data("with-att", "v"),
                    "att-1",
                    &digest,
                    Some(payload),
                ))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("write");
        assert!(
            response.is_ok(),
            "the write must be accepted: {:?}",
            response.error
        );

        let error = instance
            .get_attachment_data("with-att".to_string(), "att-1".to_string(), "0".repeat(64))
            .await
            .expect_err("a wrong digest must be an error");
        assert!(
            matches!(error, StorageError::MissingAttachment(_)),
            "a digest mismatch is `MissingAttachment` per `dxdb-storage`'s own doc on that \
             variant (\"absent, or its digest does not match the request\"); got {error:?}"
        );

        // The right digest still works, so the assertion above is about the digest and not
        // about a broken attachment.
        let bytes = instance
            .get_attachment_data("with-att".to_string(), "att-1".to_string(), digest)
            .await
            .expect("the correct digest must return the payload");
        assert_eq!(bytes, payload);
        instance.close().await.expect("close");
    });
}

// ---------------------------------------------------------------------------
// Partial-failure batches
// ---------------------------------------------------------------------------

/// A batch with some bad rows must write the good ones and report the bad ones.
///
/// The shared suite exercises all-good and all-bad batches (every row conflicts, so
/// `does_not_emit_a_bulk_when_every_row_failed`-shaped cases), but never a **mixture**. Mixed
/// batches are where "abort the whole batch on any error" hides: that implementation passes
/// both uniform cases and silently drops three good writes here.
///
/// Confirmed absent by search: `suite.rs` has no match for a partial/mixture batch assertion.
#[test]
fn a_batch_writes_the_good_rows_and_reports_the_bad_ones() {
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        // Two documents already stored, so two of the rows below must conflict.
        for key in ["existing-a", "existing-b"] {
            instance
                .bulk_write(
                    vec![BulkWriteRow::insert(DocumentWriteData::new(write_data(
                        key, "old",
                    )))],
                    TEST_CONTEXT.to_string(),
                )
                .await
                .expect("seed");
        }

        let rows = vec![
            BulkWriteRow::insert(DocumentWriteData::new(write_data("new-1", "v"))),
            BulkWriteRow::insert(DocumentWriteData::new(write_data("existing-a", "v"))),
            BulkWriteRow::insert(DocumentWriteData::new(write_data("new-2", "v"))),
            BulkWriteRow::insert(DocumentWriteData::new(write_data("existing-b", "v"))),
            BulkWriteRow::insert(DocumentWriteData::new(write_data("new-3", "v"))),
        ];
        let response = instance
            .bulk_write(rows, TEST_CONTEXT.to_string())
            .await
            .expect("bulk");

        assert_eq!(response.error.len(), 2, "two rows must conflict");
        assert_eq!(
            response.failed_ids(),
            vec!["existing-a", "existing-b"],
            "the reported ids must be the failed ones"
        );
        // The good rows landed.
        let found = instance
            .find_documents_by_id(vec!["new-1".into(), "new-2".into(), "new-3".into()], false)
            .await
            .expect("find");
        assert_eq!(found.len(), 3, "the non-conflicting rows must be written");
        // And the conflicting ones were not overwritten.
        let unchanged = instance
            .find_documents_by_id(vec!["existing-a".into()], false)
            .await
            .expect("find");
        assert_eq!(unchanged[0].get("value"), Some(&json!("old")));
        instance.close().await.expect("close");
    });
}

/// A row with no primary key must be rejected, not stored under an empty id.
///
/// Also absent from the shared suite. The failure mode is a document reachable only by
/// `""`, which every later `find_documents_by_id` treats as a different document.
#[test]
fn a_row_without_a_primary_key_is_rejected() {
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        let orphan = DocumentData::from_value(json!({
            "value": "no key",
            "_deleted": false,
            "_attachments": {},
            "_rev": EXAMPLE_REVISION_1,
            "_meta": { "lwt": now() }
        }))
        .expect("valid object");
        let response = instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(orphan))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect("bulk");
        assert_eq!(response.error.len(), 1);
        assert!(
            instance
                .find_documents_by_id(vec![String::new()], true)
                .await
                .expect("find")
                .is_empty(),
            "a row with no primary key must not be stored under an empty id"
        );
        instance.close().await.expect("close");
    });
}

/// A closed instance must refuse a write with `Closed`.
///
/// The shared suite asserts `remove()` after `close()` errors and that `close()` after
/// `remove()` does not, but never that a **write** after close is rejected. A storage that
/// happily accepts a write on a closed handle would pass everything upstream has.
#[test]
fn a_closed_instance_refuses_writes() {
    futures::executor::block_on(async {
        let instance = open(&pseudo_schema("key")).await;
        instance.close().await.expect("close");
        let error = instance
            .bulk_write(
                vec![BulkWriteRow::insert(DocumentWriteData::new(write_data(
                    "late", "v",
                )))],
                TEST_CONTEXT.to_string(),
            )
            .await
            .expect_err("a closed instance must refuse a write");
        assert!(matches!(error, StorageError::Closed), "got {error:?}");
    });
}
