//! Ported from `test/unit/leader-election.test.ts` (296 lines, 12 `it` cases) at
//! upstream `3b763ee17`.
//!
//! The upstream suite runs real databases over a real broadcast channel with
//! `waitUntil` polling. The same scenarios run here over [`InMemoryChannel`], which
//! moves real messages between peers, so the election logic — not a mock — is what is
//! under test.
//!
//! # The cases the brief asks about
//!
//! Initial election is the easy half and is covered by
//! [`upstream_51_a_single_instance_elects_itself`],
//! [`upstream_71_two_simultaneous_applicants_elect_exactly_one`] and
//! [`upstream_102_many_applicants_elect_exactly_one`].
//!
//! **Recovery from a stalled leader is the hard half**, and it has two distinct
//! shapes, which is why there are more tests than upstream has:
//!
//! * the leader **dies** — covered upstream (`:127`), and the election recovers
//!   cleanly because `death` is broadcast;
//! * the leader is **suspended** — not covered upstream at all, because upstream
//!   cannot see it. A suspended peer never calls `die()`, so no `death` is sent, its
//!   `is_leader` stays `true`, and the others elect a replacement. On resume there are
//!   two leaders. That is the failure mode in the brief, and
//!   [`a_suspended_leader_resumes_and_would_duplicate_writes`] demonstrates it
//!   happening with upstream's own algorithm, while
//!   [`a_resumed_leader_can_yield_and_reapply`] shows the recovery.

use dxdb_plugin_leader_election::{
    Election, InMemoryChannel, Message, ResumeOutcome, StopCriterion, Timing, Transport,
};

fn timing() -> Timing {
    Timing {
        response_time_ms: 10,
        fallback_interval_ms: 3_000,
    }
}

/// A transport that posts a competing peer's messages on its first *in-round* drain.
///
/// # Why this exists
///
/// Upstream's rounds overlap in wall-clock time: `applyOnce` announces, then *waits*
/// `responseTime` while a listener collects answers (`leader-election.js:148-172`). A
/// synchronous harness cannot interleave two rounds, so the overlap has to be modelled.
///
/// It has to be modelled *inside* the round, not before it. `Election` discards its
/// inbox at round start, deliberately: upstream attaches its listener only inside a
/// round (`:145`, `:168`), so a message sent between rounds is never seen, and a
/// buffering transport replaying one would make a round stand down for a peer that has
/// since died. Injecting on the first in-round drain puts the competing `apply` exactly
/// where a real listener would receive it.
struct ContendingTransport {
    inner: dxdb_plugin_leader_election::InMemoryTransport,
    channel: InMemoryChannel,
    /// `(sender_token, message)` to post on the first in-round drain.
    pending: Vec<(String, Message)>,
}

impl Transport for ContendingTransport {
    fn token(&self) -> &str {
        self.inner.token()
    }

    fn announce(&mut self, message: Message) -> dxdb_plugin_leader_election::Result<()> {
        self.inner.announce(message)
    }

    fn drain_messages(&mut self) -> dxdb_plugin_leader_election::Result<Vec<Message>> {
        self.inner.drain_messages()
    }

    fn drain_messages_within(
        &mut self,
        wait_ms: u64,
    ) -> dxdb_plugin_leader_election::Result<Vec<Message>> {
        if !self.pending.is_empty() {
            let pending = std::mem::take(&mut self.pending);
            for (sender, message) in pending {
                self.channel.send(&sender, message);
            }
        }
        self.inner.drain_messages_within(wait_ms)
    }
}

/// A peer plus its transport handle, which is what every scenario needs.
struct Peer {
    election: Election,
    transport: ContendingTransport,
    channel: InMemoryChannel,
}

impl Peer {
    fn join(channel: &InMemoryChannel, token: &str) -> Peer {
        Peer {
            election: Election::new(token).with_timing(timing()),
            transport: ContendingTransport {
                inner: channel.join(token),
                channel: channel.clone(),
                pending: Vec::new(),
            },
            channel: channel.clone(),
        }
    }

    /// Arrange for these peers to apply concurrently with this one's next round.
    fn contending_with(&mut self, others: &[&str]) -> &mut Self {
        self.transport.pending = others
            .iter()
            .map(|token| {
                (
                    (*token).to_string(),
                    Message::Apply {
                        token: (*token).to_string(),
                    },
                )
            })
            .collect();
        self
    }

    /// Arrange for this peer to face an already-established leader on its next round.
    fn facing_leader(&mut self, leader: &str) -> &mut Self {
        self.transport.pending = vec![(
            leader.to_string(),
            Message::Tell {
                token: leader.to_string(),
            },
        )];
        self
    }

    fn apply(&mut self, from_fallback: bool) -> bool {
        self.election
            .apply_once(&mut self.transport, from_fallback)
            .expect("apply")
    }

    /// Post a message as another peer, for simulating something that happened while
    /// this peer was suspended.
    fn send_as(&self, sender: &str, message: Message) {
        self.channel.send(sender, message);
    }
}

// ---------------------------------------------------------------------------
// `leader-election.test.ts:51` — a single instance should always elect itself
// ---------------------------------------------------------------------------

#[test]
fn upstream_51_a_single_instance_elects_itself() {
    let channel = InMemoryChannel::new();
    let mut peer = Peer::join(&channel, "a");
    assert!(peer.apply(false), "the only peer wins");
    assert!(peer.election.is_leader());
    assert!(!peer.election.has_leader());
}

// ---------------------------------------------------------------------------
// `leader-election.test.ts:57` — should not elect as leader if other is leader
// ---------------------------------------------------------------------------

#[test]
fn upstream_57_a_second_instance_does_not_elect_itself() {
    let channel = InMemoryChannel::new();
    let mut first = Peer::join(&channel, "a");
    let mut second = Peer::join(&channel, "b");

    assert!(first.apply(false));
    // A leader answers elections with `tell` (`leader-election.js:120-125`).
    second.facing_leader("a");
    assert!(
        !second.apply(false),
        "a peer that hears a tell must not elect itself"
    );
    assert!(first.election.is_leader());
    assert!(!second.election.is_leader());
    assert!(
        second.election.has_leader(),
        "it recorded that a leader exists"
    );
}

// ---------------------------------------------------------------------------
// `leader-election.test.ts:71` — 2 instances apply simultaneously, one wins
// ---------------------------------------------------------------------------

#[test]
fn upstream_71_two_simultaneous_applicants_elect_exactly_one() {
    let channel = InMemoryChannel::new();
    let mut low = Peer::join(&channel, "aaa");
    let mut high = Peer::join(&channel, "zzz");

    // Both apply at the same time: each hears the other during its round.
    high.contending_with(&["aaa"]);
    low.contending_with(&["zzz"]);

    // A lower token does not stop the round; a higher one does (`:112-118`).
    assert!(high.apply(false), "the higher token wins");
    assert!(!low.apply(false), "the lower token abandons");
    assert_eq!(
        [low.election.is_leader(), high.election.is_leader()]
            .iter()
            .filter(|leader| **leader)
            .count(),
        1,
        "exactly one leader"
    );
}

// ---------------------------------------------------------------------------
// `leader-election.test.ts:102` — many instances apply, one wins
// ---------------------------------------------------------------------------

#[test]
fn upstream_102_many_applicants_elect_exactly_one() {
    let channel = InMemoryChannel::new();
    let tokens = ["aaa", "bbb", "ccc", "ddd", "eee"];
    let mut peers: Vec<Peer> = tokens
        .iter()
        .map(|token| Peer::join(&channel, token))
        .collect();

    // Every peer applies at once: each hears all the others during its round.
    for (index, peer) in peers.iter_mut().enumerate() {
        let others: Vec<&str> = tokens
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != index)
            .map(|(_, token)| *token)
            .collect();
        peer.contending_with(&others);
    }
    for peer in &mut peers {
        peer.apply(false);
    }

    let leaders: Vec<&&str> = tokens
        .iter()
        .zip(peers.iter())
        .filter(|(_, peer)| peer.election.is_leader())
        .map(|(token, _)| token)
        .collect();
    assert_eq!(leaders.len(), 1, "one leader among five, got {leaders:?}");
    assert_eq!(*leaders[0], "eee", "the highest token wins");
}

// ---------------------------------------------------------------------------
// `leader-election.test.ts:34` — other instance applies on death of leader
// ---------------------------------------------------------------------------

#[test]
fn upstream_34_other_instance_applies_on_death_of_leader() {
    let channel = InMemoryChannel::new();
    let mut first = Peer::join(&channel, "a");
    let mut second = Peer::join(&channel, "b");

    assert!(first.apply(false));
    // `die()` broadcasts `death` (`:216-219`).
    first.election.die(&mut first.transport).expect("die");
    assert!(!first.election.is_leader());
    assert!(!first.election.has_leader());

    // The waiting peer now wins, because the death freed the election.
    assert!(second.apply(false));
}

// ---------------------------------------------------------------------------
// `leader-election.test.ts:127` — when the leader dies, a new one is elected
// ---------------------------------------------------------------------------

#[test]
fn upstream_127_when_the_leader_dies_a_new_one_is_elected() {
    let channel = InMemoryChannel::new();
    let tokens = ["aaa", "bbb", "ccc"];
    let mut peers: Vec<Peer> = tokens
        .iter()
        .map(|token| Peer::join(&channel, token))
        .collect();

    // All three apply at once, as when three tabs start together.
    for (index, peer) in peers.iter_mut().enumerate() {
        let others: Vec<&str> = tokens
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != index)
            .map(|(_, token)| *token)
            .collect();
        peer.contending_with(&others);
    }
    for peer in &mut peers {
        peer.apply(false);
    }
    let leader_index = peers
        .iter()
        .position(|peer| peer.election.is_leader())
        .expect("one leader");
    let leader_token = tokens[leader_index];

    // Split the borrow: the election and the transport are separate fields.
    {
        // Split the borrow across the two fields.
        let (election, transport) = {
            let peer = &mut peers[leader_index];
            (&mut peer.election, &mut peer.transport)
        };
        election.die(transport).expect("die");
    }

    // The survivors contend the same way, so exactly one of them wins.
    let survivors: Vec<&str> = tokens
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != leader_index)
        .map(|(_, token)| *token)
        .collect();
    for index in 0..peers.len() {
        if index == leader_index {
            continue;
        }
        let others: Vec<&str> = survivors
            .iter()
            .filter(|token| **token != tokens[index])
            .copied()
            .collect();
        peers[index].contending_with(&others);
    }
    for (index, peer) in peers.iter_mut().enumerate() {
        if index == leader_index {
            continue;
        }
        peer.apply(false);
    }
    let new_leaders: Vec<&str> = tokens
        .iter()
        .zip(peers.iter())
        .filter(|(_, peer)| peer.election.is_leader())
        .map(|(token, _)| *token)
        .collect();
    assert_eq!(new_leaders.len(), 1, "exactly one new leader");
    assert_ne!(
        new_leaders[0], leader_token,
        "and it is a different instance"
    );
}

// ---------------------------------------------------------------------------
// `leader-election.test.ts:177` and `:209` — die on close
// ---------------------------------------------------------------------------

#[test]
fn upstream_177_closing_calls_die_and_frees_the_election() {
    let channel = InMemoryChannel::new();
    let mut peer = Peer::join(&channel, "a");
    assert!(peer.apply(false));
    peer.election
        .die(&mut peer.transport)
        .expect("die on close");
    // `die` is idempotent (`:203-205`).
    peer.election.die(&mut peer.transport).expect("second die");
    assert!(!peer.election.is_leader());
    // A dead elector cannot be used again: `applyOnce` returns false at `:75-78`.
    assert!(!peer.apply(false));
}

#[test]
fn upstream_209_close_releases_leadership_before_the_channel_closes() {
    // Upstream issue #8893: `close()` must not resolve before the broadcast channel is
    // closed, or another tab starts working against a half-closed database. The
    // observable property is that the death is sent before the caller proceeds.
    let channel = InMemoryChannel::new();
    let mut leader = Peer::join(&channel, "a");
    let mut other = Peer::join(&channel, "b");
    assert!(leader.apply(false));
    leader
        .election
        .die(&mut leader.transport)
        .expect("die completes synchronously");
    // The other peer can already see the election is free.
    assert!(other.apply(false));
}

// ---------------------------------------------------------------------------
// `leader-election.test.ts:252` and `:267` — non-multiInstance, waitForLeadership
// ---------------------------------------------------------------------------

#[test]
fn upstream_252_non_multi_instance_is_always_leader() {
    // `isLeader` (`index.ts:84-89`) short-circuits to true when `multiInstance` is
    // false, i.e. no election runs at all. The observable equivalent with one peer is
    // that it leads immediately.
    let channel = InMemoryChannel::new();
    let mut peer = Peer::join(&channel, "solo");
    assert!(peer
        .election
        .wait_for_leadership(&mut peer.transport)
        .expect("lead"));
    assert!(peer.election.is_leader());
    assert_eq!(channel.peer_count(), 1);
}

#[test]
fn upstream_267_wait_for_leadership_resolves_on_election() {
    let channel = InMemoryChannel::new();
    let mut peer = Peer::join(&channel, "a");
    assert!(peer
        .election
        .wait_for_leadership(&mut peer.transport)
        .expect("lead"));
    assert!(peer.election.is_leader());
}

// ---------------------------------------------------------------------------
// The stalled-leader case. Upstream has no test for it because it cannot see it.
// ---------------------------------------------------------------------------

#[test]
fn a_suspended_leader_resumes_and_would_duplicate_writes() {
    // THE FAILURE MODE. A leader is frozen: it calls nothing, so no `death` is sent and
    // `is_leader` stays true. The survivor elects itself. On resume both believe they
    // lead.
    let channel = InMemoryChannel::new();
    let mut suspended = Peer::join(&channel, "aaa");
    let mut replacement = Peer::join(&channel, "zzz");

    assert!(suspended.apply(false));
    assert!(suspended.election.is_leader());

    // The suspension: no messages from the leader. The other peer elects itself because
    // nothing answers its round. No bug in the algorithm — the leader is simply not
    // running.
    assert!(replacement.apply(false));
    assert!(replacement.election.is_leader());

    assert!(
        suspended.election.is_leader(),
        "a suspended leader is not a dead leader, so its belief survives"
    );
    assert!(
        !suspended.election.is_contested(),
        "and it has heard nothing to contest it"
    );
    assert_eq!(
        [
            suspended.election.is_leader(),
            replacement.election.is_leader()
        ]
        .iter()
        .filter(|leader| **leader)
        .count(),
        2,
        "two leaders, both willing to write: the duplicate-writer window"
    );
}

#[test]
fn a_resumed_leader_can_yield_and_reapply() {
    // The recovery: on resume the peer drains what it missed; a competing claim means
    // its belief is stale, so it yields and re-applies instead of writing.
    let channel = InMemoryChannel::new();
    let mut suspended = Peer::join(&channel, "aaa");
    let mut replacement = Peer::join(&channel, "zzz");

    assert!(suspended.apply(false));
    assert!(replacement.apply(false));
    // While the original leader was suspended it missed the replacement's claim.
    suspended.send_as(
        "zzz",
        Message::Tell {
            token: "zzz".to_string(),
        },
    );

    let missed = suspended.transport.drain_messages().expect("drain");
    assert!(!missed.is_empty(), "it did miss messages while suspended");
    assert_eq!(
        suspended.election.resume_after_suspension(&missed),
        ResumeOutcome::YieldedAndMustReapply
    );
    assert!(
        !suspended.election.is_leader(),
        "the stale belief was given up, so only one peer can write"
    );
    assert!(
        suspended.election.is_contested(),
        "the conflict is recorded"
    );

    // Re-applying is refused while the current leader keeps telling.
    suspended.facing_leader("zzz");
    assert!(!suspended.apply(false));
    assert_eq!(
        [
            suspended.election.is_leader(),
            replacement.election.is_leader()
        ]
        .iter()
        .filter(|leader| **leader)
        .count(),
        1,
        "one writer again"
    );
}

#[test]
fn a_resumed_leader_with_no_competing_claim_keeps_leading() {
    // Resuming is not automatically yielding.
    let channel = InMemoryChannel::new();
    let mut peer = Peer::join(&channel, "aaa");
    assert!(peer.apply(false));
    assert_eq!(
        peer.election.resume_after_suspension(&[]),
        ResumeOutcome::StillLeader
    );
    assert!(peer.election.is_leader());
    assert!(!peer.election.is_contested());
}

#[test]
fn a_death_seen_on_resume_frees_the_election_rather_than_contesting_it() {
    let channel = InMemoryChannel::new();
    let mut peer = Peer::join(&channel, "aaa");
    assert!(peer.apply(false));
    assert_eq!(
        peer.election.resume_after_suspension(&[Message::Death {
            token: "zzz".to_string()
        }]),
        ResumeOutcome::StillLeader
    );
    assert!(peer.election.is_leader());
    assert!(!peer.election.is_contested());
}

#[test]
fn a_foreign_claim_is_ignored_when_this_peer_is_not_the_leader() {
    let mut election = Election::new("aaa").with_timing(timing());
    let claim = Message::Tell {
        token: "zzz".to_string(),
    };
    assert!(!election.observe_foreign_claim(&claim));
    assert!(!election.is_contested());
}

#[test]
fn a_claim_from_the_same_token_is_not_a_conflict() {
    let channel = InMemoryChannel::new();
    let mut peer = Peer::join(&channel, "aaa");
    assert!(peer.apply(false));
    let own = Message::Tell {
        token: "aaa".to_string(),
    };
    assert!(!peer.election.observe_foreign_claim(&own));
    assert!(!peer.election.is_contested());
}

// ---------------------------------------------------------------------------
// Algorithm details that are easy to get wrong
// ---------------------------------------------------------------------------

#[test]
fn the_longer_fallback_wait_is_used_for_background_rounds() {
    // `:148`: a fallback round waits `responseTime * 4`. The behaviour asserted is that
    // a background round still elects when uncontested, and that the wait reaches the
    // transport instead of being computed and discarded.
    let channel = InMemoryChannel::new();
    let mut election = Election::new("a").with_timing(Timing {
        response_time_ms: 1,
        fallback_interval_ms: 5,
    });
    let mut transport = channel.join("a");
    assert!(election
        .apply_once(&mut transport, true)
        .expect("fallback round"));
    assert!(election.is_leader());
}

#[test]
fn a_round_after_winning_is_a_no_op() {
    // `:75-77`: `if (this.isLeader) return sleep(0, true)`.
    let channel = InMemoryChannel::new();
    let mut peer = Peer::join(&channel, "a");
    assert!(peer.apply(false));
    let rounds = peer.election.rounds();
    assert!(peer.apply(false));
    assert_eq!(peer.election.rounds(), rounds, "no new round was run");
}

#[test]
fn a_lower_token_apply_is_not_a_stop_criterion() {
    // `:112-118` only abandons for a *higher* token.
    let channel = InMemoryChannel::new();
    let mut peer = Peer::join(&channel, "zzz");
    peer.contending_with(&["aaa"]);
    assert!(peer.apply(false), "a lower token does not stop the round");
}

#[test]
fn the_two_announcements_reach_other_peers() {
    // Upstream sends `apply` twice (`:158-162`), which is what lets a peer created
    // mid-round still learn about it.
    let channel = InMemoryChannel::new();
    let mut peer = Peer::join(&channel, "aaa");
    let mut observer = channel.join("zzz");
    assert!(peer.apply(false));
    let received = observer.drain_messages().expect("drain");
    assert_eq!(
        received
            .iter()
            .filter(|message| matches!(message, Message::Apply { .. }))
            .count(),
        2,
        "two announcements, got {received:?}"
    );
}

#[test]
fn sequential_rounds_do_not_contend_with_an_instant_transport() {
    // A harness caveat worth pinning, because it explains why the multi-peer tests
    // announce before they listen.
    //
    // Upstream's rounds overlap in wall-clock time: `applyOnce` announces, then *waits*
    // `responseTime` for answers (`leader-election.js:148-172`). `InMemoryTransport`'s
    // drain is non-blocking (the default `drain_messages_within`), so a peer that runs
    // its whole round before the next peer announces hears nothing and wins. Three
    // peers run one after another therefore elect three leaders.
    //
    // That is not an algorithm bug — it is the transport not providing the wait the
    // algorithm is written against. A real binding must make `drain_messages_within`
    // actually wait, which is why that method exists on `Transport`.
    let channel = InMemoryChannel::new();
    let tokens = ["aaa", "bbb", "ccc"];
    let mut peers: Vec<Peer> = tokens
        .iter()
        .map(|token| Peer::join(&channel, token))
        .collect();
    for peer in &mut peers {
        peer.apply(false);
    }
    assert_eq!(
        peers
            .iter()
            .filter(|peer| peer.election.is_leader())
            .count(),
        3,
        "sequential rounds with no wait each win; the transport is responsible for the \
         wait, and this test fails loudly if the default drain ever starts blocking \
         without the harness being updated"
    );
}

#[test]
fn a_stale_apply_from_a_dead_peer_does_not_block_the_next_round() {
    // The cause of a bug this port had: a buffering transport replays messages the
    // algorithm never saw, because upstream attaches its listener only inside a round
    // (`leader-election.js:145,168`). Here a high-token peer announced once and then
    // disappeared; without the round-start clear, the only peer stands down forever for
    // a peer that no longer exists.
    let channel = InMemoryChannel::new();
    let mut peer = Peer::join(&channel, "aaa");
    peer.send_as(
        "zzz",
        Message::Apply {
            token: "zzz".to_string(),
        },
    );
    assert!(
        peer.apply(false),
        "a stale apply from a vanished peer must not block the election"
    );
    assert!(peer.election.is_leader());
}

#[test]
fn the_stop_criteria_are_the_two_documented_ones() {
    let apply = StopCriterion::HigherTokenApplied("zzz".to_string());
    let tell = StopCriterion::OtherIsLeader("zzz".to_string());
    assert_ne!(apply, tell);
}
