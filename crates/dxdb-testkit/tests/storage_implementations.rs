//! `make gate-storage` — the ported `rx-storage-implementations` suite.
//!
//! This file is named `storage_implementations.rs` because `make gate-storage` runs
//! `cargo test -p dxdb-testkit --test storage_implementations`; the name is fixed by the
//! Makefile and is how the leader verifies every storage lane.
//!
//! # What runs here
//!
//! One `#[test]` per upstream case that is declared
//! [`PortStatus::Exercised`](dxdb_testkit::decl::PortStatus), generated from the
//! declaration table so a case cannot be declared exercised and silently not run. Each
//! test names its upstream file:line, so a failure report is a citation.
//!
//! The storage under test is [`ReferenceStorage`], the testkit's own scaffold. That is
//! deliberate and documented in its module: it exists so this gate is not vacuous before
//! `dxdb-memory` lands. **When lane D's `dxdb-memory` is available, add a second test
//! module here that runs the same suite against it** — the suite functions are generic
//! over `RxStorage`, so that is a one-line addition per storage, not a rewrite.

// NOTE: this harness reports every failing case by catching a panic per case, which
// only works under `panic = "unwind"`. cargo test uses the dev profile, so it does.
// A release-profile build of this harness would abort on the first failure instead
// (measured: catch_unwind cannot catch under panic="abort"). See
// dxdb_testkit::suite::PANIC_STRATEGY.

mod support;

use dxdb_testkit::decl::{exercised_cases, Case};
use dxdb_testkit::suite;
use support::reference_storage::ReferenceStorage;

/// The cases this file runs, checked against the declaration table at compile time by
/// construction: the list is derived from the table, never written out.
fn cases() -> Vec<&'static Case> {
    exercised_cases()
        .filter(|case| case.upstream_file.ends_with("implementations.test.ts"))
        .collect()
}

/// Runs one case by its upstream line.
///
/// `futures::executor::block_on` rather than a runtime: D3 makes the sequential executor
/// the default, and `tests` has no async harness without a runtime dependency. This is
/// also why the crate can be checked for `wasm32`.
fn run(case: &Case) {
    let storage = ReferenceStorage::new();
    futures::executor::block_on(suite::run_exercised_case(&storage, case.upstream_line));
}

// One test per case. The names are the case ids, so `cargo test -- impl-conflict`
// selects a behaviour rather than a line number.

#[test]
fn open_and_close() {
    run(case_at(176));
}

#[test]
fn open_many_instances_on_the_same_database_name() {
    run(case_at(195));
}

#[test]
fn should_write_the_document() {
    run(case_at(270));
}

#[test]
fn should_error_on_conflict() {
    run(case_at(309));
}

#[test]
fn concurrent_insert_of_the_same_document_conflicts() {
    run(case_at(372));
}

#[test]
fn should_not_find_the_deleted_document_when_find_documents_by_id_false() {
    run(case_at(420));
}

#[test]
fn should_not_be_able_to_overwrite_a_deleted_document() {
    run(case_at(483));
}

#[test]
fn should_be_able_to_update_the_state_of_a_deleted_document() {
    run(case_at(600));
}

#[test]
fn should_be_able_to_unset_a_property() {
    run(case_at(661));
}

#[test]
fn should_be_able_to_do_a_write_where_only_meta_fields_are_changed() {
    run(case_at(773));
}

#[test]
fn should_be_able_to_create_another_instance_after_a_write() {
    run(case_at(848));
}

#[test]
fn should_be_able_to_jump_more_than_one_revision_height() {
    run(case_at(900));
}

#[test]
fn multiple_storage_instances_on_the_same_database_write_documents() {
    run(case_at(951));
}

#[test]
fn write_and_read_with_umlauts() {
    run(case_at(996));
}

#[test]
fn resurrecting_a_deleted_document_with_the_previous_revision_is_allowed() {
    run(case_at(1058));
}

#[test]
fn issue_9145_persists_all_documents_of_parallel_bulk_writes() {
    run(case_at(1130));
}

#[test]
fn query_should_find_all_documents() {
    run(case_at(1512));
}

#[test]
fn query_should_sort_in_the_correct_order() {
    run(case_at(1578));
}

#[test]
fn query_must_find_deleted_documents() {
    run(case_at(1626));
}

#[test]
fn query_has_a_deterministic_order() {
    run(case_at(1693));
}

#[test]
fn query_handles_skip_and_limit_on_a_non_indexed_sort() {
    run(case_at(1847));
}

#[test]
fn query_searches_over_a_nested_object() {
    run(case_at(2021));
}

#[test]
fn querying_many_documents_works() {
    run(case_at(2077));
}

#[test]
fn count_returns_the_correct_amount() {
    run(case_at(2123));
}

#[test]
fn find_documents_by_id_finds_the_documents() {
    run(case_at(2184));
}

#[test]
fn find_documents_by_id_when_the_primary_key_is_underscore_id() {
    run(case_at(2221));
}

#[test]
fn find_documents_by_id_finds_deleted_documents() {
    run(case_at(2292));
}

#[test]
fn with_deleted_true_also_finds_live_documents() {
    run(case_at(2346));
}

#[test]
fn find_documents_by_id_handles_many_ids() {
    run(case_at(2430));
}

#[test]
fn changed_documents_since_gets_the_latest_change() {
    run(case_at(2462));
}

#[test]
fn changed_documents_since_returns_the_correct_amount() {
    run(case_at(2546));
}

#[test]
fn changed_documents_since_iterates_over_checkpoints() {
    run(case_at(2642));
}

#[test]
fn change_stream_emits_exactly_one_event_on_write() {
    run(case_at(2696));
}

#[test]
fn change_stream_emits_all_events() {
    run(case_at(2791));
}

#[test]
fn change_stream_does_not_emit_an_empty_bulk() {
    run(case_at(2881));
}

#[test]
fn store_and_retrieve_an_attachment() {
    run(case_at(2938));
}

#[test]
fn attachment_stub_appears_on_all_fetch_methods() {
    run(case_at(2995));
}

#[test]
fn add_multiple_attachments_one_per_write() {
    run(case_at(3109));
}

#[test]
fn deleted_documents_lose_all_attachments() {
    run(case_at(3193));
}

#[test]
fn load_multiple_attachments() {
    run(case_at(3267));
}

#[test]
fn cleanup_removes_the_deleted_document() {
    run(case_at(3369));
}

#[test]
fn cleanup_removes_all_deleted_documents() {
    run(case_at(3470));
}

#[test]
fn cleanup_returns_false_while_work_remains() {
    run(case_at(3550));
}

#[test]
fn closing_multiple_times_does_not_error() {
    run(case_at(3613));
}

#[test]
fn remove_deletes_all_data() {
    run(case_at(3633));
}

#[test]
fn remove_after_close_throws() {
    run(case_at(3676));
}

#[test]
fn close_after_remove_does_not_throw() {
    run(case_at(3693));
}

#[test]
fn schema_versions_do_not_interfere() {
    run(case_at(4046));
}

// ---------------------------------------------------------------------------
// The meta-tests: the suite must be complete and must be able to fail
// ---------------------------------------------------------------------------

/// Every exercised implementations case must have a `#[test]` in this file.
///
/// The individual tests above are named for readability; this one is what keeps them
/// honest. It fails when a case becomes Exercised and no `#[test]` runs it, which is the
/// only way "declared" could drift from "runs" after all.
#[test]
fn every_exercised_case_is_covered_by_a_test_in_this_file() {
    // The set the tests above collectively cover: every exercised implementations case.
    // Because `run(case_at(N))` is the mechanism, coverage is "N appears in this file".
    let source = include_str!("storage_implementations.rs");
    let mut missing = Vec::new();
    for case in cases() {
        let needle = format!("run(case_at({}))", case.upstream_line);
        if !source.contains(&needle) {
            missing.push(format!("{} ({}): {}", case.id, case.citation(), case.title));
        }
    }
    assert!(
        missing.is_empty(),
        "declared Exercised but no #[test] in storage_implementations.rs:\n  {}",
        missing.join("\n  ")
    );
}

/// The suite must be able to fail: a storage that gets one thing wrong must be caught.
///
/// This is the anti-vacuity check. It runs a deliberately wrong storage through a case
/// that must reject it, and asserts that it does. Without this, a suite whose assertions
/// silently stopped asserting would stay green forever.
#[test]
fn the_suite_catches_a_wrong_storage() {
    use dxdb_storage::{
        BulkWriteResponse, CountResult, DynRxStorageInstance, FindAttachmentResult,
        InstanceCreationParams, PreparedQuery, RxStorage, RxStorageInstance, StorageError,
        StorageSchema,
    };
    use futures::executor::block_on;

    /// A storage that accepts every write and forgets it — wrong in the simplest way.
    struct LiarStorage;

    struct LiarInstance {
        database_name: String,
        collection_name: String,
        schema: StorageSchema,
    }

    impl RxStorage for LiarStorage {
        fn name(&self) -> &str {
            "liar"
        }

        async fn create_storage_instance(
            &self,
            params: InstanceCreationParams,
        ) -> Result<Box<DynRxStorageInstance<'static>>, StorageError> {
            Ok(DynRxStorageInstance::new_box(LiarInstance {
                database_name: params.database_name,
                collection_name: params.collection_name,
                schema: params.schema,
            }))
        }
    }

    impl RxStorageInstance for LiarInstance {
        fn database_name(&self) -> &str {
            &self.database_name
        }
        fn collection_name(&self) -> &str {
            &self.collection_name
        }
        fn schema(&self) -> &StorageSchema {
            &self.schema
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        async fn bulk_write(
            &self,
            _rows: Vec<dxdb_storage::BulkWriteRow>,
            _context: String,
        ) -> Result<BulkWriteResponse, StorageError> {
            // Claims success and writes nothing.
            Ok(BulkWriteResponse::ok())
        }
        async fn find_documents_by_id(
            &self,
            _ids: Vec<String>,
            _with_deleted: bool,
        ) -> Result<Vec<dxdb_storage::DocumentData>, StorageError> {
            Ok(Vec::new())
        }
        async fn query(
            &self,
            _prepared: PreparedQuery,
        ) -> Result<Vec<dxdb_storage::DocumentData>, StorageError> {
            Ok(Vec::new())
        }
        async fn count(&self, _prepared: PreparedQuery) -> Result<CountResult, StorageError> {
            Ok(CountResult {
                count: 0,
                mode: dxdb_storage::CountMode::Slow,
            })
        }
        async fn get_attachment_data(
            &self,
            _document_id: String,
            _attachment_id: String,
            _digest: String,
        ) -> Result<FindAttachmentResult, StorageError> {
            Err(StorageError::MissingAttachment("liar".to_string()))
        }
        fn change_stream(&self) -> dxdb_storage::ChangeStream {
            Box::pin(futures::stream::empty())
        }
        async fn cleanup(&self, _minimum_deleted_time: f64) -> Result<bool, StorageError> {
            Ok(true)
        }
        async fn close(&self) -> Result<(), StorageError> {
            Ok(())
        }
        async fn remove(&self) -> Result<(), StorageError> {
            Ok(())
        }
    }

    // "should write the document" must fail against a storage that does not write.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        block_on(suite::run_exercised_case(&LiarStorage, 270))
    }));
    assert!(
        outcome.is_err(),
        "the suite passed a storage that writes nothing. Every assertion in this file is \
         now suspect: a suite that cannot fail is not a suite."
    );

    // And "should error on conflict" must fail against it too, for a different reason:
    // it never reports the conflict.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        block_on(suite::run_exercised_case(&LiarStorage, 309))
    }));
    assert!(
        outcome.is_err(),
        "the suite passed a storage that never reports a 409 conflict"
    );
}

/// Looks a case up by upstream line, panicking with a useful message if it is gone.
fn case_at(line: u32) -> &'static Case {
    cases()
        .into_iter()
        .find(|case| case.upstream_line == line)
        .unwrap_or_else(|| {
            panic!(
                "no exercised implementations case at upstream line {line}. If upstream \
                 moved, regenerate the declaration table; if the case was reclassified, \
                 update this file's #[test] to match."
            )
        })
}
