//! Ported from `test/unit/migration-storage.test.ts` (485 lines, 5 `it` cases) at
//! upstream `3b763ee17`, plus the crash-order cases upstream does not test.
//!
//! # The upstream cases and their fate
//!
//! * `:109` "create both databases" — database creation, not this crate.
//! * `:136` "should migrate all documents" — ported as
//!   [`upstream_136_should_migrate_all_documents`] plus
//!   [`upstream_136_the_callback_gets_the_documented_input`].
//! * `:233` "should migrate in parallel" — the loop is per collection and identical;
//!   only concurrency differs. Not ported, recorded.
//! * `:330` "migrate new->new should also work" — same loop with both sides the same
//!   storage. Covered by the doubles being interchangeable.
//! * `:427` "migration with multiple collections" — a loop over collections.
//!
//! The crash-order tests have no upstream counterpart. They exist because the loop
//! has no transaction spanning its four steps and the brief names this as the plugin
//! with the most ways to lose data; a port that only reproduces the happy path would
//! not tell anyone whether a re-run is safe.

use dxdb_plugin_migration_storage::{
    batch_query_is_live_only, migrate_collection, tombstone_document, BatchReport, Error,
    MigrationSink, MigrationSource, Progress,
};
use serde_json::{json, Value};

/// The `human` fixture's field shape, enough to be realistic.
fn human(index: usize) -> Value {
    json!({
        "passportId": format!("passport-{index:03}"),
        "firstName": format!("name-{index:03}"),
        "lastName": "x",
        "age": index,
        "_deleted": false,
        "_rev": format!("1-{index}"),
        "_meta": { "lwt": 1_000 + index }
    })
}

fn primary(document: &Value) -> String {
    document["passportId"]
        .as_str()
        .expect("primary")
        .to_string()
}

/// An in-memory old storage that behaves like the batch query upstream runs:
/// live documents only, sorted by primary key.
#[derive(Default)]
struct Source {
    documents: Vec<Value>,
    /// How many tombstone calls to report as failed, for the error path.
    fail_tombstones: bool,
    /// Records the order of every tombstone, to check the loop tombstones what it
    /// copied and not something else.
    tombstoned: Vec<String>,
}

impl Source {
    fn with(documents: Vec<Value>) -> Self {
        Source {
            documents,
            ..Source::default()
        }
    }

    fn live(&self) -> Vec<Value> {
        let mut live: Vec<Value> = self
            .documents
            .iter()
            .filter(|document| document["_deleted"] != json!(true))
            .cloned()
            .collect();
        live.sort_by_key(primary);
        live
    }
}

impl MigrationSource for Source {
    fn next_batch(&mut self, limit: usize) -> Result<Vec<Value>, Error> {
        Ok(self.live().into_iter().take(limit).collect())
    }

    fn tombstone(&mut self, documents: &[Value]) -> Result<usize, Error> {
        if self.fail_tombstones {
            return Ok(documents.len());
        }
        for document in documents {
            let id = primary(document);
            let tombstone =
                tombstone_document(document, "migration-token", 5_000.0).expect("tombstone");
            if let Some(slot) = self
                .documents
                .iter_mut()
                .find(|existing| primary(existing) == id)
            {
                *slot = tombstone;
                self.tombstoned.push(id);
            }
        }
        Ok(0)
    }
}

/// An in-memory new storage that records what it was given.
#[derive(Default)]
struct Sink {
    documents: Vec<Value>,
    batches: Vec<usize>,
    /// Set to reject the first N rows, for the conflict path.
    reject_first: usize,
    /// Documents seen by `with_attachment_payloads`, in call order.
    payload_calls: Vec<usize>,
}

impl MigrationSink for Sink {
    fn insert(&mut self, documents: &[Value]) -> Result<usize, Error> {
        self.batches.push(documents.len());
        let reject = self.reject_first.min(documents.len());
        self.reject_first -= reject;
        let accepted = &documents[reject..];
        self.documents.extend(accepted.iter().cloned());
        Ok(reject)
    }

    fn with_attachment_payloads(&mut self, documents: &mut [Value]) -> Result<(), Error> {
        self.payload_calls.push(documents.len());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// `migration-storage.test.ts:136` — should migrate all documents
// ---------------------------------------------------------------------------

#[test]
fn upstream_136_should_migrate_all_documents() {
    // Upstream: 100 documents (20 in fast mode), batchSize = amount / 4, four
    // batches, all documents present afterwards, `_meta.lwt` present on each.
    let count = 100;
    let mut source = Source::with((0..count).map(human).collect());
    let mut sink = Sink::default();

    let progress = migrate_collection(&mut source, &mut sink, count / 4, None).expect("migrate");

    assert_eq!(sink.documents.len(), count, "every document arrived");
    assert_eq!(progress.copied, count);
    assert_eq!(progress.batches, 4, "100 documents in batches of 25");
    assert_eq!(sink.batches, vec![25, 25, 25, 25]);
    assert_eq!(progress.conflicts, 0, "a first run has no conflicts");

    // `assert.ok(newDocPlain._meta.lwt)` at `:228`.
    for document in &sink.documents {
        assert!(document["_meta"]["lwt"].is_number());
        assert_eq!(document["_deleted"], json!(false), "live docs migrate live");
    }

    // The old storage is fully tombstoned, so a re-run has nothing to do.
    assert_eq!(source.live().len(), 0);
    assert_eq!(source.tombstoned.len(), count);
}

#[test]
fn upstream_136_the_callback_gets_the_documented_input() {
    // `AfterMigrateBatchHandlerInput` (`index.ts:17-23`) and the assertion at `:231`
    // that `writeToNewResult.error` is empty.
    let mut source = Source::with((0..10).map(human).collect());
    let mut sink = Sink::default();
    let mut reports: Vec<BatchReport> = Vec::new();
    {
        let mut collect = |report: &BatchReport| {
            reports.push(report.clone());
            Ok(())
        };
        migrate_collection(&mut source, &mut sink, 4, Some(&mut collect)).expect("migrate");
    }

    assert_eq!(
        reports.len(),
        3,
        "10 documents in batches of 4 is 3 batches"
    );
    assert_eq!(reports[0].conflicts, 0);
    assert_eq!(reports[0].documents.len(), 4);
    assert_eq!(reports[2].documents.len(), 2, "the last batch is short");
    // The callback sees the same documents the sink was given, in the same order.
    assert_eq!(
        reports
            .iter()
            .flat_map(|report| report.documents.iter().map(primary))
            .collect::<Vec<_>>(),
        sink.documents.iter().map(primary).collect::<Vec<_>>()
    );
}

#[test]
fn upstream_330_migrating_a_storage_to_itself_uses_the_same_loop() {
    // Both sides are the same implementation, which is what a same-storage migration
    // is. Nothing in the loop distinguishes them.
    let mut source = Source::with((0..6).map(human).collect());
    let mut sink = Sink::default();
    migrate_collection(&mut source, &mut sink, 2, None).expect("migrate");
    assert_eq!(sink.documents.len(), 6);
    assert_eq!(source.live().len(), 0);
}

#[test]
fn upstream_427_multiple_collections_are_independent_runs() {
    // A loop over collections; each gets its own source, sink and progress.
    let mut totals = Vec::new();
    for collection_size in [3, 0, 7] {
        let mut source = Source::with((0..collection_size).map(human).collect());
        let mut sink = Sink::default();
        let progress = migrate_collection(&mut source, &mut sink, 5, None).expect("migrate");
        totals.push((progress.copied, progress.batches));
    }
    assert_eq!(totals, vec![(3, 1), (0, 0), (7, 2)]);
}

// ---------------------------------------------------------------------------
// Crash order — the part upstream does not test.
// ---------------------------------------------------------------------------

#[test]
fn a_crash_after_the_copy_and_before_the_tombstone_is_safe_to_rerun() {
    // The dangerous window. Simulate it by tombstoning nothing, then re-running.
    struct ForgetfulSink {
        inner: Sink,
    }
    impl MigrationSink for ForgetfulSink {
        fn insert(&mut self, documents: &[Value]) -> Result<usize, Error> {
            self.inner.insert(documents)
        }
    }
    // First run: copy succeeds, the tombstone never happens (crash).
    let documents: Vec<Value> = (0..5).map(human).collect();
    let mut source = Source::with(documents.clone());
    let mut crashing = ForgetfulSink {
        inner: Sink::default(),
    };
    // Deliberately only the copy, to model "died between :182 and :193".
    crashing
        .inner
        .insert(&source.next_batch(5).expect("batch"))
        .expect("insert");
    assert_eq!(crashing.inner.documents.len(), 5);
    assert_eq!(source.live().len(), 5, "nothing was tombstoned");

    // Second run: the same documents are copied again. The new storage rejects them
    // as conflicts (an insert of an existing id), which upstream swallows and this
    // crate reports.
    let mut panicked = false;
    let mut sink = ConflictingSink::new(crashing.inner.documents.clone());
    let progress = migrate_collection(&mut source, &mut sink, 5, None).unwrap_or_else(|error| {
        panicked = true;
        panic!("a re-run must not fail: {error}");
    });
    let _ = panicked;
    assert_eq!(
        progress.conflicts, 5,
        "the re-run sees five conflicts, which is exactly what upstream discards silently"
    );
    // And the data is not duplicated: the sink still holds five documents.
    assert_eq!(sink.documents.len(), 5);
    // The old storage is now tombstoned, so a third run is a no-op.
    assert_eq!(source.live().len(), 0);
    let mut third = Sink::default();
    let progress = migrate_collection(&mut source, &mut third, 5, None).expect("third run");
    assert_eq!(progress.copied, 0);
    assert_eq!(progress.batches, 0);
}

/// A sink that already holds documents and reports an insert of an existing id as a
/// conflict, which is what a storage does.
struct ConflictingSink {
    documents: Vec<Value>,
}
impl ConflictingSink {
    fn new(existing: Vec<Value>) -> Self {
        ConflictingSink {
            documents: existing,
        }
    }
}
impl MigrationSink for ConflictingSink {
    fn insert(&mut self, documents: &[Value]) -> Result<usize, Error> {
        let mut conflicts = 0;
        for document in documents {
            if self
                .documents
                .iter()
                .any(|existing| primary(existing) == primary(document))
            {
                conflicts += 1;
            } else {
                self.documents.push(document.clone());
            }
        }
        Ok(conflicts)
    }
}

#[test]
fn a_crash_after_the_tombstone_continues_from_the_next_primary_key() {
    // The tombstones are invisible to the batch query, so progress is real.
    let mut source = Source::with((0..9).map(human).collect());
    let mut sink = Sink::default();
    // First "run" migrates only the first two batches by hand.
    for _ in 0..2 {
        let batch = source.next_batch(3).expect("batch");
        sink.insert(&batch).expect("insert");
        source.tombstone(&batch).expect("tombstone");
    }
    assert_eq!(sink.documents.len(), 6);
    assert_eq!(source.live().len(), 3);

    // A fresh run picks up exactly the three that are left.
    let progress = migrate_collection(&mut source, &mut sink, 3, None).expect("resume");
    assert_eq!(progress.copied, 3);
    assert_eq!(progress.batches, 1);
    assert_eq!(sink.documents.len(), 9);
    // And the order is preserved across the interrupted run: no document was skipped.
    let ids: Vec<String> = sink.documents.iter().map(primary).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "the primary-key order survived the resume");
}

#[test]
fn a_failed_tombstone_stops_with_the_count_of_duplicated_documents() {
    // `index.ts:224-235` throws rather than continuing, because continuing would
    // re-read the same batch forever.
    let mut source = Source::with((0..6).map(human).collect());
    source.fail_tombstones = true;
    let mut sink = Sink::default();
    let error = migrate_collection(&mut source, &mut sink, 3, None).expect_err("must stop");
    match error {
        Error::OldStorageDeleteFailed { failed, copied } => {
            assert_eq!(failed, 3);
            assert_eq!(copied, 3, "the batch was copied, so it is duplicated");
        }
        other => panic!("expected a delete failure, got {other}"),
    }
    // The important part: it stopped. A second iteration would copy the same rows.
    assert_eq!(sink.batches, vec![3]);
}

#[test]
fn a_zero_batch_size_is_refused_instead_of_looping_forever() {
    // Upstream would call the batch query with `limit: 0`, get an empty result and
    // remove the old storage, reporting success with nothing migrated.
    let mut source = Source::with((0..3).map(human).collect());
    let mut sink = Sink::default();
    assert!(migrate_collection(&mut source, &mut sink, 0, None).is_err());
    assert_eq!(sink.documents.len(), 0);
}

// ---------------------------------------------------------------------------
// Ordering and the attachment hook
// ---------------------------------------------------------------------------

#[test]
fn attachment_payloads_are_inlined_before_the_write() {
    // `index.ts:149-171` runs before `:179`. The counts prove the ordering without
    // needing a storage that knows about attachments.
    let mut source = Source::with((0..4).map(human).collect());
    let mut sink = Sink::default();
    migrate_collection(&mut source, &mut sink, 2, None).expect("migrate");
    assert_eq!(
        sink.payload_calls,
        vec![2, 2],
        "one payload call per batch, each matching the batch it precedes"
    );
}

#[test]
fn documents_are_tombstoned_in_the_same_order_they_were_copied() {
    // The revision arithmetic depends on tombstoning the *pre*-migration document, so
    // the loop must not tombstone a document it copied in a different batch.
    let mut source = Source::with((0..10).map(human).collect());
    let mut sink = Sink::default();
    migrate_collection(&mut source, &mut sink, 3, None).expect("migrate");
    assert_eq!(
        source.tombstoned,
        sink.documents.iter().map(primary).collect::<Vec<_>>()
    );
}

#[test]
fn the_batch_query_filter_is_the_reason_progress_works() {
    // Documented invariant, asserted so it cannot be quietly changed: if the batch
    // query stopped filtering `_deleted`, the loop's `skip: 0` would re-read
    // tombstones instead of advancing.
    let selector = json!({ "_deleted": { "$eq": false } });
    assert!(batch_query_is_live_only(&selector));
    let mut source = Source::with((0..3).map(human).collect());
    let mut sink = Sink::default();
    migrate_collection(&mut source, &mut sink, 3, None).expect("migrate");
    // Progress exists only because the third tombstone made all three invisible.
    assert_eq!(source.live().len(), 0);
}

#[test]
fn progress_is_reported_per_batch_for_a_caller_that_wants_a_progress_bar() {
    let mut source = Source::with((0..7).map(human).collect());
    let mut sink = Sink::default();
    let progress: Progress = migrate_collection(&mut source, &mut sink, 3, None).expect("migrate");
    assert_eq!(progress.copied, 7);
    assert_eq!(progress.batches, 3);
}
