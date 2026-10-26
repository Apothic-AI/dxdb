//! Single-writer election across database instances.
//!
//! Ported from `src/plugins/leader-election/index.ts` (132 lines) at upstream
//! `3b763ee17`, plus the election algorithm it delegates to,
//! `broadcast-channel@7`'s `leader-election.js` (the plugin is 132 lines of caching
//! and lifecycle around it).
//!
//! # The transport seam, stated explicitly
//!
//! Upstream's transport is a `BroadcastChannel` from the `broadcast-channel` package,
//! which itself picks between native `BroadcastChannel`, `localStorage`, IndexedDB,
//! and a Node IPC implementation. The plugin never sees that choice: it calls
//! `getBroadcastChannelReference(storage.name, token, databaseName, db)`
//! (`index.ts:43-48`) and `createLeaderElection(channel)` (`:35`).
//!
//! In Rust there is no default to inherit, so the seam is a trait:
//!
//! * [`Transport`] is one peer's ability to announce itself and hear others. It has
//!   exactly the four things the algorithm needs — its own token, `announce`,
//!   `send_tell`, and `drain_messages` — and nothing about *how* messages move.
//! * [`Channel`] is the shared bus that messages travel over. In a browser binding it
//!   is a `BroadcastChannel`; under WASI it could be a pipe; in tests it is
//!   [`InMemoryChannel`].
//!
//! A binding implements [`Transport`] once (a `web_sys` one, a node one, a
//! `futures::channel::mpsc` one) and nothing in the election changes. That is the
//! whole point of declaring it rather than hard-coding a channel: M6 must not pick a
//! transport for lanes F and G.
//!
//! # The election, exactly as upstream implements it
//!
//! From `leader-election.js:71-182`, `applyOnce`:
//!
//! 1. A peer that already believes it is leader is a no-op.
//! 2. It announces `apply` **twice**, with a wait of `response_time` (or
//!    `response_time * 4` when the call came from the fallback interval) between the
//!    announcements and after the second (`:148-172`). The second announcement is
//!    explicit in the source: "send again in case another instance was just created".
//! 3. While waiting it listens for other peers' messages:
//!    * another peer's `apply` with a **higher** token makes it abandon the round
//!      (`:112-118`), which is how simultaneous applications resolve;
//!    * another peer's `tell` (i.e. "I am already leader") makes it abandon and records
//!      `has_leader` (`:120-125`).
//! 4. If no stop criterion fired, it becomes leader (`:170-173`).
//!
//! `die()` (`:202-220`) clears leadership, marks the elector dead, and broadcasts
//! `death`, which is what lets another peer start a round (`:275-287`).
//!
//! # The failure mode the brief asks about, and what upstream actually does
//!
//! **Upstream has no liveness check.** A leader's only obligations are to answer
//! elections by having already won them and to broadcast `death` when `die()` is
//! called. If a tab is *suspended* — frozen by the browser, throttled, or paused by a
//! debugger — it stops running timers but it is **not dead**: `die()` is never
//! called, `is_leader` stays `true`, and on resume it still believes it leads while
//! another peer may have elected itself in the meantime.
//!
//! That is why the fallback interval exists at all (`:249-253`), and the longer
//! `response_time * 4` for fallback rounds is documented as making it "less likely to
//! elect duplicate leaders" (`:132-140`) — *less likely*, not impossible. The gap is
//! structural: a suspended peer cannot answer, so the others must either wait forever
//! or risk a duplicate.
//!
//! This crate reproduces upstream's behaviour exactly ([`Election`] never revokes
//! leadership on its own) **and** exposes the liveness question as a decision the
//! caller makes, in [`Election::observe_foreign_claim`] and [`resume_after_suspension`].
//! Reproducing silently would leave a port user with a duplicate-writer bug and no
//! vocabulary for it; inventing a heartbeat without saying so would be a behaviour
//! change disguised as a port. The tests cover both the upstream behaviour and the
//! recovery a caller can opt into.
//!
//! # Not ported
//!
//! * Caching electors per database and per channel (`index.ts:21-22,31-82`) and the
//!   `close()`-ordering fix (`:54-66`, upstream issue #8893). Both are lifecycle
//!   around an owned elector; [`Election`] is the elector, and a binding owns one.
//! * `waitForLeadership` as a future that resolves on election — that is
//!   [`Election::wait_for_leadership`] returning when the round succeeds; the
//!   multi-round background retry (`leader-election.js:224-300`) needs a timer.

#![forbid(unsafe_code)]

use serde_json::{json, Value};
use thiserror::Error;

/// Errors.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum Error {
    /// A round was started on an elector that has already died, which upstream treats
    /// as "not leader" (`leader-election.js:75-78`) rather than an error. Reported so
    /// a caller that keeps using a dead elector notices.
    #[error("the leader elector is dead and cannot be used again")]
    Dead,
    /// A transport delivered a message that is not a leader message.
    #[error("malformed leader message: {0}")]
    MalformedMessage(String),
}

/// Result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// The channel scope for a database's election, as upstream builds it
/// (`index.ts:43-48` passes storage name, database token and name into
/// `getBroadcastChannelReference`, which keys the channel on all of them).
///
/// Two databases with different names — or different storage engines — must not share
/// an election, so all three parts are in the key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChannelKey {
    /// The storage engine's name, e.g. `memory`.
    pub storage_name: String,
    /// The database's instance token.
    pub database_token: String,
    /// The database's name.
    pub database_name: String,
}

impl ChannelKey {
    /// Builds a key.
    pub fn new(
        storage_name: impl Into<String>,
        database_token: impl Into<String>,
        database_name: impl Into<String>,
    ) -> Self {
        ChannelKey {
            storage_name: storage_name.into(),
            database_token: database_token.into(),
            database_name: database_name.into(),
        }
    }
}

/// The two message kinds upstream sends (`leader-election.js:104-126`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// "I am applying for leadership."
    Apply {
        /// The sender's token. Higher wins a simultaneous round.
        token: String,
    },
    /// "I am the leader."
    Tell {
        /// The sender's token.
        token: String,
    },
    /// "I died", which frees the election (`:216-219`).
    Death {
        /// The sender's token.
        token: String,
    },
}

impl Message {
    /// The sender's token.
    pub fn token(&self) -> &str {
        match self {
            Message::Apply { token } | Message::Tell { token } | Message::Death { token } => token,
        }
    }

    /// Parses the wire shape, which is `{context: 'leader', action, token}`
    /// (`leader-election-util.js`).
    pub fn from_json(value: &Value) -> Result<Self> {
        let context = value.get("context").and_then(Value::as_str);
        if context != Some("leader") {
            return Err(Error::MalformedMessage(value.to_string()));
        }
        let token = value
            .get("token")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::MalformedMessage(value.to_string()))?
            .to_string();
        match value.get("action").and_then(Value::as_str) {
            Some("apply") => Ok(Message::Apply { token }),
            Some("tell") => Ok(Message::Tell { token }),
            Some("death") => Ok(Message::Death { token }),
            _ => Err(Error::MalformedMessage(value.to_string())),
        }
    }

    /// The wire shape.
    pub fn to_json(&self) -> Value {
        let action = match self {
            Message::Apply { .. } => "apply",
            Message::Tell { .. } => "tell",
            Message::Death { .. } => "death",
        };
        json!({ "context": "leader", "action": action, "token": self.token() })
    }
}

/// One peer's view of the bus. The transport seam.
///
/// Four methods, because that is all `applyOnce` uses. A binding decides whether the
/// bus is a `BroadcastChannel`, a pipe, or a channel pair; nothing here assumes one.
pub trait Transport {
    /// This peer's token. Upstream generates a random token per database and uses
    /// lexicographic order to break ties (`leader-election.js:112-118`).
    fn token(&self) -> &str;

    /// Announces `apply` to every other peer.
    fn announce(&mut self, message: Message) -> Result<()>;

    /// Collects messages other peers sent since the last call.
    ///
    /// Returns them in arrival order. The election drains this at the *start* of a
    /// round and discards the result, because upstream's listener only exists during a
    /// round: a buffering transport would otherwise replay messages the algorithm was
    /// never written to see.
    fn drain_messages(&mut self) -> Result<Vec<Message>>;

    /// Collects messages, waiting up to `wait_ms` for them first.
    ///
    /// This is where `Timing::wait_for_answer` reaches the transport, and it is why
    /// `from_fallback` is not cosmetic: a background round genuinely waits four times
    /// as long (`leader-election.js:148`), which is the mechanism the source credits
    /// with making duplicate leaders "less likely" (`:132-140`).
    ///
    /// The default is a non-blocking drain so a synchronous transport (tests, a
    /// single-threaded binding) does not have to sleep.
    fn drain_messages_within(&mut self, _wait_ms: u64) -> Result<Vec<Message>> {
        self.drain_messages()
    }
}

/// The two timing knobs of a round (`leader-election.js:297-303`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// How long to wait for other peers during a critical round.
    pub response_time_ms: u64,
    /// How long to wait when the round came from the background fallback.
    pub fallback_interval_ms: u64,
}

impl Default for Timing {
    fn default() -> Self {
        // `fallbackInterval` defaults to 3000 (`:297-299`); `responseTime` is derived
        // from the channel, so 100 is the browser value used here as the default.
        Timing {
            response_time_ms: 100,
            fallback_interval_ms: 3_000,
        }
    }
}

impl Timing {
    /// `isFromFallbackInterval ? responseTime * 4 : responseTime`
    /// (`leader-election.js:148`).
    pub fn wait_for_answer(&self, from_fallback: bool) -> u64 {
        if from_fallback {
            self.response_time_ms * 4
        } else {
            self.response_time_ms
        }
    }
}

/// Why a round ended without winning, if it did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopCriterion {
    /// Another peer applied with a higher token (`leader-election.js:112-118`).
    HigherTokenApplied(String),
    /// Another peer is already leader (`:120-125`).
    OtherIsLeader(String),
}

/// One leader elector: the state a single database instance holds.
#[derive(Debug)]
pub struct Election {
    token: String,
    is_leader: bool,
    has_leader: bool,
    is_dead: bool,
    timing: Timing,
    /// Whether this peer currently believes it leads while a competing claim exists.
    contested: bool,
    rounds: u64,
}

impl Election {
    /// A new elector for `token`.
    pub fn new(token: impl Into<String>) -> Self {
        Election {
            token: token.into(),
            is_leader: false,
            has_leader: false,
            is_dead: false,
            timing: Timing::default(),
            contested: false,
            rounds: 0,
        }
    }

    /// Sets the round timings.
    pub fn with_timing(mut self, timing: Timing) -> Self {
        self.timing = timing;
        self
    }

    /// The token.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// `isLeader` (`index.ts:84-89`).
    pub fn is_leader(&self) -> bool {
        self.is_leader
    }

    /// `hasLeader` (`leader-election.js:63-65`).
    pub fn has_leader(&self) -> bool {
        self.has_leader
    }

    /// How many rounds this peer has run.
    pub fn rounds(&self) -> u64 {
        self.rounds
    }

    /// True when this peer believes it leads **and** a competing claim has been
    /// observed. This is the duplicate-writer state, and it is not reachable by
    /// upstream's own logic — only by resuming from suspension. See
    /// [`Election::resume_after_suspension`].
    pub fn is_contested(&self) -> bool {
        self.contested
    }

    /// One application round (`applyOnce`).
    ///
    /// `from_fallback` selects the longer wait, exactly as upstream does for a round
    /// driven by the fallback interval.
    ///
    /// The return value is whether this peer is leader after the round, matching
    /// `applyOnce`'s resolved value.
    pub fn apply_once(
        &mut self,
        transport: &mut dyn Transport,
        from_fallback: bool,
    ) -> Result<bool> {
        // `if (this.isLeader) return sleep(0, true);`
        if self.is_leader {
            return Ok(true);
        }
        // `if (this.isDead) return sleep(0, false);`
        if self.is_dead {
            return Ok(false);
        }
        self.rounds += 1;

        // Discard anything that arrived while no round was running.
        //
        // Upstream attaches its message listener *inside* the round and removes it
        // afterwards (`leader-election.js:145`, `:168`), so a message sent between
        // rounds is never seen at all. A transport that buffers instead — including
        // this crate's `InMemoryChannel` — would otherwise let a stale `apply` from a
        // peer that has since died make the next round stand down for nobody. That is
        // not hypothetical: it is what happens to both survivors in
        // `upstream_127_when_the_leader_dies_a_new_one_is_elected` without this line.
        //
        // The distinction matters for a binding: `Transport` is level-triggered (an
        // inbox) but the algorithm is edge-triggered (a listener), so clearing at the
        // round boundary is the caller's job, not the transport's.
        let _ = transport.drain_messages()?;

        // `sendLeaderMessage(this, 'apply')`
        transport.announce(Message::Apply {
            token: self.token.clone(),
        })?;

        // First wait, then a second announcement "in case another instance was just
        // created" (`:158-162`), then a second wait.
        let wait = self.timing.wait_for_answer(from_fallback);
        let stop = self.listen_during_wait(transport, wait)?;
        if let Some(stop) = stop {
            self.record_stop(stop);
            return Ok(false);
        }
        transport.announce(Message::Apply {
            token: self.token.clone(),
        })?;
        let stop = self.listen_during_wait(transport, wait)?;
        if let Some(stop) = stop {
            self.record_stop(stop);
            return Ok(false);
        }

        // `if (!stopCriteria && !this.isDead) return beLeader(this).then(() => true);`
        if self.is_dead {
            return Ok(false);
        }
        self.is_leader = true;
        self.contested = false;
        Ok(true)
    }

    /// Drains messages until a stop criterion fires or the queue is empty.
    ///
    /// Upstream learns these through an event listener over a wall-clock wait; the
    /// seam delivers them as a queue, so this drains it. A real binding's `Transport`
    /// blocks (or awaits) for `timing.wait_for_answer` before returning.
    fn listen_during_wait(
        &mut self,
        transport: &mut dyn Transport,
        wait_ms: u64,
    ) -> Result<Option<StopCriterion>> {
        for message in transport.drain_messages_within(wait_ms)? {
            match message {
                // `if (msg.token > this.token)` — lexicographic, not numeric.
                Message::Apply { token }
                    if token != self.token && token.as_str() > self.token.as_str() =>
                {
                    return Ok(Some(StopCriterion::HigherTokenApplied(token)));
                }
                Message::Tell { token } if token != self.token => {
                    return Ok(Some(StopCriterion::OtherIsLeader(token)));
                }
                // A death frees the election; it is not a stop criterion.
                _ => {}
            }
        }
        Ok(None)
    }

    fn record_stop(&mut self, stop: StopCriterion) {
        match stop {
            StopCriterion::OtherIsLeader(_) => self.has_leader = true,
            StopCriterion::HigherTokenApplied(_) => {}
        }
    }

    /// `die()` (`leader-election.js:202-220`): give up leadership and announce it.
    pub fn die(&mut self, transport: &mut dyn Transport) -> Result<()> {
        if self.is_dead {
            return Ok(());
        }
        if self.is_leader {
            self.has_leader = false;
            self.is_leader = false;
        }
        self.is_dead = true;
        self.contested = false;
        transport.announce(Message::Death {
            token: self.token.clone(),
        })
    }

    /// `awaitLeadership` for the single-round case
    /// (`leader-election.js:188-197`): run a round and report whether this peer leads.
    ///
    /// The background retry loop upstream runs after a failed round (`:249-287`) needs
    /// a timer; a caller that wants it calls [`Election::apply_once`] again on the
    /// fallback interval, which is what upstream's loop does.
    pub fn wait_for_leadership(&mut self, transport: &mut dyn Transport) -> Result<bool> {
        self.apply_once(transport, false)
    }

    /// Records that another peer claimed leadership while this one believes it leads.
    ///
    /// This is the state upstream cannot represent and cannot detect: it only listens
    /// for messages *during* a round, so a `tell` that arrives after a peer has won —
    /// which is exactly what a suspended leader resumes to — is dropped.
    ///
    /// Returns `true` when the claim is a genuine conflict: this peer is leader, the
    /// claim carries a different token, and it is either a `tell` or a higher-token
    /// `apply`.
    pub fn observe_foreign_claim(&mut self, message: &Message) -> bool {
        if !self.is_leader || self.is_dead || message.token() == self.token {
            return false;
        }
        match message {
            Message::Death { .. } => false,
            Message::Tell { .. } | Message::Apply { .. } => {
                self.contested = true;
                true
            }
        }
        // (the match above is exhaustive over `Message`; clippy's
        // collapsible_match lint wants a guard instead, which reads worse here)
    }

    /// What a peer should do when it resumes from suspension.
    ///
    /// Upstream has no such step: the peer simply carries on believing it leads. This
    /// helper makes the choice explicit rather than silent, and returns the decision
    /// so a caller can log or metric it.
    pub fn resume_after_suspension(&mut self, resumed_claims: &[Message]) -> ResumeOutcome {
        if self.is_dead {
            return ResumeOutcome::Dead;
        }
        let foreign: Vec<&Message> = resumed_claims
            .iter()
            .filter(|message| message.token() != self.token)
            .collect();
        if foreign.is_empty() {
            return if self.is_leader {
                ResumeOutcome::StillLeader
            } else {
                ResumeOutcome::NotLeader
            };
        }
        // A death among the claims frees the election, so a leader keeps leading.
        let only_deaths = foreign
            .iter()
            .all(|message| matches!(message, Message::Death { .. }));
        if only_deaths {
            return if self.is_leader {
                ResumeOutcome::StillLeader
            } else {
                ResumeOutcome::NotLeader
            };
        }
        if self.is_leader {
            self.contested = true;
            // The honest resolution, and the only one that avoids two writers: a
            // resumed peer yields and re-applies instead of trusting its stale belief.
            self.is_leader = false;
            ResumeOutcome::YieldedAndMustReapply
        } else {
            ResumeOutcome::NotLeader
        }
    }
}

/// The answer from [`Election::resume_after_suspension`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeOutcome {
    /// No competing claim was seen; this peer still leads.
    StillLeader,
    /// No competing claim, and this peer was not leading.
    NotLeader,
    /// A competing claim was seen. This peer gave up leadership and must run a round
    /// before writing again.
    YieldedAndMustReapply,
    /// The elector had already died.
    Dead,
}

/// A message bus that delivers to everyone except the sender.
///
/// The counterpart to [`Transport`] for tests and for any binding that has no native
/// channel. Peers are independent, cloneable handles onto one shared bus, so several
/// can be alive at once — which is the point: a real election has every tab listening
/// simultaneously, and a harness whose transports borrowed the channel exclusively
/// would force each peer to announce before the next one existed, quietly skipping the
/// case that matters.
#[derive(Debug, Clone, Default)]
pub struct InMemoryChannel {
    inner: std::rc::Rc<std::cell::RefCell<ChannelState>>,
}

#[derive(Debug, Default)]
struct ChannelState {
    peers: Vec<PeerBuffer>,
}

#[derive(Debug, Default)]
struct PeerBuffer {
    token: String,
    inbox: Vec<Message>,
}

impl InMemoryChannel {
    /// A channel with no peers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a peer and returns a handle for it.
    pub fn join(&self, token: impl Into<String>) -> InMemoryTransport {
        let token = token.into();
        self.inner.borrow_mut().peers.push(PeerBuffer {
            token: token.clone(),
            inbox: Vec::new(),
        });
        InMemoryTransport {
            channel: self.clone(),
            token,
        }
    }

    /// Number of registered peers.
    pub fn peer_count(&self) -> usize {
        self.inner.borrow().peers.len()
    }

    /// Posts a message as `from`, delivering it to every other peer.
    ///
    /// Models a peer acting without running a round itself, e.g. a leader answering an
    /// election it is not part of.
    pub fn send(&self, from: &str, message: Message) {
        let mut state = self.inner.borrow_mut();
        for peer in &mut state.peers {
            if peer.token != from {
                peer.inbox.push(message.clone());
            }
        }
    }

    /// Collects a peer's inbox without going through a transport.
    pub fn take_messages(&self, token: &str) -> Vec<Message> {
        let mut state = self.inner.borrow_mut();
        for peer in &mut state.peers {
            if peer.token == token {
                return std::mem::take(&mut peer.inbox);
            }
        }
        Vec::new()
    }
}

/// One peer on an [`InMemoryChannel`].
#[derive(Debug, Clone)]
pub struct InMemoryTransport {
    channel: InMemoryChannel,
    token: String,
}

impl InMemoryTransport {
    /// The channel this peer is attached to, so a test can model another peer acting
    /// while a round is in progress.
    pub fn channel_handle(&self) -> InMemoryChannel {
        self.channel.clone()
    }
}

impl Transport for InMemoryTransport {
    fn token(&self) -> &str {
        &self.token
    }

    fn announce(&mut self, message: Message) -> Result<()> {
        self.channel.send(&self.token, message);
        Ok(())
    }

    fn drain_messages(&mut self) -> Result<Vec<Message>> {
        Ok(self.channel.take_messages(&self.token))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_shape_is_the_documented_one() {
        // `{context: 'leader', action, token}` (`leader-election-util.js`).
        let message = Message::Apply {
            token: "abc".to_string(),
        };
        assert_eq!(
            message.to_json(),
            json!({ "context": "leader", "action": "apply", "token": "abc" })
        );
        assert_eq!(
            Message::from_json(&message.to_json()).expect("round trip"),
            message
        );
    }

    #[test]
    fn a_non_leader_message_is_rejected() {
        assert!(matches!(
            Message::from_json(&json!({ "context": "other", "action": "apply", "token": "a" })),
            Err(Error::MalformedMessage(_))
        ));
        assert!(matches!(
            Message::from_json(&json!({ "context": "leader", "action": "nope", "token": "a" })),
            Err(Error::MalformedMessage(_))
        ));
    }

    #[test]
    fn the_fallback_wait_is_four_times_longer() {
        // `:148`.
        let timing = Timing {
            response_time_ms: 100,
            fallback_interval_ms: 3_000,
        };
        assert_eq!(timing.wait_for_answer(false), 100);
        assert_eq!(timing.wait_for_answer(true), 400);
    }
}
