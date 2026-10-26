//! `make gate-query-correctness` — the ported `rx-storage-query-correctness` suite.
//!
//! Named `query_correctness.rs` because `make gate-query-correctness` runs
//! `cargo test -p dxdb-testkit --test query_correctness`; the name is fixed by the
//! Makefile.
//!
//! # What upstream asserts, and what runs here
//!
//! Upstream's harness does four things per query (`rx-storage-query-correctness.test.ts`
//! lines 204-340):
//!
//! 1. **the statics** — `getQueryMatcher` + `getSortComparator` computed in memory over
//!    the fixture data, compared against `expectedResultDocIds`. This is the *oracle*,
//!    and it needs `dxdb-query` (lane B). Not run here.
//! 2. **the planner** — `getQueryPlan(...).selectorSatisfiedByIndex` compared against the
//!    fixture's value. Needs `dxdb-query`. Not run here.
//! 3. **`RxStorageInstance.query()`** — compared against `expectedResultDocIds`. **This is
//!    what runs here.** It is the storage half, and it is the half this crate owns.
//! 4. **`count()`** — for a query with no skip/limit, compared against the expected
//!    length. **This runs here** too.
//!
//! Upstream also runs (5) `RxCollection.find()` and `findByIds()` and (6) a
//! remove-and-reinsert live-query cycle. Both live above the storage layer and need
//! `dxdb-core`; they are declared `DeclaredNotPorted` in the line tally rather than
//! silently absent.
//!
//! Each group's declaration row is therefore
//! [`PortStatus::PartiallyExercised`](dxdb_testkit::decl::PortStatus), which exists for
//! exactly this shape of work and is why `NeedsQueryCrate` would have been wrong in both
//! directions: it would have claimed nothing runs, when half of every group does.
//!
//! Because (1) and (2) do not run, the expected results are *taken as given* rather than
//! independently recomputed. That is a real limitation and it is stated rather than
//! glossed: a storage bug and a wrong fixture would be indistinguishable here. When
//! `dxdb-query` lands, step (1) becomes the oracle and this caveat disappears.

mod support;

use dxdb_testkit::fixtures::correctness;
use dxdb_testkit::fixtures::correctness_support::{Group, Query};
use dxdb_testkit::harness::{build_prepared_query, fill_with_default_settings, Harness, InstanceSpec};
use dxdb_testkit::schemas;
use support::reference_storage::ReferenceStorage;

use dxdb_storage::RxStorage;

/// Runs one group against `storage`: insert its documents, then for each query assert
/// `query()` returns exactly the expected ids in order.
fn run_group<S: RxStorage>(storage: &S, group: &Group) {
    futures::executor::block_on(async {
        let schema = fill_with_default_settings(group.materialised_schema());
        let spec = InstanceSpec::new(schema);
        let harness = Harness::open(storage, &spec)
            .await
            .unwrap_or_else(|err| panic!("group {}: create_storage_instance: {err}", group.index));

        // Insert the group's documents, exactly as upstream does.
        let mut rows = Vec::new();
        for (n, document) in group.data.iter().enumerate() {
            let mut payload = document
                .as_object()
                .unwrap_or_else(|| panic!("group {}: a fixture document is not an object", group.index))
                .clone();
            // Upstream wraps each fixture document with the four meta fields:
            // `_deleted: false, _attachments: {}, _meta: { lwt: now() }, _rev:
            // EXAMPLE_REVISION_1`. The lwt is fixed per document here so a failure is
            // reproducible; nothing in this suite sorts on lwt within a group.
            payload.insert("_deleted".to_string(), serde_json::json!(false));
            payload.insert("_attachments".to_string(), serde_json::json!({}));
            payload.insert(
                "_meta".to_string(),
                serde_json::json!({ "lwt": 1_700_000_000_000.0 + n as f64 }),
            );
            payload.insert(
                "_rev".to_string(),
                serde_json::json!(dxdb_testkit::fixtures::EXAMPLE_REVISION_1),
            );
            rows.push(dxdb_testkit::fixtures::write::insert_row(
                serde_json::Value::Object(payload),
            ));
        }
        harness.write_ok(rows).await;

        for query in &group.queries {
            assert_query(&harness, group, query).await;
        }

        harness.remove().await;
    });
}

/// Asserts one query, with failure messages that name upstream's `info` string.
async fn assert_query(harness: &Harness, group: &Group, query: &Query) {
    let expected: Vec<String> = query
        .expected_result_doc_ids
        .iter()
        .map(|id| (*id).to_string())
        .collect();

    let prepared = build_prepared_query(&query.query, group.primary_path);
    let actual = harness.query_ids(prepared.clone()).await;

    assert_eq!(
        actual,
        expected,
        "\n  upstream: {}:{}\n  group: {}\n  query: {}\n  expected: {:?}\n  actual:   {:?}\n\
         \n  (upstream also compares this against its in-memory statics; that half needs \
         dxdb-query and is not run here, so a wrong fixture would look identical to a \
         storage bug.)",
        group.upstream_line,
        "",
        group.title,
        query.info,
        expected,
        actual,
    );

    // Upstream only asserts `.count()` when the query has neither skip nor limit
    // (`rx-storage-query-correctness.test.ts:296`), because with a limit `count` is
    // defined to equal the limited length, which is a different assertion made
    // elsewhere. Matching that condition keeps this a port rather than an improvement.
    if query.asserts_count() {
        let count = harness.count(prepared).await;
        assert_eq!(
            count.count,
            expected.len(),
            "\n  upstream: {}:{}\n  group: {}\n  query: {}\n  count() must equal the number \
             of expected documents",
            group.upstream_line,
            "",
            group.title,
            query.info,
        );
    }
}

// ---------------------------------------------------------------------------
// One test per group, named after its upstream title
// ---------------------------------------------------------------------------

#[test]
fn gt_gte() {
    run_group_named(1);
}

#[test]
fn lt_lte() {
    run_group_named(2);
}

#[test]
fn nested_properties() {
    run_group_named(3);
}

#[test]
fn or_operator() {
    run_group_named(4);
}

#[test]
fn in_operator() {
    run_group_named(5);
}

#[test]
fn in_with_index_use() {
    run_group_named(6);
}

#[test]
fn in_for_array_fields_named_skills() {
    run_group_named(7);
}

#[test]
fn nin_operator() {
    run_group_named(8);
}

#[test]
fn elem_match_and_size() {
    run_group_named(9);
}

#[test]
fn elem_match_nested_arrays_with_regex() {
    run_group_named(10);
}

#[test]
fn eq_operator() {
    run_group_named(11);
}

#[test]
fn eq_operator_with_composite_primary_key() {
    run_group_named(12);
}

#[test]
fn issue_compound_index_has_wrong_range() {
    run_group_named(13);
}

#[test]
fn type_operator() {
    run_group_named(14);
}

#[test]
fn issue_wrong_results_on_complex_index() {
    run_group_named(15);
}

#[test]
fn issue_wrong_results_using_skip_and_limit() {
    run_group_named(16);
}

#[test]
fn issue_inserting_without_an_optional_index_field() {
    run_group_named(17);
}

#[test]
fn using_undefined_in_a_selector() {
    run_group_named(18);
}

#[test]
fn number_comparison_at_exact_boundary_values() {
    run_group_named(19);
}

#[test]
fn negative_decimal_numbers() {
    run_group_named(20);
}

#[test]
fn compound_index_with_decimal_numbers() {
    run_group_named(21);
}

#[test]
fn enum_field_with_eq_is_sort_irrelevant() {
    run_group_named(22);
}

#[test]
fn gt_lt_on_field_before_boolean_index_field() {
    run_group_named(23);
}

#[test]
fn string_fields_with_control_characters() {
    run_group_named(24);
}

#[test]
fn range_bound_outside_the_schema_minimum_and_maximum() {
    run_group_named(25);
}

// ---------------------------------------------------------------------------
// Meta-tests
// ---------------------------------------------------------------------------

/// Every group must have a `#[test]` here, and every test must name a real group.
#[test]
fn every_group_runs() {
    let source = include_str!("query_correctness.rs");
    let mut missing = Vec::new();
    for group in correctness::groups() {
        let needle = format!("run_group_named({})", group.index);
        if !source.contains(&needle) {
            missing.push(format!("group {} ({}): {}", group.index, group.title, group.upstream_line));
        }
    }
    assert!(
        missing.is_empty(),
        "groups with no #[test] in query_correctness.rs:\n  {}",
        missing.join("\n  ")
    );
}

/// The suite must be able to fail: a wrong expected result must be caught.
///
/// Rather than writing a wrong storage, this perturbs the *expectation* — which is the
/// equivalent check for this suite, because the assertions compare against fixture data.
/// If the comparison were a no-op, this would pass and the suite would be worthless.
#[test]
fn the_suite_catches_a_wrong_expectation() {
    use dxdb_testkit::fixtures::correctness_support::Query;
    use std::sync::OnceLock;

    // A copy of group 1 with its first query's expected ids reversed.
    static PERTURBED: OnceLock<Group> = OnceLock::new();
    let group = PERTURBED.get_or_init(|| {
        let original = &correctness::groups()[0];
        let mut queries: Vec<Query> = original.queries.clone();
        let mut first = queries[0].clone();
        let mut reversed: Vec<&'static str> = first.expected_result_doc_ids.to_vec();
        reversed.reverse();
        // Leak a boxed slice so the `&'static [&'static str]` type is satisfied; this is
        // a test, so the leak is bounded and intentional.
        let leaked: &'static [&'static str] = Box::leak(reversed.into_boxed_slice());
        first.expected_result_doc_ids = leaked;
        queries[0] = first;
        Group {
            queries,
            ..original.clone()
        }
    });

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_group(&ReferenceStorage::new(), group);
    }));
    assert!(
        outcome.is_err(),
        "the suite passed a group whose expected result order was reversed. The \
         comparison is not actually comparing."
    );
}

/// The count assertions must only apply where upstream applies them.
#[test]
fn count_is_asserted_only_without_skip_or_limit() {
    let mut with_limit = 0;
    let mut without = 0;
    for group in correctness::groups() {
        for query in &group.queries {
            if query.asserts_count() {
                without += 1;
            } else {
                with_limit += 1;
            }
        }
    }
    assert!(
        without > 0,
        "no query in the suite is count-asserted; the count half is dead"
    );
    assert_eq!(
        with_limit + without,
        dxdb_testkit::fixtures::query_count(correctness::groups()),
        "every query must fall in exactly one of the two buckets"
    );
    // The number upstream's fixtures actually produce, pinned so a change is visible.
    //
    // Three, not five: `rx-storage-query-correctness.test.ts` has exactly three query
    // objects carrying a `limit` or a `skip` — `:873` (`limit: 50` in "$in combined with
    // range operator and desc sort") and the two at `:2030`/`:2060` in "issue: wrong
    // results using skip and limit". An earlier version of this test asserted five, which
    // was a guess written before the extraction existed; the extraction was right and the
    // constant was wrong. Verified against upstream with:
    //
    //   grep -nE '^\s+(limit|skip): [0-9]' test/unit/rx-storage-query-correctness.test.ts
    //
    // which finds those three and nothing else. If this assertion ever fails, check that
    // grep before changing the number: the constant is the thing most likely to be wrong.
    assert_eq!(
        with_limit, 3,
        "upstream's correctness fixtures contain three queries carrying a skip or a limit"
    );
}

/// Every operator the suite's fixtures use must be one PLAN.md 4.3 names.
///
/// This keeps the suite inside the documented operator set: testing an operator upstream
/// does not have would assert behaviour dxdb does not owe.
#[test]
fn fixture_operators_are_in_the_documented_set() {
    /// The operator set from PLAN.md 4.3, verbatim.
    const DOCUMENTED: &[&str] = &[
        "$eq", "$gt", "$gte", "$lt", "$lte", "$ne", "$in", "$nin", "$regex", "$options",
        "$exists", "$type", "$mod", "$not", "$size", "$elemMatch", "$and", "$or", "$nor",
    ];

    let mut seen = std::collections::BTreeSet::new();
    fn walk(value: &serde_json::Value, seen: &mut std::collections::BTreeSet<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    if key.starts_with('$') {
                        seen.insert(key.clone());
                    }
                    walk(child, seen);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    walk(item, seen);
                }
            }
            _ => {}
        }
    }
    for group in correctness::groups() {
        for query in &group.queries {
            walk(&query.query, &mut seen);
        }
    }

    let undocumented: Vec<&String> = seen
        .iter()
        .filter(|operator| !DOCUMENTED.contains(&operator.as_str()))
        .collect();
    // And the reference scaffold must be able to evaluate every one of them, or a group
    // would fail for a reason that has nothing to do with the storage contract.
    let unimplemented: Vec<&String> = seen
        .iter()
        .filter(|operator| !support::reference_storage::SUPPORTED_OPERATORS.contains(&operator.as_str()))
        .collect();
    assert!(
        unimplemented.is_empty(),
        "the fixtures use operators the reference scaffold cannot evaluate: {unimplemented:?}"
    );
    assert!(
        undocumented.is_empty(),
        "the fixtures use operators not in PLAN.md 4.3: {undocumented:?}. Either the \
         documented set is wrong or the extraction picked up a nested key."
    );

    // And report which of the documented set the suite actually exercises, so "every
    // operator" is a checked claim rather than an impression.
    let exercised: Vec<&str> = DOCUMENTED
        .iter()
        .copied()
        .filter(|operator| seen.contains(*operator))
        .collect();
    println!(
        "operators exercised by the correctness fixtures ({}/{}): {}",
        exercised.len(),
        DOCUMENTED.len(),
        exercised.join(" ")
    );
}

/// The schemas the groups name must be the ones this crate ships.
#[test]
fn named_schemas_resolve() {
    use dxdb_testkit::fixtures::correctness_support::SchemaSource;
    // The named fixtures the generator can emit, for a test that they are all wired.
    let _ = (
        schemas::human(),
        schemas::human_minimal(),
        schemas::nested_human(),
        schemas::hero_array(),
        schemas::simple_array_hero(),
    );
    for group in correctness::groups() {
        let materialised = match &group.schema {
            SchemaSource::Named(make) => make(),
            SchemaSource::WithIndexes { base, .. } => base(),
            SchemaSource::Inline(text) => {
                serde_json::from_str(text).expect("inline schema parses")
            }
        };
        assert!(
            materialised.is_object(),
            "group {} ({}) resolved its schema to a non-object",
            group.index,
            group.title
        );
    }
}

/// The scaffold's documented gaps must stay gaps until the report says otherwise.
///
/// LANE_REPORT.md §7a lists six things the reference scaffold does not do, each of which
/// means some behaviour is proven by no case today. The one that is directly observable is
/// asserted here: no index, so `count()` always reports `Slow`. If that ever changes, the
/// report is stale and the assertions built on the scaffold may now be measuring something
/// different.
///
/// The other five are absence of behaviour rather than a wrong value, so there is nothing
/// to assert; they are listed in the report instead of pretended-covered here.
#[test]
fn scaffold_reports_slow_count_as_the_report_states() {
    use dxdb_storage::CountMode;
    use support::reference_storage::ReferenceStorage;

    futures::executor::block_on(async {
        let spec = dxdb_testkit::harness::InstanceSpec::new(
            dxdb_testkit::harness::fill_with_default_settings(dxdb_testkit::schemas::human()),
        );
        let harness = dxdb_testkit::harness::Harness::open(&ReferenceStorage::new(), &spec)
            .await
            .expect("open");
        let prepared = dxdb_testkit::harness::build_prepared_query(
            &serde_json::json!({ "selector": {}, "sort": [{ "passportId": "asc" }] }),
            "passportId",
        );
        let count = harness.count(prepared).await;
        assert_eq!(
            count.mode,
            CountMode::Slow,
            "LANE_REPORT.md §7a states the scaffold has no index and therefore always counts \
             Slow. If that changed, update the report before relying on any count assertion."
        );
    });
}

/// Runs the group with the given upstream index, panicking with its title if absent.
fn run_group_named(index: usize) {
    let groups = correctness::groups();
    let group = groups
        .iter()
        .find(|group| group.index == index)
        .unwrap_or_else(|| {
            panic!("no correctness group at index {index}; the extraction changed")
        });
    run_group(&ReferenceStorage::new(), group);
}
