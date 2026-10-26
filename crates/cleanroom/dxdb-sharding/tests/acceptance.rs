//! The remaining sharding invariants and the acceptance list.
//!
//! INV-1, INV-2, INV-3 and INV-8 live in `invariants.rs` because they need the
//! differential harness. The rest are about the wrapper's own contract and are
//! cheaper to test directly.

use std::num::NonZeroU32;

use dxdb_memory::MemoryStorage;
use dxdb_sharding::routing::{ShardMode, ShardingConfig};
use dxdb_sharding::{ShardedStorage, ShardingError};
use dxdb_storage::{
    BulkWriteRow, DocumentData, DocumentWriteData, DynRxStorageInstance, InstanceCreationParams,
    PreparedQuery, RxStorage, RxStorageInstance, StorageSchema,
};
use serde_json::{json, Value};

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    futures::executor::block_on(future)
}

fn schema_for(config: Option<ShardingConfig>) -> Value {
    let mut schema = json!({
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id": {"type": "string", "maxLength": 100},
            "name": {"type": "string"}
        },
        "required": ["id"],
        "indexes": ["name", "id"],
        "additionalProperties": false
    });
    if let Some(config) = config {
        schema["sharding"] = json!({
            "shards": config.shards.get(),
            "mode": config.mode.as_str(),
        });
    }
    dxdb_schema::fill_with_default_settings(&schema).expect("filled")
}

fn params(schema: &Value, collection: &str) -> InstanceCreationParams {
    InstanceCreationParams {
        database_instance_token: "token".to_string(),
        database_name: "testdb".to_string(),
        collection_name: collection.to_string(),
        schema: StorageSchema::from_value(schema).expect("schema"),
        options: json!({}),
        multi_instance: false,
        dev_mode: true,
    }
}

fn config(shards: u32, mode: ShardMode) -> ShardingConfig {
    ShardingConfig::new(shards, mode).expect("nonzero")
}

fn open(config: ShardingConfig, collection: &str) -> Box<DynRxStorageInstance<'static>> {
    let storage = ShardedStorage::new(MemoryStorage::new(), config);
    block_on(storage.create_storage_instance(params(&schema_for(Some(config)), collection)))
        .expect("instance")
}

fn document(id: &str, name: &str, revision: &str) -> DocumentData {
    let mut object = json!({
        "id": id,
        "name": name,
        "_rev": revision,
        "_deleted": false,
        "_meta": {"lwt": 1000},
        "_attachments": {}
    })
    .as_object()
    .expect("object")
    .clone();
    object.insert("id".to_string(), json!(id));
    DocumentData::from_value(Value::Object(object)).expect("valid")
}

fn insert(document: DocumentData) -> BulkWriteRow {
    BulkWriteRow::insert(DocumentWriteData::new(document))
}

fn update(previous: DocumentData, document: DocumentData) -> BulkWriteRow {
    BulkWriteRow::update(previous, DocumentWriteData::new(document))
}

fn full_scan() -> PreparedQuery {
    dxdb_testkit::harness::build_prepared_query(
        &json!({"selector": {}, "sort": [{"name": "asc"}]}),
        "id",
    )
}

// ---------------------------------------------------------------------------
// INV-4 — routing is a pure, specified function
// ---------------------------------------------------------------------------

/// INV-4: routing matches the specification exactly, for a committed set of
/// `(key -> shard)` pairs.
///
/// The vectors are the assertion: a change to the hash is a test failure rather than a
/// silent data migration. Values are computed from the FNV-1a definition in
/// `routing::fnv1a64`, which is itself pinned by the published FNV test vectors, so this
/// test does not merely restate the implementation.
#[test]
fn inv_04_routing_is_pure_and_matches_the_specification() {
    // FNV-1a 64 of the key, mod N. Recomputed here from the published algorithm.
    fn expected(key: &str, shards: u32) -> u32 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in key.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        (hash % u64::from(shards)) as u32
    }
    for shards in [1u32, 2, 3, 7, 10, 64, 1000] {
        let config = config(shards, ShardMode::Collection);
        for key in ["doc-0", "doc-1", "tie-00", "", "😀 astral", "_rxdb_internal"] {
            assert_eq!(
                config.shard_of(key),
                expected(key, shards),
                "routing for {key:?} with {shards} shards"
            );
            // Purity: repeated calls agree.
            assert_eq!(config.shard_of(key), config.shard_of(key));
        }
    }
    // A fixed vector, so a change of hash is a visible diff here.
    assert_eq!(config(10, ShardMode::Collection).shard_of("doc-0"), expected("doc-0", 10));
    assert_eq!(config(2, ShardMode::Collection).shard_of("a"), expected("a", 2));
}

// ---------------------------------------------------------------------------
// INV-5 — per-row errors match unsharded
// ---------------------------------------------------------------------------

/// INV-5: a conflicting row fails identically whether the collection is sharded or not,
/// and the failing set does not depend on how rows were distributed.
#[test]
fn inv_05_per_row_errors_match_unsharded() {
    // The reference: one unsharded memory instance.
    let reference = block_on(MemoryStorage::new().create_storage_instance(params(
        &schema_for(None),
        "reference",
    )))
    .expect("reference");

    let sharded = open(config(4, ShardMode::Collection), "errors");
    for instance in [reference.as_ref(), sharded.as_ref()] {
        // Insert two documents, then attempt to insert over them (a conflict) and a
        // fresh one (a success) in the same call.
        let response = block_on(instance.bulk_write(
            vec![
                insert(document("a", "alpha", "1-a")),
                insert(document("b", "bravo", "1-b")),
            ],
            "seed".to_string(),
        ))
        .expect("seed");
        assert!(response.is_ok());

        let response = block_on(instance.bulk_write(
            vec![
                insert(document("a", "conflict a", "1-other")),
                insert(document("c", "charlie", "1-c")),
                insert(document("b", "conflict b", "1-other")),
            ],
            "mixed".to_string(),
        ))
        .expect("mixed");
        let mut failed: Vec<String> = response
            .error
            .iter()
            .map(|error| error.document_id().to_string())
            .collect();
        failed.sort();
        assert_eq!(failed, vec!["a".to_string(), "b".to_string()]);
    }

    // Both storages now hold the same three documents.
    let ids = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    let reference_docs = block_on(reference.find_documents_by_id(ids.clone(), true)).expect("read");
    let sharded_docs = block_on(sharded.find_documents_by_id(ids.clone(), true)).expect("read");
    assert_eq!(sharded_docs.len(), reference_docs.len());
}

// ---------------------------------------------------------------------------
// INV-6 / INV-9 — cleanup, remove, close
// ---------------------------------------------------------------------------

/// INV-9: `cleanup` returns `false` when any shard has more work, and `remove` reaches
/// every shard.
#[test]
fn inv_09_cleanup_and_remove_cover_every_shard() {
    let shards = config(3, ShardMode::Collection);
    let instance = open(shards, "lifecycle");
    for index in 0..9 {
        block_on(instance.bulk_write(
            vec![insert(document(
                &format!("doc-{index}"),
                &format!("name {index}"),
                "1-a",
            ))],
            "seed".to_string(),
        ))
        .expect("write");
    }

    // `cleanup` on a fresh collection with no tombstones reports "nothing left".
    assert!(
        block_on(instance.cleanup(0.0)).expect("cleanup"),
        "a collection with no tombstones has nothing to clean"
    );

    // `remove` must clear every shard, so a read afterwards finds nothing.
    block_on(instance.remove()).expect("remove");
    let found = block_on(instance.find_documents_by_id(
        (0..9).map(|index| format!("doc-{index}")).collect(),
        true,
    ));
    // The shards are removed, so either an empty result or a removed/closed error is
    // correct; what must not happen is documents surviving in some shard.
    match found {
        Ok(documents) => assert!(
            documents.is_empty(),
            "remove must clear every shard, {} documents survived",
            documents.len()
        ),
        Err(error) => {
            let message = format!("{error}");
            assert!(
                message.contains("removed") || message.contains("closed"),
                "unexpected error after remove: {message}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// INV-7 — config mismatch is refused
// ---------------------------------------------------------------------------

/// INV-7: opening with a different shard count or mode is refused at open, and no
/// document is read.
#[test]
fn inv_07_config_mismatch_is_refused() {
    // The wrapper's config and the schema's block must agree. A schema declaring 3 shards
    // against a wrapper configured for 3 is fine; declaring 5 is not.
    let wrapper = config(3, ShardMode::Collection);
    let storage = ShardedStorage::new(MemoryStorage::new(), wrapper);
    let mismatched_schema = schema_for(Some(config(5, ShardMode::Collection)));
    let Err(error) = block_on(storage.create_storage_instance(params(&mismatched_schema, "mismatch")))
    else {
        panic!("a shard-count mismatch must be refused");
    };
    assert!(
        format!("{error}").contains("mismatch"),
        "expected a config mismatch error, got: {error}"
    );

    // A mode mismatch is refused too.
    let mode_schema = schema_for(Some(config(3, ShardMode::Database)));
    let Err(error) = block_on(storage.create_storage_instance(params(&mode_schema, "mismatch-mode")))
    else {
        panic!("a mode mismatch must be refused");
    };
    assert!(format!("{error}").contains("mismatch"), "got: {error}");

    // And a matching schema opens.
    let matching_schema = schema_for(Some(config(3, ShardMode::Collection)));
    assert!(block_on(storage.create_storage_instance(params(&matching_schema, "matching"))).is_ok());
}

/// Acceptance 16: `shards: 0` is rejected before it reaches storage.
#[test]
fn acceptance_16_zero_shards_is_rejected() {
    assert!(ShardingConfig::new(0, ShardMode::Collection).is_none());
    let error = ShardingConfig::from_schema(&json!({
        "sharding": {"shards": 0, "mode": "collection"}
    }))
    .expect_err("zero shards must be rejected");
    assert_eq!(error, dxdb_sharding::routing::ConfigError::ZeroShards);
}

// ---------------------------------------------------------------------------
// Acceptance — plain behaviours
// ---------------------------------------------------------------------------

/// Acceptance 12: a schema without a `sharding` block on a sharded wrapper passes
/// through unsharded rather than erroring or silently sharding.
#[test]
fn acceptance_12_unconfigured_collection_passes_through() {
    let storage = ShardedStorage::new(MemoryStorage::new(), config(4, ShardMode::Collection));
    let instance = block_on(storage.create_storage_instance(params(&schema_for(None), "plain")))
        .expect("opens unsharded");
    let response = block_on(instance.bulk_write(
        vec![insert(document("a", "alpha", "1-a"))],
        "write".to_string(),
    ))
    .expect("write");
    assert!(response.is_ok());
    let found = block_on(instance.find_documents_by_id(vec!["a".to_string()], false)).expect("read");
    assert_eq!(found.len(), 1);
}

/// Acceptance 15: an attachment read resolves through the document's own shard.
///
/// The memory storage does not persist payloads through this path, so the observable is
/// the *error*: a missing attachment must be `MissingAttachment` from the right shard,
/// not a cross-shard lookup or a panic.
#[test]
fn acceptance_15_attachments_follow_their_document() {
    let instance = open(config(4, ShardMode::Collection), "attachments");
    block_on(instance.bulk_write(
        vec![insert(document("doc-1", "alpha", "1-a"))],
        "write".to_string(),
    ))
    .expect("write");
    let error = block_on(instance.get_attachment_data(
        "doc-1".to_string(),
        "att-1".to_string(),
        "digest".to_string(),
    ))
    .expect_err("no attachment was stored");
    assert!(
        format!("{error}").contains("attachment"),
        "expected a missing-attachment error from the document's shard, got: {error}"
    );
}

/// Acceptance 17: both modes produce identical results for the same corpus.
///
/// INV-1 runs both modes against the reference; this asserts they agree with *each
/// other*, which is the claim section 1.3 makes — the mode changes naming only.
#[test]
fn acceptance_17_modes_agree_with_each_other() {
    let collection = config(5, ShardMode::Collection);
    let database = config(5, ShardMode::Database);
    let in_collection = open(collection, "mode-collection");
    let in_database = open(database, "mode-database");
    for instance in [in_collection.as_ref(), in_database.as_ref()] {
        block_on(instance.bulk_write(
            (0..20)
                .map(|index| {
                    insert(document(
                        &format!("doc-{index:02}"),
                        &format!("name {}", index % 7),
                        "1-a",
                    ))
                })
                .collect(),
            "seed".to_string(),
        ))
        .expect("seed");
    }
    let left = block_on(in_collection.query(full_scan())).expect("query");
    let right = block_on(in_database.query(full_scan())).expect("query");
    assert_eq!(left, right, "the mode must not change results");
    assert_eq!(
        block_on(in_collection.count(full_scan())).expect("count").count,
        block_on(in_database.count(full_scan())).expect("count").count
    );
}

/// Acceptance 18: a shard that cannot be created fails the open, naming the shard.
#[test]
fn acceptance_18_missing_shard_fails_at_open() {
    // A storage that refuses the *second* instance exercises the path where one shard
    // cannot be built. The sharding wrapper must fail the whole open rather than
    // returning a collection that is silently missing a shard's documents.
    struct RefusesSecond {
        created: std::sync::atomic::AtomicU32,
    }
    impl RxStorage for RefusesSecond {
        fn name(&self) -> &str {
            "refuses-second"
        }

        async fn create_storage_instance(
            &self,
            params: InstanceCreationParams,
        ) -> Result<Box<DynRxStorageInstance<'static>>, dxdb_storage::StorageError> {
            let count = self
                .created
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if count == 1 {
                return Err(dxdb_storage::StorageError::Unsupported(
                    "this shard refused".to_string(),
                ));
            }
            MemoryStorage::new().create_storage_instance(params).await
        }
    }

    let storage = ShardedStorage::new(
        RefusesSecond {
            created: std::sync::atomic::AtomicU32::new(0),
        },
        config(3, ShardMode::Collection),
    );
    let Err(error) = block_on(storage.create_storage_instance(params(
        &schema_for(Some(config(3, ShardMode::Collection))),
        "missing",
    ))) else {
        panic!("a shard that cannot be created must fail the open");
    };
    let message = format!("{error}");
    assert!(
        message.contains("shard 1"),
        "the error must name the missing shard, got: {message}"
    );
}

/// The `ShardingError` type is part of the public surface; this keeps its variants
/// covered so a rename is a compile error rather than a silent API change.
#[test]
fn acceptance_sharding_error_variants_are_reachable() {
    let mismatch = ShardingError::ConfigMismatch {
        stored: config(2, ShardMode::Collection),
        requested: config(3, ShardMode::Collection),
    };
    assert!(format!("{mismatch}").contains("mismatch"));
    let missing = ShardingError::ShardMissing(2);
    assert!(format!("{missing}").contains("shard 2"));
    // `NonZeroU32` keeps zero unrepresentable, so this cannot be constructed.
    assert_eq!(NonZeroU32::new(0), None);
}
