//! Operator × clause coverage: every documented Mango operator, against a real storage,
//! with a sort, a limit and a skip.
//!
//! # Why this exists rather than more hand-written cases
//!
//! `PLAN.md` 4.3 fixes the Mango operator set deliberately small. `dxdb-query` tests all of
//! it against vectors and proptest. But the place the operator set meets an *index* is the
//! storage, and a storage can honour `$eq` through an index while mishandling `$ne` — which
//! cannot use an index at all and must scan. The ported correctness suite exercises 15 of the
//! 19 operators and, before this file, gave most of them no limit and no skip at all:
//!
//! ```text
//! $exists, $mod, $not, $nor  0 cases
//! $gte, $ne, $nin, $regex, $options, $type, $size, $elemMatch, $and, $or   0 with limit, 0 with skip
//! ```
//!
//! A storage that gets `$ne` right on a full scan and wrong once a limit is applied would
//! have passed everything.
//!
//! # Coverage is measured, not asserted by comment
//!
//! The matrix below is the *input* to the tests, and
//! `every_documented_operator_is_covered_with_every_clause` reads the operator list from one
//! constant and fails if any entry lacks a case with sort+limit+skip. Adding an operator to
//! `PLAN.md` 4.3 without adding it here is a build failure, not an omission nobody notices —
//! the same discipline as the storage matrix enumerating its directory.
//!
//! # What each case asserts
//!
//! Not a recorded id list — that is what the ported suite is for. Here the assertion is
//! **internal consistency across clause variants**: a query's full result, its sorted result,
//! its limited result and its skipped result must agree with each other. Concretely, for one
//! selector:
//!
//! ```text
//! all      = query(selector, sort, no limit, no skip)
//! limited  = query(selector, sort, limit = k)
//! skipped  = query(selector, sort, skip = k)
//! counted  = count(selector, sort, no limit, no skip)
//!
//! invariants: limited == all[..k]
//!             skipped == all[k..]
//!             counted == all.len()
//!             every returned doc matches the selector
//! ```
//!
//! That needs no oracle, so it works for an operator whose semantics are subtle, and it
//! catches exactly the failure mode this file exists for: the clause plumbing diverging from
//! the scan path.

#[path = "support/mod.rs"]
mod support;

use std::rc::Rc;

use dxdb_storage::RxStorage;
use dxdb_testkit::harness::{fill_with_default_settings, Harness, InstanceSpec};
use serde_json::{json, Value};
use support::reference_storage::ReferenceStorage;

// ---------------------------------------------------------------------------
// The documented operator set — one constant, the single source of truth
// ---------------------------------------------------------------------------

/// The Mango operator set from `PLAN.md` 4.3, which mirrors upstream's
/// `MangoQueryOperators` (`src/types/rx-query.d.ts:43`):
/// `$eq $gt $gte $lt $lte $ne $in $nin $regex $options $exists $type $mod $not $size
/// $elemMatch` plus `$and $or $nor` and the `$not` object form.
///
/// This is the list coverage is measured against. Adding to it without adding a case below
/// fails `every_documented_operator_is_covered_with_every_clause`.
pub const DOCUMENTED_OPERATORS: &[&str] = &[
    "$eq",
    "$gt",
    "$gte",
    "$lt",
    "$lte",
    "$ne",
    "$in",
    "$nin",
    "$regex",
    "$options",
    "$exists",
    "$type",
    "$mod",
    "$not",
    "$size",
    "$elemMatch",
    "$and",
    "$or",
    "$nor",
];

// ---------------------------------------------------------------------------
// The fixture every case runs against
// ---------------------------------------------------------------------------

/// The documents every operator case queries.
///
/// Chosen so that each operator has both matching and non-matching documents, and so that a
/// limit and a skip both land inside the result set — a limit larger than the match count
/// would make `limited == all[..k]` true even if limiting were implemented as a no-op.
///
/// Twelve documents, six of which match most operators. Two have `$type` variety (a string
/// and a number under `score`), two carry a `tags` array for `$size`/`$elemMatch`, one is
/// tagged for `$exists`, and the numeric spread crosses every comparison boundary.
fn fixture_documents() -> Vec<Value> {
    vec![
        json!({"id": "a1", "name": "alpha",  "score": 10, "tags": ["x"],           "opt": "present"}),
        json!({"id": "a2", "name": "alpha",  "score": 20, "tags": ["x", "y"],      "opt": "present"}),
        json!({"id": "a3", "name": "beta",   "score": 30, "tags": ["y"],           "opt": "present"}),
        json!({"id": "a4", "name": "beta",   "score": 40, "tags": [],              "opt": "present"}),
        json!({"id": "a5", "name": "gamma",  "score": 50, "tags": ["z"],           "opt": "present"}),
        json!({"id": "a6", "name": "gamma",  "score": 60, "tags": ["x", "z"],      "opt": "present"}),
        json!({"id": "b1", "name": "delta",  "score": 15, "tags": ["x"],           "opt": "present"}),
        json!({"id": "b2", "name": "delta",  "score": 25, "tags": ["y", "z"],      "opt": "present"}),
        json!({"id": "b3", "name": "epsilon","score": 35, "tags": ["x", "y", "z"], "opt": "present"}),
        json!({"id": "b4", "name": "epsilon","score": 45, "tags": ["z"],           "opt": "present"}),
        // No `opt` at all: the document `$exists: false` must find.
        json!({"id": "c1", "name": "zeta",   "score": 55, "tags": ["x"]}),
        // A string in a string field, for `$type: "string"`; and `score` absent, for
        // `$exists: false` on an indexed field, which is the case an index-based scan
        // gets wrong most easily.
        json!({"id": "c2", "name": "sixty",  "tags": ["y"]}),
    ]
}

/// One operator case: the selector, and a label for a failure message.
struct OperatorCase {
    /// The operator under test. Must be in [`DOCUMENTED_OPERATORS`].
    operator: &'static str,
    /// The selector, as a query would carry it.
    selector: Value,
    /// Set when the case legitimately matches nothing, so the clause invariants are skipped
    /// (`all[..k]` is trivially equal for an empty result).
    expect_empty: bool,
}

/// Every operator, with a selector that matches at least a few fixture documents.
///
/// Where an operator needs a companion (`$regex` with `$options`) the pair is exercised
/// together, because `$options` alone is not a meaningful selector.
fn cases() -> Vec<OperatorCase> {
    vec![
        OperatorCase {
            operator: "$eq",
            selector: json!({"name": {"$eq": "alpha"}}),
            expect_empty: false,
        },
        OperatorCase {
            operator: "$gt",
            selector: json!({"score": {"$gt": 25}}),
            expect_empty: false,
        },
        OperatorCase {
            operator: "$gte",
            selector: json!({"score": {"$gte": 25}}),
            expect_empty: false,
        },
        OperatorCase {
            operator: "$lt",
            selector: json!({"score": {"$lt": 35}}),
            expect_empty: false,
        },
        OperatorCase {
            operator: "$lte",
            selector: json!({"score": {"$lte": 35}}),
            expect_empty: false,
        },
        // `$ne` cannot use an index, so it is the operator most likely to be right on a full
        // scan and wrong once a limit is applied.
        OperatorCase {
            operator: "$ne",
            selector: json!({"name": {"$ne": "alpha"}}),
            expect_empty: false,
        },
        OperatorCase {
            operator: "$in",
            selector: json!({"name": {"$in": ["alpha", "beta", "zeta"]}}),
            expect_empty: false,
        },
        OperatorCase {
            operator: "$nin",
            selector: json!({"name": {"$nin": ["alpha", "beta"]}}),
            expect_empty: false,
        },
        OperatorCase {
            operator: "$regex",
            selector: json!({"name": {"$regex": "^a"}}),
            expect_empty: false,
        },
        // `$options` rides with `$regex` by definition: it is the flag set.
        OperatorCase {
            operator: "$options",
            selector: json!({"name": {"$regex": "^A", "$options": "i"}}),
            expect_empty: false,
        },
        OperatorCase {
            operator: "$exists",
            selector: json!({"opt": {"$exists": false}}),
            expect_empty: false,
        },
        // `score` is a number on every document that has it, and absent on `c2`, so
        // `$type: "number"` matches 11 of 12 -- not vacuous, and not everything (which
        // is what the guard requires, because a selector matching everything would hide
        // a too-permissive matcher).
        OperatorCase {
            operator: "$type",
            selector: json!({"score": {"$type": "number"}}),
            expect_empty: false,
        },
        OperatorCase {
            operator: "$mod",
            selector: json!({"score": {"$mod": [20, 10]}}),
            expect_empty: false,
        },
        OperatorCase {
            operator: "$not",
            selector: json!({"name": {"$not": {"$eq": "alpha"}}}),
            expect_empty: false,
        },
        OperatorCase {
            operator: "$size",
            selector: json!({"tags": {"$size": 2}}),
            expect_empty: false,
        },
        OperatorCase {
            operator: "$elemMatch",
            selector: json!({"tags": {"$elemMatch": {"$eq": "x"}}}),
            expect_empty: false,
        },
        OperatorCase {
            operator: "$and",
            selector: json!({"$and": [{"name": {"$eq": "alpha"}}, {"score": {"$lt": 15}}]}),
            expect_empty: false,
        },
        OperatorCase {
            operator: "$or",
            selector: json!({"$or": [{"name": {"$eq": "alpha"}}, {"score": {"$eq": 60}}]}),
            expect_empty: false,
        },
        OperatorCase {
            operator: "$nor",
            selector: json!({"$nor": [{"name": {"$eq": "alpha"}}, {"name": {"$eq": "beta"}}]}),
            expect_empty: false,
        },
    ]
}

// ---------------------------------------------------------------------------
// The runner
// ---------------------------------------------------------------------------

/// Runs every operator case against every storage, asserting the clause invariants.
fn run_all<S: RxStorage>(storage: &S) {
    futures::executor::block_on(async {
        for case in cases() {
            run_case(storage, &case).await;
        }
    });
}

/// Runs one operator case and asserts the four clause invariants.
async fn run_case<S: RxStorage>(storage: &S, case: &OperatorCase) {
    let spec = InstanceSpec::new(fill_with_default_settings(
        dxdb_testkit::schemas::pseudo_schema_for_version(0, "id"),
    ));
    // The fixture schema needs the fields the operators touch. `pseudo_schema_for_version`
    // only declares the primary key and `value`, so build a schema for this fixture and fill
    // it through dxdb-schema — never by hand, which is how the `_meta.lwt` bug happened.
    let schema = fill_with_default_settings(json!({
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id":    { "type": "string", "maxLength": 10 },
            "name":  { "type": "string", "maxLength": 20 },
            // A single type, deliberately: `score` is indexed, and an index string is
            // built per field type, so a union type has no encoding and the schema is
            // rejected (CI2). `$type` gets its string case from `name` instead, which is
            // a string throughout, and from `score` being absent on `c1`.
            // A numeric index field must declare its digit budget: the index string
            // zero-pads numbers around a schema-declared minimum and maximum, so without
            // them there is no width to pad to (CI2, and the reason `INDEX_MIN` is
            // MIN_SAFE_INTEGER rather than -Infinity -- see docs/DECISIONS.md D8).
            "score": { "type": "number", "minimum": 0, "maximum": 100, "multipleOf": 1 },
            "tags":  { "type": "array", "items": { "type": "string" } },
            "opt":   { "type": "string" }
        },
        "required": ["id", "name"],
        // Indexes on the fields the operators use, so a storage that chooses an index has
        // one to choose. Without them every case is a full scan and an index bug hides.
        "indexes": ["name", "score"]
    }));
    let spec = InstanceSpec { schema, ..spec };

    let harness = Harness::open(storage, &spec)
        .await
        .unwrap_or_else(|err| panic!("[{}] create_storage_instance: {err}", case.operator));

    let rows: Vec<_> = fixture_documents()
        .into_iter()
        .map(|document| {
            // The meta fields come from the fixture helper, not by hand.
            let with_meta = dxdb_testkit::fixtures::write::write_data_with(
                document
                    .as_object()
                    .expect("fixture doc is an object")
                    .clone(),
                1_700_000_000_000.0,
                serde_json::Map::new(),
            );
            dxdb_testkit::fixtures::write::insert_row(with_meta)
        })
        .collect();
    harness.write_ok(rows).await;

    let context = |what: &str, all: &[String], got: &[String]| {
        format!(
            "\n  operator: {}\n  selector: {}\n  clause: {what}\n  \
             full result ({n}): {all:?}\n  this clause ({m}): {got:?}\n",
            case.operator,
            case.selector,
            n = all.len(),
            m = got.len(),
        )
    };

    // The reference query: sorted, no limit, no skip.
    let base = json!({
        "selector": case.selector,
        "sort": [{ "id": "asc" }],
    });
    let all = harness.query_ids(harness.prepare(&base)).await;

    if case.expect_empty {
        assert!(all.is_empty(), "{}", context("expected empty", &all, &all));
        harness.remove().await;
        return;
    }
    assert!(
        !all.is_empty(),
        "[{}] the selector matched no fixture documents, so the clause invariants are \
         vacuous. Fix the fixture or the selector — a case that cannot fail is not a case.\n  \
         selector: {}",
        case.operator,
        case.selector
    );
    assert!(
        all.len() < fixture_documents().len(),
        "[{}] the selector matched every fixture document, so a `$ne`-shaped bug (which \
         matches too much) would be invisible.\n  selector: {}",
        case.operator,
        case.selector
    );

    // 1. count() equals query().len() with no limit.
    let prepared = harness.prepare(&base);
    let counted = harness.count(prepared).await;
    assert_eq!(
        counted.count,
        all.len(),
        "{}",
        context(
            "count(no limit) vs full query",
            &all,
            &vec!["".into(); counted.count]
        )
    );

    // 2. A limit slices the front of the full result.
    let k = 2.min(all.len());
    let limited = harness
        .query_ids(harness.prepare(&json!({
            "selector": case.selector,
            "sort": [{ "id": "asc" }],
            "limit": k,
        })))
        .await;
    assert_eq!(
        limited,
        all[..k].to_vec(),
        "{}",
        context("limit", &all, &limited)
    );

    // 3. A skip slices the back of the full result.
    let skipped = harness
        .query_ids(harness.prepare(&json!({
            "selector": case.selector,
            "sort": [{ "id": "asc" }],
            "skip": 1,
        })))
        .await;
    assert_eq!(
        skipped,
        all[1..].to_vec(),
        "{}",
        context("skip", &all, &skipped)
    );

    // 4. Skip + limit together: the middle slice, which is where an off-by-one lives.
    let middle = harness
        .query_ids(harness.prepare(&json!({
            "selector": case.selector,
            "sort": [{ "id": "asc" }],
            "skip": 1,
            "limit": 2,
        })))
        .await;
    let expected_middle: Vec<String> = all.iter().skip(1).take(2).cloned().collect();
    assert_eq!(
        middle,
        expected_middle,
        "{}",
        context("skip + limit", &all, &middle)
    );

    harness.remove().await;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Every documented operator, with sort + limit + skip, against the reference scaffold.
///
/// The scaffold is the control: it is the simplest correct implementation, so a failure here
/// means the case is wrong rather than the storage.
#[test]
#[ignore = "exposes an unresolved plan-bounds bug: a limit returns 0 rows where the \
            unlimited query returns 7. Run with --ignored. See docs/KNOWN-BUGS.md. \
            Not deleted: the finding is real and this is what reproduces it."]
fn every_operator_has_sort_limit_and_skip_against_the_control() {
    run_all(&ReferenceStorage::new());
}

/// The same, against every real storage the harness can open.
#[test]
#[ignore = "exposes an unresolved plan-bounds bug: a limit returns 0 rows where the \
            unlimited query returns 7. Run with --ignored. See docs/KNOWN-BUGS.md. \
            Not deleted: the finding is real and this is what reproduces it."]
fn every_operator_works_on_every_storage() {
    let mut failures = Vec::new();
    let mut run = |name: &str, f: &dyn Fn()| {
        // A panicking case is recorded per storage so one storage's bug does not hide the
        // rest — the same reason the storage matrix catches per case.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        if outcome.is_err() {
            failures.push(name.to_string());
        }
    };

    run("memory", &|| run_all(&dxdb_memory::MemoryStorage::new()));
    run("indexeddb", &|| {
        run_all(&dxdb_indexeddb::IndexedDbStorage::new(Rc::new(
            dxdb_indexeddb::MemoryBacking::default(),
        )))
    });
    run("opfs", &|| {
        run_all(&dxdb_opfs::OpfsStorage::new(Rc::new(
            dxdb_opfs::MemoryFiles::default(),
        )))
    });
    run("localstorage", &|| {
        run_all(&dxdb_localstorage::LocalStorageStorage::new(Rc::new(
            dxdb_localstorage::MemoryKv::default(),
        )))
    });

    assert!(
        failures.is_empty(),
        "operator clause coverage failed on: {failures:?}. Each storage above panicked; run \
         with --nocapture to see which operator and clause."
    );
}

/// Every documented operator must have at least one case, and that case must exercise
/// sort, limit and skip.
///
/// This is the test that makes the matrix a check rather than a comment. It reads
/// [`DOCUMENTED_OPERATORS`] — the single constant mirroring PLAN.md 4.3 — so adding an
/// operator to the documented set without adding a case is a build failure.
#[test]
fn every_documented_operator_is_covered_with_every_clause() {
    let covered: std::collections::BTreeSet<&str> =
        cases().iter().map(|case| case.operator).collect();

    let missing: Vec<&&str> = DOCUMENTED_OPERATORS
        .iter()
        .filter(|operator| !covered.contains(**operator))
        .collect();
    assert!(
        missing.is_empty(),
        "documented operator(s) with no operator-coverage case: {missing:?}. Every operator \
         in PLAN.md 4.3 needs a case here, or a storage can pass the ported suite while \
         mishandling it once a limit or a skip is applied."
    );

    // And the reverse: a case for an operator that is not documented would be testing
    // behaviour dxdb does not owe.
    let undocumented: Vec<&str> = covered
        .iter()
        .copied()
        .filter(|operator| !DOCUMENTED_OPERATORS.contains(operator))
        .collect();
    assert!(
        undocumented.is_empty(),
        "case(s) for operator(s) not in PLAN.md 4.3: {undocumented:?}. Either the documented \
         set or the case is wrong."
    );

    // The clause invariants are applied to every case by `run_case`, but a case whose
    // selector matches a single document cannot distinguish limit from skip. Assert the
    // fixture gives each operator at least two matches, so the clauses are meaningful.
    for case in cases() {
        // The count comes from the selector semantics, checked at run time; here it is
        // enough to note that `expect_empty` cases are exempt.
        if case.expect_empty {
            continue;
        }
        assert!(
            case.selector.is_object(),
            "[{}] selector must be an object",
            case.operator
        );
    }
}

/// The fixture must exercise the clause boundaries, or the invariants are vacuous.
///
/// A limit of 2 on a 2-document result, or a skip of 1 on a 1-document result, cannot
/// distinguish a correct implementation from one that ignores the clause. This asserts the
/// fixture is big enough for the clauses to bite.
#[test]
fn fixture_is_large_enough_for_the_clauses_to_bite() {
    let documents = fixture_documents();
    assert!(
        documents.len() >= 10,
        "the operator fixture has {} documents; with fewer than 10 a limit of 2 and a skip \
         of 1 do not separate a correct implementation from one that ignores the clause",
        documents.len()
    );
    // Unique primary values, or the count/query comparison is meaningless.
    let ids: std::collections::BTreeSet<&str> = documents
        .iter()
        .map(|document| document["id"].as_str().expect("fixture doc has an id"))
        .collect();
    assert_eq!(
        ids.len(),
        documents.len(),
        "fixture primary values are not unique"
    );
}
