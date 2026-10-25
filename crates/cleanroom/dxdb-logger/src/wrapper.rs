//! The wrapper: the logger as a storage decorator.
//!
//! Two mechanisms carry the invariants, and everything else delegates:
//!
//! * [`LoggerInstance::open`] is the only place a start record is produced, and
//!   it runs inside the returned future, so a future that is never polled emits
//!   nothing;
//! * [`TerminalGuard`] is the only place a terminal record is produced. It fires
//!   on normal completion, and its `Drop` fires with [`Phase::Cancelled`] if the
//!   caller dropped the future mid-flight. That is what makes "exactly one start
//!   and exactly one terminal record" (`INV-2`) true under `select!` and
//!   timeouts, not merely on the happy path.
//!
//! The filters are decided in one place ([`LoggerSettings::should_log`]) before
//! any clock read, so a filtered operation is silent *and* costs nothing
//! (`INV-3`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::clock::Clock;
use crate::hooks::OperationHooks;
use crate::record::{LogId, LogRecord, OperationDetails, OperationName, Phase};
use crate::settings::LoggerSettings;
use crate::storage::{
    AttachmentData, BoxFuture, BulkWriteResponse, ChangedDocuments, Checkpoint, CountResult,
    InfoReport, InstanceParams, LoggedStorage, LoggedStorageInstance, PreparedQuery, QueryResult,
    RawDocument, RawWriteRow, StorageError,
};

/// The name a wrapped storage reports.
///
/// The documentation says the logger "can be wrapped around any RxStorage", so
/// the wrapper identifies itself rather than hiding: a caller inspecting the
/// storage name can see that logging is in the stack.
pub const WRAPPED_STORAGE_NAME: &str = "dxdb-logger";

/// A storage wrapped for logging.
///
/// The inner storage is kept by value and every result is returned unchanged.
#[derive(Debug)]
pub struct LoggerStorage<S> {
    inner: S,
    settings: LoggerSettings,
    hooks: Arc<dyn OperationHooks>,
    clock: Arc<dyn Clock>,
}

impl<S> LoggerStorage<S> {
    /// Wraps `inner`.
    ///
    /// The three collaborators are passed in rather than defaulted so a target
    /// without a clock (`wasm32-unknown-unknown`) still has a complete
    /// configuration, and so tests can drive time and hooks deterministically.
    pub fn new(
        inner: S,
        settings: LoggerSettings,
        hooks: Arc<dyn OperationHooks>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            inner,
            settings,
            hooks,
            clock,
        }
    }

    /// The wrapped storage.
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// The active settings.
    pub fn settings(&self) -> &LoggerSettings {
        &self.settings
    }

    /// The active clock.
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }
}

impl<S: LoggedStorage> LoggedStorage for LoggerStorage<S> {
    type Instance = LoggerInstance<S::Instance>;

    fn storage_name(&self) -> &str {
        WRAPPED_STORAGE_NAME
    }

    fn create_instance(&self, params: InstanceParams) -> Result<Self::Instance, StorageError> {
        // Creating an instance is not one of the ten documented operations, so it
        // is not logged (`specs/logger.md` section 1.2). It only records which
        // database and collection the instance belongs to, for later records.
        let inner = self.inner.create_instance(params.clone())?;
        Ok(LoggerInstance {
            inner,
            settings: self.settings.clone(),
            hooks: Arc::clone(&self.hooks),
            clock: Arc::clone(&self.clock),
            database_name: params.database_name,
            collection_name: params.collection_name,
            next_log_id: AtomicU64::new(LogId::FIRST.get()),
        })
    }
}

/// One collection's logged storage instance.
#[derive(Debug)]
pub struct LoggerInstance<I> {
    inner: I,
    settings: LoggerSettings,
    hooks: Arc<dyn OperationHooks>,
    clock: Arc<dyn Clock>,
    database_name: String,
    collection_name: String,
    next_log_id: AtomicU64,
}

/// A logged operation in flight.
#[derive(Debug, Clone)]
struct Frame {
    log_id: LogId,
    details: OperationDetails,
    start_monotonic_ns: u64,
    start_wall_ns: Option<u64>,
}

impl<I> LoggerInstance<I> {
    /// The wrapped instance.
    pub fn inner(&self) -> &I {
        &self.inner
    }

    /// The collection name this instance logs.
    pub fn collection_name(&self) -> &str {
        &self.collection_name
    }

    /// The database name this instance logs.
    pub fn database_name(&self) -> &str {
        &self.database_name
    }

    /// Decides whether to log, and if so takes the start reading and emits the
    /// start record.
    ///
    /// Called from inside the returned future, so a future that is never polled
    /// emits nothing and reads no clock. Returns `None` when the operation is
    /// filtered out, in which case the clock is never read (`INV-3`).
    fn open(&self, operation: OperationName, details: OperationDetails) -> Option<Frame> {
        if !self.settings.should_log(operation, &self.collection_name) {
            return None;
        }
        let log_id = LogId(self.next_log_id.fetch_add(1, Ordering::Relaxed));
        let reading = if self.settings.times {
            Some(self.clock.read())
        } else {
            None
        };
        let frame = Frame {
            log_id,
            details,
            start_monotonic_ns: reading.map_or(0, |r| r.monotonic_ns),
            start_wall_ns: reading.and_then(|r| r.unix_ns),
        };
        let record = LogRecord {
            log_id: frame.log_id,
            operation,
            phase: Phase::Start,
            database_name: self.database_name.clone(),
            collection_name: self.collection_name.clone(),
            prefix: self.settings.prefix.clone(),
            wall_clock_ns: frame.start_wall_ns,
            duration_ns: None,
            details: frame.details.clone(),
            error: None,
        };
        self.deliver(&record);
        Some(frame)
    }

    /// Emits the terminal record for a completed call and returns its result
    /// unchanged.
    ///
    /// `Phase::End` for `Ok`, `Phase::Error` for `Err`, never both (`INV-2`).
    /// A duration is recorded only for a successful call with timing enabled, and
    /// it is the difference of two readings from the same clock (`INV-5`).
    fn finish<T>(
        &self,
        operation: OperationName,
        frame: Frame,
        result: Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        let reading = if self.settings.times {
            Some(self.clock.read())
        } else {
            None
        };
        let (phase, error) = match &result {
            Ok(_) => (Phase::End, None),
            Err(error) => (Phase::Error, Some(error.clone())),
        };
        let duration_ns = match (phase, reading) {
            (Phase::End, Some(reading)) => {
                Some(reading.monotonic_ns.saturating_sub(frame.start_monotonic_ns))
            }
            _ => None,
        };
        let record = LogRecord {
            log_id: frame.log_id,
            operation,
            phase,
            database_name: self.database_name.clone(),
            collection_name: self.collection_name.clone(),
            prefix: self.settings.prefix.clone(),
            wall_clock_ns: reading.and_then(|r| r.unix_ns),
            duration_ns,
            details: frame.details,
            error,
        };
        self.deliver(&record);
        result
    }

    /// Emits the terminal record for a future the caller dropped.
    ///
    /// No duration: the call never returned, so there is no completed interval to
    /// report (`INV-5`). The wall clock is read only when timing is enabled, for
    /// the same reason as everywhere else.
    fn finish_cancelled(&self, operation: OperationName, frame: Frame) {
        let reading = if self.settings.times {
            Some(self.clock.read())
        } else {
            None
        };
        let record = LogRecord {
            log_id: frame.log_id,
            operation,
            phase: Phase::Cancelled,
            database_name: self.database_name.clone(),
            collection_name: self.collection_name.clone(),
            prefix: self.settings.prefix.clone(),
            wall_clock_ns: reading.and_then(|r| r.unix_ns),
            duration_ns: None,
            details: frame.details,
            error: None,
        };
        self.deliver(&record);
    }

    /// Sends one record to `tracing` and to the hooks.
    ///
    /// Hooks are called synchronously and in record order, so a hook that appends
    /// to a shared list observes a start record before its terminal record
    /// (`INV-7`).
    fn deliver(&self, record: &LogRecord) {
        emit_tracing(record);
        match record.phase {
            Phase::Start => self.hooks.on_operation_start(record),
            Phase::End => self.hooks.on_operation_end(record),
            Phase::Error => self.hooks.on_operation_error(record),
            Phase::Cancelled => self.hooks.on_operation_cancelled(record),
        }
    }
}

/// Holds an open operation and guarantees its terminal record.
///
/// `complete` discharges the guard. If the guard is dropped instead — because the
/// caller dropped the future mid-flight — `Drop` emits `Phase::Cancelled`, so a
/// start record is never left unpaired (`INV-2`).
struct TerminalGuard<'a, I> {
    instance: &'a LoggerInstance<I>,
    operation: OperationName,
    frame: Option<Frame>,
}

impl<'a, I> TerminalGuard<'a, I> {
    fn new(
        instance: &'a LoggerInstance<I>,
        operation: OperationName,
        frame: Option<Frame>,
    ) -> Self {
        Self {
            instance,
            operation,
            frame,
        }
    }

    /// The open frame, if this operation is being logged.
    ///
    /// Operations use it to refine the argument summary with information only the
    /// response can supply, such as a `bulkWrite` call's per-row error count.
    fn frame_mut(&mut self) -> Option<&mut Frame> {
        self.frame.as_mut()
    }

    /// Emits the terminal record for `result` and disarms the guard.
    fn complete<T>(mut self, result: Result<T, StorageError>) -> Result<T, StorageError> {
        match self.frame.take() {
            Some(frame) => self.instance.finish(self.operation, frame, result),
            None => result,
        }
    }
}

impl<I> Drop for TerminalGuard<'_, I> {
    fn drop(&mut self) {
        if let Some(frame) = self.frame.take() {
            self.instance.finish_cancelled(self.operation, frame);
        }
    }
}

/// Emits one record as a `tracing` event with the documented field names.
///
/// Field names are the contract the tests assert; the values are chosen so a
/// subscriber can filter and a span exporter can convert without parsing the
/// message.
fn emit_tracing(record: &LogRecord) {
    tracing::info!(
        target: "dxdb_logger",
        log_id = record.log_id.get(),
        operation = record.operation.as_str(),
        phase = record.phase.as_str(),
        database = %record.database_name,
        collection = %record.collection_name,
        prefix = %record.prefix,
        duration_ns = ?record.duration_ns,
        document_count = ?record.details.document_count,
        error_kind = ?record.error.as_ref().map(|error| error.kind.as_str()),
        "storage operation",
    );
}

impl<I: LoggedStorageInstance> LoggedStorageInstance for LoggerInstance<I> {
    fn bulk_write(
        &self,
        rows: Vec<RawWriteRow>,
        context: String,
    ) -> BoxFuture<'_, Result<BulkWriteResponse, StorageError>> {
        let details = OperationDetails {
            document_count: Some(rows.len()),
            context: Some(context.clone()),
            ..OperationDetails::default()
        };
        Box::pin(async move {
            let mut guard = TerminalGuard::new(
                self,
                OperationName::BulkWrite,
                self.open(OperationName::BulkWrite, details),
            );
            let result = self.inner.bulk_write(rows, context).await;
            // A partial write is a successful call: the per-document errors are
            // counted on the record and the response is returned unchanged
            // (`specs/logger.md` section 4).
            if let (Some(frame), Ok(response)) = (guard.frame_mut(), result.as_ref()) {
                frame.details.error_count = Some(response.error.len());
            }
            guard.complete(result)
        })
    }

    fn find_documents_by_id(
        &self,
        ids: Vec<String>,
        with_deleted: bool,
    ) -> BoxFuture<'_, Result<Vec<RawDocument>, StorageError>> {
        let mut details = OperationDetails {
            with_deleted: Some(with_deleted),
            ..OperationDetails::default()
        };
        details.set_document_ids(ids.iter().cloned());
        Box::pin(async move {
            let guard = TerminalGuard::new(
                self,
                OperationName::FindDocumentsById,
                self.open(OperationName::FindDocumentsById, details),
            );
            let result = self.inner.find_documents_by_id(ids, with_deleted).await;
            guard.complete(result)
        })
    }

    fn query(&self, prepared: PreparedQuery) -> BoxFuture<'_, Result<QueryResult, StorageError>> {
        let details = OperationDetails {
            limit: prepared.limit,
            skip: prepared.skip,
            index: prepared.index.clone(),
            ..OperationDetails::default()
        };
        Box::pin(async move {
            let guard = TerminalGuard::new(
                self,
                OperationName::Query,
                self.open(OperationName::Query, details),
            );
            let result = self.inner.query(prepared).await;
            guard.complete(result)
        })
    }

    fn count(&self, prepared: PreparedQuery) -> BoxFuture<'_, Result<CountResult, StorageError>> {
        let details = OperationDetails {
            limit: prepared.limit,
            skip: prepared.skip,
            index: prepared.index.clone(),
            ..OperationDetails::default()
        };
        Box::pin(async move {
            let guard = TerminalGuard::new(
                self,
                OperationName::Count,
                self.open(OperationName::Count, details),
            );
            let result = self.inner.count(prepared).await;
            guard.complete(result)
        })
    }

    fn supports_info(&self) -> bool {
        self.inner.supports_info()
    }

    fn info(&self) -> BoxFuture<'_, Result<InfoReport, StorageError>> {
        Box::pin(async move {
            let mut guard = TerminalGuard::new(
                self,
                OperationName::Info,
                self.open(OperationName::Info, OperationDetails::default()),
            );
            let result = self.inner.info().await;
            if let (Some(frame), Ok(report)) = (guard.frame_mut(), result.as_ref()) {
                frame.details.document_count = report.document_count;
            }
            guard.complete(result)
        })
    }

    fn get_attachment_data(
        &self,
        document_id: String,
        attachment_id: String,
    ) -> BoxFuture<'_, Result<AttachmentData, StorageError>> {
        let mut details = OperationDetails {
            attachment_id: Some(attachment_id.clone()),
            ..OperationDetails::default()
        };
        details.set_document_ids([document_id.clone()]);
        Box::pin(async move {
            let guard = TerminalGuard::new(
                self,
                OperationName::GetAttachmentData,
                self.open(OperationName::GetAttachmentData, details),
            );
            let result = self
                .inner
                .get_attachment_data(document_id, attachment_id)
                .await;
            guard.complete(result)
        })
    }

    fn supports_get_changed_documents_since(&self) -> bool {
        self.inner.supports_get_changed_documents_since()
    }

    fn get_changed_documents_since(
        &self,
        limit: usize,
        checkpoint: Option<Checkpoint>,
    ) -> BoxFuture<'_, Result<ChangedDocuments, StorageError>> {
        let details = OperationDetails {
            limit: Some(limit),
            checkpoint_present: Some(checkpoint.is_some()),
            ..OperationDetails::default()
        };
        Box::pin(async move {
            let guard = TerminalGuard::new(
                self,
                OperationName::GetChangedDocumentsSince,
                self.open(OperationName::GetChangedDocumentsSince, details),
            );
            let result = self.inner.get_changed_documents_since(limit, checkpoint).await;
            guard.complete(result)
        })
    }

    fn cleanup(
        &self,
        minimum_deleted_time_ms: u64,
    ) -> BoxFuture<'_, Result<bool, StorageError>> {
        let details = OperationDetails {
            minimum_deleted_time_ms: Some(minimum_deleted_time_ms),
            ..OperationDetails::default()
        };
        Box::pin(async move {
            let guard = TerminalGuard::new(
                self,
                OperationName::Cleanup,
                self.open(OperationName::Cleanup, details),
            );
            let result = self.inner.cleanup(minimum_deleted_time_ms).await;
            guard.complete(result)
        })
    }

    fn close(&self) -> BoxFuture<'_, Result<(), StorageError>> {
        Box::pin(async move {
            let guard = TerminalGuard::new(
                self,
                OperationName::Close,
                self.open(OperationName::Close, OperationDetails::default()),
            );
            let result = self.inner.close().await;
            guard.complete(result)
        })
    }

    fn remove(&self) -> BoxFuture<'_, Result<(), StorageError>> {
        Box::pin(async move {
            let guard = TerminalGuard::new(
                self,
                OperationName::Remove,
                self.open(OperationName::Remove, OperationDetails::default()),
            );
            let result = self.inner.remove().await;
            guard.complete(result)
        })
    }
}
