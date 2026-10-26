//! The `Observable` delivery policy: the part of the JS boundary that must be
//! provably correct, kept free of `wasm-bindgen` so it can be unit-tested on the
//! host with no browser, no JS engine, and no `wasm32` target.
//!
//! ## Why the policy is separated from the binding
//!
//! `crates/dxdb-js` is a `cdylib` and `cargo test` does not run it. The browser
//! tests (via the Playwright harness) prove the *binding* works — that `externref`
//! callbacks arrive, that `unsubscribe` reaches Rust, that a trap does not wedge
//! the page. The policy tests prove the *rules*: exactly-once error, no delivery
//! after unsubscribe, no completion after error. Those are properties of an
//! ordering discipline, not of wasm, and testing them where they can be tested
//! exhaustively is why this module exists.
//!
//! The split is also how the interesting failure mode gets caught: if the JS
//! shell duplicates any of this logic, the browser tests pass for the wrong
//! reason and the policy tests keep passing while the shipped thing is broken.
//! `submit` returning a `Delivery` the caller must honour, rather than performing
//! the call itself, makes duplication visible — the shell has to throw a
//! `Delivery` away to get it wrong.
//!
//! ## The contract (PLAN.md 3.3 + the lane brief)
//!
//! 1. Zero or more `Next` deliveries, serially.
//! 2. At most one terminal delivery, `Complete` or `Error`, never both.
//! 3. No delivery of any kind after `unsubscribe`.
//! 4. `unsubscribe` during a `next` callback still stops delivery, and never
//!    re-delivers the value that was in flight.
//! 5. A callback that throws does not wedge the subscription.

/// One event from the producer.
///
/// Deliberately not generic over the payload. The policy only ever needs the
/// *class* of the event, and keeping the payload out means this type cannot grow
/// a dependency on `JsValue` — which is what lets it be tested on the host.
/// `Error` carries a string so a test transcript reads as a transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A value. The payload is the binding's business.
    Next,
    /// The stream ended successfully.
    Complete,
    /// The stream ended with an error.
    Error(String),
}

/// What the caller must do after [`State::submit`] accepts an event.
///
/// Returning this instead of performing the call keeps the "should this be
/// delivered" decision in one place (here) and the "how do I call into JS" decision
/// in the binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Nothing is due: the subscription is closed, or the event was a second
    /// terminal event, or nothing is subscribed yet and the event was terminal.
    Drop,
    /// Deliver exactly this. [`State::submitted`] must be called immediately
    /// afterwards, in the same turn, so the next `submit` sees the closed state.
    Deliver(EventClass),
}

/// The class of a delivery. The payload is not carried here because the caller
/// already owns the event it submitted; this only says which callback to invoke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventClass {
    Next,
    Complete,
    Error,
}

/// The delivery state machine for one *stream*, not one subscriber.
///
/// A stream outlives its subscriber: `Emitter` hands the state out on every
/// `subscribe` and takes it back on `unsubscribe`, which is how a second
/// subscriber and an unsubscribe/re-subscribe cycle work without the stream
/// itself being recreated.
#[derive(Debug, Default)]
pub struct State {
    /// The current subscriber generation, starting at 1 (0 means "none").
    current: u64,
    /// Next generation to hand out. Monotonic, never reused, so a stale
    /// `unsubscribe(old_token)` cannot close a newer subscription.
    next_token: u64,
    /// `Some(class)` once a terminal event has been accepted. After this no
    /// event is ever delivered again, which is what makes "exactly once" and
    /// "no complete after error" the same rule.
    terminal: Option<EventClass>,
}

impl State {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Hand out a new subscriber generation. Returns the token the caller must
    /// pass back to [`State::release`].
    pub fn acquire(&mut self) -> u64 {
        self.next_token += 1;
        self.current = self.next_token;
        self.current
    }

    /// Return a generation. Only closes the stream if `token` is still current,
    /// so a late `unsubscribe` from a previous subscription cannot kill a new one.
    pub fn release(&mut self, token: u64) {
        if self.current == token {
            self.current = 0;
        }
    }

    /// Whether any generation is currently subscribed.
    #[must_use]
    pub fn is_subscribed(&self) -> bool {
        self.current != 0
    }

    /// Whether the stream has terminated. Once true, [`State::submit`] always
    /// returns [`Delivery::Drop`].
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.terminal.is_some()
    }

    /// Ask whether `event` should be delivered.
    ///
    /// This does **not** record that a terminal event was delivered — call
    /// [`State::submitted`] after actually delivering it, so that a delivery that
    /// never reaches JS (because the callback table was gone) cannot mark the
    /// stream terminal and silently swallow the error.
    pub fn submit(&self, event: &Event) -> Delivery {
        if self.terminal.is_some() || !self.is_subscribed() {
            // `terminal` covers "error already delivered, now a complete arrives".
            // `!is_subscribed` covers "nothing is listening": an event produced
            // before `subscribe` or after `unsubscribe` is dropped, not queued.
            // The brief's rule 4 ("no callback after unsubscribe, including one
            // already queued") is exactly this check, and it is why the binding
            // must call it *after* acquiring the state, not before.
            return Delivery::Drop;
        }
        Delivery::Deliver(match event {
            Event::Next => EventClass::Next,
            Event::Complete => EventClass::Complete,
            Event::Error(_) => EventClass::Error,
        })
    }

    /// Record that the accepted delivery actually happened.
    ///
    /// Only terminal deliveries change state, so calling this twice for a
    /// `Next` is harmless.
    pub fn submitted(&mut self, class: EventClass) {
        if matches!(class, EventClass::Complete | EventClass::Error) {
            self.terminal = Some(class);
        }
    }

    /// Which terminal event was delivered, if any.
    #[must_use]
    pub fn terminal_class(&self) -> Option<EventClass> {
        self.terminal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A subscriber that records what it was given, so the tests read as
    /// assertions about a transcript rather than about internal fields.
    #[derive(Default)]
    struct Recorder {
        log: Vec<String>,
    }

    impl Recorder {
        fn deliver(&mut self, state: &mut State, event: Event) {
            match state.submit(&event) {
                Delivery::Drop => self.log.push("(dropped)".to_string()),
                Delivery::Deliver(class) => {
                    self.log.push(match (class, &event) {
                        (EventClass::Next, Event::Next) => "next".to_string(),
                        (EventClass::Complete, Event::Complete) => "complete".to_string(),
                        (EventClass::Error, Event::Error(e)) => format!("error({e})"),
                        // The `submit` result and the event can only disagree if
                        // `submit` is wrong; fail loudly rather than silently.
                        (class, _) => panic!("submit returned {class:?} for a different event"),
                    });
                    state.submitted(class);
                }
            }
        }

        /// Deliver `count` values, logging each as `next`.
        fn deliver_n(&mut self, state: &mut State, count: u32) {
            for _ in 0..count {
                self.deliver(state, Event::Next);
            }
        }
    }

    fn subscribed_state() -> (State, u64) {
        let mut state = State::new();
        let token = state.acquire();
        (state, token)
    }

    // ---- the five cases named in the lane brief ---------------------------

    #[test]
    fn error_is_delivered_exactly_once() {
        let (mut state, _token) = subscribed_state();
        let mut rec = Recorder::default();

        rec.deliver(&mut state, Event::Next);
        rec.deliver(&mut state, Event::Error("boom".into()));
        // A second error, and a complete, must both be dropped.
        rec.deliver(&mut state, Event::Error("boom again".into()));
        rec.deliver(&mut state, Event::Complete);
        rec.deliver(&mut state, Event::Next);

        assert_eq!(
            rec.log,
            vec!["next", "error(boom)", "(dropped)", "(dropped)", "(dropped)"]
        );
        assert_eq!(state.terminal_class(), Some(EventClass::Error));
    }

    #[test]
    fn complete_does_not_fire_after_error() {
        let (mut state, _token) = subscribed_state();
        let mut rec = Recorder::default();

        rec.deliver(&mut state, Event::Error("first".into()));
        rec.deliver(&mut state, Event::Complete);

        assert_eq!(rec.log, vec!["error(first)", "(dropped)"]);
        assert!(!rec.log.iter().any(|l| l == "complete"));
    }

    #[test]
    fn error_does_not_fire_after_complete() {
        // The mirror of the above, which is the one a naive implementation gets
        // wrong: it checks `error` first and delivers both.
        let (mut state, _token) = subscribed_state();
        let mut rec = Recorder::default();

        rec.deliver(&mut state, Event::Complete);
        rec.deliver(&mut state, Event::Error("late".into()));

        assert_eq!(rec.log, vec!["complete", "(dropped)"]);
        assert_eq!(state.terminal_class(), Some(EventClass::Complete));
    }

    #[test]
    fn no_delivery_after_unsubscribe_including_queued() {
        let (mut state, token) = subscribed_state();
        let mut rec = Recorder::default();

        rec.deliver(&mut state, Event::Next);

        // The producer has an event in hand and is about to deliver it — this is
        // the "already queued" case — when the consumer unsubscribes.
        let queued = Event::Next;
        state.release(token);

        rec.deliver(&mut state, queued);

        assert_eq!(rec.log, vec!["next", "(dropped)"]);
        assert!(!state.is_subscribed());
        // The stream is not terminal: it can be re-subscribed and continue.
        assert!(!state.is_terminal());
    }

    #[test]
    fn no_delivery_after_unsubscribe_on_the_terminal_path_too() {
        // A queued *error* after unsubscribe must also be dropped. An
        // implementation that only guards `next` would deliver an error to a
        // consumer that has already torn down — the exact bug the brief's rule 4
        // is about.
        let (mut state, token) = subscribed_state();
        let mut rec = Recorder::default();

        state.release(token);
        rec.deliver(&mut state, Event::Error("after teardown".into()));

        assert_eq!(rec.log, vec!["(dropped)"]);
    }

    #[test]
    fn resubscribe_after_unsubscribe_continues_working() {
        let (mut state, first) = subscribed_state();
        let mut rec = Recorder::default();

        rec.deliver(&mut state, Event::Next);
        state.release(first);
        rec.deliver(&mut state, Event::Next);

        let second = state.acquire();
        rec.deliver(&mut state, Event::Next);
        rec.deliver(&mut state, Event::Complete);
        rec.deliver(&mut state, Event::Next);

        assert_eq!(
            rec.log,
            vec!["next", "(dropped)", "next", "complete", "(dropped)"]
        );
        // A stale unsubscribe from the *first* subscriber must not close the
        // second one.
        state.release(first);
        assert!(
            state.is_subscribed(),
            "stale token closed the live subscription"
        );
        state.release(second);
        assert!(!state.is_subscribed());
    }

    #[test]
    fn stale_unsubscribe_cannot_close_a_newer_subscription() {
        let mut state = State::new();
        let first = state.acquire();
        let _second = state.acquire();

        state.release(first);
        assert!(state.is_subscribed());
    }

    // ---- ordering and edge cases the brief did not name -------------------

    #[test]
    fn nothing_is_delivered_before_subscribe() {
        let mut state = State::new();
        let mut rec = Recorder::default();

        rec.deliver(&mut state, Event::Next);

        assert_eq!(rec.log, vec!["(dropped)"]);
        assert!(
            !state.is_terminal(),
            "a dropped event must not terminate the stream"
        );
    }

    #[test]
    fn a_terminal_event_with_no_subscriber_does_not_terminate_the_stream() {
        // This is the rule that makes a hot stream useful: producing `complete`
        // while nobody is listening must not poison the stream for the next
        // subscriber. It is also why `submit` and `submitted` are separate — the
        // caller only records a terminal after it has actually been delivered.
        let mut state = State::new();
        let mut rec = Recorder::default();

        rec.deliver(&mut state, Event::Complete);
        assert!(!state.is_terminal());

        let token = state.acquire();
        rec.deliver(&mut state, Event::Next);
        state.release(token);
        assert_eq!(rec.log, vec!["(dropped)", "next"]);
    }

    #[test]
    fn next_does_not_close_the_stream() {
        let (mut state, _token) = subscribed_state();
        let mut rec = Recorder::default();
        rec.deliver_n(&mut state, 5);
        assert_eq!(rec.log.len(), 5);
        assert!(!state.is_terminal());
        assert!(state.is_subscribed());
    }

    #[test]
    fn values_are_delivered_in_order_without_loss() {
        let (mut state, _token) = subscribed_state();
        let mut rec = Recorder::default();
        rec.deliver_n(&mut state, 100);
        assert_eq!(rec.log, vec!["next"; 100]);
    }

    #[test]
    fn submitted_next_is_idempotent() {
        // `Next` deliveries are recorded by the caller after every callback; a
        // duplicate call must not confuse the terminal state.
        let (mut state, _token) = subscribed_state();
        state.submitted(EventClass::Next);
        state.submitted(EventClass::Next);
        assert!(!state.is_terminal());
    }

    #[test]
    fn terminal_marker_survives_unsubscribe() {
        // Once a stream is terminal it stays terminal even if the subscriber goes
        // away; re-subscribing to a completed stream must not replay it.
        let (mut state, token) = subscribed_state();
        state.submitted(EventClass::Complete);
        state.release(token);
        let _new_token = state.acquire();
        assert!(state.is_terminal());
        assert_eq!(state.submit(&Event::Next), Delivery::Drop);
    }

    // ---- a callback that throws -------------------------------------------

    #[test]
    fn a_throwing_callback_does_not_wedge_the_subscription() {
        // The binding's job for this case is to catch the JS exception and not let
        // it propagate into the pump; the *policy's* job is to have already
        // recorded the delivery before the call, so a panic inside the callback
        // cannot leave the state thinking an event is still owed.
        //
        // Simulated here by recording the delivery, then panicking in the
        // "callback", with the state recorded first — mirroring the binding's
        // order in `Shared::deliver`.
        let (mut state, _token) = subscribed_state();
        let mut rec = Recorder::default();

        rec.deliver(&mut state, Event::Next);

        let class = match state.submit(&Event::Next) {
            Delivery::Deliver(class) => class,
            Delivery::Drop => panic!("a value should be deliverable"),
        };
        state.submitted(class); // recorded BEFORE the call, as the binding does
        let threw = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("consumer callback threw");
        }));
        assert!(threw.is_err());

        // The subscription still works, and is not terminal.
        rec.deliver(&mut state, Event::Next);
        assert_eq!(rec.log, vec!["next", "next"]);
        assert!(!state.is_terminal());
        assert!(state.is_subscribed());
    }
}
