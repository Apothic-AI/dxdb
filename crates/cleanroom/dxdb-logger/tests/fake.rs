//! A recording storage shared by both test binaries.
//!
//! One file, `mod fake;`, compiled into each test binary. `allow(dead_code)` is
//! correct here: each binary uses a different subset, and an unused helper in one
//! binary is not a defect. Nothing in this file is part of the crate.
#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use dxdb_logger::{
    AttachmentData, BoxFuture, BulkWriteResponse, ChangedDocuments, Checkpoint, Clock,
    ClockReading, CountResult, InfoReport, InstanceParams, LogRecord, LoggedStorage,
    LoggedStorageInstance, OperationHooks, PreparedQuery, QueryResult, RawDocument, RawWriteRow,
    StorageError, StorageErrorKind, WriteError, WriteErrorKind,
};

/// The message every failing call reports.
pub const FAIL_MESSAGE: &str = "inner storage refused";

/// What the fake storage should do for one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Return the canned value.
    Ok,
    /// Return `StorageError::other(FAIL_MESSAGE)`.
    Fail,
}

/// Calls the fake storage observed, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    BulkWrite { rows: usize, context: String },
    FindDocumentsById { ids: Vec<String>, with_deleted: bool },
    Query { limit: Option<usize> },
    Count,
    Info,
    GetAttachmentData { document_id: String, attachment_id: String },
    GetChangedDocumentsSince { limit: usize, checkpoint: bool },
    Cleanup { minimum_deleted_time_ms: u64 },
    Close,
    Remove,
}

/// A recording fake storage.
#[derive(Debug, Clone)]
pub struct FakeStorage {
    /// Whether `get_changed_documents_since` is supported.
    pub supports_changed: bool,
    /// Whether `info` is supported.
    pub supports_info: bool,
    /// What every call does.
    pub outcome: Outcome,
    /// Per-row write errors returned by a *successful* `bulkWrite`.
    pub write_errors: Vec<WriteError>,
    /// Whether every call yields once before resolving.
    ///
    /// A yielding call is what a test needs to cancel an operation mid-flight;
    /// the non-yielding default keeps the other tests synchronous.
    pub yield_once: bool,
    /// Every call, in the order the fake received it.
    pub calls: Arc<Mutex<Vec<Call>>>,
}

impl Default for FakeStorage {
    fn default() -> Self {
        Self {
            supports_changed: true,
            supports_info: true,
            outcome: Outcome::Ok,
            write_errors: Vec::new(),
            yield_once: false,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl FakeStorage {
    /// A storage where every call succeeds.
    pub fn ok() -> Self {
        Self::default()
    }

    /// A storage where every call fails.
    pub fn failing() -> Self {
        Self {
            outcome: Outcome::Fail,
            ..Self::default()
        }
    }

    /// A storage that yields once inside every call.
    pub fn yielding(mut self) -> Self {
        self.yield_once = true;
        self
    }

    /// Sets the per-row errors `bulkWrite` reports while still succeeding.
    pub fn with_write_errors(mut self, errors: Vec<WriteError>) -> Self {
        self.write_errors = errors;
        self
    }

    /// Disables the two optional operations.
    pub fn without_optional_operations(mut self) -> Self {
        self.supports_changed = false;
        self.supports_info = false;
        self
    }

    /// Wraps this fake in a logger.
    pub fn wrap(
        &self,
        settings: dxdb_logger::LoggerSettings,
        hooks: Arc<dyn OperationHooks>,
        clock: Arc<dyn Clock>,
    ) -> dxdb_logger::LoggerStorage<Self> {
        dxdb_logger::LoggerStorage::new(self.clone(), settings, hooks, clock)
    }

    /// Wraps this fake and creates an instance for `collection`.
    pub fn logged_instance(
        &self,
        collection: &str,
        settings: dxdb_logger::LoggerSettings,
        hooks: Arc<dyn OperationHooks>,
        clock: Arc<dyn Clock>,
    ) -> dxdb_logger::LoggerInstance<FakeInstance> {
        self.wrap(settings, hooks, clock)
            .create_instance(InstanceParams::new("testdb", collection))
            .expect("fake storage creates an instance")
    }

    /// The calls observed so far.
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("call log poisoned").clone()
    }
}

impl LoggedStorage for FakeStorage {
    type Instance = FakeInstance;

    fn storage_name(&self) -> &str {
        "fake"
    }

    fn create_instance(&self, _params: InstanceParams) -> Result<Self::Instance, StorageError> {
        Ok(FakeInstance {
            supports_changed: self.supports_changed,
            supports_info: self.supports_info,
            outcome: self.outcome,
            write_errors: self.write_errors.clone(),
            yield_once: self.yield_once,
            calls: Arc::clone(&self.calls),
        })
    }
}

/// One fake instance, recording every call into the storage's log.
#[derive(Debug, Clone)]
pub struct FakeInstance {
    supports_changed: bool,
    supports_info: bool,
    outcome: Outcome,
    write_errors: Vec<WriteError>,
    yield_once: bool,
    calls: Arc<Mutex<Vec<Call>>>,
}

impl FakeInstance {
    fn record(&self, call: Call) -> Result<(), StorageError> {
        self.calls.lock().expect("call log poisoned").push(call);
        match self.outcome {
            Outcome::Ok => Ok(()),
            Outcome::Fail => Err(StorageError::new(StorageErrorKind::Other, FAIL_MESSAGE)),
        }
    }
}

/// Yields to the executor once, so a caller can cancel before the call resolves.
async fn yield_once() {
    let mut yielded = false;
    futures::future::poll_fn(move |cx| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await;
}

impl LoggedStorageInstance for FakeInstance {
    fn bulk_write(
        &self,
        rows: Vec<RawWriteRow>,
        context: String,
    ) -> BoxFuture<'_, Result<BulkWriteResponse, StorageError>> {
        let recorded = self.record(Call::BulkWrite {
            rows: rows.len(),
            context,
        });
        let response = BulkWriteResponse {
            error: self.write_errors.clone(),
        };
        let yielding = self.yield_once;
        Box::pin(async move {
            if yielding {
                yield_once().await;
            }
            recorded?;
            Ok(response)
        })
    }

    fn find_documents_by_id(
        &self,
        ids: Vec<String>,
        with_deleted: bool,
    ) -> BoxFuture<'_, Result<Vec<RawDocument>, StorageError>> {
        let documents = ids
            .iter()
            .map(|id| RawDocument::from_value(serde_json::json!({ "id": id })))
            .collect();
        let recorded = self.record(Call::FindDocumentsById { ids, with_deleted });
        let yielding = self.yield_once;
        Box::pin(async move {
            if yielding {
                yield_once().await;
            }
            recorded?;
            Ok(documents)
        })
    }

    fn query(&self, prepared: PreparedQuery) -> BoxFuture<'_, Result<QueryResult, StorageError>> {
        let recorded = self.record(Call::Query {
            limit: prepared.limit,
        });
        let yielding = self.yield_once;
        Box::pin(async move {
            if yielding {
                yield_once().await;
            }
            recorded?;
            Ok(QueryResult {
                documents: vec![RawDocument::from_value(serde_json::json!({ "id": "doc-1" }))],
            })
        })
    }

    fn count(&self, _prepared: PreparedQuery) -> BoxFuture<'_, Result<CountResult, StorageError>> {
        let recorded = self.record(Call::Count);
        let yielding = self.yield_once;
        Box::pin(async move {
            if yielding {
                yield_once().await;
            }
            recorded?;
            Ok(CountResult { count: 7 })
        })
    }

    fn supports_info(&self) -> bool {
        self.supports_info
    }

    fn info(&self) -> BoxFuture<'_, Result<InfoReport, StorageError>> {
        let recorded = self.record(Call::Info);
        let yielding = self.yield_once;
        Box::pin(async move {
            if yielding {
                yield_once().await;
            }
            recorded?;
            Ok(InfoReport {
                storage_name: "fake".into(),
                document_count: Some(7),
            })
        })
    }

    fn get_attachment_data(
        &self,
        document_id: String,
        attachment_id: String,
    ) -> BoxFuture<'_, Result<AttachmentData, StorageError>> {
        let recorded = self.record(Call::GetAttachmentData {
            document_id,
            attachment_id,
        });
        let yielding = self.yield_once;
        Box::pin(async move {
            if yielding {
                yield_once().await;
            }
            recorded?;
            Ok(AttachmentData {
                bytes: b"attachment-bytes".to_vec(),
                content_type: "text/plain".into(),
            })
        })
    }

    fn supports_get_changed_documents_since(&self) -> bool {
        self.supports_changed
    }

    fn get_changed_documents_since(
        &self,
        limit: usize,
        checkpoint: Option<Checkpoint>,
    ) -> BoxFuture<'_, Result<ChangedDocuments, StorageError>> {
        let recorded = self.record(Call::GetChangedDocumentsSince {
            limit,
            checkpoint: checkpoint.is_some(),
        });
        let yielding = self.yield_once;
        Box::pin(async move {
            if yielding {
                yield_once().await;
            }
            recorded?;
            Ok(ChangedDocuments {
                documents: vec![RawDocument::from_value(serde_json::json!({ "id": "changed" }))],
                checkpoint: Checkpoint::from_value(serde_json::json!({ "seq": 1 })),
            })
        })
    }

    fn cleanup(&self, minimum_deleted_time_ms: u64) -> BoxFuture<'_, Result<bool, StorageError>> {
        let recorded = self.record(Call::Cleanup {
            minimum_deleted_time_ms,
        });
        let yielding = self.yield_once;
        Box::pin(async move {
            if yielding {
                yield_once().await;
            }
            recorded?;
            Ok(true)
        })
    }

    fn close(&self) -> BoxFuture<'_, Result<(), StorageError>> {
        let recorded = self.record(Call::Close);
        let yielding = self.yield_once;
        Box::pin(async move {
            if yielding {
                yield_once().await;
            }
            recorded
        })
    }

    fn remove(&self) -> BoxFuture<'_, Result<(), StorageError>> {
        let recorded = self.record(Call::Remove);
        let yielding = self.yield_once;
        Box::pin(async move {
            if yielding {
                yield_once().await;
            }
            recorded
        })
    }
}

/// Hooks that append every record to a shared list and count each hook.
#[derive(Debug, Clone, Default)]
pub struct RecordingHooks {
    /// Every record delivered, in order.
    pub records: Arc<Mutex<Vec<LogRecord>>>,
    /// Fire counts in `start / end / error / cancelled` order.
    pub fired: Arc<Mutex<[usize; 4]>>,
}

impl RecordingHooks {
    /// New empty hooks.
    pub fn new() -> Self {
        Self::default()
    }

    /// The records delivered so far.
    pub fn records(&self) -> Vec<LogRecord> {
        self.records.lock().expect("hook log poisoned").clone()
    }

    /// Records for one operation, in delivery order.
    pub fn records_for(&self, operation: dxdb_logger::OperationName) -> Vec<LogRecord> {
        self.records()
            .into_iter()
            .filter(|record| record.operation == operation)
            .collect()
    }

    /// The hook fire counts.
    pub fn fired(&self) -> [usize; 4] {
        *self.fired.lock().expect("hook log poisoned")
    }

    fn push(&self, slot: usize, record: &LogRecord) {
        self.fired.lock().expect("hook log poisoned")[slot] += 1;
        self.records
            .lock()
            .expect("hook log poisoned")
            .push(record.clone());
    }
}

impl OperationHooks for RecordingHooks {
    fn on_operation_start(&self, record: &LogRecord) {
        self.push(0, record);
    }

    fn on_operation_end(&self, record: &LogRecord) {
        self.push(1, record);
    }

    fn on_operation_error(&self, record: &LogRecord) {
        self.push(2, record);
    }

    fn on_operation_cancelled(&self, record: &LogRecord) {
        self.push(3, record);
    }
}

/// A clock that advances by a fixed step on every read.
///
/// Makes a duration an exact, assertable number instead of a tolerance.
#[derive(Debug)]
pub struct SteppingClock {
    step_ns: u64,
    next: AtomicUsize,
    reads: AtomicUsize,
}

impl SteppingClock {
    /// A clock whose first read is `first_ns` and which advances `step_ns` per read.
    pub fn new(first_ns: u64, step_ns: u64) -> Self {
        Self {
            step_ns,
            next: AtomicUsize::new(usize::try_from(first_ns).unwrap_or(usize::MAX)),
            reads: AtomicUsize::new(0),
        }
    }

    /// How many times this clock has been read.
    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

impl Clock for SteppingClock {
    fn read(&self) -> ClockReading {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let value = self.next.fetch_add(
            usize::try_from(self.step_ns).unwrap_or(usize::MAX),
            Ordering::SeqCst,
        );
        ClockReading {
            monotonic_ns: u64::try_from(value).unwrap_or(u64::MAX),
            unix_ns: Some(u64::try_from(value).unwrap_or(u64::MAX)),
        }
    }
}

/// `count` per-row write errors, for tests that count them.
pub fn sample_write_errors(count: usize) -> Vec<WriteError> {
    (0..count)
        .map(|index| WriteError {
            document_id: format!("doc-{index}"),
            kind: WriteErrorKind::Conflict,
        })
        .collect()
}
