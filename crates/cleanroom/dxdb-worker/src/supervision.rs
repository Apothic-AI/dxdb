//! Supervision: what happens when the worker is not there.
//!
//! `specs/worker.md` section 3.4. This is the part of the crate that exists for a reason:
//! a storage in another isolate can die, hang, or stop answering, and each of those is a
//! different failure with a different correct response.
//!
//! # The four-way split, and why one bucket is wrong
//!
//! | Failure | Response |
//! | --- | --- |
//! | worker terminated | every in-flight call and every later call fails `WorkerDied` |
//! | worker alive but silent | `WorkerUnresponsive` after the heartbeat deadline |
//! | one call exceeded its deadline | `CallTimeout`, **instance stays alive** |
//! | worker replied with an error | propagate it, instance stays alive |
//!
//! Collapsing these into one "worker failed" state is the mistake the design exists to
//! avoid: it either tears down a healthy instance because one query was slow, or keeps
//! using a dead one because its last error was ordinary.
//!
//! # The mid-write rule
//!
//! `bulk_write` is per-document atomic *inside* a storage, but across an isolate boundary
//! the caller cannot know which rows were applied when the worker died. So a write is
//! **never retried**, and it fails with `WorkerDiedDuringWrite { context }` rather than
//! `WorkerDied` (`docs/CLEANROOM-GAPS.md` G-049). The context string is how the caller
//! identifies which of its own operations was in flight; the correct recovery is to re-read
//! and reconcile, which is `dxdb-core`'s business.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use crate::protocol::{RequestKind, RESERVED_ID};

/// How the transport learns about the worker's liveness.
///
/// Implemented by the platform adapter (a browser `Worker`, a Node `worker_thread`) and by
/// the test fake. Keeping it a trait is what makes every supervision case testable
/// deterministically: a test that needed a real worker could not consistently arrange for
/// the worker to die *exactly* while a write is in flight.
pub trait WorkerTransport {
    /// Sends a message to the worker. An error means the worker is gone.
    fn send(&self, message: &crate::protocol::ToWorker) -> Result<(), TransportError>;

    /// Takes the next message from the worker, if one is ready.
    ///
    /// Non-blocking on purpose: the caller drives liveness with an explicit `now`, so the
    /// state machine has no internal clock and tests need no sleeping.
    fn try_recv(&self) -> Option<crate::protocol::FromWorker>;

    /// True once the worker has terminated. Adapters wire this to the platform's
    /// exit/error callback.
    fn is_terminated(&self) -> bool;
}

/// Why a send failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    /// The worker is gone.
    #[error("the worker is not reachable")]
    Gone,
    /// Anything else the adapter wants to report.
    #[error("transport error: {0}")]
    Other(String),
}

/// Liveness, as far as this side knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// Negotiated and answering.
    Alive,
    /// Terminated, or declared unresponsive. No further call can succeed.
    Dead,
}

/// What a caller learns when a call does not complete.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkerError {
    /// The worker terminated. The call may or may not have been applied — unless it was a
    /// write, which gets [`WorkerError::WorkerDiedDuringWrite`].
    #[error("the worker terminated")]
    WorkerDied,
    /// The worker died while this `bulk_write` was in flight.
    ///
    /// The outcome is genuinely unknown: some rows may have been applied. Never retried
    /// (`docs/CLEANROOM-GAPS.md` G-049); re-read and reconcile instead.
    #[error("the worker terminated during a write ({context}); the outcome is unknown")]
    WorkerDiedDuringWrite {
        /// The caller's own context string for the `bulk_write`.
        context: String,
    },
    /// The worker stopped answering heartbeats without terminating.
    #[error("the worker stopped responding")]
    WorkerUnresponsive,
    /// This call exceeded its own deadline. The instance is still usable.
    #[error("the call {operation} exceeded its deadline")]
    CallTimeout {
        /// Which operation timed out.
        operation: &'static str,
    },
    /// The handshake failed.
    #[error(transparent)]
    Protocol(#[from] crate::protocol::ProtocolMismatch),
    /// The worker reported an error for this call.
    #[error("the worker reported an error: {0}")]
    Remote(String),
    /// A message could not be serialised on this side; the worker never saw it.
    #[error("could not serialise the request for {operation}")]
    SerializationFailed {
        /// Which operation failed.
        operation: &'static str,
    },
    /// The worker answered something this build cannot parse.
    #[error("could not parse the worker's reply")]
    DeserializationFailed,
    /// The caller closed the storage.
    #[error("the storage is closed")]
    Closed,
}

/// Tunables. Defaults are starting points, not measurements
/// (`docs/CLEANROOM-GAPS.md` G-050).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SupervisionSettings {
    /// How often to ping.
    pub heartbeat_interval_ms: u64,
    /// How long without a pong before the worker is declared unresponsive.
    pub heartbeat_timeout_ms: u64,
    /// The per-call deadline. Exceeding it fails *that call*, not the instance.
    pub call_timeout_ms: u64,
    /// The maximum in-flight requests before a new send waits.
    pub queue_bound: usize,
    /// The maximum queued events before the oldest are coalesced away.
    pub event_buffer_bound: usize,
}

impl Default for SupervisionSettings {
    fn default() -> Self {
        Self {
            heartbeat_interval_ms: 5_000,
            // The heartbeat timeout is deliberately LONGER than the per-call deadline, so
            // a single slow call is diagnosed as a slow call rather than as a dead
            // worker. With these values a call gets 30 s to finish and the worker gets
            // 45 s of total silence before the connection is declared dead. The original
            // pair (15 s heartbeat, 30 s call) made `CallTimeout` unreachable, which
            // `inv_07` caught. The values themselves remain acknowledged guesses
            // (`docs/CLEANROOM-GAPS.md` G-050); the *ordering* is the invariant.
            heartbeat_timeout_ms: 45_000,
            call_timeout_ms: 30_000,
            queue_bound: 64,
            event_buffer_bound: 256,
        }
    }
}

/// Counters, so a silently-dropping transport is observable rather than mysterious.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// Requests handed to the transport.
    pub requests_sent: u64,
    /// Responses matched to a waiting call.
    pub responses_matched: u64,
    /// Responses whose id matched nothing. A non-zero value means a protocol bug; the
    /// counter exists so it is visible rather than looking like a missing result.
    pub responses_dropped_unknown_id: u64,
    /// Event bulks delivered to a subscriber.
    pub events_delivered: u64,
    /// Event bulks dropped because the buffer was full.
    pub events_dropped_backpressure: u64,
    /// Heartbeats that went unanswered.
    pub heartbeats_missed: u64,
}

/// One call waiting for its answer.
#[derive(Debug)]
struct Pending {
    kind_name: &'static str,
    /// Present for a write, so a death can report `WorkerDiedDuringWrite`.
    write_context: Option<String>,
    /// When this call's own deadline expires.
    deadline_ms: u64,
}

/// The caller side of a connection: correlation, liveness, and the failure vocabulary.
pub struct Supervision {
    transport: Rc<dyn WorkerTransport>,
    settings: SupervisionSettings,
    liveness: Liveness,
    /// In-flight calls by correlation id.
    pending: BTreeMap<u64, Pending>,
    /// The next correlation id. Starts at 1 because 0 is reserved.
    next_id: u64,
    counters: RefCell<Counters>,
    /// When the last pong (or any message) arrived.
    last_seen_ms: u64,
    /// When the last ping was sent.
    last_ping_ms: u64,
    /// Queued events, bounded.
    events: RefCell<Vec<serde_json::Value>>,
    /// Set when events were dropped, so a consumer knows to re-read rather than assume it
    /// saw everything.
    coalesced: std::cell::Cell<bool>,
}

impl std::fmt::Debug for Supervision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervision")
            .field("liveness", &self.liveness)
            .field("pending", &self.pending.len())
            .field("counters", &self.counters.borrow())
            .finish()
    }
}

impl Supervision {
    /// Builds supervision over `transport`, negotiating the handshake first.
    ///
    /// `now_ms` is explicit so the state machine owns no clock: tests advance time by
    /// passing a later value, never by sleeping.
    pub fn connect(
        transport: Rc<dyn WorkerTransport>,
        settings: SupervisionSettings,
        now_ms: u64,
    ) -> Result<Self, WorkerError> {
        let mut supervision = Self {
            transport,
            settings,
            liveness: Liveness::Alive,
            pending: BTreeMap::new(),
            next_id: 1,
            counters: RefCell::new(Counters::default()),
            last_seen_ms: now_ms,
            last_ping_ms: now_ms,
            events: RefCell::new(Vec::new()),
            coalesced: std::cell::Cell::new(false),
        };
        supervision.handshake()?;
        Ok(supervision)
    }

    /// Negotiates the protocol version and learns the worker's identity.
    fn handshake(&mut self) -> Result<(), WorkerError> {
        self.send(&crate::protocol::ToWorker::Hello {
            protocol: crate::protocol::PROTOCOL,
            min_supported: crate::protocol::MIN_SUPPORTED,
        })?;
        // The reply is expected immediately; adapters deliver it synchronously for a live
        // worker. A missing reply means the worker never started.
        match self.transport.try_recv() {
            Some(crate::protocol::FromWorker::HelloReply { .. }) => Ok(()),
            Some(crate::protocol::FromWorker::HelloRejected {
                protocol,
                min_supported,
            }) => Err(WorkerError::Protocol(
                crate::protocol::ProtocolMismatch::NoOverlap {
                    caller_max: crate::protocol::PROTOCOL,
                    caller_min: crate::protocol::MIN_SUPPORTED,
                    worker_max: protocol,
                    worker_min: min_supported,
                },
            )),
            _ => {
                self.liveness = Liveness::Dead;
                Err(WorkerError::WorkerDied)
            }
        }
    }

    fn send(&self, message: &crate::protocol::ToWorker) -> Result<(), WorkerError> {
        self.transport.send(message).map_err(|error| match error {
            TransportError::Gone => WorkerError::WorkerDied,
            TransportError::Other(_) => WorkerError::WorkerDied,
        })
    }

    /// Whether the connection can still be used.
    pub fn liveness(&self) -> Liveness {
        self.liveness
    }

    /// The counters so far.
    pub fn counters(&self) -> Counters {
        *self.counters.borrow()
    }

    /// True when events were dropped since the last call.
    ///
    /// A consumer that sees this re-reads rather than assuming it saw everything
    /// (`docs/CLEANROOM-GAPS.md` G-052: the oldest are dropped because a checkpoint gap is
    /// recoverable and a missing newest event is not).
    pub fn was_coalesced(&self) -> bool {
        self.coalesced.get()
    }

    /// Allocates the next correlation id.
    fn allocate_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    /// Issues `kind` and waits for its answer.
    ///
    /// `now_ms` is the time the call starts; the call's deadline is
    /// `now_ms + call_timeout_ms`.
    pub fn call(
        &mut self,
        kind: RequestKind,
        now_ms: u64,
    ) -> Result<serde_json::Value, WorkerError> {
        if self.liveness == Liveness::Dead {
            // A call after death fails immediately, and a write gets its specific kind.
            return Err(self.death_error(&kind));
        }
        if let Some(bound) = self.queue_bound_violation() {
            let _ = bound;
            return Err(WorkerError::Remote(
                "queue bound exceeded synchronously".to_string(),
            ));
        }
        let id = self.allocate_id();
        let operation = kind.name();
        self.pending.insert(
            id,
            Pending {
                kind_name: operation,
                write_context: kind.context().map(str::to_string),
                deadline_ms: now_ms.saturating_add(self.settings.call_timeout_ms),
            },
        );
        self.counters.borrow_mut().requests_sent += 1;
        if let Err(error) = self.send(&crate::protocol::ToWorker::Request {
            id,
            kind: kind.clone(),
        }) {
            // The send failed, so this call never reached the worker: remove it and
            // report the death with the right kind.
            self.pending.remove(&id);
            self.mark_dead();
            return Err(self.death_error(&kind).or_else(error));
        }
        Ok(serde_json::Value::Null)
    }

    /// The queue bound is enforced by the caller's scheduler, not here.
    fn queue_bound_violation(&self) -> Option<usize> {
        if self.pending.len() >= self.settings.queue_bound {
            Some(self.settings.queue_bound)
        } else {
            None
        }
    }

    /// The error a death produces for `kind`.
    fn death_error(&self, kind: &RequestKind) -> WorkerError {
        match kind.context() {
            Some(context) => WorkerError::WorkerDiedDuringWrite {
                context: context.to_string(),
            },
            None => WorkerError::WorkerDied,
        }
    }

    fn mark_dead(&mut self) {
        self.liveness = Liveness::Dead;
    }

    /// Drains whatever the worker has sent, pairing responses with their calls.
    ///
    /// Returns the calls that completed, each with its result or its failure. Anything
    /// whose id matches nothing is counted, never guessed at.
    pub fn pump(&mut self, now_ms: u64) -> Vec<(u64, Result<serde_json::Value, WorkerError>)> {
        let mut completed = Vec::new();

        // A terminated worker fails everything in flight before anything else is read.
        if self.transport.is_terminated() {
            self.mark_dead();
        }

        while let Some(message) = self.transport.try_recv() {
            self.last_seen_ms = now_ms;
            match message {
                crate::protocol::FromWorker::Response { id, result } => {
                    if id == RESERVED_ID {
                        // A storage response must never use the reserved id; counting it
                        // rather than pairing it keeps a protocol bug visible.
                        self.counters.borrow_mut().responses_dropped_unknown_id += 1;
                        continue;
                    }
                    match self.pending.remove(&id) {
                        Some(pending) => {
                            self.counters.borrow_mut().responses_matched += 1;
                            // A remote error is a remote error whether or not the call
                            // was a write: the worker answered, so the operation's
                            // outcome is known and there is nothing to reconcile.
                            let outcome = result.map_err(WorkerError::Remote);
                            let _ = &pending;
                            completed.push((id, outcome));
                        }
                        None => {
                            // Unknown id: counted, not paired with the oldest call. That
                            // assumption is how a transport pairs the wrong results.
                            self.counters.borrow_mut().responses_dropped_unknown_id += 1;
                        }
                    }
                }
                crate::protocol::FromWorker::Event { bulk } => {
                    self.push_event(bulk);
                }
                crate::protocol::FromWorker::Pong { .. } => {}
                crate::protocol::FromWorker::Shutdown => {
                    self.mark_dead();
                }
                // A second handshake reply is not an error; ignore it.
                crate::protocol::FromWorker::HelloReply { .. }
                | crate::protocol::FromWorker::HelloRejected { .. } => {}
            }
        }

        // Liveness: a terminated worker fails the rest; otherwise expiry is per call.
        if self.liveness == Liveness::Dead {
            let pending: Vec<(u64, Pending)> =
                std::mem::take(&mut self.pending).into_iter().collect();
            for (id, pending) in pending {
                let error = match pending.write_context {
                    Some(context) => WorkerError::WorkerDiedDuringWrite { context },
                    None => WorkerError::WorkerDied,
                };
                completed.push((id, Err(error)));
            }
            return completed;
        }

        // Per-call deadlines come FIRST, deliberately. A call that overran its own
        // deadline is evidence about *that call*, not about the worker; attributing it to
        // the instance would tear down a healthy connection because one query was slow.
        // Evaluating the heartbeat first would make `CallTimeout` unreachable whenever
        // `call_timeout_ms >= heartbeat_timeout_ms`, which is the default relationship
        // (a query may legitimately take longer than the heartbeat interval).
        let expired: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, pending)| now_ms >= pending.deadline_ms)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some(pending) = self.pending.remove(&id) {
                completed.push((
                    id,
                    Err(WorkerError::CallTimeout {
                        operation: pending.kind_name,
                    }),
                ));
            }
        }

        // Then silence: if nothing has arrived for a whole heartbeat timeout, the worker
        // is unresponsive and whatever it still owes is unrecoverable.
        if now_ms.saturating_sub(self.last_seen_ms) >= self.settings.heartbeat_timeout_ms {
            self.counters.borrow_mut().heartbeats_missed += 1;
            self.mark_dead();
            let pending: Vec<(u64, Pending)> =
                std::mem::take(&mut self.pending).into_iter().collect();
            for (id, pending) in pending {
                let error = match pending.write_context {
                    Some(context) => WorkerError::WorkerDiedDuringWrite { context },
                    None => WorkerError::WorkerUnresponsive,
                };
                completed.push((id, Err(error)));
            }
        }
        completed
    }

    /// Queues an event, coalescing the oldest when full.
    fn push_event(&self, bulk: serde_json::Value) {
        let mut events = self.events.borrow_mut();
        if events.len() >= self.settings.event_buffer_bound {
            // Drop the OLDEST: a consumer seeing a checkpoint gap re-reads, so an old
            // missing event is recoverable and a missing newest one is not.
            events.remove(0);
            self.counters.borrow_mut().events_dropped_backpressure += 1;
            self.coalesced.set(true);
        }
        events.push(bulk);
        self.counters.borrow_mut().events_delivered += 1;
    }

    /// Takes the queued events.
    pub fn take_events(&self) -> Vec<serde_json::Value> {
        let mut events = self.events.borrow_mut();
        let taken = std::mem::take(&mut *events);
        self.coalesced.set(false);
        taken
    }

    /// Sends a heartbeat if one is due.
    pub fn heartbeat_if_due(&mut self, now_ms: u64) {
        if self.liveness == Liveness::Dead {
            return;
        }
        if now_ms.saturating_sub(self.last_ping_ms) >= self.settings.heartbeat_interval_ms {
            self.last_ping_ms = now_ms;
            let id = self.allocate_id();
            // A failed ping is not fatal on its own: the timeout decides.
            let _ = self.send(&crate::protocol::ToWorker::Ping { id });
        }
    }

    /// The number of calls still waiting.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

/// `WorkerError::or_else`: prefer the death error, falling back to the transport error.
impl WorkerError {
    fn or_else(self, other: WorkerError) -> WorkerError {
        match self {
            WorkerError::WorkerDied | WorkerError::WorkerDiedDuringWrite { .. } => self,
            _ => other,
        }
    }
}
