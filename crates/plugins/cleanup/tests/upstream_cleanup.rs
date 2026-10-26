//! Ported from `test/unit/cleanup.test.ts` (470 lines, 9 `it` cases) at upstream
//! `3b763ee17`.
//!
//! Upstream's suite drives a live database; the cases below exercise the same rules
//! against [`run_cleanup`] and [`should_purge`], which is where the rules live.
//!
//! The case the brief cares about — two collections sharing a storage with different
//! retention expectations — has no upstream counterpart, because upstream has no guard
//! for it. It appears twice here:
//!
//! * [`two_collections_over_one_storage_disagree_about_retention`] shows what happens
//!   with upstream's per-collection behaviour: the shorter window purges tombstones the
//!   longer one still needs.
//! * [`a_cleanup_group_holds_the_longest_retention`] shows [`CleanupGroup`] preventing
//!   it by taking the maximum retention across members.

use dxdb_plugin_cleanup::{
    run_cleanup, should_purge, CleanupGroup, CleanupPolicy, CleanupTarget, Error, Result,
};
use serde_json::{json, Value};

const DAY_MS: u64 = 1_000 * 60 * 60 * 24;

fn now() -> f64 {
    // A fixed clock so the ages in the tests are exact.
    10_000_000_000_000.0
}

/// A document deleted `age_ms` ago.
fn tombstone(id: &str, age_ms: u64) -> Value {
    json!({
        "id": id,
        "_deleted": true,
        "_meta": { "lwt": now() - age_ms as f64 }
    })
}

fn live(id: &str) -> Value {
    json!({ "id": id, "_deleted": false, "_meta": { "lwt": now() } })
}

/// An in-memory storage. `batch` models a storage that purges a limited number per
/// call, which is the `cleanup()` returning `false` case (`cleanup.test.ts:281`).
struct Target {
    documents: Vec<Value>,
    batch: usize,
    purge_calls: usize,
}

impl Target {
    fn new(documents: Vec<Value>) -> Self {
        Target {
            documents,
            batch: usize::MAX,
            purge_calls: 0,
        }
    }

    fn batching(documents: Vec<Value>, batch: usize) -> Self {
        Target {
            documents,
            batch,
            purge_calls: 0,
        }
    }
}

impl CleanupTarget for Target {
    fn documents(&self) -> Vec<Value> {
        self.documents.clone()
    }

    fn purge(&mut self, ids: &[String]) -> Result<usize> {
        self.purge_calls += 1;
        let take = self.batch.min(ids.len());
        let purging: Vec<&String> = ids.iter().take(take).collect();
        let before = self.documents.len();
        self.documents.retain(|document| {
            !purging
                .iter()
                .any(|id| document["id"] == json!(id.as_str()))
        });
        Ok(before - self.documents.len())
    }
}

// ---------------------------------------------------------------------------
// `cleanup.test.ts:30` — should clean up the deleted documents
// ---------------------------------------------------------------------------

#[test]
fn upstream_30_should_clean_up_the_deleted_documents() {
    let policy = CleanupPolicy::default();
    // One month retention.
    let mut target = Target::new(vec![
        tombstone("old", DAY_MS * 40),
        tombstone("recent", DAY_MS),
        live("live"),
    ]);
    let progress = run_cleanup(&mut target, "id", &policy, now(), 10).expect("cleanup");

    assert_eq!(progress.purged, 1, "only the tombstone older than a month");
    assert_eq!(
        target.documents.len(),
        2,
        "the recent tombstone and the live doc stay"
    );
    assert!(target
        .documents
        .iter()
        .any(|document| document["id"] == json!("recent")));
    assert!(target
        .documents
        .iter()
        .any(|document| document["id"] == json!("live")));
}

// ---------------------------------------------------------------------------
// `cleanup.test.ts:68` — works by manually calling cleanup()
// ---------------------------------------------------------------------------

#[test]
fn upstream_68_manual_cleanup_with_zero_retention_purges_all_tombstones() {
    // `collection.cleanup(0)` — the override path (`cleanup.ts:21-23`).
    let policy = CleanupPolicy::default().with_minimum_deleted_time(0);
    // Ages are > 0 on purpose: retention 0 makes the threshold `now`, and the rule is
    // strictly `lwt < threshold`, so a tombstone deleted at exactly `now` survives. That
    // is the same boundary the storages implement
    // (`maxDeletionTime = now() - minimumDeletedTime`, then `lwt < maxDeletionTime`), and
    // `a_retention_boundary_is_strictly_greater_than` pins it explicitly.
    let mut target = Target::new(vec![tombstone("a", 1), tombstone("b", 2), live("live")]);
    let progress = run_cleanup(&mut target, "id", &policy, now(), 10).expect("cleanup");
    assert_eq!(progress.purged, 2, "every realistically-aged tombstone");
    assert_eq!(target.documents.len(), 1);
    assert_eq!(
        target.documents[0]["id"],
        json!("live"),
        "but never a live document"
    );
}

#[test]
fn a_retention_boundary_is_strictly_greater_than() {
    // `maxDeletionTime = now() - minimumDeletedTime`, then `lwt < maxDeletionTime`, so a
    // tombstone exactly at the boundary survives.
    let policy = CleanupPolicy::default().with_minimum_deleted_time(1_000);
    let at_boundary = tombstone("exact", 1_000);
    let past_boundary = tombstone("past", 1_001);
    assert!(!should_purge(&at_boundary, &policy, now()).expect("rule"));
    assert!(should_purge(&past_boundary, &policy, now()).expect("rule"));
}

// ---------------------------------------------------------------------------
// `cleanup.test.ts:243` — minimumDeletedTime not respected
// ---------------------------------------------------------------------------

#[test]
fn upstream_243_the_configured_minimum_deleted_time_is_the_one_used() {
    // The upstream case spies on the storage calls and asserts the values passed are
    // `0`, `5`, then the default. Here the same thing is asserted by outcome: the same
    // tombstone is purged only when the retention allows it.
    let tombstone = tombstone("a", 5);
    assert!(
        should_purge(
            &tombstone,
            &CleanupPolicy::default().with_minimum_deleted_time(0),
            now()
        )
        .expect("rule"),
        "retention 0 purges a 5ms-old tombstone"
    );
    assert!(
        !should_purge(
            &tombstone,
            &CleanupPolicy::default().with_minimum_deleted_time(5),
            now()
        )
        .expect("rule"),
        "retention 5 does not, because the boundary is strict"
    );
    assert!(
        !should_purge(&tombstone, &CleanupPolicy::default(), now()).expect("rule"),
        "the default month certainly does not"
    );
}

// ---------------------------------------------------------------------------
// `cleanup.test.ts:281` — batched cleanup must loop until done
// ---------------------------------------------------------------------------

#[test]
fn upstream_281_a_batched_storage_is_looped_until_done() {
    // A storage that purges one document per call and reports "more work" by returning
    // fewer than it was given. Stopping at the first short return would leave tombstones
    // forever.
    let policy = CleanupPolicy::default().with_minimum_deleted_time(0);
    let mut target = Target::batching(
        vec![tombstone("a", 1), tombstone("b", 1), tombstone("c", 1)],
        1,
    );
    let progress = run_cleanup(&mut target, "id", &policy, now(), 20).expect("cleanup");
    assert_eq!(progress.purged, 3, "all three were purged across passes");
    assert!(
        progress.passes >= 4,
        "three batched purges plus a confirming pass, got {}",
        progress.passes
    );
    assert_eq!(target.documents.len(), 0);
}

#[test]
fn a_storage_that_never_finishes_is_an_error_not_a_hang() {
    // The bound exists so a misbehaving storage is a loud failure.
    struct Stuck {
        documents: Vec<Value>,
    }
    impl CleanupTarget for Stuck {
        fn documents(&self) -> Vec<Value> {
            self.documents.clone()
        }
        fn purge(&mut self, _ids: &[String]) -> Result<usize> {
            // Claims to have removed one, removes none.
            Ok(1)
        }
    }
    let policy = CleanupPolicy::default().with_minimum_deleted_time(0);
    let mut target = Stuck {
        documents: vec![tombstone("a", 1)],
    };
    let error = run_cleanup(&mut target, "id", &policy, now(), 3).expect_err("must give up");
    assert_eq!(error, Error::DidNotFinish(3));
}

// ---------------------------------------------------------------------------
// `cleanup.test.ts:359` — cleanup() returns true as the type says
// ---------------------------------------------------------------------------

#[test]
fn upstream_359_a_finished_pass_reports_that_it_finished() {
    // Upstream's return value is `boolean`: `true` means done. Here a pass that
    // terminates returns `Ok`, which is the same statement, and `passes` records how
    // many iterations it needed.
    let policy = CleanupPolicy::default().with_minimum_deleted_time(0);
    let mut target = Target::new(vec![tombstone("a", 1)]);
    let progress = run_cleanup(&mut target, "id", &policy, now(), 10).expect("cleanup");
    assert_eq!(progress.purged, 1);
    assert_eq!(
        progress.passes, 2,
        "one purge pass plus the confirming pass"
    );
}

// ---------------------------------------------------------------------------
// `cleanup.test.ts:208` — a re-inserted document is found normally
// ---------------------------------------------------------------------------

#[test]
fn upstream_208_a_reinserted_document_is_a_live_document_again() {
    // "must find a document that was re-inserted after the cleanup purged its
    // tombstone". The purge removes the row; the re-insert is a fresh live document and
    // must not be considered purgeable. The `_rev` is deliberately *not* consulted by
    // `should_purge`, so a new revision chain cannot confuse the rule.
    let policy = CleanupPolicy::default().with_minimum_deleted_time(0);
    let mut target = Target::new(vec![tombstone("a", 1)]);
    run_cleanup(&mut target, "id", &policy, now(), 10).expect("cleanup");
    assert_eq!(target.documents.len(), 0, "the tombstone is gone");

    // Re-insert: a live document with a new revision.
    target.documents.push(json!({
        "id": "a",
        "_deleted": false,
        "_rev": "1-newtoken",
        "_meta": { "lwt": now() }
    }));
    let progress = run_cleanup(&mut target, "id", &policy, now(), 10).expect("cleanup");
    assert_eq!(progress.purged, 0, "a live document is never purged");
    assert_eq!(target.documents.len(), 1);
}

// ---------------------------------------------------------------------------
// The cross-instance case the brief asks for. No upstream counterpart.
// ---------------------------------------------------------------------------

#[test]
fn two_collections_over_one_storage_disagree_about_retention() {
    // WHAT GOES WRONG with upstream's per-collection behaviour. Two collections share a
    // storage. `short` was configured with a one-minute window, `long` with a month
    // because a replication still needs to see the deletions.
    let short = CleanupPolicy::default().with_minimum_deleted_time(60_000);
    let long = CleanupPolicy::default().with_minimum_deleted_time(DAY_MS * 31);

    let mut group = CleanupGroup::new();
    group.add("short", short).add("long", long);

    let documents = vec![
        tombstone("deleted-2-min-ago", 120_000),
        tombstone("deleted-2-days-ago", DAY_MS * 2),
    ];

    // Per-member counts make the disagreement concrete: `short` would purge both,
    // `long` neither.
    let counts = group
        .per_member_would_purge(&documents, now(), "id")
        .expect("count");
    assert_eq!(
        counts,
        vec![("short".to_string(), 2), ("long".to_string(), 0)],
        "the shorter window wants to purge what the longer one needs"
    );

    // And running the short collection's own policy destroys the tombstones `long`
    // depends on.
    let mut target = Target::new(documents.clone());
    let purged = run_cleanup(&mut target, "id", &short, now(), 10).expect("cleanup");
    assert_eq!(purged.purged, 2);
    assert_eq!(
        target.documents.len(),
        0,
        "both tombstones are gone, including the one `long` still needs"
    );

    // The same run under `long`'s policy keeps them.
    let mut target = Target::new(documents);
    let purged = run_cleanup(&mut target, "id", &long, now(), 10).expect("cleanup");
    assert_eq!(purged.purged, 0);
    assert_eq!(target.documents.len(), 2);
}

#[test]
fn a_cleanup_group_holds_the_longest_retention() {
    // THE FIX. The group's effective retention is the maximum across its members, so no
    // member can purge a tombstone another member still expects.
    let mut group = CleanupGroup::new();
    group
        .add(
            "short",
            CleanupPolicy::default().with_minimum_deleted_time(60_000),
        )
        .add(
            "long",
            CleanupPolicy::default().with_minimum_deleted_time(DAY_MS * 31),
        );

    assert_eq!(
        group.effective_minimum_deleted_time_ms(),
        Some(DAY_MS * 31),
        "the longest retention wins"
    );
    let effective = group.effective_policy("short").expect("member");
    assert_eq!(
        effective.minimum_deleted_time_ms,
        DAY_MS * 31,
        "the short collection runs with the group's retention"
    );
    // Its own settings are otherwise untouched.
    assert_eq!(effective.run_each_ms, CleanupPolicy::default().run_each_ms);
    assert!(effective.wait_for_leadership);

    // Running the effective policy purges nothing that `long` needs.
    let documents = vec![tombstone("deleted-2-days-ago", DAY_MS * 2)];
    let mut target = Target::new(documents);
    let purged = run_cleanup(&mut target, "id", &effective, now(), 10).expect("cleanup");
    assert_eq!(purged.purged, 0);
    assert_eq!(
        target.documents.len(),
        1,
        "the tombstone survives for `long`"
    );

    // While a tombstone older than the *longest* retention is still purged, so the
    // group does not leak disk forever.
    let mut target = Target::new(vec![tombstone("very-old", DAY_MS * 40)]);
    let purged = run_cleanup(&mut target, "id", &effective, now(), 10).expect("cleanup");
    assert_eq!(purged.purged, 1);
}

#[test]
fn a_single_member_group_uses_that_members_retention() {
    // The group must not change behaviour when there is nothing to reconcile.
    let policy = CleanupPolicy::default().with_minimum_deleted_time(1_000);
    let mut group = CleanupGroup::new();
    group.add("only", policy);
    assert_eq!(group.effective_minimum_deleted_time_ms(), Some(1_000));
    assert_eq!(group.effective_policy("only"), Some(policy));
}

#[test]
fn an_empty_group_has_no_retention_to_offer() {
    let group = CleanupGroup::new();
    assert_eq!(group.effective_minimum_deleted_time_ms(), None);
    assert_eq!(group.effective_policy("anything"), None);
    assert_eq!(group.member_policy("anything"), None);
}

#[test]
fn a_three_member_group_takes_the_maximum_not_the_last_or_the_first() {
    let mut group = CleanupGroup::new();
    group
        .add("a", CleanupPolicy::default().with_minimum_deleted_time(10))
        .add("b", CleanupPolicy::default().with_minimum_deleted_time(30))
        .add("c", CleanupPolicy::default().with_minimum_deleted_time(20));
    assert_eq!(group.effective_minimum_deleted_time_ms(), Some(30));
}

// ---------------------------------------------------------------------------
// The policy flags the crate carries but does not act on
// ---------------------------------------------------------------------------

#[test]
fn the_defaults_are_one_month_sixty_seconds_and_five_minutes() {
    // `cleanup-helper.ts:5-11`, asserted together so a change to any of the three is
    // visible in one place.
    let policy = CleanupPolicy::default();
    assert_eq!(
        policy.minimum_deleted_time_ms, 2_678_400_000,
        "one month in ms"
    );
    assert_eq!(policy.minimum_collection_age_ms, 60_000, "60 seconds");
    assert_eq!(policy.run_each_ms, 300_000, "5 minutes");
    assert!(policy.await_replications_in_sync);
    assert!(policy.wait_for_leadership);
}
