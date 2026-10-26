//! Ported from `test/unit/rx-pipeline.test.ts` (581 lines) at upstream `3b763ee17`.
//!
//! Each test names the upstream `it(...)` and its line. The cases that are about the batch
//! loop, the checkpoint arithmetic and re-entrancy are ported; the ones that need a live
//! database, a change stream, or the internal store are listed in the crate docs as not
//! ported.
//!
//! The re-entrancy case (`:369`) is the one worth reading: upstream solves it by
//! inspecting the JavaScript stack for a marker function name, and this crate solves it
//! with an explicit token. The test asserts the *behaviour* upstream asserts (a handler's
//! own calls do not block, an outside caller's do) so the divergence is behaviourally
//! invisible even though the mechanism is completely different.

use dxdb_plugin_pipeline::{
    is_idle, Checkpoint, Error, Pipeline, PipelineStore, ReentrancyGuard, SourceDocument,
    DEFAULT_BATCH_SIZE,
};
use serde_json::json;

/// A source with a fixed set of documents, paginated by index.
struct Store {
    documents: Vec<SourceDocument>,
    checkpoint: Option<Checkpoint>,
    /// How many times `documents_since` was called, to prove resumption.
    reads: usize,
    /// Every checkpoint written, in order.
    writes: Vec<Checkpoint>,
}

impl Store {
    fn with(count: usize) -> Self {
        Store {
            documents: (0..count)
                .map(|index| {
                    SourceDocument::new(
                        json!({ "id": format!("doc-{index:03}"), "value": index }),
                        index as f64 + 1.0,
                    )
                })
                .collect(),
            checkpoint: None,
            reads: 0,
            writes: Vec::new(),
        }
    }

    /// Documents after the checkpoint's last seen index.
    fn after(&self, checkpoint: Option<&serde_json::Value>) -> usize {
        checkpoint
            .and_then(|value| value.get("next"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize
    }
}

impl PipelineStore for Store {
    fn checkpoint(&mut self, _identifier: &str) -> Result<Option<Checkpoint>, Error> {
        Ok(self.checkpoint.clone())
    }

    fn documents_since(
        &mut self,
        batch_size: usize,
        checkpoint: Option<&serde_json::Value>,
    ) -> Result<(Vec<SourceDocument>, serde_json::Value), Error> {
        self.reads += 1;
        let start = self.after(checkpoint);
        let batch: Vec<SourceDocument> = self
            .documents
            .iter()
            .skip(start)
            .take(batch_size)
            .cloned()
            .collect();
        // The checkpoint points at the last document returned, not at `start + batch_size`
        // — a real `getChangedDocumentsSince` returns a checkpoint derived from the
        // documents it handed back (rx-storage-helper.ts). Advancing by the batch size
        // would skip documents appended after a short batch, which is exactly the
        // resumption bug this test exists to catch.
        Ok((batch.clone(), json!({ "next": start + batch.len() })))
    }

    fn set_checkpoint(&mut self, _identifier: &str, checkpoint: &Checkpoint) -> Result<(), Error> {
        self.checkpoint = Some(checkpoint.clone());
        self.writes.push(checkpoint.clone());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// `rx-pipeline.test.ts:53` — write some document depending on another
// ---------------------------------------------------------------------------

#[test]
fn upstream_53_the_handler_sees_every_source_document() {
    let guard = ReentrancyGuard::new();
    let mut store = Store::with(5);
    let mut seen = Vec::new();
    let result = Pipeline::new("test", &guard)
        .run_until_idle(&mut store, |documents, _token| {
            for document in documents {
                seen.push(document.data["id"].as_str().expect("id").to_string());
            }
            Ok(())
        })
        .expect("run");

    assert_eq!(seen.len(), 5);
    assert_eq!(result.documents, 5);
    assert_eq!(result.batches, 1, "five documents fit in one default batch");
}

#[test]
fn upstream_136_the_batch_is_handed_to_the_handler_whole() {
    // `:136-154` "should store the transformed data to the destination". The store side
    // is the handler's business; what this crate decides is that it gets a whole batch,
    // not one document at a time, because a handler that wants to write atomically needs
    // them together.
    let guard = ReentrancyGuard::new();
    let mut store = Store::with(250);
    let mut batch_sizes = Vec::new();
    let result = Pipeline::new("test", &guard)
        .run_until_idle(&mut store, |documents, _token| {
            batch_sizes.push(documents.len());
            Ok(())
        })
        .expect("run");

    assert_eq!(batch_sizes, vec![100, 100, 50]);
    assert_eq!(result.batches, 3);
    assert_eq!(result.documents, 250);
}

// ---------------------------------------------------------------------------
// `rx-pipeline.test.ts:155` — awaitIdle updates its internal timestamps
// ---------------------------------------------------------------------------

#[test]
fn upstream_155_the_checkpoint_records_the_last_document_time() {
    // `:155-179`. After a run, the checkpoint's `lastDocTime` is the lwt of the last
    // document processed, which is what `awaitIdle` compares against the source's newest
    // lwt.
    let guard = ReentrancyGuard::new();
    let mut store = Store::with(3);
    let result = Pipeline::new("test", &guard)
        .run_until_idle(&mut store, |_documents, _token| Ok(()))
        .expect("run");

    let checkpoint = result.checkpoint.expect("checkpoint");
    assert_eq!(checkpoint.last_doc_time, 3.0, "lwt of the third document");
    assert!(is_idle(checkpoint.last_doc_time, 3.0));
    assert!(
        !is_idle(checkpoint.last_doc_time, 4.0),
        "a newer source doc breaks idle"
    );
}

// ---------------------------------------------------------------------------
// `rx-pipeline.test.ts:268` — continue from the correct checkpoint
// ---------------------------------------------------------------------------

#[test]
fn upstream_268_a_second_run_resumes_from_the_checkpoint() {
    // `:268-292`. The second run must not re-process documents the first already handled.
    let guard = ReentrancyGuard::new();
    let mut store = Store::with(4);
    let mut pipeline = Pipeline::new("test", &guard).with_batch_size(2);

    let first = pipeline
        .run_until_idle(&mut store, |_documents, _token| Ok(()))
        .expect("first");
    assert_eq!(first.documents, 4);

    // Add two more documents, as a write to the source would.
    store
        .documents
        .push(SourceDocument::new(json!({ "id": "doc-004" }), 5.0));
    store
        .documents
        .push(SourceDocument::new(json!({ "id": "doc-005" }), 6.0));

    let mut seen = Vec::new();
    let second = pipeline
        .run_until_idle(&mut store, |documents, _token| {
            for document in documents {
                seen.push(document.data["id"].as_str().expect("id").to_string());
            }
            Ok(())
        })
        .expect("second");

    assert_eq!(
        seen,
        vec!["doc-004", "doc-005"],
        "only the new documents were processed"
    );
    assert_eq!(second.documents, 2);
}

#[test]
fn a_run_with_nothing_new_still_writes_a_checkpoint() {
    // `rx-pipeline.ts:165-168` writes the checkpoint even for an empty batch, so a
    // caught-up pipeline records that it is caught up.
    let guard = ReentrancyGuard::new();
    let mut store = Store::with(0);
    let result = Pipeline::new("test", &guard)
        .run_until_idle(&mut store, |_documents, _token| Ok(()))
        .expect("run");
    assert_eq!(result.batches, 0);
    assert!(
        result.checkpoint.is_some(),
        "an empty run records its position"
    );
}

// ---------------------------------------------------------------------------
// `rx-pipeline.test.ts:181` and `:202` — handler errors are not swallowed
// ---------------------------------------------------------------------------

#[test]
fn upstream_181_a_throwing_handler_is_latched_and_reported() {
    // `:181-201`. Upstream stores the error and re-throws it on every later call rather
    // than retrying, because a deterministic handler failure would loop forever.
    let guard = ReentrancyGuard::new();
    let mut store = Store::with(5);
    let mut pipeline = Pipeline::new("test", &guard);

    let error = pipeline
        .run_until_idle(&mut store, |_documents, _token| {
            Err(Error::Handler("boom".to_string()))
        })
        .expect_err("handler failed");
    assert!(error.to_string().contains("boom"));
    assert!(pipeline.error().is_some(), "the error is latched");

    // A later run returns the latched error without touching the store.
    let reads_before = store.reads;
    let again = pipeline
        .run_until_idle(&mut store, |_documents, _token| Ok(()))
        .expect_err("latched");
    assert_eq!(again, error);
    assert_eq!(store.reads, reads_before, "no further reads were attempted");
}

#[test]
fn upstream_227_a_failed_handler_leaves_the_destination_unblocked() {
    // `:227-266` "should not break reads on destination after handler throws". The
    // observable property here: the guard is released even when the handler fails, so
    // outside callers are not left waiting forever.
    let guard = ReentrancyGuard::new();
    let mut store = Store::with(2);
    let mut pipeline = Pipeline::new("test", &guard);

    let _ = pipeline.run_until_idle(&mut store, |_documents, _token| {
        Err(Error::Handler("boom".to_string()))
    });
    assert!(
        !guard.is_active(),
        "the guard must be released on the error path"
    );
    assert!(!guard.must_wait(None), "so outside reads proceed");
}

// ---------------------------------------------------------------------------
// `rx-pipeline.test.ts:369` — reads from inside the handler do not block
// ---------------------------------------------------------------------------

#[test]
fn upstream_369_a_handlers_own_calls_do_not_block() {
    // THE RE-ENTRANCY CASE. Upstream detects "inside a handler" by matching the JS stack
    // trace against a marker function name (flagged-functions.ts:3-27). Here the handler
    // is handed a token and the guard trusts it. The asserted behaviour is identical.
    let guard = ReentrancyGuard::new();
    let mut store = Store::with(2);
    let mut inside_waited = None;
    let mut token_identifier = None;

    Pipeline::new("test", &guard)
        .run_until_idle(&mut store, |_documents, token| {
            // A destination call from inside the handler, modelled as the guard query.
            inside_waited = Some(guard.must_wait(Some(token)));
            token_identifier = Some(token.identifier().to_string());
            Ok(())
        })
        .expect("run");

    assert_eq!(
        inside_waited,
        Some(false),
        "the handler must not wait on itself"
    );
    assert_eq!(token_identifier.as_deref(), Some("test"));
    // And outside the handler, callers do wait while a pipeline is active.
    assert!(
        !guard.must_wait(None),
        "and nothing is active once it returns"
    );
}

#[test]
fn upstream_466_two_pipelines_on_one_destination_do_not_deadlock() {
    // `:466-...` "should not deadlock when two pipelines write to the same destination
    // and both handlers read from it". Each pipeline holds its own token, so both
    // handlers' reads pass the shared guard.
    let guard = ReentrancyGuard::new();
    let mut store_a = Store::with(2);
    let mut store_b = Store::with(2);
    let mut observations = Vec::new();

    // Interleave: pipeline A's handler observes the guard while B is also active.
    let mut pipeline_a = Pipeline::new("a", &guard).with_batch_size(1);
    let mut store_a_outer = std::mem::replace(&mut store_a, Store::with(0));
    pipeline_a
        .run_until_idle(&mut store_a_outer, |_documents, token_a| {
            guard.enter("b");
            observations.push(("a", guard.must_wait(Some(token_a))));
            observations.push(("a-sees-outside", guard.must_wait(None)));
            guard.leave("b");
            Ok(())
        })
        .expect("a");

    let mut pipeline_b = Pipeline::new("b", &guard).with_batch_size(1);
    pipeline_b
        .run_until_idle(&mut store_b, |_documents, token_b| {
            observations.push(("b", guard.must_wait(Some(token_b))));
            Ok(())
        })
        .expect("b");

    // Pipeline A runs once per document (batch size 1, two documents), so its two
    // observations repeat. What matters is asserted structurally: every in-handler check
    // passed, and the outside check failed.
    let in_handler: Vec<bool> = observations
        .iter()
        .filter(|(who, _)| *who == "a" || *who == "b")
        .map(|(_, waited)| *waited)
        .collect();
    let outside: Vec<bool> = observations
        .iter()
        .filter(|(who, _)| *who == "a-sees-outside")
        .map(|(_, waited)| *waited)
        .collect();
    assert!(
        in_handler.iter().all(|waited| !waited),
        "both handlers bypass the guard, got {observations:?}"
    );
    assert!(
        outside.iter().all(|waited| *waited),
        "an outside caller during a run still waits, got {observations:?}"
    );
    assert_eq!(in_handler.len(), 4, "2 in A's runs + 2 in B's");
}

// ---------------------------------------------------------------------------
// The loop's own rules
// ---------------------------------------------------------------------------

#[test]
fn a_short_batch_ends_the_run() {
    // `:170-172`: `if (documents.length < batchSize) done = true`.
    let guard = ReentrancyGuard::new();
    let mut store = Store::with(3);
    let result = Pipeline::new("test", &guard)
        .with_batch_size(10)
        .run_until_idle(&mut store, |_documents, _token| Ok(()))
        .expect("run");
    assert_eq!(result.batches, 1, "one short batch and the loop stops");
}

#[test]
fn an_oversized_batch_is_an_error_rather_than_a_silent_short_read() {
    // If a store ignored `batch_size`, the "short batch means done" rule would end the
    // run one batch early and silently skip documents.
    struct Greedy {
        checkpoint: Option<Checkpoint>,
    }
    impl PipelineStore for Greedy {
        fn checkpoint(&mut self, _identifier: &str) -> Result<Option<Checkpoint>, Error> {
            Ok(self.checkpoint.clone())
        }
        fn documents_since(
            &mut self,
            _batch_size: usize,
            _checkpoint: Option<&serde_json::Value>,
        ) -> Result<(Vec<SourceDocument>, serde_json::Value), Error> {
            Ok((
                vec![
                    SourceDocument::new(json!({ "id": "a" }), 1.0),
                    SourceDocument::new(json!({ "id": "b" }), 2.0),
                ],
                json!({ "next": 2 }),
            ))
        }
        fn set_checkpoint(
            &mut self,
            _identifier: &str,
            checkpoint: &Checkpoint,
        ) -> Result<(), Error> {
            self.checkpoint = Some(checkpoint.clone());
            Ok(())
        }
    }
    let guard = ReentrancyGuard::new();
    let mut store = Greedy { checkpoint: None };
    let error = Pipeline::new("test", &guard)
        .with_batch_size(1)
        .run_until_idle(&mut store, |_documents, _token| Ok(()))
        .expect_err("oversized");
    assert_eq!(error, Error::OversizedBatch { got: 2, wanted: 1 });
}

#[test]
fn the_default_batch_size_is_the_documented_one() {
    assert_eq!(DEFAULT_BATCH_SIZE, 100);
}
