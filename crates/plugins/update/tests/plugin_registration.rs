//! The plugin on the core registry, plus the extension-trait surface that replaces
//! upstream's prototype mutation.

use dxdb_core::{Hooks, PluginSet};
use dxdb_plugin_update::{apply_update, UpdateDocument, UpdatePlugin, HOOKS};

#[test]
fn the_plugin_installs_and_registers_no_hooks() {
    // Upstream's `RxDBUpdatePlugin` has no `hooks` block (`src/plugins/update/index.ts:45-56`):
    // it is a prototype plugin. The registry accepts it, records it, and has nothing to
    // dispatch — which is the honest shape, not a gap.
    let mut hooks = Hooks::new();
    let mut set = PluginSet::new();
    let handle = set.add(UpdatePlugin, &mut hooks).expect("install");

    assert_eq!(handle.name(), "update");
    assert_eq!(handle.registered(), 0);
    assert!(HOOKS.is_empty());
    assert_eq!(set.names(), vec!["update"]);
    assert!(hooks.is_empty(), "nothing was registered");
}

#[test]
fn adding_the_update_plugin_twice_is_a_no_op() {
    let mut hooks = Hooks::new();
    let mut set = PluginSet::new();
    let first = set.add(UpdatePlugin, &mut hooks).expect("first");
    let second = set.add(UpdatePlugin, &mut hooks).expect("second");
    assert!(first.is_armed());
    assert!(!second.is_armed());
    assert_eq!(set.len(), 1);
}

#[test]
fn the_extension_trait_provides_the_documented_methods() {
    // The Rust spelling of `doc.update()` and `doc.incrementalUpdate()`.
    let document = serde_json::json!({ "id": "a", "age": 20, "tags": ["x"] });

    let updated = document
        .update(&serde_json::json!({ "$inc": { "age": 1 } }))
        .expect("update");
    assert_eq!(updated["age"], 21);

    let updated = document
        .incremental_update(&serde_json::json!({ "$push": { "tags": "y" } }))
        .expect("incremental_update");
    assert_eq!(updated["tags"], serde_json::json!(["x", "y"]));

    // Both are the same pure transformation as `apply_update`, which is the function a
    // `dxdb-core` WriteQueue retry loop would call.
    assert_eq!(
        document
            .update(&serde_json::json!({ "$set": { "age": 1 } }))
            .expect("update"),
        apply_update(&document, &serde_json::json!({ "$set": { "age": 1 } })).expect("apply")
    );
}

#[test]
fn an_unsupported_modifier_is_still_an_error_through_the_trait() {
    // The brief's rule has to survive the migration to the trait surface.
    let document = serde_json::json!({ "id": "a" });
    let error = document
        .update(&serde_json::json!({ "$setOnInsert": { "a": 1 } }))
        .expect_err("unsupported modifier");
    assert!(error.to_string().contains("Unknown update operator"));
}
