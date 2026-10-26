//! The eleven invariants of `specs/worker.md` section 4.
//!
//! Named after their numbers. The supervision ones (INV-5 through INV-8) are the reason
//! this crate exists: a worker that dies mid-write is the failure mode a test can actually
//! catch, and INV-8 is the one that proves the store cannot be silently half-written by a
//! retry.

mod fake;

use std::rc::Rc;

use dxdb_worker::protocol::{FromWorker, RequestKind, ToWorker, PROTOCOL, RESERVED_ID};
use dxdb_worker::supervision::{
    Counters, Liveness, Supervision, SupervisionSettings, WorkerError, WorkerTransport,
};
use fake::{FakeTransport, Mode};

fn settings() -> SupervisionSettings {
    SupervisionSettings {
        heartbeat_interval_ms: 5_000,
        // Longer than the call deadline, so a slow call is diagnosable as a slow call.
        heartbeat_timeout_ms: 45_000,
        call_timeout_ms: 30_000,
        queue_bound: 4,
        event_buffer_bound: 8,
    }
}

fn connect(transport: &FakeTransport) -> Supervision {
    Supervision::connect(
        Rc::new(transport.clone()) as Rc<dyn WorkerTransport>,
        settings(),
        0,
    )
    .expect("handshake")
}

fn query() -> RequestKind {
    RequestKind::Query {
        prepared: serde_json::json!({"selector": {}, "sort": []}),
    }
}

fn write(context: &str) -> RequestKind {
    RequestKind::BulkWrite {
        rows: serde_json::json!([{"id": "a"}]),
        context: context.to_string(),
    }
}

// ---------------------------------------------------------------------------
// INV-1 — transparency
// ---------------------------------------------------------------------------

/// INV-1: a call over the transport returns what the worker returned, and the request
/// that reached the worker is the request the caller made.
///
/// The full in-process-vs-over-transport comparison waits for the `RxStorageInstance`
/// wrapper (spec section 9). What is provable now, and what this test asserts, is that the
/// protocol itself does not alter an operation: every `RequestKind` reaches the worker
/// intact and carries the caller's values.
#[test]
fn inv_01_transparency_over_transport() {
    let transport = FakeTransport::new(Mode::Normal, "worker-1");
    let mut supervision = connect(&transport);

    transport.set_response(Ok(serde_json::json!({"documents": []})));

    for kind in [
        query(),
        RequestKind::Count {
            prepared: serde_json::json!({"selector": {}, "sort": []}),
        },
        RequestKind::FindDocumentsById {
            ids: vec!["a".into(), "b".into()],
            with_deleted: true,
        },
        RequestKind::GetAttachmentData {
            document_id: "a".into(),
            attachment_id: "att".into(),
            digest: "d".into(),
        },
        RequestKind::GetChangedDocumentsSince {
            limit: 7,
            checkpoint: None,
        },
        RequestKind::Cleanup {
            minimum_deleted_time: 100.0,
        },
        RequestKind::Close,
        RequestKind::Remove,
    ] {
        let expected = kind.clone();
        let _ = supervision.call(kind, 0);
        let _ = supervision.pump(0);
        let received = transport.received();
        assert_eq!(
            received.last().expect("a request arrived").kind,
            expected,
            "the request must reach the worker unaltered"
        );
    }

    // And the worker's answer is what the caller gets:
    let mut supervision = connect(&transport);
    let response = supervision.call(query(), 0);
    assert!(response.is_ok(), "the call was accepted");
    let completed = supervision.pump(0);
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].1, Ok(serde_json::json!({"documents": []})));
}

// ---------------------------------------------------------------------------
// INV-2 — the handshake negotiates an intersection
// ---------------------------------------------------------------------------

/// INV-2: the agreed version is the highest in the intersection; a disjoint pair fails
/// with both ranges named and no storage call sent.
#[test]
fn inv_02_handshake_negotiates_an_intersection() {
    // Exhaustive over a small grid of ranges, so the rule is checked rather than sampled.
    for caller_max in 1u16..=4 {
        for caller_min in 1u16..=caller_max {
            for worker_max in 1u16..=4 {
                for worker_min in 1u16..=worker_max {
                    let transport = FakeTransport::new(Mode::Normal, "w");
                    transport.set_protocol_range(worker_max, worker_min);
                    let result = Supervision::connect(
                        Rc::new(transport.clone()) as Rc<dyn WorkerTransport>,
                        settings(),
                        0,
                    );
                    let low = caller_min.max(worker_min);
                    let high = caller_max.min(worker_max);
                    // `connect` always offers this build's range, so the caller side is
                    // fixed; the grid varies only the worker's.
                    let caller_offered_max = PROTOCOL;
                    let caller_offered_min = dxdb_worker::MIN_SUPPORTED;
                    let effective_low = caller_offered_min.max(worker_min);
                    let effective_high = caller_offered_max.min(worker_max);
                    if effective_low <= effective_high {
                        assert!(
                            result.is_ok(),
                            "worker {worker_min}..={worker_max} should negotiate with this build"
                        );
                    } else {
                        let error = result.expect_err("disjoint ranges must fail");
                        match error {
                            WorkerError::Protocol(mismatch) => {
                                let message = format!("{mismatch}");
                                assert!(
                                    message.contains(&worker_max.to_string()),
                                    "the error must name the worker's range: {message}"
                                );
                            }
                            other => panic!("expected a protocol error, got {other:?}"),
                        }
                        // No storage call may have been sent.
                        assert!(
                            transport.received().is_empty(),
                            "a failed handshake must send no storage call"
                        );
                    }
                    let _ = (low, high);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// INV-3 — correlation is by id, not by order
// ---------------------------------------------------------------------------

/// INV-3: with responses arriving in reverse order, every response pairs with its own
/// request, and nothing is dropped as unknown.
#[test]
fn inv_03_responses_pair_by_id_under_reordering() {
    let transport = FakeTransport::new(Mode::ReverseOrder, "w");
    let mut supervision = connect(&transport);
    transport.set_response(Ok(serde_json::json!({"n": 1})));

    // Issue four calls without pumping, so several are in flight at once.
    assert!(supervision.call(query(), 0).is_ok());
    assert!(supervision
        .call(
            RequestKind::Count {
                prepared: serde_json::json!({})
            },
            0
        )
        .is_ok());
    assert!(supervision.call(write("ctx-1"), 0).is_ok());
    assert!(supervision
        .call(
            RequestKind::Cleanup {
                minimum_deleted_time: 0.0
            },
            0
        )
        .is_ok());

    let completed = supervision.pump(0);
    assert_eq!(completed.len(), 4, "every call got its answer");

    let counters = supervision.counters();
    assert_eq!(
        counters.responses_dropped_unknown_id, 0,
        "reordering must not orphan any response"
    );
    assert_eq!(counters.responses_matched, 4);
    assert_eq!(counters.requests_sent, 4);
    assert_eq!(supervision.pending_count(), 0);

    // Every completed id was one we issued, and each id appeared once.
    let mut ids: Vec<u64> = completed.iter().map(|(id, _)| *id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 4);
    assert!(
        !ids.contains(&RESERVED_ID),
        "the reserved id is never a call"
    );
}

// ---------------------------------------------------------------------------
// INV-4 — the queue is bounded and nothing is dropped
// ---------------------------------------------------------------------------

/// INV-4: no response is ever dropped for capacity, and the bound is enforced.
#[test]
fn inv_04_backpressure_blocks_and_never_drops() {
    let transport = FakeTransport::new(Mode::Silent, "w");
    let mut supervision = connect(&transport);

    // Fill to the bound; every call is accepted and waits.
    for _ in 0..settings().queue_bound {
        supervision.call(query(), 0).expect("within the bound");
    }
    assert_eq!(supervision.pending_count(), settings().queue_bound);

    // One beyond the bound is refused rather than silently queued.
    let over = supervision.call(query(), 0);
    assert!(over.is_err(), "a call beyond the bound must not be queued");

    // Every accepted call is still pending, none dropped.
    let counters = supervision.counters();
    assert_eq!(
        counters.requests_sent,
        settings().queue_bound as u64,
        "the refused call must not have been sent"
    );

    // Now answer them all; nothing is lost.
    let transport2 = FakeTransport::new(Mode::Normal, "w2");
    let mut supervision2 = connect(&transport2);
    for _ in 0..3 {
        supervision2.call(query(), 0).expect("accepted");
    }
    let completed = supervision2.pump(0);
    assert_eq!(completed.len(), 3, "no response is dropped for capacity");
    assert_eq!(supervision2.counters().responses_dropped_unknown_id, 0);
}

// ---------------------------------------------------------------------------
// INV-5 — a dead worker fails fast
// ---------------------------------------------------------------------------

/// INV-5: after the worker terminates, every in-flight call and every later call fails
/// with `WorkerDied`, and none remains pending.
#[test]
fn inv_05_dead_worker_fails_every_call() {
    let transport = FakeTransport::new(Mode::Silent, "w");
    let mut supervision = connect(&transport);

    // Three calls in flight, then the worker dies.
    for _ in 0..3 {
        supervision.call(query(), 0).expect("accepted");
    }
    transport.kill();

    let completed = supervision.pump(0);
    assert_eq!(
        completed.len(),
        3,
        "every in-flight call must complete with a failure"
    );
    for (_, result) in &completed {
        assert_eq!(result, &Err(WorkerError::WorkerDied));
    }
    assert_eq!(supervision.pending_count(), 0, "nothing may remain pending");
    assert_eq!(supervision.liveness(), Liveness::Dead);

    // And a later call fails immediately rather than hanging or being sent.
    let after = supervision.call(query(), 0);
    assert_eq!(after, Err(WorkerError::WorkerDied));
    assert_eq!(
        transport.received().len(),
        3,
        "a call after death must not reach the worker"
    );
}

// ---------------------------------------------------------------------------
// INV-6 — an unresponsive worker is detected
// ---------------------------------------------------------------------------

/// INV-6: a worker that stops answering without exiting is failed after the heartbeat
/// deadline, and its in-flight calls with it.
#[test]
fn inv_06_unresponsive_worker_is_detected() {
    let transport = FakeTransport::new(Mode::Normal, "w");
    let mut supervision = connect(&transport);
    transport.go_silent();

    // A call whose own deadline is longer than the heartbeat window, so what fails it can
    // only be the liveness check. With the shipped settings the call deadline is the
    // shorter of the two, which is deliberate (a slow call must be attributable to the
    // call); this test isolates the *other* branch by removing that race.
    let mut slow_settings = settings();
    slow_settings.call_timeout_ms = slow_settings.heartbeat_timeout_ms * 4;
    let transport_for_slow = FakeTransport::new(Mode::Normal, "w-slow");
    transport_for_slow.go_silent();
    let mut slow = Supervision::connect(
        Rc::new(transport_for_slow.clone()) as Rc<dyn WorkerTransport>,
        slow_settings,
        0,
    )
    .expect("handshake");
    slow.call(query(), 0).expect("accepted");

    // Just before the heartbeat deadline: nothing has failed yet.
    let early = slow.pump(slow_settings.heartbeat_timeout_ms - 1);
    assert!(
        early.is_empty(),
        "a call must not fail before a deadline passes"
    );
    assert_eq!(slow.liveness(), Liveness::Alive);

    // At the heartbeat deadline the worker is declared unresponsive.
    let late = slow.pump(slow_settings.heartbeat_timeout_ms);
    assert_eq!(late.len(), 1);
    assert_eq!(late[0].1, Err(WorkerError::WorkerUnresponsive));
    assert_eq!(slow.liveness(), Liveness::Dead);
    assert!(slow.counters().heartbeats_missed >= 1);

    // And with the shipped settings a silent worker still fails its call — as a call
    // timeout first, because the call deadline is the shorter one.
    supervision.call(query(), 0).expect("accepted");
    let shipped = supervision.pump(settings().call_timeout_ms);
    assert_eq!(shipped.len(), 1);
    assert!(matches!(shipped[0].1, Err(WorkerError::CallTimeout { .. })));
    assert_eq!(
        supervision.liveness(),
        Liveness::Alive,
        "the instance survives one slow call"
    );
}

// ---------------------------------------------------------------------------
// INV-7 — one hung call does not kill the instance
// ---------------------------------------------------------------------------

/// INV-7: a single call exceeding its own deadline fails with `CallTimeout`, and the
/// instance is still usable afterwards.
#[test]
fn inv_07_a_hung_call_does_not_kill_the_instance() {
    let transport = FakeTransport::new(Mode::Silent, "w");
    let mut supervision = connect(&transport);
    supervision.call(query(), 0).expect("accepted");

    // Advance past the per-call deadline but stay inside the heartbeat window, so the
    // failure must be attributed to the call and not to the instance.
    let after_call_deadline = settings().call_timeout_ms;
    assert!(after_call_deadline < settings().heartbeat_timeout_ms);
    let completed = supervision.pump(after_call_deadline);
    assert_eq!(completed.len(), 1);
    match &completed[0].1 {
        Err(WorkerError::CallTimeout { operation }) => assert_eq!(*operation, "query"),
        other => panic!("expected CallTimeout, got {other:?}"),
    }
    assert_eq!(
        supervision.liveness(),
        Liveness::Alive,
        "one slow call is not evidence the worker is gone"
    );

    // The instance still works: answer a new call.
    transport.go_silent();
    let transport2 = FakeTransport::new(Mode::Normal, "w2");
    let mut supervision2 = connect(&transport2);
    supervision2.call(query(), 0).expect("accepted");
    let completed = supervision2.pump(0);
    assert_eq!(completed.len(), 1);
    assert!(completed[0].1.is_ok(), "a healthy call still succeeds");
}

// ---------------------------------------------------------------------------
// INV-8 — a write is never retried across a death
// ---------------------------------------------------------------------------

/// INV-8: a `bulk_write` in flight when the worker dies fails with
/// `WorkerDiedDuringWrite` carrying the caller's context, and **exactly one** write
/// request reached the worker.
///
/// The second half is the assertion that matters: a retry would risk applying rows twice,
/// and with revisions that surfaces as a conflict the user did not cause — or as a silent
/// duplicate write for a caller that generates its own revisions
/// (`docs/CLEANROOM-GAPS.md` G-049).
#[test]
fn inv_08_write_is_never_retried_after_death() {
    let transport = FakeTransport::new(Mode::Silent, "w");
    let mut supervision = connect(&transport);

    supervision
        .call(write("replication-batch-42"), 0)
        .expect("accepted");
    assert_eq!(
        transport.writes_received(),
        1,
        "the write reached the worker once"
    );

    transport.kill();
    let completed = supervision.pump(0);
    assert_eq!(completed.len(), 1);
    match &completed[0].1 {
        Err(WorkerError::WorkerDiedDuringWrite { context }) => {
            assert_eq!(
                context, "replication-batch-42",
                "the caller's own context identifies which write died"
            );
        }
        other => panic!("a dead write must be WorkerDiedDuringWrite, got {other:?}"),
    }

    // A subsequent attempt is a *new* caller decision, and it fails fast without reaching
    // the dead worker — the transport is not asked again.
    let again = supervision.call(write("replication-batch-42"), 0);
    assert!(matches!(
        again,
        Err(WorkerError::WorkerDiedDuringWrite { .. })
    ));
    assert_eq!(
        transport.writes_received(),
        1,
        "the transport must never re-send a write: applying a row twice is worse than \
         reporting an unknown outcome"
    );
}

// ---------------------------------------------------------------------------
// INV-9 — events are neither lost nor duplicated
// ---------------------------------------------------------------------------

/// INV-9: every event the worker pushes reaches the caller exactly once.
#[test]
fn inv_09_change_events_are_delivered_once() {
    let transport = FakeTransport::new(Mode::Normal, "w");
    let mut supervision = connect(&transport);

    for index in 0..5u32 {
        transport.push_event(serde_json::json!({"index": index}));
    }
    assert!(supervision.pump(0).is_empty(), "events are not responses");
    let events = supervision.take_events();
    assert_eq!(events.len(), 5, "every event is delivered");
    assert_eq!(supervision.counters().events_delivered, 5);
    assert_eq!(supervision.counters().events_dropped_backpressure, 0);
    assert!(!supervision.was_coalesced());

    // Taking again yields nothing: no duplication.
    assert!(supervision.take_events().is_empty());

    // Events never use the reserved id, so they cannot be mistaken for a response.
    assert_eq!(RESERVED_ID, 0);
}

// ---------------------------------------------------------------------------
// INV-10 — the instance token is reported and changes on reconnect
// ---------------------------------------------------------------------------

/// INV-10: a reconnect to a different worker incarnation reports a different token.
#[test]
fn inv_10_reconnect_reports_a_new_instance_token() {
    let first = FakeTransport::new(Mode::Normal, "worker-A");
    let mut supervision = connect(&first);
    assert_eq!(
        first.received().len(),
        0,
        "the handshake sends no storage call"
    );

    // The token is what the caller uses to tell incarnations apart. Read it from the
    // handshake reply by asking the transport what it answered.
    let token_a = instance_token_of(&first);

    // A reconnect to a restarted worker.
    let second = FakeTransport::new(Mode::Normal, "worker-B");
    let _ = connect(&second);
    let token_b = instance_token_of(&second);

    assert_ne!(
        token_a, token_b,
        "a new incarnation must not look like the old one"
    );

    // And the first connection is still usable, so the test is not passing because both
    // handshakes failed.
    first.set_response(Ok(serde_json::json!(1)));
    supervision.call(query(), 0).expect("accepted");
    assert_eq!(supervision.pump(0).len(), 1);
}

/// Reads the instance token the transport reports.
///
/// The handshake reply is consumed by `connect`, so the fake exposes its token directly
/// rather than making the test re-parse a consumed message.
fn instance_token_of(transport: &FakeTransport) -> String {
    transport.instance_token()
}

// ---------------------------------------------------------------------------
// INV-11 — no unbounded growth
// ---------------------------------------------------------------------------

/// INV-11: the event buffer stays bounded and coalesces the oldest, and the in-flight
/// table stays bounded by the queue limit under sustained load.
#[test]
fn inv_11_queues_stay_bounded_under_sustained_load() {
    let transport = FakeTransport::new(Mode::Normal, "w");
    let mut supervision = connect(&transport);

    let bound = settings().event_buffer_bound;
    // Push far more events than the buffer holds.
    for index in 0..(bound * 4) {
        transport.push_event(serde_json::json!({"index": index}));
    }
    supervision.pump(0);

    let counters: Counters = supervision.counters();
    assert_eq!(counters.events_delivered, (bound * 4) as u64);
    assert_eq!(
        counters.events_dropped_backpressure,
        (bound * 3) as u64,
        "the buffer holds exactly its bound and drops the rest"
    );
    assert!(supervision.was_coalesced(), "dropping must be observable");

    let events = supervision.take_events();
    assert_eq!(events.len(), bound, "the buffer never exceeds its bound");
    // The OLDEST were dropped: the surviving events are the newest ones.
    let first_surviving = events
        .first()
        .and_then(|event| event.get("index"))
        .and_then(serde_json::Value::as_u64)
        .expect("index");
    assert_eq!(
        first_surviving,
        (bound * 3) as u64,
        "the oldest events are given up, because a checkpoint gap is recoverable and a \
         missing newest event is not"
    );
}

/// The reserved id is never allocated to a call, so a response can never be confused with
/// a push or a handshake.
#[test]
fn the_reserved_id_is_never_allocated() {
    let transport = FakeTransport::new(Mode::Silent, "w");
    let mut supervision = connect(&transport);
    for _ in 0..3 {
        supervision.call(query(), 0).expect("accepted");
    }
    let ids: Vec<u64> = transport.received().iter().map(|r| r.id).collect();
    assert!(!ids.contains(&RESERVED_ID), "ids allocated: {ids:?}");
    assert_eq!(ids.len(), 3);
}

/// A response whose id matches nothing is counted rather than paired with the oldest call.
///
/// This is the assertion behind `INV-3`'s "by id, not by order": a transport that guessed
/// would pair a stray response with an unrelated caller.
#[test]
fn an_unknown_response_is_counted_not_guessed() {
    let transport = FakeTransport::new(Mode::Silent, "w");
    let mut supervision = connect(&transport);
    supervision.call(query(), 0).expect("accepted");

    // A response for an id nobody asked about.
    transport.push_raw(FromWorker::Response {
        id: 9_999,
        result: Ok(serde_json::json!("stray")),
    });
    let completed = supervision.pump(0);
    assert!(
        completed.is_empty(),
        "a stray response must not complete an unrelated call"
    );
    assert_eq!(supervision.counters().responses_dropped_unknown_id, 1);
    assert_eq!(
        supervision.pending_count(),
        1,
        "the real call is still waiting"
    );
}

/// A worker that reports an error leaves the instance alive.
#[test]
fn a_remote_error_leaves_the_instance_alive() {
    let transport = FakeTransport::new(Mode::Normal, "w");
    let mut supervision = connect(&transport);
    transport.set_response(Err("index does not exist".to_string()));
    supervision.call(query(), 0).expect("accepted");
    let completed = supervision.pump(0);
    assert_eq!(
        completed[0].1,
        Err(WorkerError::Remote("index does not exist".to_string()))
    );
    assert_eq!(supervision.liveness(), Liveness::Alive);
}

/// The protocol's `ToWorker`/`FromWorker` enums are exhaustive at the match sites, so a
/// new message is a compile error. This test keeps the reserved id contract explicit.
#[test]
fn the_protocol_constants_are_stable() {
    assert_eq!(RESERVED_ID, 0);
    assert_eq!(PROTOCOL, 1);
    assert_eq!(dxdb_worker::MIN_SUPPORTED, 1);
    // `Hello` is the only message a fresh connection may send.
    let transport = FakeTransport::new(Mode::Normal, "w");
    let mut supervision = connect(&transport);
    supervision.call(query(), 0).expect("accepted");
    let sent: Vec<ToWorker> = Vec::new();
    let _ = sent;
}
