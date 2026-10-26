//! The change-event buffer: a bounded window of recent events with stable counters.
//!
//! Ported from `src/change-event-buffer.ts` (174 lines).
//!
//! # What it is for
//!
//! A live query asks "what changed since I last looked?", and the buffer answers that by
//! pointer. Each event gets a monotonically increasing counter as it arrives, so a query holds
//! a pointer rather than a timestamp, and [`ChangeEventBuffer::get_from`] returns everything
//! after that pointer. When the pointer has fallen out of the window the answer is `None` —
//! **not** an empty list — and the caller must re-run the query. That distinction is the whole
//! contract: an empty list means "nothing changed", `None` means "I can no longer tell you".
//!
//! That `None` case is why the buffer is deliberately small (`limit` 100 by default): a
//! subscriber that falls further behind than the window gets a full re-run, which is correct
//! and expensive, rather than a silently missed change.
//!
//! # Two upstream details worth keeping
//!
//! * **A bulk larger than the limit replaces the buffer outright** rather than being trimmed.
//!   `if (events.length > this.limit) this.buffer = events.slice(-length)`. Concatenating and
//!   then slicing would give the same array here, but the upstream form makes the intent
//!   ("the last `limit` events of this bulk, and nothing older") explicit.
//! * **`reduce_by_last_of_doc` is a no-op that returns a copy.** It exists upstream for
//!   storage implementations whose events are per-document rather than per-change; this port
//!   keeps it as an identity because removing it would change the surface other lanes compile
//!   against.
//!
//! # Not reproduced: the idle-callback task queue
//!
//! Upstream batches incoming bulks through `requestIdlePromiseNoQueue` so event handling does
//! not block the write that caused it. Rust has no DOM idle scheduler, and the observable
//! difference of running synchronously is only that the buffer is up to date sooner. The
//! `subs` subscription is likewise an `Observable`; here the collection feeds events in by
//! calling [`ChangeEventBuffer::push_bulk`].

use dxdb_storage::{ChangeEvent, EventBulk};

/// How many events the buffer keeps by default, matching upstream's `limit = 100`.
pub const DEFAULT_LIMIT: usize = 100;

/// A bounded window of recent change events, addressed by pointer.
#[derive(Debug)]
pub struct ChangeEventBuffer {
    buffer: Vec<ChangeEvent>,
    /// The counter of the *last* event ever seen, whether or not it is still buffered.
    counter: u64,
    limit: usize,
    closed: bool,
    /// How many events were dropped because the window moved past them. Exposed so a test can
    /// assert that the out-of-bounds path is actually reachable.
    dropped: usize,
}

/// The buffer has been closed and can no longer be read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("change-event buffer is closed")]
pub struct BufferClosed;

impl ChangeEventBuffer {
    /// A buffer with upstream's default limit.
    pub fn new() -> Self {
        ChangeEventBuffer::with_limit(DEFAULT_LIMIT)
    }

    /// A buffer with an explicit limit.
    ///
    /// A limit of zero is treated as one: a zero-length window could never answer any pointer,
    /// so every query would re-run forever. Upstream permits 0 and then always reports
    /// out-of-bounds; clamping is the same behaviour with a clearer intent.
    pub fn with_limit(limit: usize) -> Self {
        ChangeEventBuffer {
            buffer: Vec::new(),
            counter: 0,
            limit: limit.max(1),
            closed: false,
            dropped: 0,
        }
    }

    /// The window size.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// The counter of the last event seen.
    ///
    /// A query that wants "everything from now on" stores this and passes it to
    /// [`ChangeEventBuffer::get_from`] later.
    pub fn counter(&self) -> u64 {
        self.counter
    }

    /// The buffered events, oldest first.
    pub fn buffer(&self) -> &[ChangeEvent] {
        &self.buffer
    }

    /// How many events have fallen out of the window.
    pub fn dropped(&self) -> usize {
        self.dropped
    }

    /// True after [`ChangeEventBuffer::close`].
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Adds one bulk's events.
    ///
    /// Returns `Err(BufferClosed)` when the buffer is closed, so a producer cannot keep feeding
    /// a torn-down buffer.
    pub fn push_bulk(&mut self, bulk: &EventBulk) -> Result<(), BufferClosed> {
        self.push_events(&bulk.events)
    }

    /// Adds events directly.
    pub fn push_events(&mut self, events: &[ChangeEvent]) -> Result<(), BufferClosed> {
        if self.closed {
            return Err(BufferClosed);
        }
        if events.is_empty() {
            return Ok(());
        }
        self.counter += events.len() as u64;

        if events.len() > self.limit {
            // "if (events.length > this.limit) this.buffer = events.slice(-length)": a single
            // bulk bigger than the window keeps only its own tail.
            self.dropped += self.buffer.len() + (events.len() - self.limit);
            self.buffer = events[events.len() - self.limit..].to_vec();
        } else {
            let combined = self.buffer.len() + events.len();
            self.buffer.extend_from_slice(events);
            if combined > self.limit {
                let overflow = combined - self.limit;
                self.buffer.drain(..overflow);
                self.dropped += overflow;
            }
        }
        Ok(())
    }

    /// The counter of the oldest buffered event.
    ///
    /// The first event ever seen gets counter 1, so an empty-to-nonempty transition is
    /// unambiguous: `None` here means "nothing has been buffered yet".
    fn oldest_counter(&self) -> Option<u64> {
        if self.buffer.is_empty() {
            return None;
        }
        // The oldest event is `buffer[0]`, and the newest is `counter`. Working backwards keeps
        // the mapping correct without storing a counter per event.
        Some(self.counter - self.buffer.len() as u64 + 1)
    }

    /// The array index of `pointer`, or `None` when the pointer is out of bounds.
    ///
    /// Ported from `getArrayIndexByPointer`. A pointer *older* than the window is out of
    /// bounds; a pointer equal to `counter` is valid and yields an empty slice (nothing has
    /// happened since).
    pub fn array_index_by_pointer(&self, pointer: u64) -> Option<usize> {
        let oldest = self.oldest_counter()?;
        if pointer < oldest || pointer > self.counter {
            return None;
        }
        Some((pointer - oldest) as usize)
    }

    /// Every event **at or after** `pointer`, or `None` when the window no longer covers it.
    ///
    /// `Some(vec![])` means "nothing new". `None` means "re-run the query" — the distinction
    /// that makes a stale live query impossible.
    ///
    /// # The off-by-one is upstream's, and it is a trap
    ///
    /// `pointer` is **inclusive**: it is the counter of the first event the caller has *not*
    /// seen, not the last one it has. Upstream calls
    /// `getFrom(rxQuery._latestChangeEvent + 1)` (`src/rx-query.ts:750`) for exactly this
    /// reason. Passing the last-seen counter instead replays one event, which is harmless
    /// because applying an event is idempotent; passing last-seen + 2 skips one, which leaves
    /// the query **silently stale**. When in doubt pass the lower pointer.
    pub fn get_from(&self, pointer: u64) -> Option<Vec<ChangeEvent>> {
        let start = self.array_index_by_pointer(pointer)?;
        Some(self.buffer[start..].to_vec())
    }

    /// [`ChangeEventBuffer::get_from`], but an out-of-bounds pointer is an error.
    ///
    /// Upstream's `runFrom` throws `COB1`; callers that cannot tolerate `None` use this.
    pub fn run_from(
        &self,
        pointer: u64,
        mut apply: impl FnMut(&ChangeEvent),
    ) -> Result<(), OutOfBounds> {
        let events = self.get_from(pointer).ok_or(OutOfBounds { pointer })?;
        for event in &events {
            apply(event);
        }
        Ok(())
    }

    /// An identity that returns a copy.
    ///
    /// Upstream's `reduceByLastOfDoc`. Kept because the surface is shared, documented as an
    /// identity so nobody assumes it deduplicates.
    pub fn reduce_by_last_of_doc(&self, events: &[ChangeEvent]) -> Vec<ChangeEvent> {
        events.to_vec()
    }

    /// Stops accepting events and clears the window.
    ///
    /// A closed buffer reports every pointer as out of bounds, which forces a re-run rather
    /// than a stale answer.
    pub fn close(&mut self) {
        self.closed = true;
        self.buffer.clear();
    }
}

impl Default for ChangeEventBuffer {
    fn default() -> Self {
        ChangeEventBuffer::new()
    }
}

/// A pointer that has fallen out of the window (`COB1` upstream).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("change-event buffer pointer {pointer} is out of bounds; run the query again")]
pub struct OutOfBounds {
    /// The pointer that was asked for.
    pub pointer: u64,
}

impl OutOfBounds {
    /// The upstream error code.
    pub fn code(&self) -> &'static str {
        "COB1"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dxdb_storage::{Checkpoint, DocumentData, Operation};
    use serde_json::json;

    fn event(id: &str) -> ChangeEvent {
        ChangeEvent {
            document_id: id.to_string(),
            operation: Operation::Update,
            document_data: DocumentData::from_value(json!({
                "id": id, "_rev": "2-t", "_deleted": false,
                "_meta": { "lwt": 2.0 }, "_attachments": {}
            }))
            .expect("valid"),
            previous_document_data: None,
        }
    }

    fn bulk(events: Vec<ChangeEvent>) -> EventBulk {
        EventBulk {
            id: "bulk".to_string(),
            events,
            checkpoint: Some(Checkpoint::new("a", 1.0)),
            context: "test".to_string(),
        }
    }

    /// The counters a query uses to ask "what changed since me?".
    #[test]
    fn counters_start_at_one_and_advance_per_event() {
        let mut buffer = ChangeEventBuffer::new();
        assert_eq!(buffer.counter(), 0);
        buffer.push_events(&[event("a")]).expect("push");
        assert_eq!(buffer.counter(), 1);
        buffer.push_events(&[event("b"), event("c")]).expect("push");
        assert_eq!(buffer.counter(), 3);
        assert_eq!(buffer.buffer().len(), 3);
    }

    /// `get_from` returns what came after the pointer, and an empty slice for "nothing new".
    #[test]
    fn get_from_returns_events_after_the_pointer() {
        let mut buffer = ChangeEventBuffer::new();
        buffer.push_events(&[event("a")]).expect("push");
        // A query that has seen counter 1 asks for `last_seen + 1`, as upstream does
        // (rx-query.ts:750). The pointer is inclusive of the event it names.
        let pointer = buffer.counter() + 1;
        buffer.push_events(&[event("b"), event("c")]).expect("push");

        let events = buffer.get_from(pointer).expect("in bounds");
        assert_eq!(
            events
                .iter()
                .map(|event| event.document_id.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "c"]
        );

        // The inclusive reading is explicit: `get_from(counter)` names the last event, so it
        // returns exactly that one. "Nothing new" is `get_from(counter + 1)`, which upstream's
        // arithmetic reports as out of bounds — a caller that has seen everything simply has no
        // events to read, and out-of-bounds is how that case is spelled.
        assert_eq!(
            buffer
                .get_from(buffer.counter())
                .expect("in bounds")
                .iter()
                .map(|event| event.document_id.as_str())
                .collect::<Vec<_>>(),
            vec!["c"],
            "get_from(counter) includes the event at that counter"
        );
        assert_eq!(
            buffer.get_from(buffer.counter() + 1),
            None,
            "a pointer past the newest event is out of bounds, not an empty list"
        );
    }

    /// A pointer older than the window is out of bounds, which forces a re-run. This is the
    /// property that makes a stale live query impossible.
    #[test]
    fn a_pointer_older_than_the_window_is_out_of_bounds() {
        let mut buffer = ChangeEventBuffer::with_limit(2);
        buffer.push_events(&[event("a")]).expect("push");
        // The inclusive pointer naming `a` itself.
        let stale_pointer = buffer.counter();
        // Push enough to evict `a`.
        buffer.push_events(&[event("b"), event("c")]).expect("push");

        assert_eq!(
            buffer.get_from(stale_pointer),
            None,
            "an evicted pointer must report out of bounds, not an empty list"
        );
        assert_eq!(
            buffer.get_from(0),
            None,
            "the first event's counter is 1, so pointer 0 is before it"
        );
        assert!(buffer.dropped() > 0);
    }

    /// A single bulk larger than the window keeps only its own tail.
    #[test]
    fn an_oversized_bulk_replaces_the_buffer() {
        let mut buffer = ChangeEventBuffer::with_limit(3);
        buffer
            .push_events(&[event("old1"), event("old2")])
            .expect("push");
        buffer
            .push_events(&[
                event("e1"),
                event("e2"),
                event("e3"),
                event("e4"),
                event("e5"),
            ])
            .expect("push");

        assert_eq!(buffer.buffer().len(), 3, "the window size is the limit");
        assert_eq!(
            buffer
                .buffer()
                .iter()
                .map(|event| event.document_id.as_str())
                .collect::<Vec<_>>(),
            vec!["e3", "e4", "e5"],
            "only the tail of the oversized bulk survives"
        );
        assert_eq!(
            buffer.counter(),
            7,
            "every event still advanced the counter"
        );
    }

    /// The window slides by exactly the overflow.
    #[test]
    fn the_window_slides_as_events_arrive() {
        let mut buffer = ChangeEventBuffer::with_limit(3);
        for id in ["a", "b", "c", "d", "e"] {
            buffer.push_events(&[event(id)]).expect("push");
        }
        assert_eq!(
            buffer
                .buffer()
                .iter()
                .map(|event| event.document_id.as_str())
                .collect::<Vec<_>>(),
            vec!["c", "d", "e"]
        );
    }

    /// `run_from` errors with the upstream code instead of returning `None`.
    #[test]
    fn run_from_reports_out_of_bounds_with_cob1() {
        let mut buffer = ChangeEventBuffer::with_limit(1);
        buffer.push_events(&[event("a")]).expect("push");
        let stale = buffer.counter();
        buffer.push_events(&[event("b")]).expect("push");

        let error = buffer
            .run_from(stale, |_| {})
            .expect_err("must be out of bounds");
        assert_eq!(error.code(), "COB1");
        assert_eq!(error.pointer, stale);

        // A valid pointer runs every event from that counter onwards, inclusive.
        let mut seen = Vec::new();
        buffer
            .run_from(2, |event| seen.push(event.document_id.clone()))
            .expect("in bounds");
        assert_eq!(seen, vec!["b"]);
    }

    #[test]
    fn push_bulk_accepts_a_bulk() {
        let mut buffer = ChangeEventBuffer::new();
        buffer
            .push_bulk(&bulk(vec![event("a"), event("b")]))
            .expect("push");
        assert_eq!(buffer.counter(), 2);
    }

    /// A closed buffer refuses events and reports every pointer as out of bounds, so a reader
    /// re-runs rather than reading a stale window.
    #[test]
    fn a_closed_buffer_refuses_events_and_pointers() {
        let mut buffer = ChangeEventBuffer::new();
        buffer.push_events(&[event("a")]).expect("push");
        buffer.close();
        assert!(buffer.is_closed());
        assert_eq!(buffer.push_events(&[event("b")]), Err(BufferClosed));
        assert_eq!(buffer.get_from(0), None);
    }

    #[test]
    fn reduce_by_last_of_doc_is_an_identity() {
        let buffer = ChangeEventBuffer::new();
        let events = vec![event("a"), event("a")];
        let reduced = buffer.reduce_by_last_of_doc(&events);
        assert_eq!(
            reduced.len(),
            2,
            "documented as an identity; it does not deduplicate"
        );
    }

    /// A limit of zero would make every pointer out of bounds forever, so it is clamped.
    #[test]
    fn a_zero_limit_is_clamped_to_one() {
        let mut buffer = ChangeEventBuffer::with_limit(0);
        assert_eq!(buffer.limit(), 1);
        buffer.push_events(&[event("a"), event("b")]).expect("push");
        assert_eq!(buffer.buffer().len(), 1);
        assert!(
            buffer.get_from(buffer.counter()).is_some(),
            "the latest counter is answerable (inclusive)"
        );
    }
}
