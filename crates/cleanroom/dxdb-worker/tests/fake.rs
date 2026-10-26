//! A fake worker transport: in-process, deterministic, and able to fail on command.
//!
//! This is the seam that makes supervision testable at all. A test that needed a real
//! browser worker could not arrange for the worker to die *exactly* while a `bulk_write` is
//! in flight; here that is one flag.

use std::cell::RefCell;
use std::rc::Rc;

use dxdb_worker::protocol::{FromWorker, RequestKind, ToWorker, PROTOCOL};
use dxdb_worker::supervision::{TransportError, WorkerTransport};

/// How the fake worker behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Answers every request immediately, in order.
    Normal,
    /// Answers every request, but in reverse order of arrival.
    ReverseOrder,
    /// Accepts requests and never answers.
    Silent,
    /// Terminates: every send fails and `is_terminated` reports true.
    Dead,
}

/// One request the fake received.
#[derive(Debug, Clone, PartialEq)]
pub struct Received {
    /// The correlation id.
    pub id: u64,
    /// The operation.
    pub kind: RequestKind,
}

/// The fake transport.
#[derive(Clone)]
pub struct FakeTransport {
    inner: Rc<RefCell<FakeState>>,
}

#[derive(Debug)]
struct FakeState {
    mode: Mode,
    /// Messages the caller sent.
    sent: Vec<ToWorker>,
    /// Requests received, in arrival order (including writes).
    received: Vec<Received>,
    /// Messages waiting to be delivered to the caller.
    inbox: Vec<FromWorker>,
    terminated: bool,
    /// How many bulk_write requests arrived. `INV-8` asserts this is exactly 1.
    writes_received: usize,
    /// The instance token this worker reports.
    instance_token: String,
    /// A canned response value per operation name, or an error message.
    response: Result<serde_json::Value, String>,
    /// The worker's protocol range, for handshake tests.
    protocol: u16,
    min_supported: u16,
}

impl FakeTransport {
    /// A transport in `mode`, reporting `instance_token`.
    pub fn new(mode: Mode, instance_token: &str) -> Self {
        Self {
            inner: Rc::new(RefCell::new(FakeState {
                mode,
                sent: Vec::new(),
                received: Vec::new(),
                inbox: Vec::new(),
                terminated: mode == Mode::Dead,
                writes_received: 0,
                instance_token: instance_token.to_string(),
                response: Ok(serde_json::json!({"ok": true})),
                protocol: PROTOCOL,
                min_supported: 1,
            })),
        }
    }

    /// The requests the fake received.
    pub fn received(&self) -> Vec<Received> {
        self.inner.borrow().received.clone()
    }

    /// The instance token this worker reports in its handshake reply.
    pub fn instance_token(&self) -> String {
        self.inner.borrow().instance_token.clone()
    }

    /// How many `bulk_write` requests arrived.
    pub fn writes_received(&self) -> usize {
        self.inner.borrow().writes_received
    }

    /// Sets the canned response.
    pub fn set_response(&self, response: Result<serde_json::Value, String>) {
        self.inner.borrow_mut().response = response;
    }

    /// Sets the protocol range this worker advertises.
    pub fn set_protocol_range(&self, max: u16, min: u16) {
        let mut state = self.inner.borrow_mut();
        state.protocol = max;
        state.min_supported = min;
    }

    /// Kills the worker. In-flight calls observe this on the next `pump`.
    pub fn kill(&self) {
        let mut state = self.inner.borrow_mut();
        state.mode = Mode::Dead;
        state.terminated = true;
        state.inbox.clear();
    }

    /// Stops answering without terminating, simulating a hang.
    pub fn go_silent(&self) {
        self.inner.borrow_mut().mode = Mode::Silent;
    }

    /// Queues an event push.
    pub fn push_event(&self, bulk: serde_json::Value) {
        self.inner
            .borrow_mut()
            .inbox
            .push(FromWorker::Event { bulk });
    }

    /// Queues a raw message, for protocol-abuse tests.
    pub fn push_raw(&self, message: FromWorker) {
        self.inner.borrow_mut().inbox.push(message);
    }

    fn handle(&self, message: &ToWorker) {
        let mut state = self.inner.borrow_mut();
        state.sent.push(message.clone());
        match message {
            ToWorker::Hello {
                protocol,
                min_supported,
            } => {
                let worker_max = state.protocol;
                let worker_min = state.min_supported;
                let low = (*min_supported).max(worker_min);
                let high = (*protocol).min(worker_max);
                // Clone before mutating `state.inbox`, so the borrow checker can see the
                // reads finish first.
                let token = state.instance_token.clone();
                if low > high {
                    state.inbox.push(FromWorker::HelloRejected {
                        protocol: worker_max,
                        min_supported: worker_min,
                    });
                } else {
                    state.inbox.push(FromWorker::HelloReply {
                        protocol: high,
                        storage_name: "fake".to_string(),
                        instance_token: token,
                    });
                }
            }
            ToWorker::Ping { id } => {
                if state.mode != Mode::Silent && !state.terminated {
                    state.inbox.push(FromWorker::Pong { id: *id });
                }
            }
            ToWorker::Request { id, kind } => {
                state.received.push(Received {
                    id: *id,
                    kind: kind.clone(),
                });
                if matches!(kind, RequestKind::BulkWrite { .. }) {
                    state.writes_received += 1;
                }
                if state.mode == Mode::Silent || state.terminated {
                    return;
                }
                let response = match &state.response {
                    Ok(value) => Ok(value.clone()),
                    Err(message) => Err(message.clone()),
                };
                let reply = FromWorker::Response {
                    id: *id,
                    result: response,
                };
                match state.mode {
                    Mode::ReverseOrder => state.inbox.insert(0, reply),
                    _ => state.inbox.push(reply),
                }
            }
            ToWorker::Subscribe | ToWorker::Unsubscribe | ToWorker::Goodbye => {}
        }
    }
}

impl WorkerTransport for FakeTransport {
    fn send(&self, message: &ToWorker) -> Result<(), TransportError> {
        if self.inner.borrow().terminated {
            return Err(TransportError::Gone);
        }
        self.handle(message);
        Ok(())
    }

    fn try_recv(&self) -> Option<FromWorker> {
        let mut state = self.inner.borrow_mut();
        if state.inbox.is_empty() {
            None
        } else {
            Some(state.inbox.remove(0))
        }
    }

    fn is_terminated(&self) -> bool {
        self.inner.borrow().terminated
    }
}
