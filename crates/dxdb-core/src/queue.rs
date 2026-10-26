//! The incremental write queue.
//!
//! Ported from `src/incremental-write.ts:48` `IncrementalWriteQueue`. It exists because
//! two `incrementalModify` calls on one document must not lose an update:
//!
//! ```text
//! task A: read rev 1, compute rev 2
//! task B: read rev 1, compute rev 2      <- both read the same state
//! write A succeeds, write B conflicts     <- B's change is lost
//! ```
//!
//! The queue fixes it by resolving the current state **inside** the queue slot, so
//! task B reads rev 2 after A commits. That is the whole contract, and it is a
//! correctness property rather than an optimisation: without it, `incrementalModify`
//! silently drops writes under concurrency.
//!
//! # What upstream does that this does not need
//!
//! Upstream keeps an `idleQueue` of pending promises per document and `await`s the
//! previous one at the start of the next. In Rust the same thing is expressed by running
//! the queue's tasks one at a time from the collection's write path, which is what
//! [`WriteQueue::run`] does. There is no per-document promise map.
//!
//! What is preserved exactly: **the modifier sees the newest state, and a task that
//! fails does not block the queue**.

use std::collections::HashMap;

use dxdb_storage::DocumentData;

use crate::cache::revision_height;
use crate::error::{CoreError, CoreResult};

/// A function that computes the next state of a document from its current state.
///
/// Upstream's modifier receives an `RxDocument`; here it receives the stored state, so a
/// modifier is a pure function and can be tested without a collection. Returning `None`
/// means "no change", which is what upstream's `modify()` returning the same document
/// does: no write is issued, so no revision is burned.
pub type Modifier = Box<dyn FnMut(Option<&DocumentData>) -> CoreResult<Option<DocumentData>>>;

/// Serialises incremental writes per document.
///
/// One queue per collection; the key is the document id.
#[derive(Default)]
pub struct WriteQueue {
    /// The newest state each id has reached through this queue, so a modifier can be
    /// handed a state even before the storage round-trip is visible to a read.
    latest: HashMap<String, DocumentData>,
    /// How many tasks are waiting per id. Exposed so a test can assert that two
    /// concurrent modifications really were serialised rather than interleaved.
    depth: HashMap<String, usize>,
    /// Total tasks run, for assertions.
    runs: usize,
}

impl WriteQueue {
    /// An empty queue.
    pub fn new() -> Self {
        WriteQueue::default()
    }

    /// Records the newest known state for `id` without running a task.
    ///
    /// Called after any write to the collection, so a modifier started immediately
    /// afterwards sees the state that write produced even if a read would not.
    pub fn observe(&mut self, id: &str, state: DocumentData) -> CoreResult<()> {
        let incoming = revision_height(&state)?;
        match self.latest.get(id) {
            Some(existing) => {
                if incoming >= revision_height(existing)? {
                    self.latest.insert(id.to_string(), state);
                }
            }
            None => {
                self.latest.insert(id.to_string(), state);
            }
        }
        Ok(())
    }

    /// The newest state this queue knows for `id`.
    pub fn latest(&self, id: &str) -> Option<&DocumentData> {
        self.latest.get(id)
    }

    /// How many tasks are queued or running for `id`.
    pub fn pending(&self, id: &str) -> usize {
        self.depth.get(id).copied().unwrap_or_default()
    }

    /// How many tasks have run since construction.
    pub fn runs(&self) -> usize {
        self.runs
    }

    /// Runs one queued task for `id`.
    ///
    /// The caller supplies `read`, which must return the **current stored state** — the
    /// queue does not read storage itself, because the collection owns the storage
    /// handle and the read has to go through the same revision-assignment path as
    /// everything else. The queue's job is to guarantee that `read` is called while no
    /// other task for `id` is between its read and its write.
    ///
    /// Returns the task's output, or `None` when the modifier declined to change
    /// anything.
    pub fn run<T>(
        &mut self,
        id: &str,
        read: impl FnOnce() -> CoreResult<Option<DocumentData>>,
        mut modifier: impl FnMut(Option<&DocumentData>) -> CoreResult<Option<T>>,
    ) -> CoreResult<Option<T>> {
        *self.depth.entry(id.to_string()).or_insert(0) += 1;
        let result = (|| {
            let stored = read()?;
            // Prefer the stored state, but if the queue has seen something newer (a
            // write whose read has not caught up), use the newer one. That is what makes
            // two back-to-back modifications compose instead of the second reading a
            // stale value.
            let current = match (&stored, self.latest.get(id)) {
                (Some(stored_state), Some(seen)) => {
                    if revision_height(seen)? > revision_height(stored_state)? {
                        Some(seen.clone())
                    } else {
                        Some(stored_state.clone())
                    }
                }
                (Some(stored_state), None) => Some(stored_state.clone()),
                (None, Some(seen)) => Some(seen.clone()),
                (None, None) => None,
            };
            modifier(current.as_ref())
        })();
        self.runs += 1;
        if let Some(count) = self.depth.get_mut(id) {
            *count = count.saturating_sub(1);
        }
        result
    }

    /// Forgets one id, for a document that was removed.
    pub fn forget(&mut self, id: &str) {
        self.latest.remove(id);
        self.depth.remove(id);
    }

    /// Forgets everything, on collection teardown.
    pub fn clear(&mut self) {
        self.latest.clear();
        self.depth.clear();
    }
}

/// The error returned when a queued task's modifier declines to run because the document
/// vanished between the read and the write.
pub fn document_vanished(id: &str, collection: &str) -> CoreError {
    CoreError::DocumentNotFound {
        collection: collection.to_string(),
        id: id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state(id: &str, height: u32, marker: &str) -> DocumentData {
        DocumentData::from_value(json!({
            "id": id,
            "_rev": format!("{height}-t"),
            "_deleted": false,
            "_meta": { "lwt": height as f64 },
            "_attachments": {},
            "marker": marker
        }))
        .expect("valid")
    }

    /// The property the queue exists for: a modifier is handed the newest state, so two
    /// sequential modifications compose rather than both deriving from the first.
    #[test]
    fn a_modifier_sees_the_newest_state() {
        let mut queue = WriteQueue::new();
        queue.observe("a", state("a", 1, "one")).expect("observe");

        // First task: bump to 2.
        let first = queue
            .run(
                "a",
                || Ok(Some(state("a", 1, "one"))),
                |current| {
                    let current = current.expect("present");
                    Ok(Some(format!(
                        "{}-first-from-{}",
                        current
                            .get("marker")
                            .and_then(|value| value.as_str())
                            .unwrap_or(""),
                        current.rev().unwrap_or("")
                    )))
                },
            )
            .expect("first")
            .expect("some");
        assert_eq!(first, "one-first-from-1-t");

        // The write the first task produced becomes the newest known state.
        queue.observe("a", state("a", 2, "two")).expect("observe");

        // Second task must see 2, not the stale 1 a naive read would return.
        let second = queue
            .run(
                "a",
                || Ok(Some(state("a", 1, "one"))),
                |current| {
                    let current = current.expect("present");
                    Ok(Some(current.rev().unwrap_or("").to_string()))
                },
            )
            .expect("second")
            .expect("some");
        assert_eq!(
            second, "2-t",
            "the queue must hand the modifier the newer state, not the stale read"
        );
    }

    #[test]
    fn a_declining_modifier_produces_no_result() {
        let mut queue = WriteQueue::new();
        let result = queue
            .run::<()>("a", || Ok(Some(state("a", 1, "one"))), |_current| Ok(None))
            .expect("run");
        assert!(result.is_none(), "no change means no write and no result");
        assert_eq!(queue.runs(), 1);
    }

    /// A failing task must not leave the queue wedged: the depth counter has to come back
    /// down even when the task errors.
    #[test]
    fn a_failing_task_does_not_wedge_the_queue() {
        let mut queue = WriteQueue::new();
        let error = queue
            .run::<()>(
                "a",
                || Err(CoreError::InvalidDocument("boom".into())),
                |_| Ok(None),
            )
            .expect_err("must propagate");
        assert!(matches!(error, CoreError::InvalidDocument(_)));
        assert_eq!(queue.pending("a"), 0, "depth must be restored on failure");

        // And the next task runs.
        let ok = queue
            .run("a", || Ok(Some(state("a", 1, "one"))), |_| Ok(Some(1u8)))
            .expect("next task");
        assert_eq!(ok, Some(1));
    }

    #[test]
    fn an_older_observation_does_not_lower_the_newest_state() {
        let mut queue = WriteQueue::new();
        queue
            .observe("a", state("a", 3, "three"))
            .expect("observe 3");
        queue.observe("a", state("a", 1, "one")).expect("observe 1");
        assert_eq!(
            queue.latest("a").expect("present").rev(),
            Some("3-t"),
            "an out-of-order observation must not roll the queue back"
        );
    }

    #[test]
    fn a_modifier_on_a_missing_document_sees_none() {
        let mut queue = WriteQueue::new();
        let seen = queue
            .run(
                "missing",
                || Ok(None),
                |current| Ok(Some(current.is_none())),
            )
            .expect("run")
            .expect("some");
        assert!(seen, "a missing document must be handed as None");
    }
}
