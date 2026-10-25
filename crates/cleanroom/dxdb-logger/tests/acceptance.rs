//! The numbered acceptance list from `specs/logger.md`, one test per statement.
//!
//! Each test name carries its number so a reviewer can go from the spec to the
//! evidence. The list is the contract; the implementation is measured against it.

mod fake;

use std::sync::Arc;

use dxdb_logger::{
    LoggedStorage, LoggedStorageInstance, LoggerSettings, OperationDetails, OperationName, Phase,
    PreparedQuery, RawWriteRow, StorageErrorKind,
};
use fake::{FakeStorage, RecordingHooks, SteppingClock};

fn hooks_and_instance(
    storage: &FakeStorage,
    settings: LoggerSettings,
    collection: &str,
) -> (
    RecordingHooks,
    dxdb_logger::LoggerInstance<fake::FakeInstance>,
) {
    let hooks = RecordingHooks::new();
    let instance = storage.logged_instance(
        collection,
        settings,
        Arc::new(hooks.clone()),
        Arc::new(SteppingClock::new(1_000, 250)),
    );
    (hooks, instance)
}

/// Calls every documented operation once and returns each result.
fn call_every_operation(
    instance: &dxdb_logger::LoggerInstance<fake::FakeInstance>,
) -> Vec<Result<(), dxdb_logger::StorageError>> {
    let mut results = Vec::new();
    for operation in OperationName::ALL {
        results.push(futures::executor::block_on(async {
            match operation {
                OperationName::BulkWrite => instance
                    .bulk_write(
                        vec![RawWriteRow::new(
                            "doc-1",
                            dxdb_logger::RawDocument::from_value(
                                serde_json::json!({ "id": "doc-1", "name": "alice" }),
                            ),
                        )],
                        "ctx".into(),
                    )
                    .await
                    .map(|_| ()),
                OperationName::FindDocumentsById => instance
                    .find_documents_by_id(vec!["doc-1".into()], true)
                    .await
                    .map(|_| ()),
                OperationName::Query => instance
                    .query(PreparedQuery {
                        limit: Some(5),
                        skip: 2,
                        index: Some(vec!["name".into()]),
                        ..PreparedQuery::default()
                    })
                    .await
                    .map(|_| ()),
                OperationName::Count => instance.count(PreparedQuery::default()).await.map(|_| ()),
                OperationName::Info => instance.info().await.map(|_| ()),
                OperationName::GetAttachmentData => instance
                    .get_attachment_data("doc-1".into(), "att-1".into())
                    .await
                    .map(|_| ()),
                OperationName::GetChangedDocumentsSince => instance
                    .get_changed_documents_since(
                        3,
                        Some(dxdb_logger::Checkpoint::from_value(
                            serde_json::json!({ "seq": 1 }),
                        )),
                    )
                    .await
                    .map(|_| ()),
                OperationName::Cleanup => instance.cleanup(60_000).await.map(|_| ()),
                OperationName::Close => instance.close().await,
                OperationName::Remove => instance.remove().await,
            }
        }));
    }
    results
}

/// Acceptance 1: every documented operation produces one start and one end
/// record, and the inner storage is called exactly once.
#[test]
fn acceptance_01_all_documented_operations_are_logged() {
    let storage = FakeStorage::ok();
    let (hooks, instance) = hooks_and_instance(&storage, LoggerSettings::default(), "humans");
    let results = call_every_operation(&instance);
    assert!(results.iter().all(Result::is_ok));
    assert_eq!(
        storage.calls().len(),
        OperationName::ALL.len(),
        "each operation reached the inner storage exactly once"
    );
    for operation in OperationName::ALL {
        let records = hooks.records_for(operation);
        assert_eq!(
            records.len(),
            2,
            "{} produces two records",
            operation.as_str()
        );
        assert_eq!(records[0].phase, Phase::Start);
        assert_eq!(records[1].phase, Phase::End);
    }
}

/// Acceptance 2: A disabled operation is silent and still executes.
#[test]
fn acceptance_02_disabled_operation_is_silent_and_unchanged() {
    for disabled in OperationName::ALL {
        let storage = FakeStorage::ok();
        let settings = LoggerSettings::default().with_operation(disabled, false);
        let (hooks, instance) = hooks_and_instance(&storage, settings, "humans");
        let results = call_every_operation(&instance);
        assert!(results.iter().all(Result::is_ok), "calls still succeed");
        assert!(
            hooks.records_for(disabled).is_empty(),
            "{} is silent when disabled",
            disabled.as_str()
        );
        assert_eq!(
            hooks.records().len(),
            (OperationName::ALL.len() - 1) * 2,
            "every other operation is still logged"
        );
    }
}

/// Acceptance 3: The two documented internal collection names are filtered.
#[test]
fn acceptance_03_meta_storage_instances_are_filtered_by_collection_name() {
    for collection in [
        dxdb_logger::INTERNAL_COLLECTION_NAME,
        dxdb_logger::LOCAL_DOCUMENTS_COLLECTION_NAME,
    ] {
        let storage = FakeStorage::ok();
        let (hooks, instance) = hooks_and_instance(
            &storage,
            LoggerSettings::default().with_meta_storage_instances(false),
            collection,
        );
        let results = call_every_operation(&instance);
        assert!(results.iter().all(Result::is_ok));
        assert!(
            hooks.records().is_empty(),
            "{collection} is filtered as an internal storage instance"
        );
        // With the setting on (the default), it is logged like any other.
        let (hooks, instance) = hooks_and_instance(&storage, LoggerSettings::default(), collection);
        let _ = call_every_operation(&instance);
        assert_eq!(
            hooks.records().len(),
            OperationName::ALL.len() * 2,
            "{collection} is logged when the filter is on"
        );
    }
    // An ordinary collection is unaffected by the filter.
    let storage = FakeStorage::ok();
    let (hooks, instance) = hooks_and_instance(
        &storage,
        LoggerSettings::default().with_meta_storage_instances(false),
        "humans",
    );
    let _ = call_every_operation(&instance);
    assert_eq!(hooks.records().len(), OperationName::ALL.len() * 2);
}

/// Acceptance 4: The prefix appears verbatim on every record, and defaults to empty.
#[test]
fn acceptance_04_prefix_appears_on_every_record() {
    for prefix in ["", "my-prefix", "👑 not a product name"] {
        let storage = FakeStorage::ok();
        let (hooks, instance) = hooks_and_instance(
            &storage,
            LoggerSettings::default().with_prefix(prefix),
            "humans",
        );
        let _ = call_every_operation(&instance);
        let records = hooks.records();
        assert!(!records.is_empty());
        for record in records {
            assert_eq!(record.prefix, prefix, "prefix is verbatim on every record");
        }
    }
}

/// Acceptance 5: With a stepping clock, every duration is exactly the step.
#[test]
fn acceptance_05_duration_equals_clock_readings() {
    let storage = FakeStorage::ok();
    let (hooks, instance) = hooks_and_instance(&storage, LoggerSettings::default(), "humans");
    let _ = call_every_operation(&instance);
    let mut ends = 0;
    for record in hooks.records() {
        if record.phase == Phase::End {
            assert_eq!(
                record.duration_ns,
                Some(250),
                "exactly one clock step elapsed"
            );
            ends += 1;
        }
    }
    assert_eq!(
        ends,
        OperationName::ALL.len(),
        "every successful call has a duration"
    );
}

/// Acceptance 6: with timing off, no record carries a duration and a sentry
/// conversion refuses rather than inventing a timestamp.
#[test]
fn acceptance_06_timing_off_never_reads_the_clock() {
    let hooks = RecordingHooks::new();
    let clock = Arc::new(SteppingClock::new(1_000, 250));
    let storage = FakeStorage::ok();
    let instance = storage.logged_instance(
        "humans",
        LoggerSettings::default().with_times(false),
        Arc::new(hooks.clone()),
        clock.clone(),
    );
    let _ = call_every_operation(&instance);
    assert_eq!(clock.reads(), 0, "the clock is never read with timing off");
    for record in hooks.records() {
        assert_eq!(record.duration_ns, None);
        assert_eq!(record.wall_clock_ns, None);
    }
    let records = hooks.records();
    let error = dxdb_logger::SentrySpan::from_records(
        &records[0],
        &records[1],
        dxdb_logger::SpanIds::deterministic(1),
    )
    .expect_err("no wall clock means no span");
    assert_eq!(error, dxdb_logger::LogError::NoWallClock);
}

/// Acceptance 7: The hooks fire one-to-one with phases, and a failed call never fires end.
#[test]
fn acceptance_07_hooks_map_one_to_one_onto_phases() {
    let storage = FakeStorage::failing();
    let (hooks, instance) = hooks_and_instance(&storage, LoggerSettings::default(), "humans");
    let results = call_every_operation(&instance);
    assert!(results.iter().all(Result::is_err));
    let fired = hooks.fired();
    assert_eq!(
        fired[0],
        OperationName::ALL.len(),
        "one start hook per call"
    );
    assert_eq!(
        fired[1], 0,
        "with every call failing, the end hook never fires"
    );
    assert_eq!(
        fired[2],
        OperationName::ALL.len(),
        "one error hook per call"
    );
    assert_eq!(fired[3], 0, "nothing was cancelled");

    let storage = FakeStorage::ok();
    let (hooks, instance) = hooks_and_instance(&storage, LoggerSettings::default(), "humans");
    let _ = call_every_operation(&instance);
    let fired = hooks.fired();
    assert_eq!(fired[0], OperationName::ALL.len());
    assert_eq!(fired[1], OperationName::ALL.len());
    assert_eq!(fired[2], 0);
}

/// Acceptance 8: A failing call returns the inner error unchanged and records it once.
#[test]
fn acceptance_08_errors_pass_through_unchanged_and_are_recorded_once() {
    let storage = FakeStorage::failing();
    let (hooks, instance) = hooks_and_instance(&storage, LoggerSettings::default(), "humans");
    for operation in OperationName::ALL {
        let result = futures::executor::block_on(async {
            match operation {
                OperationName::Query => instance.query(PreparedQuery::default()).await.map(|_| ()),
                OperationName::Count => instance.count(PreparedQuery::default()).await.map(|_| ()),
                OperationName::Info => instance.info().await.map(|_| ()),
                OperationName::BulkWrite => instance
                    .bulk_write(Vec::new(), String::new())
                    .await
                    .map(|_| ()),
                OperationName::FindDocumentsById => instance
                    .find_documents_by_id(Vec::new(), false)
                    .await
                    .map(|_| ()),
                OperationName::GetAttachmentData => instance
                    .get_attachment_data("d".into(), "a".into())
                    .await
                    .map(|_| ()),
                OperationName::GetChangedDocumentsSince => instance
                    .get_changed_documents_since(1, None)
                    .await
                    .map(|_| ()),
                OperationName::Cleanup => instance.cleanup(0).await.map(|_| ()),
                OperationName::Close => instance.close().await,
                OperationName::Remove => instance.remove().await,
            }
        });
        let error = result.expect_err("the fake fails every call");
        assert_eq!(error.message, fake::FAIL_MESSAGE, "the error is unchanged");
        let records = hooks.records_for(operation);
        let errors: Vec<_> = records
            .iter()
            .filter(|record| record.phase == Phase::Error)
            .collect();
        assert_eq!(errors.len(), 1, "exactly one error record");
        assert_eq!(
            errors[0].error.as_ref().map(|error| error.message.clone()),
            Some(fake::FAIL_MESSAGE.to_string()),
            "the record carries the same error"
        );
    }
}

/// Acceptance 9: an unsupported optional operation reports not-supported and
/// never fabricates a result.
#[test]
fn acceptance_09_optional_changed_documents_since_is_not_fabricated() {
    let storage = FakeStorage::ok().without_optional_operations();
    let (hooks, instance) = hooks_and_instance(&storage, LoggerSettings::default(), "humans");
    assert!(
        !LoggedStorageInstance::supports_get_changed_documents_since(&instance),
        "support is reported"
    );
    let result = futures::executor::block_on(instance.get_changed_documents_since(5, None));
    let error = result.expect_err("not supported, not fabricated");
    assert_eq!(error.kind, StorageErrorKind::NotSupported);
    let records = hooks.records_for(OperationName::GetChangedDocumentsSince);
    assert_eq!(records.len(), 2, "the attempt is still logged");
    assert_eq!(records[1].phase, Phase::Error);
    assert!(
        storage.calls().is_empty(),
        "the inner storage was never asked"
    );
}

/// Acceptance 10: `info` exists as a setting and reports not-supported when the
/// inner storage lacks it.
#[test]
fn acceptance_10_info_operation_exists_and_logs_when_supported() {
    let storage = FakeStorage::ok();
    let (hooks, instance) = hooks_and_instance(&storage, LoggerSettings::default(), "humans");
    assert!(LoggedStorageInstance::supports_info(&instance));
    let report = futures::executor::block_on(instance.info()).expect("supported");
    assert_eq!(report.document_count, Some(7));
    let records = hooks.records_for(OperationName::Info);
    assert_eq!(records.len(), 2);
    assert_eq!(
        records[1].details.document_count,
        Some(7),
        "the report's count lands on the terminal record"
    );

    let storage = FakeStorage::ok().without_optional_operations();
    let (hooks, instance) = hooks_and_instance(&storage, LoggerSettings::default(), "humans");
    assert!(!LoggedStorageInstance::supports_info(&instance));
    let error = futures::executor::block_on(instance.info()).expect_err("not supported");
    assert_eq!(error.kind, StorageErrorKind::NotSupported);
    assert_eq!(hooks.records_for(OperationName::Info).len(), 2);
    assert!(storage.calls().is_empty());
}

/// Acceptance 11: Interleaved operations pair by id, in order.
#[test]
fn acceptance_11_interleaved_operations_pair_by_id_in_order() {
    let storage = FakeStorage::ok().yielding();
    let (hooks, instance) = hooks_and_instance(&storage, LoggerSettings::default(), "humans");
    let _ = futures::executor::block_on(async {
        let query = instance.query(PreparedQuery::default());
        let count = instance.count(PreparedQuery::default());
        futures::join!(query, count)
    });
    let records = hooks.records();
    assert_eq!(records.len(), 4);
    assert_eq!(records[0].operation, OperationName::Query);
    assert_eq!(records[1].operation, OperationName::Count);
    assert_eq!(records[2].phase, Phase::End);
    assert_eq!(records[3].phase, Phase::End);
    assert_eq!(records[0].log_id, records[2].log_id, "query pairs");
    assert_eq!(records[1].log_id, records[3].log_id, "count pairs");
    assert!(
        records[0].log_id < records[1].log_id,
        "ids follow call order"
    );
}

/// Acceptance 12: Payloads pass through by value and never appear in a record.
#[test]
fn acceptance_12_payloads_pass_through_without_inclusion() {
    let storage = FakeStorage::ok();
    let (hooks, instance) = hooks_and_instance(&storage, LoggerSettings::default(), "humans");

    let documents = futures::executor::block_on(
        instance.find_documents_by_id(vec!["doc-1".into(), "doc-2".into()], false),
    )
    .expect("ok");
    assert_eq!(
        documents
            .iter()
            .map(|document| document.as_value()["id"].clone())
            .collect::<Vec<_>>(),
        vec![
            serde_json::Value::String("doc-1".into()),
            serde_json::Value::String("doc-2".into()),
        ],
        "documents come back unchanged and in order"
    );

    let query = futures::executor::block_on(instance.query(PreparedQuery::default())).expect("ok");
    assert_eq!(query.documents.len(), 1);

    let count = futures::executor::block_on(instance.count(PreparedQuery::default())).expect("ok");
    assert_eq!(count.count, 7);

    let attachment =
        futures::executor::block_on(instance.get_attachment_data("doc-1".into(), "att-1".into()))
            .expect("ok");
    assert_eq!(attachment.bytes, b"attachment-bytes");
    assert_eq!(attachment.content_type, "text/plain");

    let changed =
        futures::executor::block_on(instance.get_changed_documents_since(1, None)).expect("ok");
    assert_eq!(changed.documents.len(), 1);
    assert_eq!(changed.checkpoint.as_value()["seq"], serde_json::json!(1));

    // A record may carry identifiers (that is the point of `document_ids`) but
    // never a payload value.
    let serialized = serde_json::to_string(
        &hooks
            .records()
            .into_iter()
            .map(|record| {
                serde_json::json!({
                    "details": format!("{:?}", record.details),
                    "collection": record.collection_name,
                    "database": record.database_name,
                    "prefix": record.prefix,
                })
            })
            .collect::<Vec<_>>(),
    )
    .expect("records serialize");
    for forbidden in ["attachment-bytes", "alice", "text/plain"] {
        assert!(
            !serialized.contains(forbidden),
            "record text must not contain {forbidden:?}: {serialized}"
        );
    }

    // And a batch write's ids are capped, with the true count kept.
    let rows: Vec<RawWriteRow> = (0..(OperationDetails::MAX_LOGGED_IDS + 5))
        .map(|index| {
            RawWriteRow::new(
                format!("doc-{index}"),
                dxdb_logger::RawDocument::from_value(
                    serde_json::json!({ "id": format!("doc-{index}") }),
                ),
            )
        })
        .collect();
    let _ = futures::executor::block_on(instance.bulk_write(rows, "ctx".into())).expect("ok");
    let records = hooks.records_for(OperationName::BulkWrite);
    let start = &records[0];
    assert_eq!(
        start.details.document_ids.len(),
        OperationDetails::MAX_LOGGED_IDS
    );
    assert!(start.details.document_ids_truncated);
    assert_eq!(
        start.details.document_count,
        Some(OperationDetails::MAX_LOGGED_IDS + 5),
        "the true count is kept even when the id list is capped"
    );
}

/// Acceptance 13: a completed operation maps to a valid Sentry span, including
/// the failure status mapping.
#[test]
fn acceptance_13_sentry_span_has_the_documented_field_set() {
    let storage = FakeStorage::ok();
    let (hooks, instance) = hooks_and_instance(&storage, LoggerSettings::default(), "humans");
    let _ = call_every_operation(&instance);
    let records = hooks.records();
    let span = dxdb_logger::SentrySpan::from_records(
        &records[0],
        &records[1],
        dxdb_logger::SpanIds::deterministic(records[0].log_id.get()),
    )
    .expect("valid");
    assert_eq!(span.span_id.len(), 16);
    assert_eq!(span.trace_id.len(), 32);
    assert!(span.start_timestamp <= span.timestamp);
    assert_eq!(span.op, "db.bulk_write");
    assert_eq!(span.status, dxdb_logger::SpanStatus::Ok);
    assert_eq!(span.data["db.system"], "dxdb");
    assert_eq!(span.data["db.operation.name"], "bulkWrite");
    assert_eq!(span.data["db.collection.name"], "humans");

    // A failed operation maps its error kind onto the published vocabulary.
    let storage = FakeStorage::failing();
    let (hooks, instance) = hooks_and_instance(&storage, LoggerSettings::default(), "humans");
    let _ = futures::executor::block_on(instance.query(PreparedQuery::default()));
    let records = hooks.records();
    let span = dxdb_logger::SentrySpan::from_records(
        &records[0],
        &records[1],
        dxdb_logger::SpanIds::deterministic(records[0].log_id.get()),
    )
    .expect("valid");
    assert_eq!(span.status, dxdb_logger::SpanStatus::Unknown);
    assert_eq!(span.data["error.kind"], "other");
}

/// Acceptance 14: records reach `tracing` with the documented field names.
///
/// The capture is process-wide (see `fake::tracing_capture`) and the other tests in
/// this binary run in parallel, so this test drives an instance with a database
/// name nothing else uses and filters the capture to it. That keeps the assertion
/// exact without depending on test order or on which tests happened to run first.
#[test]
fn acceptance_14_tracing_receives_the_documented_fields() {
    const UNIQUE_DATABASE: &str = "acceptance-14-tracing-db";

    let events = fake::tracing_capture::install();
    let storage = FakeStorage::ok();
    let hooks = RecordingHooks::new();
    let instance = storage
        .wrap(
            LoggerSettings::default(),
            Arc::new(hooks.clone()),
            Arc::new(SteppingClock::new(1_000, 250)),
        )
        .create_instance(dxdb_logger::InstanceParams::new(UNIQUE_DATABASE, "humans"))
        .expect("instance");

    let _ = futures::executor::block_on(instance.query(PreparedQuery::default()));

    // `%value` in `tracing` renders unquoted, while a bare `&str` field renders
    // quoted, so the marker is the bare name and the field assertions below use
    // the quoted form for the two `&str` fields.
    let marker = format!("database={UNIQUE_DATABASE}");
    let captured: Vec<String> = events
        .lock()
        .expect("capture poisoned")
        .iter()
        .filter(|event| event.contains(&marker))
        .cloned()
        .collect();
    assert_eq!(captured.len(), 2, "one event per record: {captured:?}");
    for event in &captured {
        for field in [
            "log_id=",
            "operation=",
            "phase=",
            "database=",
            "collection=",
            "prefix=",
            "duration_ns=",
        ] {
            assert!(event.contains(field), "event carries {field}: {event}");
        }
    }
    assert!(captured[0].contains("phase=\"start\""), "{}", captured[0]);
    assert!(captured[1].contains("phase=\"end\""), "{}", captured[1]);
    assert!(
        captured[0].contains("operation=\"query\""),
        "{}",
        captured[0]
    );
    assert_eq!(hooks.records().len(), 2, "hooks still fired");
}
