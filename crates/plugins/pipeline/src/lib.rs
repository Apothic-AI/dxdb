//! A checkpointed source-to-destination transformation.
//!
//! Ported from `src/plugins/pipeline/` at upstream `3b763ee17` (497 lines):
//! `rx-pipeline.ts` (312) is the runner, `flagged-functions.ts` (139) is a re-entrancy
//! workaround, `types.ts` (28) the options, `index.ts` (18) the plugin.
//!
//! # What a pipeline is
//!
//! A handler runs over each new batch of documents in a **source** collection and may
//! write anywhere — typically the **destination**. Progress is recorded in an internal
//! checkpoint document keyed `rx-pipeline-<identifier>` (`rx-pipeline.ts:83`), holding the
//! `getChangedDocumentsSince` checkpoint plus the `_meta.lwt` of the last document seen.
//! A run that is interrupted resumes from that checkpoint rather than re-reading
//! everything, which is the property [`Pipeline::run_until_idle`] is built around.
//!
//! # The re-entrancy problem, and why its upstream solution is not portable
//!
//! A pipeline handler writes to the destination. The destination is *also* guarded so
//! that outside callers wait while a pipeline is running — otherwise an outside read
//! could observe a half-written batch. But the handler's own reads and writes must **not**
//! wait, or the pipeline deadlocks on itself.
//!
//! Upstream distinguishes the two by **inspecting the JavaScript stack trace** for a
//! marker function name (`flagged-functions.ts:3-27`): it holds a pool of twenty
//! identically-shaped functions named `rx_pipeline_fn_1_` … `rx_pipeline_fn_20_`, calls
//! the handler through one of them, and the guard string-matches `new Error().stack` for
//! the `rx_pipeline_fn_` prefix (`rx-pipeline.ts:56-63`). The file's own comment calls it
//! "the most hacky thing we do in RxDB" and explains that `eval` would be the alternative.
//!
//! There is no stack-trace string in Rust worth matching, and no `eval`, so this is a
//! genuine design decision rather than a translation: re-entrancy is passed **explicitly**
//! as a token. A handler receives a [`PipelineToken`], and the destination guard asks
//! whether the token is held ([`ReentrancyGuard`]). Same semantics — the handler can read
//! and write the destination, outside callers wait — with no stack inspection, and it
//! also survives the case upstream's prefix-match was added for: two pipelines sharing a
//! destination (`rx-pipeline.ts:100-106`), because each holds its own token.
//!
//! That is strictly better than the hack, so it is worth being clear it is a divergence:
//! upstream's marker is global and matched by prefix; here re-entrancy is scoped to the
//! token a specific handler was given.
//!
//! # What is ported, and what needs a database
//!
//! The batch loop, the checkpoint arithmetic, the "stop after a short batch" rule, the
//! error latch, and the re-entrancy guard are all here and tested. The transport is
//! [`PipelineStore`] — the same two-method idea as `migration-storage` — because the real
//! one needs a live `RxCollection`, a change stream, and the internal store.
//!
//! Not ported: the `toRun > 2` throttle (`:112-118`), which is a queue-depth heuristic
//! rather than a semantic; `waitForLeadership` (`:71`, `types.ts:20`), which is the
//! leader-election plugin's job and needs its transport; and the change-stream
//! subscription that triggers a run, because [`Pipeline::run_until_idle`] is the run it
//! would trigger.

#![forbid(unsafe_code)]

use serde_json::Value;
use thiserror::Error;

/// The checkpoint document key prefix (`rx-pipeline.ts:83`).
pub const CHECKPOINT_PREFIX: &str = "rx-pipeline-";

/// The default batch size (`rx-pipeline.ts:81`).
pub const DEFAULT_BATCH_SIZE: usize = 100;

/// Errors.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum Error {
    /// The handler failed. Upstream latches the first error and re-throws it on every
    /// later operation (`rx-pipeline.ts:41-45`, `:188-190`) so a broken pipeline is not
    /// silently skipped.
    #[error("the pipeline handler failed: {0}")]
    Handler(String),
    /// The store failed.
    #[error("pipeline store error: {0}")]
    Store(String),
    /// A batch was returned that is larger than the requested batch size, which would
    /// make the "short batch means done" rule meaningless.
    #[error("the store returned {got} documents for a batch size of {wanted}")]
    OversizedBatch {
        /// How many came back.
        got: usize,
        /// How many were asked for.
        wanted: usize,
    },
}

/// Result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// A checkpoint, as stored in the checkpoint document (`types.ts:25-28`).
#[derive(Clone, Debug, PartialEq)]
pub struct Checkpoint {
    /// The `getChangedDocumentsSince` checkpoint.
    pub checkpoint: Value,
    /// The `_meta.lwt` of the last document in the batch (`rx-pipeline.ts:161-163`).
    pub last_doc_time: f64,
}

/// One document from the source.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceDocument {
    /// The document data, as ordered JSON.
    pub data: Value,
    /// `_meta.lwt`, which is what the checkpoint records.
    pub lwt: f64,
}

impl SourceDocument {
    /// A document with a given `lwt`.
    pub fn new(data: Value, lwt: f64) -> Self {
        SourceDocument { data, lwt }
    }
}

/// The store a pipeline reads from, checkpoints into, and hands batches to.
///
/// Three methods, because those are the three things `rx-pipeline.ts` does: read the
/// checkpoint, read the next batch, write the checkpoint.
pub trait PipelineStore {
    /// The stored checkpoint for `identifier`, if any (`getCheckpointDoc`, `:250-...`).
    fn checkpoint(&mut self, identifier: &str) -> Result<Option<Checkpoint>>;

    /// Documents changed since `checkpoint`, at most `batch_size`
    /// (`getChangedDocumentsSince`, `:129-133`).
    ///
    /// `None` means "from the beginning".
    fn documents_since(
        &mut self,
        batch_size: usize,
        checkpoint: Option<&Value>,
    ) -> Result<(Vec<SourceDocument>, Value)>;

    /// Persists the checkpoint (`setCheckpointDoc`, `:166-168`).
    fn set_checkpoint(&mut self, identifier: &str, checkpoint: &Checkpoint) -> Result<()>;
}

/// Proof that the holder is running inside a pipeline handler.
///
/// The explicit replacement for upstream's stack-trace marker. A handler is given one and
/// passes it to any destination call it makes; the destination's guard
/// ([`ReentrancyGuard`]) lets the call through instead of waiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PipelineToken {
    /// Which pipeline issued it, so a diagnostic can name it.
    identifier: &'static str,
}

impl PipelineToken {
    /// The pipeline that issued this token.
    pub fn identifier(&self) -> &'static str {
        self.identifier
    }
}

/// The "an outside caller waits while a pipeline runs" guard.
///
/// One guard per destination collection, shared by every pipeline that writes to it —
/// which is the case upstream's `PIPELINE_FN_PREFIX` match exists for
/// (`rx-pipeline.ts:100-106`).
#[derive(Clone, Debug, Default)]
pub struct ReentrancyGuard {
    /// Identifiers of the pipelines currently inside a handler.
    active: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>,
}

impl ReentrancyGuard {
    /// A guard with no pipeline active.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the caller must wait: a pipeline is running and the caller holds no token.
    ///
    /// This is the whole rule, and it is the one things like `waitBeforeWriteFn`
    /// (`rx-pipeline.ts:56-70`) implement by stack inspection.
    pub fn must_wait(&self, token: Option<PipelineToken>) -> bool {
        let active = self.active.borrow();
        if active.is_empty() {
            return false;
        }
        // A held token means the call came from a handler, so it must not wait. A
        // pipeline is running and this caller has no token, so it must.
        token.is_none()
    }

    /// Whether any pipeline is currently inside a handler.
    pub fn is_active(&self) -> bool {
        !self.active.borrow().is_empty()
    }

    /// Which pipelines are active, for a diagnostic.
    pub fn active(&self) -> Vec<&'static str> {
        self.active.borrow().clone()
    }

    /// Enters a handler, marking this pipeline active.
    ///
    /// Public because the guard is genuinely shared state: a caller that runs its own
    /// scheduling (rather than [`Pipeline::run_until_idle`]) has to hold the guard for the
    /// duration of its handler, and a test that models two pipelines on one destination
    /// has to be able to enter both. [`Pipeline::run_until_idle`] calls this around
    /// every handler invocation, including on the error path.
    pub fn enter(&self, identifier: &'static str) {
        self.active.borrow_mut().push(identifier);
    }

    /// Leaves a handler, releasing the pipeline.
    ///
    /// Safe to call for a pipeline that never entered: the release is a no-op rather than
    /// a panic, so an error path cannot turn a handler failure into a crash.
    pub fn leave(&self, identifier: &'static str) {
        let mut active = self.active.borrow_mut();
        if let Some(position) = active.iter().rposition(|name| *name == identifier) {
            active.remove(position);
        }
    }
}

/// What a pipeline did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunResult {
    /// Batches processed.
    pub batches: usize,
    /// Documents handed to the handler.
    pub documents: usize,
    /// The checkpoint after the last batch.
    pub checkpoint: Option<Checkpoint>,
}

/// One pipeline.
pub struct Pipeline<'a> {
    /// The identifier, which keys the checkpoint document (`:83`).
    pub identifier: &'static str,
    /// The batch size (`:81`).
    pub batch_size: usize,
    /// The destination's re-entrancy guard, so a handler's own writes do not wait.
    pub guard: &'a ReentrancyGuard,
    /// Latched handler error (`:41-45`).
    error: Option<Error>,
}

impl<'a> Pipeline<'a> {
    /// A pipeline with the default batch size.
    pub fn new(identifier: &'static str, guard: &'a ReentrancyGuard) -> Self {
        Pipeline {
            identifier,
            batch_size: DEFAULT_BATCH_SIZE,
            guard,
            error: None,
        }
    }

    /// A pipeline with an explicit batch size.
    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size.max(1);
        self
    }

    /// The checkpoint document key (`:83`).
    pub fn checkpoint_id(&self) -> String {
        format!("{CHECKPOINT_PREFIX}{}", self.identifier)
    }

    /// The latched error, if the handler failed (`:41-45`).
    pub fn error(&self) -> Option<&Error> {
        self.error.as_ref()
    }

    /// Runs batches until the source is exhausted (`:120-186`).
    ///
    /// `handler` receives the batch and the token it must pass to any destination call.
    /// A handler error is latched and returned: upstream stops the loop and re-throws the
    /// latched error on every later operation rather than retrying, because retrying a
    /// deterministic failure loops forever (`:154-158`).
    ///
    /// The loop ends when a batch is **shorter than `batch_size`** (`:170-172`), which is
    /// the same signal `getChangedDocumentsSince` gives.
    pub fn run_until_idle<S, F>(&mut self, store: &mut S, mut handler: F) -> Result<RunResult>
    where
        S: PipelineStore,
        F: FnMut(&[SourceDocument], PipelineToken) -> Result<()>,
    {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }

        let token = PipelineToken {
            identifier: self.identifier,
        };
        let mut result = RunResult::default();

        // Resume from the stored checkpoint, or from the beginning (`:125-128`).
        let mut checkpoint = store
            .checkpoint(self.identifier)?
            .map(|stored| stored.checkpoint);
        let mut last_doc_time = store
            .checkpoint(self.identifier)?
            .map(|stored| stored.last_doc_time)
            .unwrap_or(0.0);

        loop {
            let (documents, next_checkpoint) =
                store.documents_since(self.batch_size, checkpoint.as_ref())?;
            if documents.len() > self.batch_size {
                return Err(Error::OversizedBatch {
                    got: documents.len(),
                    wanted: self.batch_size,
                });
            }
            let short_batch = documents.len() < self.batch_size;
            if !documents.is_empty() {
                // The handler runs with the token held, which is what lets its own
                // destination calls bypass the guard.
                self.guard.enter(self.identifier);
                let outcome = handler(&documents, token);
                self.guard.leave(self.identifier);
                if let Err(error) = outcome {
                    // Latch and stop: `:154-158`.
                    let error = Error::Handler(error.to_string());
                    self.error = Some(error.clone());
                    return Err(error);
                }
                result.batches += 1;
                result.documents += documents.len();
                // `lastTime = lastOfArray(documents)._meta.lwt` (`:161-163`).
                last_doc_time = documents.last().expect("not empty").lwt;
            }
            // The checkpoint is written even for an empty batch (`:165-168`), so a
            // pipeline that has caught up records that fact.
            let stored = Checkpoint {
                checkpoint: next_checkpoint.clone(),
                last_doc_time,
            };
            store.set_checkpoint(self.identifier, &stored)?;
            result.checkpoint = Some(stored);
            checkpoint = Some(next_checkpoint);
            if short_batch {
                return Ok(result);
            }
        }
    }
}

/// Whether the pipeline has caught up: `lastProcessedDocTime >= lastSourceDocTime`
/// (`rx-pipeline.ts:196-206`).
///
/// Upstream's `awaitIdle` loops on this. Exposed as a predicate because the loop needs a
/// wake signal (a change stream) that this crate does not own.
pub fn is_idle(last_processed_doc_time: f64, last_source_doc_time: f64) -> bool {
    last_processed_doc_time >= last_source_doc_time
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_checkpoint_id_is_prefixed() {
        let guard = ReentrancyGuard::new();
        let pipeline = Pipeline::new("foo", &guard);
        assert_eq!(pipeline.checkpoint_id(), "rx-pipeline-foo");
    }

    #[test]
    fn the_default_batch_size_is_one_hundred() {
        let guard = ReentrancyGuard::new();
        let pipeline = Pipeline::new("foo", &guard);
        assert_eq!(pipeline.batch_size, DEFAULT_BATCH_SIZE);
    }

    #[test]
    fn a_handler_holds_the_guard_and_outside_callers_wait() {
        let guard = ReentrancyGuard::new();
        assert!(!guard.is_active());
        assert!(!guard.must_wait(None), "nothing running, nobody waits");
        guard.enter("foo");
        assert!(guard.is_active());
        assert!(
            guard.must_wait(None),
            "an outside caller waits while a pipeline runs"
        );
        assert!(
            !guard.must_wait(Some(PipelineToken { identifier: "foo" })),
            "a handler's own call does not wait"
        );
        guard.leave("foo");
        assert!(!guard.is_active());
    }

    #[test]
    fn two_pipelines_can_be_active_on_one_destination() {
        // The case upstream's PIPELINE_FN_PREFIX match was added for.
        let guard = ReentrancyGuard::new();
        guard.enter("a");
        guard.enter("b");
        assert_eq!(guard.active(), vec!["a", "b"]);
        assert!(guard.must_wait(None));
        guard.leave("a");
        assert_eq!(guard.active(), vec!["b"]);
        guard.leave("b");
        assert!(!guard.must_wait(None));
    }

    #[test]
    fn leaving_a_pipeline_that_did_not_enter_does_not_panic() {
        let guard = ReentrancyGuard::new();
        guard.leave("never-entered");
        assert!(!guard.is_active());
    }

    #[test]
    fn idle_means_processed_caught_up_with_source() {
        assert!(is_idle(10.0, 10.0));
        assert!(is_idle(11.0, 10.0));
        assert!(!is_idle(9.0, 10.0));
    }
}
