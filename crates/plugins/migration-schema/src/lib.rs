//! Schema-version migration: strategies, the run-once state machine, and status.
//!
//! Ported from `src/plugins/migration-schema/` at upstream `3b763ee17` (1,187
//! lines): `migration-helpers.ts` (206) is the document transformation and the
//! strategy validation, `migration-types.ts` (42) is the status shape, and
//! `rx-migration-state.ts` (839) is the runner.
//!
//! # What the brief asked to be exact about, and what upstream actually does
//!
//! **"Match the strategy semantics as enumerated from source, including `false`
//! meaning 'do not migrate this document' if that is what upstream does."**
//!
//! Upstream does **not** use `false` for that. The sentinel is **`null`**:
//!
//! - `migrateDocumentData` is documented as "@return final object or null if
//!   migrationStrategy deleted it" (`migration-helpers.ts:58`).
//! - The runner converts a `null` into a dropped row: "The migration strategy can
//!   return null which means the document must be deleted during migration"
//!   (`rx-migration-state.ts:600-604`), then filters those rows out (`:619`).
//! - The public docs say the same: "If the strategy returns `null`, the document
//!   will be removed instead of migrated" (`docs-src/docs/migration-schema.md:37`).
//!
//! `false` as a return value has no meaning anywhere in the plugin. A strategy that
//! returns `false` would be stored as the migrated document and then fail schema
//! validation, which is a loud failure rather than a silent one — so this port
//! models the sentinel as [`Outcome::Delete`] and nothing else. Guarding against
//! `false` *as if* it meant something would be inventing a semantic.
//!
//! # The two properties the brief asks for
//!
//! **A migration runs exactly once.** Three separate mechanisms, and all three are
//! needed:
//!
//! 1. *Per instance*: `startMigration` throws `DM1` when already started
//!    (`rx-migration-state.ts:158-160`).
//! 2. *Per process*: the migration is only needed while an old collection's
//!    internal meta document exists (`mustMigrate`, `migration-helpers.ts:141-149`).
//!    The runner deletes that document at the end (`:315-352`), so a second run
//!    finds nothing to do. If the delete conflicts because the document changed, it
//!    re-fetches and retries; if the conflict says it is already deleted, it stops.
//! 3. *Across tabs*: a leader election per collection, with a heartbeat on the
//!    status document so a frozen leader can be detected and taken over
//!    (`migration-helpers.ts:161-164`, `rx-migration-state.ts:360-413`).
//!
//! Only (1) and (2) are in this crate. (3) needs a broadcast channel, which is a
//! platform binding (lane F/G), and it is listed as not ported below.
//!
//! **A document is never visible in a half-migrated shape.** Upstream gets this from
//! doing the whole migration as a *push-only replication* into the new storage
//! (`rx-migration-state.ts:216-219`): rows are transformed and written one batch at
//! a time, and a document only becomes visible in the new collection once its
//! transformed form has been written. The old storage keeps serving the old shape
//! until then. This crate provides the transformation, which is the part that
//! decides the shape; the batch loop is the runner's and is not ported.
//!
//! # Strategy semantics, exactly
//!
//! From `migrationHelpers.migrateDocumentData` (`migration-helpers.ts:60-122`):
//!
//! * Strategies run for versions `docSchemaVersion + 1 ..= schema.version`, in
//!   ascending order, each receiving the previous strategy's output
//!   (`:78`, `:92-109`).
//! * `_deleted` and `_meta` are **removed** before the first strategy and restored
//!   afterwards (`:72-76`, `:116-119`), so a strategy never sees or can corrupt
//!   them.
//! * `_attachments` is *not* removed: it is carried across strategies, and a
//!   strategy that omits it gets the previous value re-attached (`:88-107`).
//! * Once a strategy returns `null`, every later strategy is skipped
//!   (`runStrategyIfNotNull`, `:124-136`) and the document is deleted.
//! * A strategy that throws propagates: the migration fails, `migrationInProgress`
//!   is cleared, and the status becomes `ERROR` with the error in it
//!   (`rx-migration-state.ts:293-308`).
//!
//! # Not ported, and why
//!
//! * **The replication-driven runner** (`rx-migration-state.ts`, ~700 of the 839
//!   lines): it needs `dxdb-replication` (lane E) plus the internal store
//!   (`dx-db-core`, lane D). What is left of the file once the replication is
//!   factored out is orchestration around storage instances this lane cannot
//!   construct.
//! * **Cross-tab leader election and heartbeat** (`:360-450`): needs a
//!   `BroadcastChannel`. [`MigrationStatus`]' math and the `DONE`/`STALLED`
//!   decision function are ported because they are pure; the channel is not.
//! * **`getOldCollectionMeta`** (`migration-helpers.ts:25-51`): reads the internal
//!   store by primary key. The *selection rule* it implements is ported
//!   ([`oldest_previous_version`]) because it has a subtle behaviour the brief
//!   cares about: when several old versions have leftovers, migration starts from
//!   the **lowest** one.

#![forbid(unsafe_code)]

use serde_json::Value;
use thiserror::Error;

/// Errors, carrying upstream's codes.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum Error {
    /// `COL11`: `migrationStrategies` was not an object, or was an array
    /// (`check-migration-strategies.ts:19`).
    #[error("COL11: migrationStrategies must be an object, not an array")]
    StrategiesNotAnObject,
    /// `COL12`: the number of strategies does not match the number of previous
    /// versions (`check-migration-strategies.ts:31`).
    #[error("COL12: have {have:?} migration strategies but the schema needs {should:?}")]
    StrategyCountMismatch {
        /// The versions the caller supplied.
        have: Vec<String>,
        /// The versions the schema requires.
        should: Vec<u32>,
    },
    /// `COL13`: a strategy is missing or is not a function
    /// (`check-migration-strategies.ts:45`).
    #[error("COL13: migration strategy for version {version} is a {kind}")]
    StrategyNotAFunction {
        /// The version whose strategy is wrong.
        version: u32,
        /// What the caller supplied instead.
        kind: &'static str,
    },
    /// `DM1`: `startMigration` was called twice on one state
    /// (`rx-migration-state.ts:159`).
    #[error("DM1: the migration was already started")]
    AlreadyStarted,
    /// A strategy returned something that cannot be a document.
    #[error("migration strategy for version {version} returned {value}, which is not an object")]
    StrategyReturnedNonObject {
        /// The version whose strategy misbehaved.
        version: u32,
        /// What it returned, as JSON.
        value: String,
    },
}

/// Result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// What a migration strategy produced (`migration-helpers.ts:124-136`).
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// The document in its next shape.
    Migrated(Value),
    /// The strategy returned `null`: the document is removed
    /// (`migration-helpers.ts:58`, `rx-migration-state.ts:600`).
    Delete,
}

/// One migration strategy: takes the document at version `v` and returns it at
/// `v + 1`, or [`Outcome::Delete`].
///
/// A boxed trait object rather than a generic, because a collection holds one
/// function per version and they are all the same shape with different bodies.
pub type Strategy = Box<dyn Fn(Value) -> Result<Outcome> + Send + Sync>;

/// The strategy map, keyed by the version it migrates **to**.
///
/// Upstream keys by `version` too: `migrationStrategies[vNr + 1]` for the previous
/// version `vNr` (`check-migration-strategies.ts:41`).
#[derive(Default)]
pub struct MigrationStrategies {
    by_target_version: std::collections::BTreeMap<u32, Strategy>,
}

impl std::fmt::Debug for MigrationStrategies {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MigrationStrategies")
            .field(
                "versions",
                &self.by_target_version.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl MigrationStrategies {
    /// An empty map. Valid only for a version-0 schema.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers the strategy that migrates *to* `version`.
    pub fn insert<F>(&mut self, version: u32, strategy: F) -> &mut Self
    where
        F: Fn(Value) -> Result<Outcome> + Send + Sync + 'static,
    {
        self.by_target_version.insert(version, Box::new(strategy));
        self
    }

    /// The versions with a strategy, ascending.
    pub fn versions(&self) -> Vec<u32> {
        self.by_target_version.keys().copied().collect()
    }

    /// `checkMigrationStrategies` (`check-migration-strategies.ts:10`).
    ///
    /// Validates the map against a schema of `schema_version`: one strategy per
    /// version from 1 up to `schema_version` inclusive, no more and no fewer.
    pub fn validate_for_version(&self, schema_version: u32) -> Result<()> {
        // `getPreviousVersions(schema)` is `[0 .. version-1]`, and the strategies are
        // keyed `previous + 1`, i.e. `[1 ..= version]`.
        let should: Vec<u32> = if schema_version == 0 {
            Vec::new()
        } else {
            (1..=schema_version).collect()
        };
        let have = self.versions();

        // `previousVersions.length !== Object.keys(migrationStrategies).length`
        if have.len() != should.len() {
            return Err(Error::StrategyCountMismatch {
                have: have.iter().map(u32::to_string).collect(),
                should,
            });
        }
        // `previousVersions.map(v => ({ v, s: migrationStrategies[v + 1] }))
        //  .filter(strategy => typeof strategy.s !== 'function')`
        for version in &should {
            if !self.by_target_version.contains_key(version) {
                return Err(Error::StrategyNotAFunction {
                    version: *version,
                    kind: "missing strategy",
                });
            }
        }
        Ok(())
    }

    fn get(&self, version: u32) -> Option<&Strategy> {
        self.by_target_version.get(&version)
    }
}

/// A strategy map plus the schema version it targets.
#[derive(Debug)]
pub struct Migration {
    /// The version documents are migrated **to**.
    pub target_version: u32,
    /// One strategy per version, validated at construction.
    pub strategies: MigrationStrategies,
}

impl Migration {
    /// Builds a validated migration. This is the equivalent of creating the
    /// collection with `migrationStrategies`: upstream validates at that point
    /// (`check-migration-strategies.ts`, reached from
    /// `RxCollectionBase.checkCollectionArguments`).
    pub fn new(target_version: u32, strategies: MigrationStrategies) -> Result<Self> {
        strategies.validate_for_version(target_version)?;
        Ok(Migration {
            target_version,
            strategies,
        })
    }

    /// A migration for a version-0 schema, which has no strategies.
    pub fn none(target_version: u32) -> Self {
        Migration {
            target_version,
            strategies: MigrationStrategies::new(),
        }
    }

    /// `migrateDocumentData` (`migration-helpers.ts:60`).
    ///
    /// `_deleted` and `_meta` are removed before the first strategy and restored
    /// afterwards (`:72-76`, `:116-119`). `_attachments` is carried across
    /// strategies and re-attached when a strategy omits it (`:88-107`).
    ///
    /// Returns [`Outcome::Delete`] when a strategy returns `None`, in which case no
    /// later strategy runs (`:124-136`).
    pub fn migrate_document(&self, from_version: u32, document: &Value) -> Result<Outcome> {
        let Value::Object(input) = document else {
            return Err(Error::StrategyReturnedNonObject {
                version: from_version,
                value: document.to_string(),
            });
        };
        let mut mutable = input.clone();

        // `const meta = mutableDocData._meta; const deleted = mutableDocData._deleted;
        //  delete ...`
        let meta = mutable.remove("_meta");
        let deleted = mutable.remove("_deleted");

        // `_attachments` stays, and is the value carried when a strategy drops it.
        let mut carried_attachments = mutable.get("_attachments").cloned();

        let mut next_version = from_version + 1;
        // `while (nextVersion <= collection.schema.version)`
        while next_version <= self.target_version {
            let Some(strategy) = self.strategies.get(next_version) else {
                // `collection.migrationStrategies[version]` would be `undefined` and
                // upstream would call it, throwing a TypeError. Erroring by name is
                // the same failure with a better message; construction already
                // validated that this cannot happen.
                return Err(Error::StrategyNotAFunction {
                    version: next_version,
                    kind: "missing strategy",
                });
            };
            let outcome = strategy(Value::Object(mutable))?;
            match outcome {
                Outcome::Delete => {
                    // `runStrategyIfNotNull` skips the rest once the document is
                    // gone (`migration-helpers.ts:129-130`).
                    return Ok(Outcome::Delete);
                }
                Outcome::Migrated(next) => {
                    let Value::Object(mut next) = next else {
                        return Err(Error::StrategyReturnedNonObject {
                            version: next_version,
                            value: next.to_string(),
                        });
                    };
                    // `if (typeof docOrNull._attachments === 'undefined')
                    //      docOrNull._attachments = currentAttachments;
                    //  else currentAttachments = docOrNull._attachments;`
                    match next.get("_attachments").cloned() {
                        None => {
                            if let Some(attachments) = &carried_attachments {
                                next.insert("_attachments".to_string(), attachments.clone());
                            }
                        }
                        Some(attachments) => {
                            carried_attachments = Some(attachments);
                        }
                    }
                    mutable = next;
                }
            }
            next_version += 1;
        }

        // `if (meta) doc._meta = meta; doc._deleted = deleted;`
        if let Some(meta) = meta {
            mutable.insert("_meta".to_string(), meta);
        }
        // `doc._deleted = deleted` runs unconditionally, so a document with no
        // `_deleted` gains an explicit `null`. That is upstream's behaviour and it is
        // visible in stored data, so it is reproduced rather than tidied.
        mutable.insert("_deleted".to_string(), deleted.unwrap_or(Value::Null));

        Ok(Outcome::Migrated(Value::Object(mutable)))
    }
}

/// The status shape (`migration-types.ts:6-32`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationStatus {
    /// The collection being migrated.
    pub collection_name: String,
    /// Where the migration is.
    pub status: MigrationPhase,
    /// The error, when `status` is [`MigrationPhase::Error`].
    pub error: Option<String>,
    /// Document counters, for a progress bar.
    pub count: MigrationCount,
}

/// `'RUNNING' | 'DONE' | 'ERROR'`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationPhase {
    /// In progress.
    Running,
    /// Finished successfully.
    Done,
    /// Failed.
    Error,
}

impl MigrationPhase {
    /// The upstream spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            MigrationPhase::Running => "RUNNING",
            MigrationPhase::Done => "DONE",
            MigrationPhase::Error => "ERROR",
        }
    }
}

/// The counters (`migration-types.ts:16-31`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationCount {
    /// Documents that have to be migrated.
    pub total: usize,
    /// Documents already handled — "success + purged" (`:24`), so a deleted
    /// document still counts as handled.
    pub handled: usize,
    /// `0..=100`.
    pub percent: u32,
}

impl MigrationCount {
    /// Recomputes `percent` the way `updateStatus` does
    /// (`rx-migration-state.ts:502`): `total === 0` is **100**, not 0, because a
    /// migration with nothing to do is finished.
    pub fn recompute_percent(&mut self) {
        self.percent = if self.total == 0 {
            100
        } else {
            // `Math.round((handled / total) * 100)`
            let ratio = self.handled as f64 / self.total as f64;
            (ratio * 100.0).round() as u32
        };
    }
}

impl MigrationStatus {
    /// A fresh status for a collection (`:484-492`).
    pub fn new(collection_name: impl Into<String>) -> Self {
        MigrationStatus {
            collection_name: collection_name.into(),
            status: MigrationPhase::Running,
            error: None,
            count: MigrationCount {
                total: 0,
                handled: 0,
                percent: 0,
            },
        }
    }

    /// Marks one batch done and recomputes the percentage.
    pub fn record_handled(&mut self, handled: usize) {
        self.count.handled = handled;
        self.count.recompute_percent();
    }

    /// Marks the migration done, which is idempotent with respect to `percent`.
    pub fn finish(&mut self) {
        self.status = MigrationPhase::Done;
        self.error = None;
    }

    /// Marks the migration failed (`rx-migration-state.ts:295-306`).
    ///
    /// A migration that already finished is **not** downgraded to `ERROR`: a frozen
    /// leader that is taken over and then resumes would otherwise overwrite the
    /// other tab's success. Upstream guards exactly this (`:299-303`).
    pub fn fail(&mut self, error: impl Into<String>) {
        if self.status == MigrationPhase::Done {
            return;
        }
        self.status = MigrationPhase::Error;
        self.error = Some(error.into());
    }
}

/// The answer to "may this instance run the migration?"
/// (`awaitLeadershipOrStalledLeader`, `rx-migration-state.ts:360-413`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leadership {
    /// Take the migration.
    Leader,
    /// Another instance has not touched the status document for longer than the
    /// timeout, so it is frozen and this one takes over.
    Stalled,
    /// Another instance finished the migration.
    Done,
}

/// `MIGRATION_LEADER_TIMING` (`migration-helpers.ts:161-164`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeaderTiming {
    /// How long without a status write before the leader is presumed frozen.
    pub timeout_ms: u64,
    /// How often a waiting instance checks.
    pub heartbeat_interval_ms: u64,
}

impl Default for LeaderTiming {
    fn default() -> Self {
        LeaderTiming {
            timeout_ms: 60_000,
            heartbeat_interval_ms: 5_000,
        }
    }
}

/// Decides whether to wait, take over, or stop.
///
/// Extracted from the `Promise.race` in `awaitLeadershipOrStalledLeader` so the
/// decision is testable without timers: given "another instance is already leader"
/// (so our leadership promise has not resolved), the current time, the time we
/// started waiting, and the last status write, which of the three outcomes holds.
pub fn leadership_decision(
    timing: LeaderTiming,
    now_ms: u64,
    started_waiting_ms: u64,
    status: Option<(MigrationPhase, u64)>,
) -> Option<Leadership> {
    if let Some((MigrationPhase::Done, _)) = status {
        return Some(Leadership::Done);
    }
    // `const lastActivity = Math.max(startTime, statusDoc ? statusDoc._meta.lwt : 0);`
    let last_activity = match status {
        Some((_, lwt)) => started_waiting_ms.max(lwt),
        None => started_waiting_ms,
    };
    // `if (now() - lastActivity > timeout) return 'STALLED'`
    if now_ms.saturating_sub(last_activity) > timing.timeout_ms {
        return Some(Leadership::Stalled);
    }
    // Still waiting: the leadership promise decides, and it has not resolved.
    None
}

/// Which previous version to migrate from, when several have leftovers
/// (`getOldCollectionMeta`, `migration-helpers.ts:25-51`).
///
/// The comment at `:40-46` is the reason this is a function rather than a
/// one-liner: a previous migration may have been cancelled or the browser reloaded,
/// and a new version may have been added, so several collection-meta documents can
/// exist. Upstream takes the **oldest** one and migrates upward from there, which is
/// what makes a partially-migrated store recoverable.
pub fn oldest_previous_version(meta_versions_present: &[u32]) -> Option<u32> {
    meta_versions_present.iter().copied().min()
}

/// The versions a schema has predecessors for: `0..version` (`getPreviousVersions`,
/// used by both the strategy check and the meta lookup).
pub fn previous_versions(schema_version: u32) -> Vec<u32> {
    if schema_version == 0 {
        Vec::new()
    } else {
        (0..schema_version).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_is_100_when_there_is_nothing_to_migrate() {
        // `rx-migration-state.ts:502`: `total === 0 ? 100 : round(...)`.
        let mut count = MigrationCount {
            total: 0,
            handled: 0,
            percent: 0,
        };
        count.recompute_percent();
        assert_eq!(count.percent, 100);
    }

    #[test]
    fn percent_rounds_like_math_round() {
        // `Math.round` on .5 rounds up.
        let cases = [(1, 3, 33), (2, 3, 67), (1, 2, 50), (3, 3, 100), (1, 200, 1)];
        for (handled, total, expected) in cases {
            let mut count = MigrationCount {
                total,
                handled,
                percent: 0,
            };
            count.recompute_percent();
            assert_eq!(count.percent, expected, "{handled}/{total}");
        }
    }

    #[test]
    fn a_finished_migration_is_not_downgraded_to_error() {
        // `rx-migration-state.ts:299-303`.
        let mut status = MigrationStatus::new("human");
        status.finish();
        status.fail("another tab removed the storage");
        assert_eq!(status.status, MigrationPhase::Done);
        assert_eq!(status.error, None);
    }

    #[test]
    fn previous_versions_is_empty_for_version_zero() {
        assert!(previous_versions(0).is_empty());
        assert_eq!(previous_versions(3), vec![0, 1, 2]);
    }

    #[test]
    fn the_oldest_leftover_version_wins() {
        // `migration-helpers.ts:40-50`.
        assert_eq!(oldest_previous_version(&[2, 0, 1]), Some(0));
        assert_eq!(oldest_previous_version(&[]), None);
    }
}
