//! The JS-facing side of the `Observable` adapter.
//!
//! [`super::state`] holds the rules; this module holds the objects JS touches and
//! is the only place that crosses the boundary. It is `#[cfg(target_arch =
//! "wasm32")]` because `wasm_bindgen`'s callback types are not useful on the host,
//! where the policy is tested directly.
//!
//! ## Shape
//!
//! Three exported classes, which is the smallest set that satisfies PLAN.md 3.3's
//! "no RxJS, adapt at the boundary":
//!
//! - `Subscription` — what `subscribe` returns. `unsubscribe()` releases the
//!   stream's generation. `closed` reports whether *this* subscription is closed.
//! - `Observable` — `subscribe(next, error, complete)`. Holds the stream.
//! - `Emitter` — the manual push API. Not the plan's long-term public surface;
//!   it exists because a Rust `Stream` cannot cross the boundary, so JS needs *a*
//!   way to drive an observable in tests and in `dxdb-remote`'s message channel.
//!
//! The real production path is [`from_stream`], which drives a
//! `futures::Stream` with `spawn_local` and calls exactly the same policy.
//!
//! ## Cancellation, stated honestly
//!
//! Unsubscribing does not abort a running stream pump. It cannot, safely: the
//! pump may be parked in `poll_next`, and there is no cancellation protocol to
//! await until `dxdb-storage` publishes one. Instead the pump keeps polling and
//! every value lands in `Inner::deliver`, which returns immediately when nothing
//! is subscribed. So the *contract* holds ("no callback after unsubscribe") at
//! the cost of polling an abandoned stream until it ends. Recorded in
//! `docs/lanes/m4-browser-plan.md` §9 open question 3.

use std::cell::{Cell, RefCell};
use std::pin::Pin;
use std::rc::Rc;

use futures::Stream;
use wasm_bindgen::prelude::*;

use super::state::{Delivery, Event, EventClass, State};

/// The item shape a source stream produces: `Ok` becomes `next`, `Err` becomes
/// `error` and ends the stream, exhaustion becomes `complete`.
pub type StreamItem = Result<JsValue, JsValue>;

/// A boxed source stream. `dyn` because the concrete type is the storage's.
type Source = Pin<Box<dyn Stream<Item = StreamItem>>>;

/// A JS-side callback table for one subscription.
///
/// `Option` per callback because `subscribe` accepts a bare `next` function, an
/// observer object, or a subset — an absent callback is a no-op, not an error.
struct Callbacks {
    next: Option<js_sys::Function>,
    error: Option<js_sys::Function>,
    complete: Option<js_sys::Function>,
}

/// Everything an observable owns, wrapped in one `Rc` so a pump can hold it while
/// JS holds the observable. Never stored by value: every constructor produces an
/// `Rc<Shared>` and every method takes `&Rc<Shared>`, which is what lets
/// `spawn_local` capture a handle.
///
/// `RefCell`/`Cell` rather than atomics because wasm32 has one thread by default
/// (PLAN.md 3.4: threads need a different target and are not a default).
pub(super) struct Shared {
    state: RefCell<State>,
    /// The callbacks of the currently live subscription, if any.
    callbacks: RefCell<Option<Rc<Callbacks>>>,
    /// A source stream, for the `from_stream` path. `None` for an `Emitter`,
    /// which is pushed to directly.
    source: RefCell<Option<Source>>,
    /// Set once the producer knows no more events will arrive, so a later
    /// `subscribe` completes immediately instead of hanging.
    source_ended: Cell<bool>,
}

impl Shared {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            state: RefCell::new(State::new()),
            callbacks: RefCell::new(None),
            source: RefCell::new(None),
            source_ended: Cell::new(false),
        })
    }

    /// Deliver `event` if the policy allows it. The whole point of the module:
    /// the policy decides, this only performs the call.
    ///
    /// No `RefCell` borrow is held while JS runs, which is what makes
    /// `unsubscribe()` from inside a callback work instead of panicking.
    fn deliver(&self, event: &Event, to_js: impl FnOnce(&Callbacks) -> Result<(), JsValue>) {
        let class = {
            let mut state = self.state.borrow_mut();
            match state.submit(event) {
                Delivery::Drop => return,
                Delivery::Deliver(class) => {
                    // Record BEFORE calling out. If the consumer's callback throws
                    // and we unwind, the state must not think this delivery is
                    // still owed — that is what would wedge the subscription.
                    state.submitted(class);
                    class
                }
            }
        };

        // Clone the `Rc` out and let the borrow end before crossing into JS, so a
        // callback that calls `unsubscribe()` can take the table without
        // tripping the RefCell.
        let callbacks = self.callbacks.borrow().clone();
        let Some(callbacks) = callbacks else {
            return;
        };

        // A throwing consumer callback must not trap the module. `call1` returns
        // Err instead of propagating, and the error is deliberately swallowed:
        // the value was accepted by the policy and delivered, and re-surfacing a
        // consumer's own bug as a database failure would misattribute it (plan
        // §2.2 rule 5). The subscription stays open.
        let _ = to_js(&callbacks);

        // A terminal delivery closes the subscription from the producer side, so
        // a `next` after `complete` is dropped by the policy rather than called
        // on a torn-down observer.
        if matches!(class, EventClass::Complete | EventClass::Error) {
            self.callbacks.borrow_mut().take();
        }
    }

    fn next_value(&self, value: JsValue) {
        let payload = value.clone();
        self.deliver(&Event::Next, move |cbs| match &cbs.next {
            Some(f) => f.call1(&JsValue::NULL, &payload).map(|_| ()),
            None => Ok(()),
        });
    }

    fn fail(&self, err: JsValue) {
        self.source_ended.set(true);
        // The policy only classifies the event; the payload stays here. The
        // `String` is for diagnosis if the policy ever has to log a drop.
        let err_text = err
            .as_string()
            .or_else(|| js_sys::JSON::stringify(&err).ok().map(String::from))
            .unwrap_or_else(|| "<non-string error>".to_string());
        let payload = err;
        self.deliver(&Event::Error(err_text), move |cbs| match &cbs.error {
            Some(f) => f.call1(&JsValue::NULL, &payload).map(|_| ()),
            None => Ok(()),
        });
    }

    fn finish(&self) {
        self.source_ended.set(true);
        self.deliver(&Event::Complete, |cbs| match &cbs.complete {
            Some(f) => f.call0(&JsValue::NULL).map(|_| ()),
            None => Ok(()),
        });
    }

    /// Start pumping the attached source, if any.
    ///
    /// `self_rc` is passed in rather than manufactured here because `Inner` is
    /// never owned by value — that is the whole reason it is an `Rc`.
    fn start_pump(self: &Rc<Self>) {
        let Some(source) = self.source.borrow_mut().take() else {
            return;
        };
        let inner = Rc::clone(self);
        use futures::StreamExt as _;
        wasm_bindgen_futures::spawn_local(async move {
            let mut source = source;
            while let Some(item) = source.next().await {
                match item {
                    Ok(value) => inner.next_value(value),
                    Err(err) => {
                        inner.fail(err);
                        return;
                    }
                }
            }
            inner.finish();
        });
    }

    /// Open a subscription over `self_rc` and return its handle.
    fn subscribe_impl(
        self: &Rc<Self>,
        next: JsValue,
        error: JsValue,
        complete: JsValue,
    ) -> Subscription {
        let callbacks = Rc::new(parse_callbacks(next, error, complete));

        // A terminal stream cannot produce again. Handing back a pre-closed
        // subscription is the honest answer: the consumer does not hang, and it
        // does not get a replayed `complete`.
        if self.state.borrow().is_terminal() {
            return Subscription {
                shared: Rc::clone(self),
                token: 0,
                closed: Rc::new(Cell::new(true)),
            };
        }

        let token = self.state.borrow_mut().acquire();
        let closed = Rc::new(Cell::new(false));

        // A new subscription replaces the previous one's callbacks. A stream has
        // one consumer at a time, and `acquire` already moved the generation, so a
        // stale `unsubscribe` cannot affect this one.
        *self.callbacks.borrow_mut() = Some(callbacks);

        // Complete a late subscriber immediately rather than leave it waiting for
        // an event that cannot come.
        if self.source_ended.get() {
            self.finish();
        } else {
            self.start_pump();
        }

        Subscription {
            shared: Rc::clone(self),
            token,
            closed,
        }
    }
}

/// Read the `subscribe(...)` arguments into a callback table.
///
/// Kept as a free function so the argument-shape rules are stated once, and so
/// both `Observable` and `Emitter` paths behave identically.
fn parse_callbacks(next: JsValue, error: JsValue, complete: JsValue) -> Callbacks {
    let as_fn = |v: &JsValue| -> Option<js_sys::Function> {
        if v.is_function() {
            Some(js_sys::Function::from(v.clone()))
        } else {
            None
        }
    };

    // The observer-object form: if `next` is not a function but is an object,
    // read its three members. Reading it here rather than in JS keeps the whole
    // contract in one file.
    if !next.is_function() && next.is_object() && !next.is_null() {
        let get = |key: &str| -> Option<js_sys::Function> {
            let value = js_sys::Reflect::get(&next, &JsValue::from_str(key)).ok()?;
            as_fn(&value)
        };
        return Callbacks {
            next: get("next"),
            error: get("error"),
            complete: get("complete"),
        };
    }

    Callbacks {
        next: as_fn(&next),
        error: as_fn(&error),
        complete: as_fn(&complete),
    }
}

// ---------------------------------------------------------------------------
// Subscription
// ---------------------------------------------------------------------------

/// The object `Observable.subscribe` returns.
#[wasm_bindgen]
pub struct Subscription {
    shared: Rc<Shared>,
    token: u64,
    closed: Rc<Cell<bool>>,
}

#[wasm_bindgen]
impl Subscription {
    /// `true` once this subscription has been closed, by `unsubscribe` or by a
    /// terminal event. **Not** updated by a terminal event on the producer side:
    /// this reports whether *this* consumer has closed, which is the question a
    /// consumer is actually asking. Use the stream's completion callbacks for the
    /// rest.
    #[wasm_bindgen(getter)]
    pub fn closed(&self) -> bool {
        self.closed.get()
    }

    /// Stop delivery. Idempotent. After this returns, no callback fires — not
    /// even one the producer had already queued.
    pub fn unsubscribe(&self) {
        if self.closed.replace(true) {
            return;
        }
        // Order matters: drop the JS callback references first so a re-entrant
        // call cannot observably reach them, then release the generation so the
        // producer's *next* `submit` returns `Drop`. Both halves are required —
        // the policy alone would still deliver to a callback that is gone.
        self.shared.callbacks.borrow_mut().take();
        self.shared.state.borrow_mut().release(self.token);
    }
}

// ---------------------------------------------------------------------------
// Observable
// ---------------------------------------------------------------------------

/// A stream of values, subscribed with `(next, error, complete)` callbacks.
///
/// Structurally an `Observable` in the sense an RxJS `from()` accepts, without
/// this crate depending on RxJS. That is PLAN.md 3.3's boundary condition.
#[wasm_bindgen]
pub struct Observable {
    shared: Rc<Shared>,
}

#[wasm_bindgen]
impl Observable {
    /// `subscribe(next, error, complete) -> Subscription`.
    ///
    /// `next` may be a function, or an observer object `{ next?, error?,
    /// complete? }`; `error` and `complete` are functions or `undefined`.
    /// A missing callback is a no-op.
    #[wasm_bindgen]
    pub fn subscribe(&self, next: JsValue, error: JsValue, complete: JsValue) -> Subscription {
        self.shared.subscribe_impl(next, error, complete)
    }
}

// ---------------------------------------------------------------------------
// Emitter: the manual push API
// ---------------------------------------------------------------------------

/// The producer side of an `Observable`, pushed to by hand.
///
/// The real stream path is [`from_stream`]; this is what a non-Rust source (a
/// `MessageChannel`, a DOM event) and the browser tests use.
#[wasm_bindgen]
pub struct Emitter {
    shared: Rc<Shared>,
}

#[wasm_bindgen]
impl Emitter {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Emitter {
        Emitter {
            shared: Shared::new(),
        }
    }

    /// The `Observable` half. Multiple calls return observables over the same
    /// stream, which is what a "hot" source is.
    pub fn observable(&self) -> Observable {
        Observable {
            shared: Rc::clone(&self.shared),
        }
    }

    /// Emit a value. Dropped silently when nothing is subscribed, which is the
    /// documented behaviour of a hot stream rather than an error.
    pub fn next(&self, value: JsValue) {
        self.shared.next_value(value);
    }

    /// End the stream successfully. `complete` fires at most once, never after an
    /// error, and never after `unsubscribe`.
    pub fn complete(&self) {
        self.shared.finish();
    }

    /// Fail the stream. `error` fires at most once, never after `complete`, and
    /// never after `unsubscribe`.
    pub fn error(&self, message: JsValue) {
        self.shared.fail(message);
    }
}

impl Default for Emitter {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// The futures::Stream entry point
// ---------------------------------------------------------------------------

/// Wrap a `futures::Stream` as an `Observable`.
///
/// This is the path real change streams and query streams take. It has no
/// `#[wasm_bindgen]` attribute because a Rust `Stream` cannot cross the boundary:
/// the only callers are Rust code inside this crate and above it, and JS receives
/// the `Observable` that comes out.
pub fn from_stream<S>(stream: S) -> Observable
where
    S: Stream<Item = StreamItem> + 'static,
{
    let shared = Shared::new();
    *shared.source.borrow_mut() = Some(Box::pin(stream));
    Observable { shared }
}
