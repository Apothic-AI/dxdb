//! `dxdb-memory` against the ported `rx-storage-query-correctness` suite.
//!
//! # Why this file exists
//!
//! `crates/dxdb-testkit/tests/query_correctness.rs` runs the 31 query-correctness groups
//! against `ReferenceStorage`, the testkit's own scaffold — the same situation
//! `tests/storage_implementations.rs` was in before `dxdb-memory` landed. A gate that only
//! ever runs against a scaffold answers "is the fixture self-consistent?", not "does a real
//! storage satisfy the contract", and that scaffold's own report records that it has no
//! index and therefore always counts `Slow`.
//!
//! This is the query half of the gap, next to `tests/conformance.rs` for the storage half.
//!
//! # Why it lives here and not in `dxdb-testkit`
//!
//! The testkit is another lane's crate, and its `run_group` is private to its test binary
//! anyway. Everything this file needs — `fixtures::correctness::groups()`, `Group`,
//! `InstanceSpec`, and the whole `Harness` surface — is already public and re-exported from
//! `dxdb_testkit`'s root, so the runner can be driven from outside the crate with no changes
//! to it. That is the same shape `dxdb-filesystem`'s `tests/conformance.rs` and
//! `dxdb-sharding` use: the storage that wants to prove itself owns the driver, and the
//! shared oracle stays untouched.
//!
//! The runner below is a deliberate near-copy of `query_correctness.rs`'s, because there is
//! no shared `pub` version to call. It differs in two places, both marked: the panic text
//! names `dxdb-memory`, and the group loop continues past a failure. A runner that silently
//! drifted from the oracle's would be worse than a runner that is duplicated; if the
//! testkit ever publishes one, delete this and call it.
//!
//! # The limitation this file inherits, stated rather than glossed
//!
//! Expected results are taken as given, not independently recomputed. Upstream also compares
//! each result against its in-memory statics; that half needs a query engine and is not run
//! here. So a wrong fixture and a storage bug are indistinguishable in this comparison. What
//! this file adds over the scaffold run is that the storage under test is the one callers
//! actually use.
//!
//! Run:
//! ```text
//! cargo test -p dxdb-memory --test query_correctness -- --nocapture
//! ```

use dxdb_memory::MemoryStorage;
use dxdb_storage::RxStorage;
use dxdb_testkit::fixtures::{
    correctness,
    correctness_support::{Group, Query},
};
use dxdb_testkit::harness::{fill_with_default_settings, Harness, InstanceSpec};

/// Inserts one group's documents and asserts every query in it.
///
/// `futures::executor::block_on` for the same reason as `tests/conformance.rs`: D3 makes the
/// sequential executor the default and this crate has no runtime dependency.
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
                .unwrap_or_else(|| {
                    panic!("group {}: a fixture document is not an object", group.index)
                })
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

    // Build against the *instance's* schema so the plan names an index the storage has. A
    // synthesized index name fails a correct storage with `UnknownIndex`.
    let prepared = harness.prepare(&query.query);
    let actual = harness.query_ids(prepared.clone()).await;

    assert_eq!(
        actual, expected,
        "\n  upstream: {}:{}\n  group: {}\n  query: {}\n  expected: {:?}\n  actual:   {:?}\n\
         \n  (dxdb-memory against the ported query-correctness suite. Upstream also compares \
         this against its in-memory statics; that half needs a query engine and is not run \
         here, so a wrong fixture would look identical to a storage bug.)",
        group.upstream_line, "", group.title, query.info, expected, actual,
    );

    // Upstream only asserts `.count()` when the query has neither skip nor limit
    // (`rx-storage-query-correctness.test.ts:296`), because with a limit `count` is defined
    // to equal the limited length, which is a different assertion made elsewhere. Matching
    // that condition keeps this a port rather than an improvement.
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

/// Every group in the fixture set, against `dxdb-memory`, in one test.
///
/// One test rather than 31 so the report is the complete list of failing groups instead of
/// stopping at the first — a storage with three query bugs should be told about three. Each
/// group opens its own `Harness` with a fresh random database name, so a panic in one cannot
/// leave state that changes another's answer.
#[test]
fn memory_passes_the_query_correctness_suite() {
    let storage = MemoryStorage::new();
    let groups = correctness::groups();
    let total = groups.len();

    // Collect the panic text through the payload rather than through a hook: catching it at
    // the `catch_unwind` gives the same message, and avoiding a global hook means this test
    // cannot interfere with a concurrent `#[test]` on another thread.
    let mut failures: Vec<(usize, &'static str, String)> = Vec::new();
    for group in groups {
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_group(&storage, group)));
        if let Err(payload) = outcome {
            let message = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
                .unwrap_or_else(|| "<non-string panic payload>".to_string());
            failures.push((group.index, group.title, message));
        }
    }

    let passed = total - failures.len();
    println!("\n| storage | groups | passed | failed | status |");
    println!("| --- | ---: | ---: | ---: | --- |");
    println!(
        "| `memory` | {total} | {passed} | {} | {} |",
        failures.len(),
        if failures.is_empty() {
            "passing"
        } else {
            "failing"
        }
    );
    for (index, title, message) in &failures {
        println!("\n- group {index} — {title}\n  - {message}");
    }

    assert!(
        failures.is_empty(),
        "{} of {total} query-correctness groups failed against dxdb-memory; see the table above",
        failures.len()
    );
}

/// The gate must not be vacuous.
///
/// The test above passes just as well against an empty group list, which is exactly how a
/// query gate silently stops measuring anything. These are the properties that make the run
/// meaningful: groups exist, they hold queries, and the counts the fixture module publishes
/// agree with what is actually iterated here.
///
/// No count is written as a literal. A literal is a number someone has to remember to update,
/// which is how coverage drops without anyone noticing — the same reason
/// `tests/conformance.rs` reads its total from the fixture set.
#[test]
fn the_query_gate_measures_something() {
    let groups = correctness::groups();
    assert!(
        !groups.is_empty(),
        "the fixture set exposes no groups; `memory_passes_the_query_correctness_suite` would \
         pass while asserting nothing"
    );

    let queries: usize = groups.iter().map(|group| group.queries.len()).sum();
    assert!(
        queries > 0,
        "the groups hold no queries; the gate above would pass while asserting nothing"
    );
    assert_eq!(
        dxdb_testkit::fixtures::query_count(groups),
        queries,
        "the fixture module's query tally disagrees with the sum over its groups, so one of \
         the two is counting something the other does not run"
    );

    // Every group must carry documents: a group with none would insert nothing and then
    // assert that queries over an empty store return the expected (probably empty) result,
    // which measures nothing about querying.
    for group in groups {
        assert!(
            !group.data.is_empty(),
            "group {} ({}) carries no documents",
            group.index,
            group.title
        );
        assert!(
            !group.queries.is_empty(),
            "group {} ({}) carries no queries",
            group.index,
            group.title
        );
    }

    // Group indices must be distinct, or a failure report names the wrong group.
    let mut indices: Vec<usize> = groups.iter().map(|group| group.index).collect();
    let total = indices.len();
    indices.sort_unstable();
    indices.dedup();
    assert_eq!(
        indices.len(),
        total,
        "two groups share an index, so a failure would be attributed to the wrong one"
    );
}
