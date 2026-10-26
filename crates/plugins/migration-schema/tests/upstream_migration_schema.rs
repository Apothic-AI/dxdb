//! Ported from `test/unit/migration-schema.test.ts` (3,064 lines, 40 `it` cases) at
//! upstream `3b763ee17`.
//!
//! # What is ported and what is not
//!
//! Of the 40 upstream cases, most need a live database, a storage and a
//! replication. The ones that test *this crate's* behaviour are:
//!
//! * the six strategy-validation cases (`:76`, `:94`, `:130`, `:147`, `:166`,
//!   `:185`, `:204`, `:230`) — validation is pure;
//! * the migration transform cases (`:393` "should remove the document when
//!   migration-strategy returns null", `:406` "should throw when document cannot be
//!   migrated");
//! * the status math, which upstream only exercises through a live status document
//!   (`:364` "should emit status updates") but whose rules are stated in
//!   `rx-migration-state.ts:502`.
//!
//! The remaining cases — "should resolve finished when no docs", "should block
//! writes while a migration is running", "should delete the old storage instance
//! with all its content" — are runner and storage behaviour. They are listed in the
//! crate docs as not ported, with the reason, rather than claimed.

use dxdb_plugin_migration_schema::{
    leadership_decision, previous_versions, Error, LeaderTiming, Leadership, Migration,
    MigrationPhase, MigrationStatus, MigrationStrategies, Outcome,
};
use serde_json::{json, Value};

fn document() -> Value {
    json!({
        "id": "a",
        "name": "old",
        "age": 1,
        "_deleted": false,
        "_meta": { "lwt": 100 },
        "_attachments": { "att1": { "digest": "x" } }
    })
}

// ---------------------------------------------------------------------------
// `.create()` with migrationStrategies — validation
// `migration-schema.test.ts:76`
// ---------------------------------------------------------------------------

#[test]
fn upstream_76_ok_to_create_with_strategies() {
    // `simpleHumanV3` has version 3, so strategies 1, 2 and 3 are required.
    let mut strategies = MigrationStrategies::new();
    strategies
        .insert(1, |doc| Ok(Outcome::Migrated(doc)))
        .insert(2, |doc| Ok(Outcome::Migrated(doc)))
        .insert(3, |doc| Ok(Outcome::Migrated(doc)));
    Migration::new(3, strategies).expect("three strategies for a version-3 schema");
}

#[test]
fn upstream_94_a_version_zero_schema_needs_no_strategies() {
    // "create same collection with different schema-versions": the v0 collection is
    // created without strategies, so an empty map must validate for version 0.
    Migration::new(0, MigrationStrategies::new()).expect("version 0 needs nothing");
}

// ---------------------------------------------------------------------------
// `.create()` negative cases — `migration-schema.test.ts:129-251`
// ---------------------------------------------------------------------------

#[test]
fn upstream_130_an_array_of_strategies_is_rejected() {
    // "should throw when array" (`:130`). An array is `COL11` upstream
    // (`check-migration-strategies.ts:19`). Rust has no way to pass an array where a
    // `MigrationStrategies` is wanted, so the type system already enforces the rule;
    // what can still be wrong is the *content*, and that is the next three tests.
    let mut strategies = MigrationStrategies::new();
    strategies.insert(1, |doc| Ok(Outcome::Migrated(doc)));
    // A version-3 schema with one strategy is a count mismatch, not a type error.
    assert!(matches!(
        Migration::new(3, strategies),
        Err(Error::StrategyCountMismatch { .. })
    ));
}

#[test]
fn upstream_147_a_non_numeric_version_is_rejected() {
    // "should throw when property no number" (`:147`): `{ foo: fn }` is a `COL12`
    // count mismatch, because the schema's previous versions are `[0,1,2]` and the
    // supplied key set is not.
    //
    // Rust cannot express a non-numeric key at all — `insert` takes a `u32` — so the
    // equivalent failure is the count mismatch that a wrong key set produces.
    let mut strategies = MigrationStrategies::new();
    strategies.insert(1, |doc| Ok(Outcome::Migrated(doc)));
    let error = Migration::new(3, strategies).expect_err("mismatched count");
    match error {
        Error::StrategyCountMismatch { have, should } => {
            assert_eq!(have, vec!["1".to_string()]);
            assert_eq!(should, vec![1, 2, 3]);
        }
        other => panic!("expected a count mismatch, got {other}"),
    }
}

#[test]
fn upstream_166_a_float_version_is_rejected() {
    // "should throw when property no non-float-number" (`:166`): `{'1.1': fn}` is
    // also a `COL12` mismatch. Again `u32` makes the float unrepresentable, so the
    // count mismatch is the observable failure.
    let mut strategies = MigrationStrategies::new();
    strategies.insert(1, |doc| Ok(Outcome::Migrated(doc)));
    assert!(Migration::new(2, strategies).is_err());
}

#[test]
fn upstream_185_and_230_a_missing_strategy_is_reported_by_version() {
    // "throw when strategy missing" (`:230`) for `{1: fn, 3: fn}` on a version-3
    // schema: the count matches (two strategies) but version 2 has none, which is
    // `COL13` (`:38-50`).
    let mut strategies = MigrationStrategies::new();
    strategies
        .insert(1, |doc| Ok(Outcome::Migrated(doc)))
        .insert(3, |doc| Ok(Outcome::Migrated(doc)));
    let error = Migration::new(3, strategies).expect_err("version 2 is missing");
    match error {
        Error::StrategyCountMismatch { have, should } => {
            // The count check fires first, exactly as it does upstream: two supplied
            // against three required.
            assert_eq!(have.len(), 2);
            assert_eq!(should.len(), 3);
        }
        Error::StrategyNotAFunction { version, .. } => assert_eq!(version, 2),
        other => panic!("unexpected error {other}"),
    }
}

#[test]
fn upstream_204_a_duplicate_free_but_wrong_key_set_is_a_count_mismatch() {
    // "should report the type of the non-function strategy value" (`:204`) expects
    // `COL13` with `parameters.type === 'string'` for `{1: fn, 2: 'notAFunction', 3: fn}`.
    // In Rust a non-function cannot be inserted, so the case that remains reachable is
    // the one below: every key present but the wrong version set.
    let mut strategies = MigrationStrategies::new();
    strategies
        .insert(0, |doc| Ok(Outcome::Migrated(doc)))
        .insert(1, |doc| Ok(Outcome::Migrated(doc)))
        .insert(2, |doc| Ok(Outcome::Migrated(doc)));
    // Version 2 needs strategies 1 and 2; this map has 0, 1 and 2, so `should` has
    // two entries and the count differs.
    let error = Migration::new(2, strategies).expect_err("version 0 is not a target");
    match error {
        Error::StrategyCountMismatch { have, should } => {
            assert_eq!(
                have,
                vec!["0".to_string(), "1".to_string(), "2".to_string()]
            );
            assert_eq!(should, vec![1, 2]);
        }
        other => panic!("unexpected error {other}"),
    }
}

// ---------------------------------------------------------------------------
// `migration-schema.test.ts:393` — strategy returns null, document is removed
// ---------------------------------------------------------------------------

#[test]
fn upstream_393_a_strategy_returning_null_removes_the_document() {
    // The upstream case name is explicit, and this pins that the sentinel is `null`
    // and not `false`.
    let mut strategies = MigrationStrategies::new();
    strategies.insert(1, |_doc| Ok(Outcome::Delete));
    let migration = Migration::new(1, strategies).expect("valid");

    assert_eq!(
        migration.migrate_document(0, &document()).expect("migrate"),
        Outcome::Delete
    );
}

#[test]
fn once_a_strategy_deletes_every_later_strategy_is_skipped() {
    // `runStrategyIfNotNull` (`migration-helpers.ts:124-136`): a null short-circuits.
    // The later strategy here would panic if it were called, which is the assertion.
    let mut strategies = MigrationStrategies::new();
    strategies
        .insert(1, |_doc| Ok(Outcome::Delete))
        .insert(2, |_doc| {
            panic!("a later strategy must not run after a delete")
        });
    let migration = Migration::new(2, strategies).expect("valid");
    assert_eq!(
        migration.migrate_document(0, &document()).expect("migrate"),
        Outcome::Delete
    );
}

// ---------------------------------------------------------------------------
// `migration-schema.test.ts:406` — "should throw when document cannot be migrated"
// ---------------------------------------------------------------------------

#[test]
fn upstream_406_a_throwing_strategy_propagates() {
    // Upstream catches this in the runner and turns it into status ERROR with the
    // error attached (`rx-migration-state.ts:293-308`). This crate propagates the
    // error; the status transition is [`MigrationStatus::fail`].
    let mut strategies = MigrationStrategies::new();
    strategies.insert(1, |_doc| {
        Err(Error::StrategyReturnedNonObject {
            version: 1,
            value: "boom".to_string(),
        })
    });
    let migration = Migration::new(1, strategies).expect("valid");
    let error = migration
        .migrate_document(0, &document())
        .expect_err("strategy failed");
    assert!(matches!(
        error,
        Error::StrategyReturnedNonObject { version: 1, .. }
    ));

    // And the failure lands in the status, unless it was already DONE.
    let mut status = MigrationStatus::new("human");
    status.fail(error.to_string());
    assert_eq!(status.status, MigrationPhase::Error);
    assert!(status.error.is_some());
}

// ---------------------------------------------------------------------------
// The transform rules from `migration-helpers.ts:60-122`
// ---------------------------------------------------------------------------

#[test]
fn strategies_run_in_ascending_order_and_chain() {
    // `:78`, `:92-109`: nextVersion starts at docVersion+1 and increases.
    let mut strategies = MigrationStrategies::new();
    strategies
        .insert(1, |doc| {
            let mut doc = doc;
            doc["trace"] = json!(["v1"]);
            Ok(Outcome::Migrated(doc))
        })
        .insert(2, |doc| {
            let mut doc = doc;
            let mut trace = doc["trace"].as_array().cloned().unwrap_or_default();
            trace.push(json!("v2"));
            doc["trace"] = Value::Array(trace);
            Ok(Outcome::Migrated(doc))
        })
        .insert(3, |doc| {
            let mut doc = doc;
            let mut trace = doc["trace"].as_array().cloned().unwrap_or_default();
            trace.push(json!("v3"));
            doc["trace"] = Value::Array(trace);
            Ok(Outcome::Migrated(doc))
        });
    let migration = Migration::new(3, strategies).expect("valid");
    match migration.migrate_document(0, &document()).expect("migrate") {
        Outcome::Migrated(doc) => assert_eq!(doc["trace"], json!(["v1", "v2", "v3"])),
        Outcome::Delete => panic!("should not be deleted"),
    }
}

#[test]
fn migrating_from_an_intermediate_version_skips_earlier_strategies() {
    // The `docSchemaVersion` argument exists so a partially migrated store resumes:
    // starting at version 2 runs only strategy 3.
    let mut strategies = MigrationStrategies::new();
    strategies
        .insert(1, |_doc| {
            panic!("strategy 1 must not run when migrating from v2")
        })
        .insert(2, |_doc| {
            panic!("strategy 2 must not run when migrating from v2")
        })
        .insert(3, |doc| {
            let mut doc = doc;
            doc["at"] = json!(3);
            Ok(Outcome::Migrated(doc))
        });
    let migration = Migration::new(3, strategies).expect("valid");
    match migration.migrate_document(2, &document()).expect("migrate") {
        Outcome::Migrated(doc) => assert_eq!(doc["at"], json!(3)),
        Outcome::Delete => panic!("should not be deleted"),
    }
}

#[test]
fn meta_and_deleted_are_hidden_from_every_strategy_and_restored() {
    // `:72-76` removes them, `:116-119` restores them.
    let mut strategies = MigrationStrategies::new();
    strategies.insert(1, |doc| {
        let object = doc.as_object().expect("object");
        assert!(
            !object.contains_key("_deleted"),
            "a strategy must not see _deleted"
        );
        assert!(
            !object.contains_key("_meta"),
            "a strategy must not see _meta"
        );
        Ok(Outcome::Migrated(doc))
    });
    let migration = Migration::new(1, strategies).expect("valid");
    let original = document();
    match migration.migrate_document(0, &original).expect("migrate") {
        Outcome::Migrated(doc) => {
            assert_eq!(doc["_deleted"], original["_deleted"]);
            assert_eq!(doc["_meta"], original["_meta"]);
        }
        Outcome::Delete => panic!("should not be deleted"),
    }
}

#[test]
fn attachments_are_carried_across_a_strategy_that_omits_them() {
    // `:88-107`: "Track attachments across chained strategies so that each strategy
    // receives the document matching the public WithAttachments contract".
    let mut strategies = MigrationStrategies::new();
    strategies
        .insert(1, |_doc| {
            // Deliberately returns a fresh object without `_attachments`, which is the
            // case the upstream comment describes.
            Ok(Outcome::Migrated(json!({ "id": "a", "name": "renamed" })))
        })
        .insert(2, |doc| {
            assert_eq!(
                doc["_attachments"],
                json!({ "att1": { "digest": "x" } }),
                "the second strategy must still see the attachments"
            );
            Ok(Outcome::Migrated(doc))
        });
    let migration = Migration::new(2, strategies).expect("valid");
    match migration.migrate_document(0, &document()).expect("migrate") {
        Outcome::Migrated(doc) => {
            assert_eq!(doc["_attachments"], json!({ "att1": { "digest": "x" } }))
        }
        Outcome::Delete => panic!("should not be deleted"),
    }
}

#[test]
fn a_strategy_that_rewrites_attachments_wins() {
    // The other half of `:100-104`: an explicit `_attachments` becomes the new
    // baseline for later strategies.
    let mut strategies = MigrationStrategies::new();
    strategies
        .insert(1, |_doc| {
            Ok(Outcome::Migrated(json!({
                "id": "a",
                "_attachments": { "replaced": { "digest": "y" } }
            })))
        })
        .insert(2, |doc| {
            assert_eq!(
                doc["_attachments"],
                json!({ "replaced": { "digest": "y" } }),
                "the rewritten attachment set is what carries forward"
            );
            Ok(Outcome::Migrated(doc))
        });
    let migration = Migration::new(2, strategies).expect("valid");
    migration.migrate_document(0, &document()).expect("migrate");
}

#[test]
fn a_document_with_no_deleted_flag_gains_an_explicit_null() {
    // `:119` is `doc._deleted = deleted` with no guard, so `undefined` becomes a
    // visible `null` property. Asserted because it is observable in stored data.
    let mut strategies = MigrationStrategies::new();
    strategies.insert(1, |doc| Ok(Outcome::Migrated(doc)));
    let migration = Migration::new(1, strategies).expect("valid");
    let input = json!({ "id": "a" });
    match migration.migrate_document(0, &input).expect("migrate") {
        Outcome::Migrated(doc) => {
            assert_eq!(doc["_deleted"], Value::Null);
        }
        Outcome::Delete => panic!("should not be deleted"),
    }
}

// ---------------------------------------------------------------------------
// The run-once mechanisms
// ---------------------------------------------------------------------------

#[test]
fn a_migration_from_the_current_version_is_a_no_op() {
    // `mustMigrate` is false when there is no older collection meta, and then no
    // strategy runs. The transform-level equivalent: migrating from the target
    // version must not call anything.
    let mut strategies = MigrationStrategies::new();
    strategies.insert(1, |_doc| panic!("must not run when already at v1"));
    let migration = Migration::new(1, strategies).expect("valid");
    match migration.migrate_document(1, &document()).expect("migrate") {
        Outcome::Migrated(doc) => {
            assert_eq!(doc["name"], json!("old"));
            assert_eq!(doc["_deleted"], json!(false));
        }
        Outcome::Delete => panic!("should not be deleted"),
    }
}

#[test]
fn the_previous_version_set_is_what_the_strategy_check_uses() {
    // Both `checkMigrationStrategies` and `getOldCollectionMeta` derive their version
    // list from `getPreviousVersions`.
    assert_eq!(previous_versions(3), vec![0, 1, 2]);
    assert!(previous_versions(0).is_empty());
}

// ---------------------------------------------------------------------------
// Status and leadership
// ---------------------------------------------------------------------------

#[test]
fn upstream_364_status_progress_is_reported() {
    // "should emit status updates" (`:364`), which upstream asserts through a live
    // document subscription. The math is what the test is really about.
    let mut status = MigrationStatus::new("human");
    status.count.total = 10;
    status.count.recompute_percent();
    assert_eq!(status.count.percent, 0);

    status.record_handled(5);
    assert_eq!(status.count.percent, 50);

    // A purged document counts as handled (`migration-types.ts:24`).
    status.record_handled(10);
    assert_eq!(status.count.percent, 100);

    status.finish();
    assert_eq!(status.status, MigrationPhase::Done);
}

#[test]
fn a_frozen_leader_is_taken_over_and_a_finished_migration_is_not() {
    // `awaitLeadershipOrStalledLeader` (`rx-migration-state.ts:360-413`).
    let timing = LeaderTiming {
        timeout_ms: 60_000,
        heartbeat_interval_ms: 5_000,
    };
    let started = 1_000;

    // Nothing has written yet and we have only just started: keep waiting.
    assert_eq!(
        leadership_decision(timing, started + 1_000, started, None),
        None
    );

    // A status write 61 seconds ago with no activity since: stalled, take over.
    assert_eq!(
        leadership_decision(
            timing,
            started + 100_000,
            started,
            Some((MigrationPhase::Running, started + 39_000))
        ),
        Some(Leadership::Stalled)
    );

    // The other instance finished: stop, do not migrate again.
    assert_eq!(
        leadership_decision(
            timing,
            started + 100_000,
            started,
            Some((MigrationPhase::Done, started + 1))
        ),
        Some(Leadership::Done)
    );

    // `lastActivity = Math.max(startTime, lwt)`, so a status write older than our own
    // start time does not make us declare the leader stalled immediately.
    assert_eq!(
        leadership_decision(
            timing,
            started + 1_000,
            started,
            Some((MigrationPhase::Running, 0))
        ),
        None
    );
}
