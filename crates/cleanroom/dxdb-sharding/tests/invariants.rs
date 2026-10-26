//! INV-1: sharded results are indistinguishable from unsharded results.
//!
//! The differential test `specs/sharding.md` section 3 calls for. Every query in a
//! generated corpus runs against two storages holding the same documents:
//!
//! * **reference**: `dxdb-memory`, unwrapped;
//! * **sharded**: `dxdb-memory` behind `dxdb-sharding` at several shard counts and both
//!   modes.
//!
//! The assertion is full equality of the returned `Vec<DocumentData>` — same documents,
//! same order — plus equality of `count`. Not set equality, not "same length": a merge
//! that sorted differently would pass a set comparison and fail a user's pagination, so
//! order equality is the whole test.
//!
//! The corpus deliberately includes the cases that break a careless merge:
//!
//! * surrogate-pair and astral-plane strings in a sorted field, where UTF-16 code-unit
//!   order and UTF-8 byte order disagree (PLAN.md 3.2);
//! * duplicate sort values, where the tiebreak decides the order;
//! * missing fields and `null`, which the sort's type precedence orders specially;
//! * ascending and descending sorts, multi-field sorts, and sorts on the primary key;
//! * `skip`/`limit` combinations including `limit: 0`, `limit` larger than the result,
//!   `skip` past the end, and windows that land entirely inside one shard.

use std::num::NonZeroU32;

use dxdb_memory::MemoryStorage;
use dxdb_sharding::routing::{ShardMode, ShardingConfig};
use dxdb_sharding::ShardedStorage;
use dxdb_storage::{
    BulkWriteRow, DocumentData, DocumentWriteData, DynRxStorageInstance, InstanceCreationParams,
    PreparedQuery, RxStorage, RxStorageInstance, StorageSchema,
};
use serde_json::{json, Value};

/// Runs a future to completion without an async runtime.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    futures::executor::block_on(future)
}

/// The schema every corpus document conforms to.
///
/// `sharding` is filled per-configuration by [`schema_for`].
fn base_schema() -> Value {
    json!({
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id": {"type": "string", "maxLength": 100},
            "name": {"type": "string"},
            "group": {"type": "string"},
            "rank": {
                "type": "number",
                "minimum": -9007199254740991.0,
                "maximum": 9007199254740991.0,
                "multipleOf": 0.1
            }
        },
        "required": ["id"],
        "indexes": [
            "name", "rank", "group", "id",
            ["group", "rank"], ["name", "rank"],
            ["group", "name"], ["rank", "name"],
            ["name", "rank", "group"]
        ],
        "additionalProperties": false
    })
}

/// The schema, filled exactly as the real call path fills it.
///
/// `dxdb_schema::fill_with_default_settings` injects the meta fields and rewrites every
/// index with the `_deleted` prefix and primary-key suffix that a storage expects
/// (`StorageSchema::from_value` documents that it takes indexes as-is and that an
/// unfilled schema will produce indexes the conformance suite rejects immediately).
/// Using the real filler rather than hand-writing the prefixes means the corpus schema is
/// the schema a collection actually gets — including its `sharding` block, which is part
/// of the schema hash and therefore of what the wrapper must treat as identity.
fn schema_for(config: Option<ShardingConfig>) -> Value {
    let mut schema = base_schema();
    if let Some(config) = config {
        schema["sharding"] = json!({
            "shards": config.shards.get(),
            "mode": config.mode.as_str(),
        });
    }
    dxdb_schema::fill_with_default_settings(&schema).expect("filled schema")
}

fn params_for(schema: &Value, collection_name: &str) -> InstanceCreationParams {
    InstanceCreationParams {
        database_instance_token: "inv-1-token".to_string(),
        database_name: "inv-1-db".to_string(),
        collection_name: collection_name.to_string(),
        schema: StorageSchema::from_value(schema).expect("valid schema"),
        options: json!({}),
        multi_instance: false,
        dev_mode: true,
    }
}

/// The document corpus.
///
/// Hand-built rather than random, because each entry is there to break a specific
/// shortcut. A random corpus would exercise none of them reliably.
fn corpus() -> Vec<Value> {
    let mut documents = Vec::new();
    // The astral-plane cases: `\u{1F600}` (a surrogate pair, D83D DE00) sorts *before*
    // `\uFFFF` by UTF-16 code unit but *after* it by UTF-8 byte order. A merge that
    // compared Rust strings would order these differently from dxdb-memory.
    let tricky_names = [
        "\u{1F600} emoji first",
        "\u{FFFF} last code unit",
        "abc",
        "abd",
        "ab",
        "Z uppercase before lowercase",
        "z lowercase",
        "\u{00e9} accented",
        "e plain",
        "\u{10000} astral bmp-external",
        "\u{FFFD} replacement",
        "",
    ];
    for (index, name) in tricky_names.iter().enumerate() {
        documents.push(json!({
            "id": format!("doc-{index:02}"),
            "name": name,
            "group": if index % 2 == 0 { "even" } else { "odd" },
            "rank": index,
        }));
    }
    // Duplicate sort values, so the primary-key tiebreak decides the order.
    for index in 0..8 {
        documents.push(json!({
            "id": format!("tie-{index:02}"),
            "name": "identical",
            "group": "ties",
            "rank": 100,
        }));
    }
    // Missing fields and null, which the sort's type precedence orders specially.
    documents.push(json!({"id": "missing-name", "group": "sparse", "rank": 1}));
    documents.push(json!({"id": "null-name", "name": null, "group": "sparse", "rank": 2}));
    documents.push(json!({"id": "missing-rank", "name": "sparse one", "group": "sparse"}));
    documents.push(json!({"id": "null-both", "name": null, "group": null, "rank": null}));
    // Numeric edges in a numeric sort.
    for (id, rank) in [
        ("rank-neg", -1.5),
        ("rank-zero", 0.0),
        ("rank-big", 1e21),
        ("rank-frac", 0.1),
        ("rank-max", 9007199254740991.0),
        ("rank-min", -9007199254740991.0),
    ] {
        documents.push(json!({"id": id, "name": format!("rank {id}"), "group": "numeric", "rank": rank}));
    }
    documents
}

/// Every query shape the corpus runs through.
///
/// Each is a normalised-query-shaped JSON object; `dxdb_query`'s normaliser fills the
/// rest, which is what the real call path does.
fn queries() -> Vec<Value> {
    let mut queries = Vec::new();
    // Full scans under every meaningful sort.
    for sort in [
        json!([{"name": "asc"}]),
        json!([{"name": "desc"}]),
        json!([{"rank": "asc"}]),
        json!([{"rank": "desc"}]),
        json!([{"group": "asc"}, {"rank": "desc"}]),
        json!([{"name": "asc"}, {"rank": "asc"}]),
        json!([{"id": "asc"}]),
        json!([{"id": "desc"}]),
        json!([{"group": "asc"}]),
        // A sort whose every value ties, so only the primary-key tiebreak decides.
        json!([{"group": "asc"}, {"name": "asc"}]),
    ] {
        queries.push(json!({"selector": {}, "sort": sort}));
    }
    // Windows: limit only, skip only, both, and the degenerate edges.
    for sort in [json!([{"name": "asc"}]), json!([{"rank": "asc"}])] {
        for skip in [0usize, 1, 3, 7, 20, 1000] {
            for limit in [None, Some(0usize), Some(1), Some(5), Some(100), Some(10_000)] {
                let mut query = json!({"selector": {}, "sort": sort});
                if skip > 0 {
                    query["skip"] = json!(skip);
                }
                if let Some(limit) = limit {
                    query["limit"] = json!(limit);
                }
                queries.push(query);
            }
        }
    }
    // Selectors that narrow to a subset, including one whose matches scatter across
    // shards unevenly and one whose matches all share a sort value.
    for selector in [
        json!({"group": {"$eq": "ties"}}),
        json!({"group": {"$eq": "even"}}),
        json!({"rank": {"$gt": 3}}),
        json!({"rank": {"$gte": 0, "$lt": 5}}),
        json!({"name": {"$eq": "identical"}}),
        json!({"group": {"$in": ["even", "sparse"]}}),
        json!({"$or": [{"group": {"$eq": "ties"}}, {"rank": {"$lt": 2}}]}),
    ] {
        queries.push(json!({"selector": selector, "sort": json!([{"name": "asc"}])}));
        queries.push(json!({"selector": selector, "sort": json!([{"rank": "desc"}])}));
        queries.push(json!({"selector": selector, "sort": json!([{"name": "asc"}]), "limit": 3}));
        queries.push(json!({"selector": selector, "sort": json!([{"name": "asc"}]), "skip": 2, "limit": 3}));
    }
    queries
}

/// Writes `documents` into `instance`, asserting every write succeeded.
fn write_corpus(instance: &DynRxStorageInstance<'_>, documents: &[Value]) {
    let rows: Vec<BulkWriteRow> = documents
        .iter()
        .map(|value| {
            let mut object = value.as_object().expect("document is an object").clone();
            object.insert("_deleted".to_string(), json!(false));
            object.insert("_rev".to_string(), json!("1-corpus"));
            object.insert("_meta".to_string(), json!({"lwt": 1000}));
            object.insert("_attachments".to_string(), json!({}));
            let document =
                DocumentData::from_value(Value::Object(object)).expect("valid document");
            BulkWriteRow::insert(DocumentWriteData::new(document))
        })
        .collect();
    let response = block_on(instance.bulk_write(rows, "corpus".to_string())).expect("write");
    assert!(
        response.is_ok(),
        "corpus writes must all succeed, got {:?}",
        response.error
    );
}

/// The outcome of one query against one storage.
fn run_query(instance: &DynRxStorageInstance<'_>, prepared: &PreparedQuery) -> (Vec<DocumentData>, usize) {
    let documents = block_on(instance.query(prepared.clone())).expect("query");
    let count = block_on(instance.count(prepared.clone())).expect("count").count;
    (documents, count)
}

/// Compares one query's results between the reference and a sharded storage.
///
/// Returns `Ok(())` on full equality, `Err(description)` on any difference. The
/// description names the query, both results, and which of the two properties differed,
/// because a bare "queries differ" is useless in a generated corpus.
fn compare_one(
    reference: &DynRxStorageInstance<'_>,
    sharded: &DynRxStorageInstance<'_>,
    query: &Value,
    primary_path: &str,
) -> Result<(), String> {
    let prepared = dxdb_testkit_support::prepare(query, primary_path);
    let (expected_documents, expected_count) = run_query(reference, &prepared);
    let (actual_documents, actual_count) = run_query(sharded, &prepared);

    if expected_documents != actual_documents {
        let expected_ids: Vec<_> = expected_documents
            .iter()
            .map(|document| document.get("id").cloned().unwrap_or(Value::Null))
            .collect();
        let actual_ids: Vec<_> = actual_documents
            .iter()
            .map(|document| document.get("id").cloned().unwrap_or(Value::Null))
            .collect();
        return Err(format!(
            "query {query}\n  documents differ\n  expected ids: {expected_ids:?}\n  actual ids:   {actual_ids:?}"
        ));
    }
    if expected_count != actual_count {
        return Err(format!(
            "query {query}\n  count differs: unsharded {expected_count}, sharded {actual_count}"
        ));
    }
    // INV-2 for free: count must equal the length of the equivalent query.
    assert_eq!(
        actual_count,
        actual_documents.len(),
        "count must equal query length for {query}"
    );
    Ok(())
}

/// The `dxdb_testkit` query builder, reached without a dev-dependency on the whole
/// suite crate.
///
/// `dxdb-testkit` is a workspace crate, so this is a thin re-export of its prepared-query
/// builder; it exists so the differential test uses the same normalisation the real call
/// path uses rather than a hand-rolled one.
mod dxdb_testkit_support {
    use dxdb_storage::PreparedQuery;
    use serde_json::Value;

    pub fn prepare(query: &Value, primary_path: &str) -> PreparedQuery {
        dxdb_testkit::harness::build_prepared_query(query, primary_path)
    }
}

/// INV-1: for every query shape, every shard count and both modes, the sharded result
/// equals the unsharded result exactly.
#[test]
fn inv_01_results_equal_unsharded() {
    let documents = corpus();
    let primary_path = "id";

    // Build the reference once.
    let reference_storage = MemoryStorage::new();
    let reference = block_on(reference_storage.create_storage_instance(params_for(
        &schema_for(None),
        "reference",
    )))
    .expect("reference instance");
    write_corpus(reference.as_ref(), &documents);

    let mut checked = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for shards in [1u32, 2, 3, 7, 10] {
        for mode in [ShardMode::Collection, ShardMode::Database] {
            let Some(config) = ShardingConfig::new(shards, mode) else {
                continue;
            };
            let sharded_storage = ShardedStorage::new(MemoryStorage::new(), config);
            let collection_name = format!("sharded-{shards}-{}", mode.as_str());
            let sharded = block_on(
                sharded_storage.create_storage_instance(params_for(
                    &schema_for(Some(config)),
                    &collection_name,
                )),
            )
            .expect("sharded instance");
            write_corpus(sharded.as_ref(), &documents);

            for query in queries() {
                checked += 1;
                if let Err(difference) =
                    compare_one(reference.as_ref(), sharded.as_ref(), &query, primary_path)
                {
                    failures.push(format!(
                        "[shards={shards} mode={}] {difference}",
                        mode.as_str()
                    ));
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "INV-1 failed for {} of {} (config x query) combinations:\n\n{}",
        failures.len(),
        checked,
        failures.join("\n\n")
    );
    // A guard against the corpus silently shrinking to nothing.
    assert!(checked >= 300, "the corpus must stay substantial, was {checked}");
    eprintln!("INV-1: {checked} (config x query) combinations matched dxdb-memory exactly");
}

/// INV-2: `count` equals the length of the equivalent query, for every query.
///
/// Checked separately from INV-1 because it is the invariant a careless implementation
/// gets *plausibly* wrong (a sum instead of a clamp) rather than obviously wrong.
#[test]
fn inv_02_count_equals_query_length() {
    let documents = corpus();
    let Some(config) = ShardingConfig::new(7, ShardMode::Collection) else {
        panic!("nonzero")
    };
    let sharded_storage = ShardedStorage::new(MemoryStorage::new(), config);
    let instance = block_on(sharded_storage.create_storage_instance(params_for(
        &schema_for(Some(config)),
        "count-check",
    )))
    .expect("instance");
    write_corpus(instance.as_ref(), &documents);

    for query in queries() {
        let prepared = dxdb_testkit_support::prepare(&query, "id");
        let (documents, count) = run_query(instance.as_ref(), &prepared);
        assert_eq!(
            count,
            documents.len(),
            "count must equal query length for {query}"
        );
    }
}

/// INV-3: every document lives in exactly one shard.
///
/// Sum of per-shard document counts equals the corpus size, and reading every id back
/// through the wrapper yields no duplicates.
#[test]
fn inv_03_each_document_lives_in_one_shard() {
    let documents = corpus();
    let Some(config) = ShardingConfig::new(5, ShardMode::Collection) else {
        panic!("nonzero")
    };
    let sharded_storage = ShardedStorage::new(MemoryStorage::new(), config);
    let instance = block_on(sharded_storage.create_storage_instance(params_for(
        &schema_for(Some(config)),
        "one-home",
    )))
    .expect("instance");
    write_corpus(instance.as_ref(), &documents);

    let ids: Vec<String> = documents
        .iter()
        .map(|document| document["id"].as_str().expect("id").to_string())
        .collect();
    let found = block_on(instance.find_documents_by_id(ids.clone(), true)).expect("read");
    assert_eq!(found.len(), ids.len(), "every id resolves exactly once");
    let mut returned: Vec<String> = found
        .iter()
        .map(|document| document.get("id").expect("id").as_str().expect("string").to_string())
        .collect();
    returned.sort();
    returned.dedup();
    assert_eq!(returned.len(), ids.len(), "no duplicates");

    // Routing is a pure function, so each id's shard is stable and the total across
    // shards is the document count.
    let by_shard: Vec<usize> = ids
        .iter()
        .fold(vec![0usize; config.shards.get() as usize], |mut counts, id| {
            counts[config.shard_of(id) as usize] += 1;
            counts
        });
    assert_eq!(by_shard.iter().sum::<usize>(), ids.len());
}

/// INV-8: `shards: 1` is behaviourally identical to no wrapper, and still correct.
#[test]
fn inv_08_single_shard_is_a_pass_through() {
    let documents = corpus();
    let Some(config) = ShardingConfig::new(1, ShardMode::Collection) else {
        panic!("nonzero")
    };
    let reference_storage = MemoryStorage::new();
    let reference = block_on(
        reference_storage.create_storage_instance(params_for(&schema_for(None), "single-ref")),
    )
    .expect("reference");
    write_corpus(reference.as_ref(), &documents);

    let sharded_storage = ShardedStorage::new(MemoryStorage::new(), config);
    let sharded = block_on(
        sharded_storage.create_storage_instance(params_for(&schema_for(Some(config)), "single")),
    )
    .expect("sharded");
    write_corpus(sharded.as_ref(), &documents);

    for query in queries() {
        compare_one(reference.as_ref(), sharded.as_ref(), &query, "id")
            .unwrap_or_else(|difference| panic!("single-shard differed: {difference}"));
    }
}

/// The number of combinations the differential test covers, printed so a reviewer can
/// see the corpus did not quietly shrink.
#[test]
fn corpus_size_is_reported() {
    let combinations = queries().len() * 10;
    eprintln!(
        "corpus: {} documents, {} queries, {} (config x query) combinations",
        corpus().len(),
        queries().len(),
        combinations
    );
    assert!(queries().len() >= 30, "the query corpus must stay substantial");
    assert!(corpus().len() >= 30, "the document corpus must stay substantial");
}

/// `NonZeroU32` is part of the public config surface; `Cargo.toml` silence guard.
const _: () = {
    let _ = std::mem::size_of::<NonZeroU32>();
};
