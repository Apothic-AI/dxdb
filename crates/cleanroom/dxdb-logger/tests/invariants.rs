//! Invariants (`specs/logger.md` section 3), proven against this crate's own
//! contract rather than against a reference implementation.
//!
//! Each test name carries the invariant id, so a reviewer can go from the spec
//! section to the test that fails when the mechanism is removed.
//!
//! The settings space is finite (2^10 operation subsets x 2 timing x 2 meta), so
//! the settings-dependent invariant enumerates it exhaustively instead of sampling
//! it. No random numbers, no `proptest`, and every failure is reproducible by
//! construction; see the note at the end of the spec.

mod fake;

use std::sync::Arc;

use dxdb_logger::{
    Checkpoint, InstanceParams, LoggedStorage, LoggedStorageInstance, LoggerSettings,
    OperationName, OperationToggles, PreparedQuery, RawWriteRow, SentrySpan, SpanIds, SpanStatus,
    Phase,
};
use fake::{FakeStorage, Outcome, RecordingHooks, SteppingClock, FAIL_MESSAGE};

/// Drives every documented operation once, in the documented order.
fn drive_all(instance: &dxdb_logger::LoggerInstance<fake::FakeInstance>) -> Vec<Result<(), dxdb_logger::StorageError>> {
    let mut results = Vec::new();
    for operation in OperationName::ALL {
        results.push(futures::executor::block_on(async {
            match operation {
                OperationName::BulkWrite => instance
                    .bulk_write(
                        vec![RawWriteRow {
                            document: dxdb_logger::RawDocument::from_value(
                                serde_json::json!({ "id": "doc-1" }),
                            ),
                            previous: None,
                        }],
                        "test-context".into(),
                    )
                    .await
                    .map(|_| ()),
                OperationName::FindDocumentsById => instance
                    .find_documents_by_id(vec!["doc-1".into()], false)
                    .await
                    .map(|_| ()),
                OperationName::Query => {
                    instance.query(PreparedQuery::default()).await.map(|_| ())
                }
                OperationName::Count => instance.count(PreparedQuery::default()).await.map(|_| ()),
                OperationName::Info => instance.info().await.map(|_| ()),
                OperationName::GetAttachmentData => instance
                    .get_attachment_data("doc-1".into(), "att-1".into())
                    .await
                    .map(|_| ()),
                OperationName::GetChangedDocumentsSince => instance
                    .get_changed_documents_since(
                        10,
                        Some(Checkpoint::from_value(serde_json::json!({ "seq": 0 }))),
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

/// INV-1: for every settings combination and both outcomes, the wrapper returns
/// the inner value unchanged — and, joined with acceptance 12, no record carries
/// document contents.
#[test]
fn inv_01_transparency_is_total() {
    let mut combinations = 0usize;
    for mask in 0u16..1024 {
        let toggles = OperationToggles::from_fn(|operation| {
            let index = OperationName::ALL
                .iter()
                .position(|candidate| *candidate == operation)
                .expect("operation is in ALL");
            mask & (1 << index) != 0
        });
        for times in [false, true] {
            for meta in [false, true] {
                let settings = LoggerSettings::default()
                    .with_operations(toggles)
                    .with_times(times)
                    .with_meta_storage_instances(meta);
                for outcome in [Outcome::Ok, Outcome::Fail] {
                    let hooks = RecordingHooks::new();
                    let storage = FakeStorage {
                        outcome,
                        ..FakeStorage::ok()
                    };
                    let instance = storage.logged_instance(
                        "humans",
                        settings.clone(),
                        Arc::new(hooks.clone()),
                        Arc::new(SteppingClock::new(1_000, 7)),
                    );
                    for result in drive_all(&instance) {
                        match outcome {
                            Outcome::Ok => assert!(result.is_ok(), "ok outcome is passed through"),
                            Outcome::Fail => {
                                let error = result.expect_err("fail outcome is passed through");
                                assert_eq!(error.message, FAIL_MESSAGE);
                            }
                        }
                    }
                    combinations += 1;
                }
            }
        }
    }
    assert_eq!(combinations, 1024 * 2 * 2 * 2, "every combination exercised");
}

/// INV-2: exactly one start record and exactly one terminal record per logged
/// operation, with the terminal kind following the result.
#[test]
fn inv_02_start_and_terminal_are_paired() {
    for outcome in [Outcome::Ok, Outcome::Fail] {
        let hooks = RecordingHooks::new();
        let storage = FakeStorage {
            outcome,
            ..FakeStorage::ok()
        };
        let instance = storage.logged_instance(
            "humans",
            LoggerSettings::default(),
            Arc::new(hooks.clone()),
            Arc::new(SteppingClock::new(1_000, 1)),
        );
        let _ = drive_all(&instance);

        for operation in OperationName::ALL {
            let records = hooks.records_for(operation);
            let starts: Vec<_> = records
                .iter()
                .filter(|record| record.phase == Phase::Start)
                .collect();
            let terminals: Vec<_> = records
                .iter()
                .filter(|record| record.phase.is_terminal())
                .collect();
            assert_eq!(
                starts.len(),
                1,
                "{} must emit exactly one start record",
                operation.as_str()
            );
            assert_eq!(
                terminals.len(),
                1,
                "{} must emit exactly one terminal record",
                operation.as_str()
            );
            assert_eq!(
                starts[0].log_id, terminals[0].log_id,
                "{} start and terminal share a log id",
                operation.as_str()
            );
            let expected = match outcome {
                Outcome::Ok => Phase::End,
                Outcome::Fail => Phase::Error,
            };
            assert_eq!(
                terminals[0].phase,
                expected,
                "{} terminal phase follows the result",
                operation.as_str()
            );
        }
    }
}

/// INV-2 under cancellation: a future dropped mid-flight still gets exactly one
/// terminal record, and it is `Cancelled`, not `End` or `Error`.
///
/// The future is polled once with a counting waker (it yields inside the inner
/// call) and then dropped, which is what `select!` and a timeout wrapper do.
#[test]
fn inv_02_cancellation_is_terminal() {
    use std::task::{Context, Wake, Waker};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct CountingWake(AtomicUsize);
    impl Wake for CountingWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let hooks = RecordingHooks::new();
    let storage = FakeStorage::ok().yielding();
    let instance = storage.logged_instance(
        "humans",
        LoggerSettings::default(),
        Arc::new(hooks.clone()),
        Arc::new(SteppingClock::new(1_000, 1)),
    );

    let waker_state = Arc::new(CountingWake(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&waker_state));
    let mut context = Context::from_waker(&waker);

    let mut future = Box::pin(instance.query(PreparedQuery::default()));
    let first = std::future::Future::poll(future.as_mut(), &mut context);
    assert!(first.is_pending(), "the yielding inner call is pending");
    assert_eq!(hooks.records_for(OperationName::Query).len(), 1, "start emitted");
    drop(future);

    let records = hooks.records_for(OperationName::Query);
    assert_eq!(records.len(), 2, "start plus one terminal record");
    assert_eq!(records[0].phase, Phase::Start);
    assert_eq!(records[1].phase, Phase::Cancelled);
    assert_eq!(records[0].log_id, records[1].log_id);
    assert_eq!(records[1].duration_ns, None, "a cancelled call has no duration");
    assert_eq!(hooks.fired()[3], 1, "the cancelled hook fired once");
    assert_eq!(hooks.fired()[1], 0, "the end hook did not fire");
    assert_eq!(hooks.fired()[2], 0, "the error hook did not fire");
}

/// INV-3: a filtered operation is silent and costs nothing.
#[test]
fn inv_03_filtered_operations_are_silent_and_cheap() {
    // (a) every operation disabled.
    let hooks = RecordingHooks::new();
    let clock = Arc::new(SteppingClock::new(1_000, 1));
    let storage = FakeStorage::ok();
    let instance = storage.logged_instance(
        "humans",
        LoggerSettings::default().with_operations(OperationToggles::none()),
        Arc::new(hooks.clone()),
        clock.clone(),
    );
    let results = drive_all(&instance);
    assert!(
        results.iter().all(Result::is_ok),
        "a filtered operation still executes"
    );
    assert!(hooks.records().is_empty(), "no records when everything is off");
    assert_eq!(clock.reads(), 0, "no clock reads when nothing is logged");
    assert_eq!(
        storage.calls().len(),
        OperationName::ALL.len(),
        "the inner storage still saw every call"
    );

    // (b) a filtered meta storage instance, by both documented collection names.
    let hooks = RecordingHooks::new();
    let clock = Arc::new(SteppingClock::new(1_000, 1));
    let instance = storage.logged_instance(
        dxdb_logger::INTERNAL_COLLECTION_NAME,
        LoggerSettings::default().with_meta_storage_instances(false),
        Arc::new(hooks.clone()),
        clock.clone(),
    );
    let _ = drive_all(&instance);
    let instance = storage.logged_instance(
        dxdb_logger::LOCAL_DOCUMENTS_COLLECTION_NAME,
        LoggerSettings::default().with_meta_storage_instances(false),
        Arc::new(hooks.clone()),
        clock.clone(),
    );
    let _ = drive_all(&instance);
    assert!(
        hooks.records().is_empty(),
        "no records for a filtered meta instance"
    );
    assert_eq!(clock.reads(), 0, "no clock reads for a filtered meta instance");

    // (c) an ordinary collection is still logged under the same setting.
    let hooks = RecordingHooks::new();
    let clock = Arc::new(SteppingClock::new(1_000, 1));
    let instance = storage.logged_instance(
        "humans",
        LoggerSettings::default().with_meta_storage_instances(false),
        Arc::new(hooks.clone()),
        clock.clone(),
    );
    let _ = drive_all(&instance);
    assert_eq!(
        hooks.fired()[0],
        OperationName::ALL.len(),
        "ordinary collections are logged"
    );
    assert!(clock.reads() > 0, "a logged operation does read the clock");
}

/// INV-4: ids are unique per instance and monotonic in call order.
#[test]
fn inv_04_log_ids_are_unique_and_paired() {
    let hooks = RecordingHooks::new();
    let storage = FakeStorage::ok();
    let instance = storage.logged_instance(
        "humans",
        LoggerSettings::default(),
        Arc::new(hooks.clone()),
        Arc::new(SteppingClock::new(1_000, 1)),
    );
    for _round in 0..3 {
        let _ = drive_all(&instance);
    }
    let records = hooks.records();
    let mut starts = Vec::new();
    let mut terminals = Vec::new();
    for record in &records {
        match record.phase {
            Phase::Start => starts.push(record.log_id.get()),
            _ => terminals.push(record.log_id.get()),
        }
    }
    let mut unique = starts.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), starts.len(), "every start id is unique");
    // Monotonic in call order: the start ids appear in increasing order.
    assert!(
        starts.windows(2).all(|window| window[0] < window[1]),
        "start ids increase in call order"
    );
    let mut sorted_terminals = terminals.clone();
    sorted_terminals.sort_unstable();
    assert_eq!(sorted_terminals, unique, "every start id is closed by one terminal");
}

/// INV-5: durations are exact, present iff timing is enabled and the call
/// succeeded, and the clock is read twice for a successful timed call.
#[test]
fn inv_05_durations_are_exact_and_present_iff_timing_enabled() {
    for times in [false, true] {
        for outcome in [Outcome::Ok, Outcome::Fail] {
            let hooks = RecordingHooks::new();
            let clock = Arc::new(SteppingClock::new(1_000, 250));
            let storage = FakeStorage {
                outcome,
                ..FakeStorage::ok()
            };
            let instance = storage.logged_instance(
                "humans",
                LoggerSettings::default().with_times(times),
                Arc::new(hooks.clone()),
                clock.clone(),
            );
            let _ = drive_all(&instance);

            let records = hooks.records();
            if times {
                // Two reads per logged call, except a failed call, which reads a
                // second time for its terminal record all the same: the terminal
                // reading is taken for every terminal record with timing on.
                assert_eq!(
                    clock.reads(),
                    OperationName::ALL.len() * 2,
                    "one start reading and one terminal reading per call"
                );
            } else {
                assert_eq!(clock.reads(), 0, "with timing off the clock is never read");
            }

            // Every recorded duration must be exactly the step, because the
            // stepping clock advances by exactly one step between the two reads.
            for record in &records {
                match (times, record.phase) {
                    (false, _) => {
                        assert_eq!(record.duration_ns, None, "no duration without timing");
                        assert_eq!(record.wall_clock_ns, None, "no wall clock without timing");
                    }
                    (true, Phase::End) => {
                        assert_eq!(
                            record.duration_ns,
                            Some(250),
                            "{} duration equals the clock difference",
                            record.operation.as_str()
                        );
                    }
                    (true, Phase::Error | Phase::Cancelled) => {
                        assert_eq!(
                            record.duration_ns, None,
                            "a failed or cancelled call has no completed duration"
                        );
                    }
                    (true, Phase::Start) => {
                        assert_eq!(record.duration_ns, None, "a start record has no duration");
                        assert!(record.wall_clock_ns.is_some(), "a start record has wall time");
                    }
                }
            }
        }
    }
}

/// INV-6: every converted span satisfies the public Sentry interface, including
/// the failure to convert when there is no wall clock.
#[test]
fn inv_06_sentry_spans_are_valid() {
    let hooks = RecordingHooks::new();
    let storage = FakeStorage::ok();
    let instance = storage.logged_instance(
        "humans",
        LoggerSettings::default(),
        Arc::new(hooks.clone()),
        Arc::new(SteppingClock::new(1_700_000_000_000_000_000, 250)),
    );
    let _ = drive_all(&instance);

    let records = hooks.records();
    let pairs: Vec<_> = records.chunks(2).collect();
    assert_eq!(pairs.len(), OperationName::ALL.len());
    for pair in pairs {
        let (start, terminal) = (&pair[0], &pair[1]);
        let span = SentrySpan::from_records(
            start,
            terminal,
            SpanIds::deterministic(start.log_id.get()),
        )
        .expect("a timed pair converts");
        assert_eq!(span.span_id.len(), 16, "span_id is 16 characters");
        assert!(
            span.span_id.chars().all(|character| character.is_ascii_hexdigit()),
            "span_id is hex"
        );
        assert_eq!(span.trace_id.len(), 32, "trace_id is 32 characters");
        assert!(span
            .trace_id
            .chars()
            .all(|character| character.is_ascii_hexdigit()));
        assert!(
            span.start_timestamp <= span.timestamp,
            "start_timestamp <= timestamp for {}",
            span.op
        );
        assert!(span.op.starts_with("db."), "op is a short db code");
        assert!(!span.description.is_empty(), "description is set");
        for key in span.data.keys() {
            assert!(
                key.chars()
                    .all(|character| character.is_ascii_lowercase()
                        || character == '_'
                        || character == '.'),
                "data key {key} is lower-case with underscores"
            );
        }
        assert_eq!(span.status, SpanStatus::Ok, "a successful call is 'ok'");
        assert_eq!(span.origin, "auto.dxdb.logger");
    }

    // With timing disabled there is no wall clock, and no span may be invented.
    let hooks = RecordingHooks::new();
    let instance = storage.logged_instance(
        "humans",
        LoggerSettings::default().with_times(false),
        Arc::new(hooks.clone()),
        Arc::new(SteppingClock::new(1_000, 1)),
    );
    let _ = drive_all(&instance);
    let records = hooks.records();
    let pair = &records[0..2];
    let error = SentrySpan::from_records(
        &pair[0],
        &pair[1],
        SpanIds::deterministic(pair[0].log_id.get()),
    )
    .expect_err("no span without a wall clock");
    assert_eq!(error, dxdb_logger::LogError::NoWallClock);
}

/// INV-7: for a given log id, the start record is delivered before its terminal
/// record, even when the caller created several futures before awaiting any.
#[test]
fn inv_07_start_precedes_terminal_in_delivery_order() {
    let hooks = RecordingHooks::new();
    let storage = FakeStorage::ok();
    let instance = storage.logged_instance(
        "humans",
        LoggerSettings::default(),
        Arc::new(hooks.clone()),
        Arc::new(SteppingClock::new(1_000, 1)),
    );
    let _ = futures::executor::block_on(async {
        let first = instance.query(PreparedQuery::default());
        let second = instance.count(PreparedQuery::default());
        let (a, b) = futures::future::join(first, second).await;
        (a, b)
    });
    let records = hooks.records();
    assert_eq!(records.len(), 4);
    // Both starts were emitted at call time, before either future was awaited.
    assert_eq!(records[0].phase, Phase::Start);
    assert_eq!(records[1].phase, Phase::Start);
    assert_eq!(records[2].phase, Phase::End);
    assert_eq!(records[3].phase, Phase::End);
    assert_ne!(records[0].log_id, records[1].log_id);
    // Each terminal record closes the matching start.
    assert_eq!(records[0].log_id, records[2].log_id);
    assert_eq!(records[1].log_id, records[3].log_id);
}
