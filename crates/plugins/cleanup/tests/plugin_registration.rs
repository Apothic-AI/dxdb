//! The plugin on the core registry.
//!
//! These tests are the migration's acceptance criterion: the plugin installs through a
//! real [`dxdb_core::PluginSet`], its hooks land in the tier it asked for, and its
//! teardown is exact.
//!
//! `cleanup` is the interesting case for the registry because it is the only plugin in
//! this lane with genuine upstream hooks — `createRxCollection` and `createRxState`, both
//! `after` (`src/plugins/cleanup/index.ts:30-41`). `query-builder` and `update` are pure
//! prototype plugins with no hooks block at all, so they prove the *declaration* half and
//! cleanup proves the *dispatch* half.

use dxdb_core::{HookName, HookOutcome, HookPayload, Hooks, Order, Plugin, PluginSet};
use dxdb_plugin_cleanup::{CleanupPlugin, CleanupRegistry, HOOKS};

fn collection_payload(name: &str) -> HookPayload {
    HookPayload::Collection {
        database: "db".to_string(),
        name: name.to_string(),
        schema: serde_json::json!({ "version": 0 }),
    }
}

fn fire_collection_created(hooks: &mut Hooks, name: &str) {
    let mut payload = collection_payload(name);
    hooks.fire(HookName::CreateRxCollection, &mut payload);
}

#[test]
fn the_plugin_installs_through_the_registry() {
    let mut hooks = Hooks::new();
    let mut set = PluginSet::new();
    let plugin = CleanupPlugin::new(CleanupRegistry::default());
    let handle = set.add(plugin, &mut hooks).expect("install");

    assert_eq!(handle.name(), "cleanup");
    assert_eq!(
        handle.registered(),
        2,
        "upstream registers exactly createRxCollection and createRxState"
    );
    assert_eq!(set.names(), vec!["cleanup"]);
    // The hook list is the plugin's published contract.
    assert_eq!(
        HOOKS,
        &[HookName::CreateRxCollection, HookName::CreateRxState]
    );
}

#[test]
fn both_hooks_land_in_the_after_tier() {
    // Upstream registers both as `after` (`index.ts:31,36`). Getting this wrong means
    // cleanup observes a half-built collection, which is the whole reason the tier exists.
    let mut hooks = Hooks::new();
    let mut set = PluginSet::new();
    set.add(CleanupPlugin::new(CleanupRegistry::default()), &mut hooks)
        .expect("install");

    for hook in HOOKS {
        let order = hooks.order_of(*hook);
        assert_eq!(order.len(), 1, "{hook:?} has one handler");
        assert_eq!(
            hooks.order_of_id(*hook, order[0]),
            Some(Order::After),
            "{hook:?} must be registered After"
        );
    }
}

#[test]
fn the_collection_hook_records_the_collection() {
    let mut hooks = Hooks::new();
    let mut set = PluginSet::new();
    let registry = CleanupRegistry::default();
    let plugin = CleanupPlugin::new(registry.clone());
    let _handle = set.add(plugin, &mut hooks).expect("install");

    assert!(registry.collections().is_empty());
    fire_collection_created(&mut hooks, "humans");
    fire_collection_created(&mut hooks, "pets");
    assert_eq!(registry.collections(), vec!["humans", "pets"]);
}

#[test]
fn the_hook_runs_after_other_plugins_normal_handlers() {
    // The observable consequence of `After`: a plugin registered *later* at `Normal`
    // still runs before cleanup, so cleanup sees a collection other plugins have
    // finished configuring.
    struct Normal;
    impl Plugin for Normal {
        fn name(&self) -> &'static str {
            "normal"
        }
        fn install(
            &self,
            registrations: &mut dxdb_core::Registrations,
            _overwritables: &mut dxdb_core::Overwritables,
        ) -> std::result::Result<(), dxdb_core::PluginError> {
            registrations.on(HookName::CreateRxCollection, |_| HookOutcome::Continue);
            Ok(())
        }
    }

    let mut hooks = Hooks::new();
    let mut set = PluginSet::new();
    set.add(CleanupPlugin::new(CleanupRegistry::default()), &mut hooks)
        .expect("install cleanup first");
    set.add(Normal, &mut hooks).expect("then a normal plugin");

    let order = hooks.order_of(HookName::CreateRxCollection);
    assert_eq!(order.len(), 2);
    let tiers: Vec<Option<Order>> = order
        .iter()
        .map(|id| hooks.order_of_id(HookName::CreateRxCollection, *id))
        .collect();
    assert_eq!(
        tiers,
        vec![Some(Order::Normal), Some(Order::After)],
        "cleanup runs last despite being installed first"
    );
}

#[test]
fn teardown_removes_only_cleanups_handlers() {
    struct Other;
    impl Plugin for Other {
        fn name(&self) -> &'static str {
            "other"
        }
        fn install(
            &self,
            registrations: &mut dxdb_core::Registrations,
            _overwritables: &mut dxdb_core::Overwritables,
        ) -> std::result::Result<(), dxdb_core::PluginError> {
            registrations.on(HookName::CreateRxCollection, |_| HookOutcome::Continue);
            Ok(())
        }
    }

    let mut hooks = Hooks::new();
    let mut set = PluginSet::new();
    let cleanup = set
        .add(CleanupPlugin::new(CleanupRegistry::default()), &mut hooks)
        .expect("install");
    let _other = set.add(Other, &mut hooks).expect("install other");
    assert_eq!(hooks.len(HookName::CreateRxCollection), 2);

    cleanup.uninstall(&mut hooks);
    assert_eq!(
        hooks.len(HookName::CreateRxCollection),
        1,
        "only the other plugin's handler is left"
    );
    assert_eq!(hooks.len(HookName::CreateRxState), 0);
}

#[test]
fn a_second_collection_hook_does_not_disturb_the_registry_order() {
    // Registering at `After` must not reorder anything else, and the list stays sorted.
    let mut hooks = Hooks::new();
    let mut set = PluginSet::new();
    set.add(CleanupPlugin::new(CleanupRegistry::default()), &mut hooks)
        .expect("install");
    let before = hooks.order_of(HookName::CreateRxCollection);
    fire_collection_created(&mut hooks, "humans");
    assert_eq!(
        hooks.order_of(HookName::CreateRxCollection),
        before,
        "firing a hook does not change its order"
    );
}
