//! The change-feed checkpoint must be **exclusive on the `(lwt, primary)` pair**.
//!
//! These are direct tests of one property, written because it failed in the conformance
//! suite (`impl-changedsince-01/02/03`) and the failure mode is invisible to any test
//! that polls once. They do not replace the suite; they pin the mechanism so a future
//! refactor of the index cursor cannot reintroduce it quietly.
//!
//! # The bug, for the record
//!
//! The cursor for an exclusive lower bound was `"{name}|{lower}\u{0}"`, on the theory
//! that `\u{0}` sorts before everything. It does — but `\u{0}` also sorts *before* the
//! `|` that separates the encoded index string from the primary value, so the cursor
//! landed **before** the checkpoint's own row instead of after it. The checkpoint
//! document was returned again on every poll.
//!
//! # Why the pair matters, and why `lwt > checkpoint.lwt` is not the fix
//!
//! Replication checkpoints are `(_meta.lwt, primary)`. The clock can assign the same
//! `lwt` to several documents, so `lwt > checkpoint.lwt` would skip every sibling that
//! shares the checkpoint's timestamp. The bound has to be exclusive on the *pair*:
//! after the checkpoint's primary value at that `lwt`, and everything with a greater
//! `lwt`. Both halves are asserted below.

mod support;

use std::rc::Rc;

use serde_json::{json, Value};
use support::watchdog::within;

use dxdb_indexeddb::{IndexedDbStorage, MemoryBacking};
use dxdb_storage::{
    BulkWriteRow, DocumentData, DocumentWriteData, InstanceCreationParams, RxStorage,
    RxStorageInstance, StorageSchema,
};

/// `_meta.lwt` the tests write.
fn lwt(n: u64) -> f64 {
    n as f64
}

fn schema() -> Value {
    json!({
        "version": 0,
        "primaryKey": "key",
        "type": "object",
        "properties": {
            "key": { "type": "string", "maxLength": 100 },
            "value": { "type": "string", "maxLength": 100 },
            "_deleted": { "type": "boolean" },
            "_rev": { "type": "string", "maxLength": 100 },
            "_attachments": { "type": "object" },
            "_meta": {
                "type": "object",
                "properties": {
                    "lwt": {"type": "number", "minimum": 1, "maximum": 1e15, "multipleOf": 0.01}
                },
                "required": ["lwt"]
            }
        },
        "required": ["key", "value", "_deleted", "_rev", "_meta"],
        // The change-feed index is what `get_changed_documents_since` paginates over;
        // `fill_with_default_settings` appends it in production, and a hand-written
        // schema has to include it or the plan names an index the instance does not
        // materialise (which is `UnknownIndex`, correctly).
        "indexes": [["value"], ["_meta.lwt", "key"]]
    })
}

fn storage() -> IndexedDbStorage<MemoryBacking> {
    IndexedDbStorage::new(Rc::new(MemoryBacking::new()))
}

async fn open(
    s: &IndexedDbStorage<MemoryBacking>,
) -> Box<dxdb_storage::DynRxStorageInstance<'static>> {
    let params = InstanceCreationParams {
        database_instance_token: "ckpt".to_string(),
        database_name: "ckpt".to_string(),
        collection_name: "docs".to_string(),
        schema: StorageSchema::from_value(&schema()).expect("valid schema"),
        options: json!({}),
        multi_instance: false,
        dev_mode: false,
    };
    s.create_storage_instance(params).await.expect("opens")
}

fn write_doc(key: &str, lwt_value: f64) -> BulkWriteRow {
    let document = DocumentData::from_value(json!({
        "key": key,
        "value": "v",
        "_deleted": false,
        "_rev": format!("1-{key}"),
        "_meta": { "lwt": lwt_value }
    }))
    .expect("valid document");
    BulkWriteRow::insert(DocumentWriteData::new(document))
}

fn keys(documents: &[DocumentData]) -> Vec<String> {
    documents
        .iter()
        .map(|doc| {
            doc.get_object_path("key")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The property the suite was failing on
// ---------------------------------------------------------------------------

/// Diagnostic: prints the plan the default `get_changed_documents_since` builds, so a
/// bound problem can be told apart from a cursor problem. Kept as a test because the
/// plan's `inclusive_start` and encoded bounds are exactly what went wrong once.
#[test]
fn the_default_plan_is_an_exclusive_pair_bound() {
    let checkpoint = dxdb_storage::Checkpoint::new("k1", 1.0);
    let prepared = dxdb_storage::crate_changed_documents_since_query("key", &checkpoint, 1);
    assert!(
        !prepared.query_plan.inclusive_start,
        "must be exclusive at the start"
    );
    assert!(
        prepared.query_plan.inclusive_end,
        "must be inclusive at the end"
    );
    assert_eq!(
        prepared.query_plan.index,
        vec!["_meta.lwt".to_string(), "key".to_string()],
        "the pair is (lwt, primary), in that order"
    );
    // And the *encoded* lower bound must be exactly the checkpoint's own index string,
    // so that an exclusive cursor at that string skips precisely the checkpoint row.
    let encoded_lower = within("encode lower", async {
        dxdb_indexeddb::encoder_bound_for_test(
            &schema(),
            &prepared.query_plan.index,
            &prepared.query_plan.start_keys,
            true,
        )
        .expect("encodes")
    });
    let checkpoint_row = within("encode row", async {
        let mut doc = DocumentData::from_value(json!({
            "key": "k1",
            "value": "v",
            "_deleted": false,
            "_rev": "1-k1",
            "_meta": {"lwt": 1.0}
        }))
        .expect("valid");
        let _ = &mut doc;
        dxdb_indexeddb::encoded_index_string_for_test(&schema(), &prepared.query_plan.index, &doc)
            .expect("encodes")
    });
    assert_eq!(
        encoded_lower, checkpoint_row,
        "the lower bound must equal the checkpoint document's index string\n\
         lower={encoded_lower:?}\nrow  ={checkpoint_row:?}"
    );
}

/// Resuming from a checkpoint returns the *next* document, never the checkpoint one.
/// Diagnostic: what the index keys look like versus the cursor derived from a
/// checkpoint, for the same data. Prints both so a prefix/format mismatch is visible.
#[test]
fn the_cursor_sorts_strictly_after_the_checkpoint_row() {
    let storage = storage();
    let (keys_seen, cursor, lower) = within("cursor vs row", async {
        let instance = open(&storage).await;
        for n in 1..=3u64 {
            instance
                .bulk_write(
                    vec![write_doc(&format!("k{n}"), lwt(n))],
                    "test".to_string(),
                )
                .await
                .expect("write");
        }
        let first = instance
            .get_changed_documents_since(1, None)
            .await
            .expect("first");
        let plan = dxdb_storage::crate_changed_documents_since_query("key", &first.checkpoint, 1);
        let lower = dxdb_indexeddb::encoder_bound_for_test(
            &schema(),
            &plan.query_plan.index,
            &plan.query_plan.start_keys,
            true,
        )
        .expect("encodes");
        let row = dxdb_indexeddb::encoded_index_string_for_test(
            &schema(),
            &plan.query_plan.index,
            &first.documents[0],
        )
        .expect("encodes");
        (keys(&first.documents), row, lower)
    });
    assert_eq!(keys_seen, vec!["k1"]);
    assert_eq!(
        lower, cursor,
        "the checkpoint's encoded bound must equal the checkpoint document's index string"
    );
}

/// Resuming from a checkpoint returns the *next* document, never the checkpoint one.
#[test]
fn the_checkpoint_is_exclusive() {
    let storage = storage();
    let report = within("checkpoint exclusive", async {
        let instance = open(&storage).await;
        for n in 1..=3u64 {
            instance
                .bulk_write(
                    vec![write_doc(&format!("k{n}"), lwt(n))],
                    "test".to_string(),
                )
                .await
                .expect("write");
        }

        let first = instance
            .get_changed_documents_since(1, None)
            .await
            .expect("first poll");
        let resumed = instance
            .get_changed_documents_since(1, Some(first.checkpoint.clone()))
            .await
            .expect("resume");

        (
            keys(&first.documents),
            first.checkpoint.id,
            keys(&resumed.documents),
            resumed.checkpoint.id,
        )
    });

    let (first_keys, first_id, resumed_keys, resumed_id) = report;
    assert_eq!(first_keys, vec!["k1"], "the first change is the lowest lwt");
    assert_eq!(first_id, "k1");
    assert_eq!(
        resumed_keys,
        vec!["k2"],
        "resuming from k1 must return k2, not k1 again"
    );
    assert_eq!(resumed_id, "k2");
}

/// Polling repeatedly with the returned checkpoint makes strict progress: no repeats,
/// no skips, and the set of documents seen equals the set written.
#[test]
fn repeated_polls_make_strict_progress() {
    let storage = storage();
    let seen = within("repeated polls", async {
        let instance = open(&storage).await;
        for n in 1..=5u64 {
            instance
                .bulk_write(
                    vec![write_doc(&format!("k{n}"), lwt(n))],
                    "test".to_string(),
                )
                .await
                .expect("write");
        }

        let mut seen: Vec<String> = Vec::new();
        let mut checkpoint = None;
        // Bounded by 20 iterations: a non-terminating poll is the symptom of an
        // inclusive checkpoint, and this turns that into a failure rather than a hang.
        for _ in 0..20 {
            let batch = instance
                .get_changed_documents_since(1, checkpoint.clone())
                .await
                .expect("poll");
            if batch.documents.is_empty() {
                break;
            }
            seen.extend(keys(&batch.documents));
            checkpoint = Some(batch.checkpoint);
        }
        seen
    });

    assert_eq!(
        seen,
        vec!["k1", "k2", "k3", "k4", "k5"],
        "each poll must return exactly the one new document, in order"
    );
}

/// A `limit` of N must return N *new* documents. With an inclusive checkpoint it
/// returns N-1 new plus one repeat, so a bounded poll silently under-progresses —
/// the interaction that a single-poll test cannot see.
#[test]
fn a_bounded_poll_returns_a_full_batch_of_new_documents() {
    let storage = storage();
    let batches = within("bounded poll", async {
        let instance = open(&storage).await;
        for n in 1..=6u64 {
            instance
                .bulk_write(
                    vec![write_doc(&format!("k{n}"), lwt(n))],
                    "test".to_string(),
                )
                .await
                .expect("write");
        }

        let first = instance
            .get_changed_documents_since(2, None)
            .await
            .expect("first");
        assert_eq!(keys(&first.documents), vec!["k1", "k2"], "first batch of 2");

        let second = instance
            .get_changed_documents_since(2, Some(first.checkpoint.clone()))
            .await
            .expect("second");
        assert_eq!(
            keys(&second.documents),
            vec!["k3", "k4"],
            "a limit-2 poll after k2 must return two new documents, not k2 again plus k3"
        );
        (keys(&first.documents), keys(&second.documents))
    });

    assert_eq!(batches.0.len(), 2);
    assert_eq!(batches.1.len(), 2, "a bounded poll must not shrink");
}

// ---------------------------------------------------------------------------
// The half that `lwt > checkpoint.lwt` would get wrong
// ---------------------------------------------------------------------------

/// Documents sharing the checkpoint's `lwt` are still returned, ordered by primary
/// value. This is why the bound is exclusive on the *pair* rather than on `lwt`:
/// `lwt > checkpoint.lwt` would skip these, and the clock does assign equal
/// timestamps.
#[test]
fn documents_sharing_the_checkpoints_lwt_are_not_skipped() {
    let storage = storage();
    let (first, second) = within("shared lwt", async {
        let instance = open(&storage).await;
        // All three at the same lwt, written in a non-sorted order so the index has to
        // order them by primary value.
        for key in ["b", "a", "c"] {
            instance
                .bulk_write(vec![write_doc(key, 500.0)], "test".to_string())
                .await
                .expect("write");
        }

        let first = instance
            .get_changed_documents_since(1, None)
            .await
            .expect("first");
        let second = instance
            .get_changed_documents_since(1, Some(first.checkpoint.clone()))
            .await
            .expect("second");
        (keys(&first.documents), keys(&second.documents))
    });

    assert_eq!(first, vec!["a"], "same lwt orders by primary value");
    assert_eq!(
        second,
        vec!["b"],
        "the sibling sharing the checkpoint's lwt must be returned, not skipped"
    );
}

/// Walking the whole feed with `limit: 1` over three same-`lwt` documents visits each
/// exactly once.
#[test]
fn the_whole_feed_is_walked_exactly_once_across_shared_lwts() {
    let storage = storage();
    let seen = within("walk shared lwt", async {
        let instance = open(&storage).await;
        for key in ["a", "b", "c"] {
            instance
                .bulk_write(vec![write_doc(key, 700.0)], "test".to_string())
                .await
                .expect("write");
        }

        let mut seen = Vec::new();
        let mut checkpoint = None;
        for _ in 0..10 {
            let batch = instance
                .get_changed_documents_since(1, checkpoint.clone())
                .await
                .expect("poll");
            if batch.documents.is_empty() {
                break;
            }
            seen.extend(keys(&batch.documents));
            checkpoint = Some(batch.checkpoint);
        }
        seen
    });

    assert_eq!(seen, vec!["a", "b", "c"]);
}
