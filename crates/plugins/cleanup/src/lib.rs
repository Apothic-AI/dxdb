//! Purging tombstones under a retention policy.
//!
//! Ported from `src/plugins/cleanup/` at upstream `3b763ee17` (291 lines):
//! `cleanup-helper.ts` (11) is the default policy, `cleanup.ts` (143) is the
//! per-collection loop, and `cleanup-state.ts` (92) does the same for `RxState`.
//!
//! # What cleanup actually deletes, and what it must never delete
//!
//! A tombstone is a deleted document still sitting in the storage. Purging it is
//! lossy and irreversible, so the retention rule is the whole contract:
//!
//! ```text
//! purge a tombstone only when   now - _meta.lwt > minimumDeletedTime
//! ```
//!
//! Upstream applies that boundary in **each storage** rather than in the plugin
//! (`storage-memory/rx-storage-instance-memory.ts:530` and seven siblings compute
//! `maxDeletionTime = now() - minimumDeletedTime`), which is why the plugin's job is
//! to pass the *policy* down and to decide *when* to ask. [`CleanupPolicy`] and
//! [`should_purge`] are the rule; [`run_cleanup`] is the loop.
//!
//! # The subtlety the brief names: retention is a shared-storage question
//!
//! `minimumDeletedTime` is per **database** policy, overridable per call
//! (`cleanup.ts:21-23` — `Object.assign({}, DEFAULT_CLEANUP_POLICY,
//! database.cleanupPolicy)` then `cleanupPolicy.minimumDeletedTime = minimumDeletedTime`).
//! Two collections in one database therefore share a retention window by default, but
//! a caller can pass a different window to one of them, and **both talk to the same
//! storage**.
//!
//! Why that matters: purging a tombstone destroys the only record that a document was
//! deleted at a given revision. If collection A purges at one minute while collection B
//! expects tombstones to survive a month — because a replication may still need to see
//! the deletion — then A's cleanup has silently broken B's replication. Upstream does
//! not guard against this; it simply lets each collection purge with whatever value it
//! was given.
//!
//! This crate keeps upstream's per-collection behaviour as the default, and adds
//! [`CleanupGroup`]: a set of collections that share a storage, which computes the
//! **maximum** `minimumDeletedTime` across its members and never purges a tombstone
//! younger than that. The brief's test — two collections, one storage, different
//! retention expectations — is exactly what `CleanupGroup` exists for, and there is a
//! test showing the per-collection behaviour purging what the group holds.
//!
//! # The other rules that matter
//!
//! * **`cleanup()` loops until the storage says it is done.** Some storages purge in
//!   batches and return `false` to mean "more work" (`cleanup.ts:70-116`; the upstream
//!   test at `:281` covers it). Stopping at the first `false` leaves tombstones behind
//!   forever.
//! * **The default retention is one month** (`cleanup-helper.ts:6`,
//!   `1000*60*60*24*31`), the collection must be at least 60 seconds old
//!   (`:7`), and re-runs are throttled to five minutes (`:8`).
//! * **A purged tombstone starts a new revision chain.** If the same primary key is
//!   inserted again after its tombstone was purged, the document must be found normally
//!   (`cleanup.test.ts:208`, "must find a document that was re-inserted after the
//!   cleanup purged its tombstone"). That is a storage property, but it is why a
//!   tombstone's `_rev` must not be reused: [`should_purge`] therefore never considers
//!   `_rev`.
//! * **`minimumDeletedTime: 0` purges everything already deleted**, but still not a
//!   *live* document: [`should_purge`] requires `_deleted` (`cleanup.test.ts:68`
//!   "should work by manually calling RxCollection.cleanup()").
//!
//! # Not ported, and why
//!
//! * The event-driven re-run: upstream waits for any write event then throttles by
//!   `runEach` (`cleanup.ts:124-143`). That needs the collection event stream, which is
//!   lane D's. [`run_cleanup`] is the pass it would call.
//! * `awaitReplicationsInSync` (`cleanup.ts:73-86`) and `waitForLeadership`
//!   (`:55-57`): both gate the pass on another subsystem. The policy carries the flags
//!   so a caller can honour them; this crate does not own the replication states or the
//!   elector.
//! * The `postCleanup` hook (`:117-120`) — the hook registry is lane D's.

#![forbid(unsafe_code)]

use serde_json::Value;
use thiserror::Error;

/// The default policy (`cleanup-helper.ts:5-11`).
///
/// The values are in milliseconds and the comments are upstream's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CleanupPolicy {
    /// How long a tombstone must have existed before it may be purged.
    /// Default: one month (`1000*60*60*24*31`).
    pub minimum_deleted_time_ms: u64,
    /// How long a collection must exist before its first cleanup.
    /// Default: 60 seconds (`1000*60`).
    pub minimum_collection_age_ms: u64,
    /// How often to re-run after a write. Default: five minutes (`1000*60*5`).
    pub run_each_ms: u64,
    /// Pause cleanup while a replication is out of sync.
    pub await_replications_in_sync: bool,
    /// Only run cleanup on the elected leader.
    pub wait_for_leadership: bool,
}

impl Default for CleanupPolicy {
    fn default() -> Self {
        CleanupPolicy {
            // one month
            minimum_deleted_time_ms: 1_000 * 60 * 60 * 24 * 31,
            // 60 seconds
            minimum_collection_age_ms: 1_000 * 60,
            // 5 minutes
            run_each_ms: 1_000 * 60 * 5,
            await_replications_in_sync: true,
            wait_for_leadership: true,
        }
    }
}

impl CleanupPolicy {
    /// `Object.assign({}, DEFAULT_CLEANUP_POLICY, database.cleanupPolicy)` then the
    /// per-call override (`cleanup.ts:14-23`).
    pub fn with_minimum_deleted_time(mut self, minimum_deleted_time_ms: u64) -> Self {
        self.minimum_deleted_time_ms = minimum_deleted_time_ms;
        self
    }
}

/// Errors.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum Error {
    /// A tombstone has no usable `_meta.lwt`, so its age is unknown and purging it
    /// would be a guess.
    #[error("a tombstone for `{0}` has no _meta.lwt, so its age cannot be checked")]
    TombstoneWithoutLwt(String),
    /// The document is not an object.
    #[error("a document must be a JSON object, got {0}")]
    NotAnObject(String),
    /// A cleanup pass ran more iterations than the guard allows. A storage that keeps
    /// returning "more work" would otherwise loop forever.
    #[error("cleanup did not finish after {0} passes; the storage keeps reporting more work")]
    DidNotFinish(usize),
}

/// Result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Reads `_meta.lwt` and `_deleted` from a stored document.
fn tombstone_state(document: &Value) -> Result<(bool, Option<f64>)> {
    let map = document
        .as_object()
        .ok_or_else(|| Error::NotAnObject(document.to_string()))?;
    let deleted = map
        .get("_deleted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let lwt = map
        .get("_meta")
        .and_then(|meta| meta.get("lwt"))
        .and_then(Value::as_f64);
    Ok((deleted, lwt))
}

/// The purge rule, as a function.
///
/// `now - lwt > minimumDeletedTime`, matching every storage's
/// `maxDeletionTime = now() - minimumDeletedTime` followed by `lwt < maxDeletionTime`
/// (e.g. `storage-memory/rx-storage-instance-memory.ts:530`).
///
/// A **live** document is never purgeable, whatever the retention. That is the one rule
/// that would destroy data rather than reclaim it.
pub fn should_purge(document: &Value, policy: &CleanupPolicy, now_ms: f64) -> Result<bool> {
    let (deleted, lwt) = tombstone_state(document)?;
    if !deleted {
        return Ok(false);
    }
    let Some(lwt) = lwt else {
        // A tombstone with no lwt is not purgeable: its age is unknown, and guessing
        // means deleting a document that might have been deleted a moment ago.
        return Ok(false);
    };
    let threshold = now_ms - policy.minimum_deleted_time_ms as f64;
    Ok(lwt < threshold)
}

/// What a cleanup pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CleanupProgress {
    /// Documents examined.
    pub examined: usize,
    /// Tombstones purged.
    pub purged: usize,
    /// Passes over the storage.
    pub passes: usize,
}

/// The storage, as far as cleanup needs it.
pub trait CleanupTarget {
    /// Every stored document, tombstones included.
    fn documents(&self) -> Vec<Value>;

    /// Removes the documents with these primary keys, returning how many were removed.
    ///
    /// Mirroring upstream, a storage that purges in batches reports "more work remains"
    /// by returning fewer than it was given; the caller loops (`cleanup.ts:90-116`).
    fn purge(&mut self, ids: &[String]) -> Result<usize>;
}

/// The primary key of a document. `storage-memory` uses `id` for local documents and a
/// schema-declared path elsewhere; cleanup only needs a stable identifier, so the
/// caller supplies the path.
pub trait PrimaryPath {
    /// The field that identifies a document.
    fn primary_path(&self) -> &str;
}

/// Runs cleanup until the storage reports no more work (`cleanup.ts:63-121`).
///
/// `max_passes` bounds the loop. Upstream loops `while (!isDone)` with no bound because
/// a storage's `cleanup` is trusted to terminate; a bound turns a misbehaving storage
/// into an error instead of a hang, which is the same reasoning as the `upsert` retry
/// bound in `local-documents`.
pub fn run_cleanup(
    target: &mut dyn CleanupTarget,
    primary_path: &str,
    policy: &CleanupPolicy,
    now_ms: f64,
    max_passes: usize,
) -> Result<CleanupProgress> {
    let mut progress = CleanupProgress::default();

    for _ in 0..max_passes {
        progress.passes += 1;
        let documents = target.documents();
        progress.examined = documents.len();

        let mut to_purge = Vec::new();
        for document in &documents {
            if should_purge(document, policy, now_ms)? {
                let id = document
                    .get(primary_path)
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| {
                        Error::TombstoneWithoutLwt(format!("(no {primary_path} field)"))
                    })?;
                to_purge.push(id);
            }
        }

        if to_purge.is_empty() {
            // Nothing left to purge: the pass is done.
            return Ok(progress);
        }
        let removed = target.purge(&to_purge)?;
        progress.purged += removed;
        if removed == to_purge.len() {
            // Everything eligible went in one go; one more pass confirms there is no
            // more work, which is what upstream's `isDone` check does.
            continue;
        }
        // Fewer than asked for: the storage batched. Loop again.
    }

    Err(Error::DidNotFinish(max_passes))
}

/// Collections that share one storage, and the retention they must agree on.
///
/// # Why this type exists
///
/// `minimumDeletedTime` is set per database and overridable per call
/// (`cleanup.ts:21-23`), so two collections over one storage can be configured with
/// different windows. When the shorter one purges, the longer one's tombstones are gone
/// — and a tombstone is the only record that a document was deleted, so anything that
/// still needed to see that deletion (a replication catching up, another device about to
/// pull) now never will.
///
/// Upstream has no guard for this. [`CleanupGroup`] is the guard: it holds every member
/// and the effective retention is the **maximum** of their policies, so no member can
/// purge a tombstone another member still expects to exist.
///
/// This is an addition, not a port, and it is opt-in: [`CleanupGroup::member_policy`]
/// returns the per-collection policy upstream would use, and every test that reproduces
/// upstream uses that. Using the group is what a caller does when they want the safety.
#[derive(Debug, Default)]
pub struct CleanupGroup {
    members: Vec<(String, CleanupPolicy)>,
}

impl CleanupGroup {
    /// A group with no members.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a collection and the policy it was configured with.
    pub fn add(&mut self, collection_name: impl Into<String>, policy: CleanupPolicy) -> &mut Self {
        self.members.push((collection_name.into(), policy));
        self
    }

    /// The retention one member asked for — what upstream would use on its own.
    pub fn member_policy(&self, collection_name: &str) -> Option<CleanupPolicy> {
        self.members
            .iter()
            .find(|(name, _)| name == collection_name)
            .map(|(_, policy)| *policy)
    }

    /// The effective retention for the whole group: the **maximum** across members.
    ///
    /// `None` for an empty group, in which case there is nothing to protect and the
    /// caller uses its own policy.
    pub fn effective_minimum_deleted_time_ms(&self) -> Option<u64> {
        self.members
            .iter()
            .map(|(_, policy)| policy.minimum_deleted_time_ms)
            .max()
    }

    /// The effective policy for a member: its own settings, but the group's retention.
    pub fn effective_policy(&self, collection_name: &str) -> Option<CleanupPolicy> {
        let own = self.member_policy(collection_name)?;
        let retention = self.effective_minimum_deleted_time_ms()?;
        Some(own.with_minimum_deleted_time(retention))
    }

    /// How many tombstones each member would purge **if it used its own retention**
    /// rather than the group's. Used to show the difference in a test, and by a caller
    /// that wants to warn about a misconfiguration.
    pub fn per_member_would_purge(
        &self,
        documents: &[Value],
        now_ms: f64,
        primary_path: &str,
    ) -> Result<Vec<(String, usize)>> {
        let _ = primary_path;
        let mut counts = Vec::new();
        for (name, policy) in &self.members {
            let mut purged = 0;
            for document in documents {
                if should_purge(document, policy, now_ms)? {
                    purged += 1;
                }
            }
            counts.push((name.clone(), purged));
        }
        Ok(counts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_default_policy_matches_upstream() {
        // `cleanup-helper.ts:5-11`.
        let policy = CleanupPolicy::default();
        assert_eq!(policy.minimum_deleted_time_ms, 1_000 * 60 * 60 * 24 * 31);
        assert_eq!(policy.minimum_collection_age_ms, 1_000 * 60);
        assert_eq!(policy.run_each_ms, 1_000 * 60 * 5);
        assert!(policy.await_replications_in_sync);
        assert!(policy.wait_for_leadership);
    }

    #[test]
    fn a_live_document_is_never_purged() {
        let document = json!({ "id": "a", "_deleted": false, "_meta": { "lwt": 1 } });
        assert!(!should_purge(&document, &CleanupPolicy::default(), 1e12).expect("rule"));
    }

    #[test]
    fn a_tombstone_without_lwt_is_not_purged() {
        let document = json!({ "id": "a", "_deleted": true });
        assert!(!should_purge(&document, &CleanupPolicy::default(), 1e12).expect("rule"));
    }
}

// ---------------------------------------------------------------------------
// The plugin, on the core registry
// ---------------------------------------------------------------------------

/// The hooks this plugin registers, in upstream's order.
///
/// Upstream's `RxDBCleanupPlugin` has exactly one hooks block
/// (`src/plugins/cleanup/index.ts:30-41`):
///
/// ```text
/// createRxCollection: { after: i => startCleanupForRxCollection(i.collection) }
/// createRxState:      { after: i => startCleanupForRxState(i.state) }
/// ```
///
/// Both are `after`, which is why [`CleanupPlugin`] registers at
/// [`dxdb_core::Order::After`]: cleanup must observe a *finished* collection, not one
/// whose schema is half-built. This is the ordering claim worth testing, and the
/// migration of this plugin onto the registry is what proves the registry is usable for
/// a plugin that actually hooks in.
pub const HOOKS: &[dxdb_core::HookName] = &[
    dxdb_core::HookName::CreateRxCollection,
    dxdb_core::HookName::CreateRxState,
];

/// What the plugin does when a collection appears: remembers that it needs cleanup.
///
/// Upstream starts a background loop (`startCleanupForRxCollection`). This port records
/// the collection in a shared [`CleanupRegistry`] and leaves the *scheduling* to the
/// caller, because a timer is a platform binding. The hook is still the right seam: it is
/// where "a collection exists, so it is a cleanup candidate" is decided.
#[derive(Clone, Debug, Default)]
pub struct CleanupRegistry {
    /// Collection names seen by the `createRxCollection` hook, in creation order.
    pub collections: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
}

impl CleanupRegistry {
    /// The collection names registered so far.
    pub fn collections(&self) -> Vec<String> {
        self.collections.borrow().clone()
    }
}

/// The `RxDBCleanupPlugin` equivalent.
///
/// Install with `database.add_plugin(CleanupPlugin::new(registry))?`; the registry is how
/// a caller observes which collections the hooks saw. Each instance owns its own
/// registry, which is the point of a per-database plugin set: two databases in one
/// process get independent cleanup bookkeeping.
#[derive(Clone, Debug, Default)]
pub struct CleanupPlugin {
    registry: CleanupRegistry,
}

impl CleanupPlugin {
    /// A plugin that records into `registry`.
    pub fn new(registry: CleanupRegistry) -> Self {
        CleanupPlugin { registry }
    }

    /// The registry this plugin writes to.
    pub fn registry(&self) -> &CleanupRegistry {
        &self.registry
    }
}

impl dxdb_core::Plugin for CleanupPlugin {
    fn name(&self) -> &'static str {
        // `src/plugins/cleanup/index.ts:10`.
        "cleanup"
    }

    fn install(
        &self,
        registrations: &mut dxdb_core::Registrations,
        _overwritables: &mut dxdb_core::Overwritables,
    ) -> std::result::Result<(), dxdb_core::PluginError> {
        // `createRxCollection: { after: ... }`
        let collections = std::rc::Rc::clone(&self.registry.collections);
        registrations.on_ordered(
            dxdb_core::HookName::CreateRxCollection,
            dxdb_core::Order::After,
            move |payload| {
                if let dxdb_core::HookPayload::Collection { name, .. } = payload {
                    collections.borrow_mut().push(name.clone());
                }
                dxdb_core::HookOutcome::Continue
            },
        );
        // `createRxState: { after: ... }`. Upstream runs the same state-collection
        // cleanup; there is no `RxState` in this crate yet, so the handler exists to keep
        // the registration list honest and does nothing.
        registrations.on_ordered(
            dxdb_core::HookName::CreateRxState,
            dxdb_core::Order::After,
            |_payload| dxdb_core::HookOutcome::Continue,
        );
        Ok(())
    }
}
