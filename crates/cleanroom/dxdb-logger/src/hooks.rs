//! The documented callbacks: `onOperationStart` / `onOperationEnd` /
//! `onOperationError`.
//!
//! The public docs show three optional closures taking
//! `(operationsName, logId, args)` plus the error for the third. dxdb models that
//! as one trait with three defaulted methods, because a struct of three optional
//! closures is a worse Rust API than a trait with defaults
//! (`docs/CLEANROOM-GAPS.md` G-004).

use crate::record::LogRecord;

/// The three documented hook points.
///
/// Hooks are called synchronously, in order, by the thread performing the
/// operation, so a hook that appends to a `Vec` observes start before terminal
/// for each operation (`INV-7`).
///
/// A panicking hook propagates to the caller. That is deliberate: a logger that
/// swallows panics corrupts the caller's error handling
/// (`specs/logger.md` section 4).
pub trait OperationHooks: Send + Sync + std::fmt::Debug {
    /// Called before the inner storage is asked to do the work.
    fn on_operation_start(&self, _record: &LogRecord) {}

    /// Called after the inner storage returned a value.
    ///
    /// Never called for a failed operation, which fires
    /// [`Self::on_operation_error`] instead (`INV-2`).
    fn on_operation_end(&self, _record: &LogRecord) {}

    /// Called after the inner storage returned an error.
    fn on_operation_error(&self, _record: &LogRecord) {}

    /// Called when the caller dropped the operation before it finished.
    ///
    /// Not in the documented callback trio, because a JavaScript `Promise` has no
    /// cancellation and the paid plugin could not have had one. In Rust a future
    /// that is dropped mid-flight is a normal event (`select!`, a timeout wrapper,
    /// a cancelled query), and leaving its start record unpaired would break
    /// `INV-2`. The default is a no-op, so a caller that never cancels is
    /// unaffected.
    fn on_operation_cancelled(&self, _record: &LogRecord) {}
}

/// Hooks that do nothing, for callers that only want `tracing` output.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopHooks;

impl OperationHooks for NoopHooks {}

#[cfg(test)]
mod tests {
    use super::{NoopHooks, OperationHooks};
    use crate::record::{LogId, LogRecord, OperationDetails, OperationName, Phase};

    fn record() -> LogRecord {
        LogRecord {
            log_id: LogId::FIRST,
            operation: OperationName::Query,
            phase: Phase::Start,
            database_name: "db".into(),
            collection_name: "humans".into(),
            prefix: String::new(),
            wall_clock_ns: None,
            duration_ns: None,
            details: OperationDetails::default(),
            error: None,
        }
    }

    #[test]
    fn default_hooks_do_nothing() {
        let hooks = NoopHooks;
        let record = record();
        hooks.on_operation_start(&record);
        hooks.on_operation_end(&record);
        hooks.on_operation_error(&record);
    }
}
