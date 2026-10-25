//! Settings: the documented `settings` object, with the documented defaults.
//!
//! The public documentation says "Be default, all settings are true"
//! (`docs-src/docs/logger.md`), so every default here is on. Mapping the
//! JavaScript object onto two Rust structs is recorded in
//! `docs/CLEANROOM-GAPS.md` G-004.

use crate::record::OperationName;

/// One boolean per documented operation.
///
/// All default to `true`, matching the documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperationToggles {
    /// Log `bulkWrite` calls.
    pub bulk_write: bool,
    /// Log `findDocumentsById` calls.
    pub find_documents_by_id: bool,
    /// Log `query` calls.
    pub query: bool,
    /// Log `count` calls.
    pub count: bool,
    /// Log `info` calls. See `docs/CLEANROOM-GAPS.md` G-005.
    pub info: bool,
    /// Log `getAttachmentData` calls.
    pub get_attachment_data: bool,
    /// Log `getChangedDocumentsSince` calls.
    pub get_changed_documents_since: bool,
    /// Log `cleanup` calls.
    pub cleanup: bool,
    /// Log `close` calls.
    pub close: bool,
    /// Log `remove` calls.
    pub remove: bool,
}

impl Default for OperationToggles {
    fn default() -> Self {
        Self {
            bulk_write: true,
            find_documents_by_id: true,
            query: true,
            count: true,
            info: true,
            get_attachment_data: true,
            get_changed_documents_since: true,
            cleanup: true,
            close: true,
            remove: true,
        }
    }
}

impl OperationToggles {
    /// Every operation disabled. Convenient for enabling one at a time.
    pub const fn none() -> Self {
        Self {
            bulk_write: false,
            find_documents_by_id: false,
            query: false,
            count: false,
            info: false,
            get_attachment_data: false,
            get_changed_documents_since: false,
            cleanup: false,
            close: false,
            remove: false,
        }
    }

    /// Every operation enabled, which is the documented default.
    pub const fn all() -> Self {
        Self {
            bulk_write: true,
            find_documents_by_id: true,
            query: true,
            count: true,
            info: true,
            get_attachment_data: true,
            get_changed_documents_since: true,
            cleanup: true,
            close: true,
            remove: true,
        }
    }

    /// The toggle for `operation`.
    pub const fn is_enabled(&self, operation: OperationName) -> bool {
        match operation {
            OperationName::BulkWrite => self.bulk_write,
            OperationName::FindDocumentsById => self.find_documents_by_id,
            OperationName::Query => self.query,
            OperationName::Count => self.count,
            OperationName::Info => self.info,
            OperationName::GetAttachmentData => self.get_attachment_data,
            OperationName::GetChangedDocumentsSince => self.get_changed_documents_since,
            OperationName::Cleanup => self.cleanup,
            OperationName::Close => self.close,
            OperationName::Remove => self.remove,
        }
    }

    /// Returns these toggles with `operation` enabled or disabled.
    pub fn with(mut self, operation: OperationName, enabled: bool) -> Self {
        match operation {
            OperationName::BulkWrite => self.bulk_write = enabled,
            OperationName::FindDocumentsById => self.find_documents_by_id = enabled,
            OperationName::Query => self.query = enabled,
            OperationName::Count => self.count = enabled,
            OperationName::Info => self.info = enabled,
            OperationName::GetAttachmentData => self.get_attachment_data = enabled,
            OperationName::GetChangedDocumentsSince => self.get_changed_documents_since = enabled,
            OperationName::Cleanup => self.cleanup = enabled,
            OperationName::Close => self.close = enabled,
            OperationName::Remove => self.remove = enabled,
        }
        self
    }

    /// Builds toggles from `enabled(operation)`.
    pub fn from_fn(mut enabled: impl FnMut(OperationName) -> bool) -> Self {
        let mut toggles = Self::none();
        for operation in OperationName::ALL {
            toggles = toggles.with(operation, enabled(operation));
        }
        toggles
    }
}

/// The documented `settings` object. All fields default to the documented value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggerSettings {
    /// "can used to prefix all log strings, default=''".
    pub prefix: String,
    /// "if true, it will log timings...". Default `true`.
    ///
    /// When `false`, no duration is recorded *and the clock is never read*
    /// (`INV-5`); records then carry no wall clock either, and a Sentry
    /// conversion reports `NoWallClock` rather than inventing a timestamp.
    pub times: bool,
    /// "if false, it will not log meta storage instances like used in
    /// replication". Default `true`.
    pub meta_storage_instances: bool,
    /// The per-operation toggles. Default: all enabled.
    pub operations: OperationToggles,
}

impl Default for LoggerSettings {
    fn default() -> Self {
        Self {
            prefix: String::new(),
            times: true,
            meta_storage_instances: true,
            operations: OperationToggles::all(),
        }
    }
}

impl LoggerSettings {
    /// Sets the prefix, as the documented `prefix` setting does.
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Enables or disables timing, as the documented `times` setting does.
    pub const fn with_times(mut self, times: bool) -> Self {
        self.times = times;
        self
    }

    /// Enables or disables meta storage instance logging, as the documented
    /// `metaStorageInstances` setting does.
    pub const fn with_meta_storage_instances(mut self, enabled: bool) -> Self {
        self.meta_storage_instances = enabled;
        self
    }

    /// Replaces the per-operation toggles.
    pub fn with_operations(mut self, operations: OperationToggles) -> Self {
        self.operations = operations;
        self
    }

    /// Enables or disables one operation.
    pub fn with_operation(mut self, operation: OperationName, enabled: bool) -> Self {
        self.operations = self.operations.with(operation, enabled);
        self
    }

    /// Whether `operation` on `collection_name` produces records.
    ///
    /// This is the single place the filters are decided, so `INV-3` has one
    /// mechanism to audit.
    pub fn should_log(&self, operation: OperationName, collection_name: &str) -> bool {
        if !self.operations.is_enabled(operation) {
            return false;
        }
        if !self.meta_storage_instances && crate::storage::meta::is_meta_collection(collection_name)
        {
            return false;
        }
        true
    }
}
