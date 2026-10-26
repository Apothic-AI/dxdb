//! The plugin on the core registry, plus `continue_building` — the Rust equivalent of
//! upstream's `runBuildingStep`.

use dxdb_core::{Hooks, PluginSet};
use dxdb_plugin_query_builder::{continue_building, Error, QueryBuilderPlugin, SortArg, HOOKS};

fn empty_query() -> dxdb_plugin_query_builder::MangoQuery {
    dxdb_plugin_query_builder::MangoQuery {
        selector: serde_json::json!({}),
        sort: Vec::new(),
        skip: 0,
        limit: None,
        index: None,
    }
}

#[test]
fn the_plugin_installs_and_registers_no_hooks() {
    // Upstream's `RxDBQueryBuilderPlugin` has a `prototypes` block and no `hooks` block
    // (`src/plugins/query-builder/index.ts:54-81`).
    let mut hooks = Hooks::new();
    let mut set = PluginSet::new();
    let handle = set.add(QueryBuilderPlugin, &mut hooks).expect("install");
    assert_eq!(handle.name(), "query-builder");
    assert_eq!(handle.registered(), 0);
    assert!(HOOKS.is_empty());
    assert_eq!(set.names(), vec!["query-builder"]);
}

#[test]
fn continue_building_applies_one_step_and_reports_the_path() {
    // `runBuildingStep` (`index.ts:15-35`): clone the query, build, apply one method,
    // return the new query plus the path the next step resumes from.
    let query = empty_query();
    let json = continue_building(&query, None, |builder| {
        builder.where_("age").gt(serde_json::json!(4))?;
        Ok(())
    })
    .expect("step");
    assert_eq!(
        json.query.selector,
        serde_json::json!({ "age": { "$gt": 4 } })
    );
    assert_eq!(json.path.as_deref(), Some("age"));

    // The next step resumes from the reported path, which is the `queryBuilderPath`
    // bookkeeping upstream keeps in `other`.
    let json = continue_building(&json.query, json.path.as_deref(), |builder| {
        builder.ne(serde_json::json!(10))?;
        Ok(())
    })
    .expect("second step");
    assert_eq!(
        json.query.selector,
        serde_json::json!({ "age": { "$gt": 4, "$ne": 10 } }),
        "the resumed step lands on the same field"
    );
}

#[test]
fn continue_building_reports_the_error_a_step_returns() {
    let query = empty_query();
    let error = continue_building(&query, None, |builder| {
        // No `where`, so a comparison has no path.
        builder.gt(serde_json::json!(1))?;
        Ok(())
    })
    .expect_err("no path");
    assert_eq!(error, Error::NoPath { method: "gt" });
}

#[test]
fn a_built_query_round_trips_through_the_step_api() {
    let query = empty_query();
    let json = continue_building(&query, None, |builder| {
        builder
            .where_("name")
            .eq(serde_json::json!("alice"))?
            .sort(SortArg::Object(vec![(
                "name".to_string(),
                serde_json::json!("desc"),
            )]))?;
        Ok(())
    })
    .expect("step");
    assert_eq!(json.query.sort.len(), 1);
    assert_eq!(json.query.sort[0].field, "name");
}
