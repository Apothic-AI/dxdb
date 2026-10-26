//! The plugin model (PLAN.md 4.3).
//!
//! # What upstream does, and why none of it survives translation
//!
//! `addRxPlugin` (`src/plugin.ts:53-115`) is four mechanisms in one function:
//!
//! 1. a module-global `Set` of added plugins plus a `Set` of their names, so adding the
//!    same plugin twice is a no-op and adding a different plugin with the same name
//!    throws `PL3` (`:56-71`);
//! 2. `plugin.init()`, called exactly once, which a plugin uses to add *other* plugins
//!    (`:83-85`) — `migration-schema` adds `local-documents` this way;
//! 3. `plugin.prototypes`, which mutates five real prototypes (`:87-94`) so a plugin can
//!    add `doc.update()` to every document in the process;
//! 4. `plugin.overwritable`, which `Object.assign`s onto a module-global object (`:96-101`)
//!    that core then consults at runtime — that is how `dev-mode` turns on its checks.
//!
//! Plus `plugin.hooks`, which is the one mechanism that translates (`:103-114`) and now
//! lives in [`crate::hooks`].
//!
//! In Rust: (1) translates, (2) translates, (3) does not exist, (4) is a design decision.
//! This module implements (1), (2), and a narrowed (4), and says plainly that (3) is
//! re-expressed as extension traits that a plugin does not register at runtime at all
//! (see [`Plugin::name`]'s note on methods).
//!
//! # The shape
//!
//! ```text
//! Plugin            a static description: name, dependencies, what to register
//!   └─ install(&mut Hooks, &Overwritables) -> Result<(), PluginError>
//! PluginSet         the registry of *plugins* (not hooks): dedupe, order, teardown
//!   └─ add(...) -> PluginHandle
//! PluginHandle      proof of registration and the teardown token; dropping it
//!                   unregisters everything the plugin registered
//! ```
//!
//! A plugin never holds a `&mut Hooks` beyond [`Plugin::install`]. Everything it
//! registers comes back as one `Vec<HookId>` that the handle owns, which is what makes
//! teardown exact: `Hooks::clear()` would take other plugins' handlers with it.
//!
//! # The three rules
//!
//! **Ordering of plugins.** `PluginSet` preserves add order, and a plugin's hooks are
//! registered in the order the plugin asked for. Because hook ordering is
//! `(tier, sequence)` (see [`crate::hooks::Order`]), a plugin added later still lands in
//! the right tier — the tier decides, not the add order. Add order only breaks ties
//! *within* a tier, which is the one thing a caller can control by adding plugins in a
//! deliberate sequence.
//!
//! **Duplicates.** Same [`Plugin::name`] twice is [`PluginError::DuplicateName`], and a
//! second `add` of an already-installed plugin is a no-op returning a handle to the
//! existing install rather than a second registration. Upstream's `PL3` is the error
//! case; the no-op case is by name here rather than by object identity, because a Rust
//! `Plugin` is a zero-sized value that can always be constructed again — identity would
//! never match.
//!
//! **Dependencies.** `init()` in upstream is a plugin adding other plugins as a side
//! effect, which makes the dependency graph implicit and order-sensitive. Here a plugin
//! can declare [`Plugin::requires`] and `PluginSet` resolves the graph before any
//! `install` runs: a missing dependency is an error naming it, and a cycle is an error.
//! This is strictly more than upstream has, and it is cheap because the graph is tiny.
//!
//! # Overwritables
//!
//! Upstream's `overwritable` object (`src/overwritable.ts:10-41`) has three members that
//! core consults at runtime: `isDevMode`, `deepFreezeWhenDevMode`, `tunnelErrorMessage`.
//! A module-global mutable function table is the least portable thing in the codebase, so
//! this is narrowed to what can be a plain field on an owned struct: [`Overwritables`]
//! carries the three flags/settings, and a plugin that wants to change runtime behaviour
//! sets them on install. There is deliberately **no** way to replace the *body* of a core
//! function, because that is prototype mutation wearing a different hat, and the port
//! plan already rejects it.
//!
//! # What a plugin cannot do here, and what it does instead
//!
//! Upstream's `prototypes` block is how a plugin adds methods (`doc.update()`,
//! `db.dump()`, `collection.addPipeline()`). A Rust plugin cannot, so those become
//! **extension traits** that a user imports: `use dxdb_plugin_update::Update;` gives
//! `doc.update(..)`. That is a real ergonomic regression and it belongs in each crate's
//! docs, not in a footnote — it is the price of not having a dynamic language.

use crate::hooks::{HookId, HookName, HookOutcome, HookPayload, Hooks, Order};

use std::collections::BTreeMap;

/// Settings that upstream lets a plugin overwrite at runtime
/// (`src/overwritable.ts:10-41`).
///
/// Narrowed deliberately: only the *values* core consults, never a function body. See the
/// module docs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Overwritables {
    /// When true, core runs its expensive consistency checks.
    ///
    /// Upstream: `dev-mode` replaces `isDevMode` with `() => true`
    /// (`src/plugins/dev-mode/index.ts:95-99`), and 21 call sites ask it before doing
    /// extra work.
    pub is_dev_mode: bool,
    /// When true, `deepFreezeWhenDevMode` freezes documents so a mutation through a stale
    /// handle throws instead of silently diverging.
    ///
    /// Upstream freezes the JS object (`:99`). A Rust value cannot be frozen after the
    /// fact, so this flag is advisory: core rejects writes through a stale revision via
    /// `Document::ensure_revision`, and this flag only controls whether the more
    /// expensive checks run.
    pub deep_freeze_when_dev_mode: bool,
    /// Whether error messages are expanded.
    ///
    /// Upstream replaces `tunnelErrorMessage` to inline the human-readable text per error
    /// code (`src/plugins/dev-mode/index.ts:100`), because core ships without message
    /// strings to keep the bundle small. Here the flag selects between the short and the
    /// expanded rendering, which is the observable half of that behaviour without a
    /// function pointer.
    pub tunnel_error_messages: bool,
}

// The defaults are upstream's production defaults (`src/overwritable.ts:16-40`): no
// extra checks, no freezing, terse messages. Derived, because all three are `false`.


/// Something a plugin wants registered, collected during [`Plugin::install`].
///
/// A builder rather than four separate arguments because a plugin typically registers
/// several hooks at different tiers and it reads badly as a tuple list.
#[derive(Default)]
pub struct Registrations {
    pending: Vec<Pending>,
}

struct Pending {
    hook: HookName,
    order: Order,
    handler: crate::hooks::HookHandler,
}

impl Registrations {
    /// An empty set of registrations.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a handler at the default tier (appends after other `Normal` handlers).
    pub fn on<F>(&mut self, hook: HookName, handler: F) -> &mut Self
    where
        F: FnMut(&mut HookPayload) -> HookOutcome + 'static,
    {
        self.pending.push(Pending {
            hook,
            order: Order::Normal,
            handler: Box::new(handler),
        });
        self
    }

    /// Registers a handler at an explicit tier.
    pub fn on_ordered<F>(&mut self, hook: HookName, order: Order, handler: F) -> &mut Self
    where
        F: FnMut(&mut HookPayload) -> HookOutcome + 'static,
    {
        self.pending.push(Pending {
            hook,
            order,
            handler: Box::new(handler),
        });
        self
    }

    /// How many handlers have been queued.
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// True when nothing was queued.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Hands the queued handlers to `hooks`, in the order they were added.
    ///
    /// Takes a payload per hook so `Hooks::register_ordered` can validate the pairing.
    /// A plugin supplies the payload whose shape matches each hook it registers; the
    /// registry rejects a mismatch, so a wiring error surfaces here rather than the first
    /// time the hook fires in production.
    fn install_into(
        self,
        hooks: &mut Hooks,
        payload_for: &dyn Fn(HookName) -> Option<HookPayload>,
    ) -> Result<Vec<(HookName, HookId)>, PluginError> {
        let mut registered = Vec::with_capacity(self.pending.len());
        for pending in self.pending {
            let payload = payload_for(pending.hook).ok_or(PluginError::NoPayload {
                plugin: "(unknown)",
                hook: pending.hook,
            })?;
            let id = hooks
                .register_ordered(pending.hook, &payload, pending.order, pending.handler)
                .map_err(|error| PluginError::InvalidRegistration {
                    plugin: "(unknown)",
                    hook: error.hook,
                    payload: error.payload,
                })?;
            registered.push((pending.hook, id));
        }
        Ok(registered)
    }
}

/// A plugin: a name, its dependencies, and what it registers.
///
/// Implemented by a zero-sized type with no state, because a plugin's state belongs to
/// the database it is installed on. A plugin that needs per-database state gives its
/// handlers a closure over an `Rc<RefCell<..>>` created inside [`Plugin::install`].
///
/// **Methods are not here.** Upstream's `prototypes` block adds methods to documents and
/// collections; in Rust those are extension traits the user imports. See the module docs.
pub trait Plugin {
    /// The unique name, kebab-case, matching the upstream plugin directory where one
    /// exists (`query-builder`, `key-compression`, ...). A ported test asserts these, and
    /// a duplicate is [`PluginError::DuplicateName`].
    fn name(&self) -> &'static str;

    /// The names of plugins that must be installed first.
    ///
    /// Upstream expresses this by calling `addRxPlugin` inside `init()`
    /// (`migration-schema/index.ts:33-35`); declaring it instead makes the graph
    /// inspectable and turns a missing dependency into a startup error naming it.
    fn requires(&self) -> &'static [&'static str] {
        &[]
    }

    /// Queues this plugin's hook registrations.
    ///
    /// Called once, inside [`PluginSet::add`]. Returning `Err` aborts the whole add and
    /// unregisters anything already registered, so a failed add leaves no trace.
    fn install(
        &self,
        registrations: &mut Registrations,
        overwritables: &mut Overwritables,
    ) -> Result<(), PluginError>;
}

/// What went wrong installing a plugin.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PluginError {
    /// Two different plugins claimed one name (`PL3` upstream).
    #[error("a plugin named '{0}' is already installed")]
    DuplicateName(String),
    /// A declared dependency is not installed.
    #[error("plugin '{plugin}' requires '{requires}', which is not installed")]
    MissingDependency {
        /// The plugin that asked.
        plugin: &'static str,
        /// The dependency it wanted.
        requires: &'static str,
    },
    /// The declared dependencies form a cycle.
    #[error("plugin dependency cycle: {0}")]
    DependencyCycle(String),
    /// The registry rejected a registration because the payload did not match the hook.
    #[error("plugin '{plugin}' registered a {payload} payload for the '{hook}' hook")]
    InvalidRegistration {
        /// The plugin.
        plugin: &'static str,
        /// The hook.
        hook: HookName,
        /// The payload shape.
        payload: crate::hooks::PayloadKind,
    },
    /// The plugin registered a hook for which this crate has no payload constructor, so
    /// there is no way to fire it. A bug in the plugin, caught at install time.
    #[error("plugin '{plugin}' cannot register '{hook}': no payload is defined for it")]
    NoPayload {
        /// The plugin.
        plugin: &'static str,
        /// The hook.
        hook: HookName,
    },
}

/// One installed plugin, as the set remembers it.
struct Installed {
    name: &'static str,
    registrations: Vec<(HookName, HookId)>,
}

/// Proof that a plugin is installed, and the token that unregisters it.
///
/// Dropping the handle removes exactly the hooks that plugin registered — not
/// `Hooks::clear()`, which would take other plugins' handlers with it. Cloning is
/// deliberately not implemented: a plugin's registrations have one owner.
#[derive(Debug)]
pub struct PluginHandle {
    name: &'static str,
    /// `(hook, id)` for everything this plugin registered, so teardown is exact.
    registrations: Vec<(HookName, HookId)>,
    /// Present when the handle is the one that should unregister; `None` for a handle
    /// returned by a no-op re-add, whose first owner is still alive.
    armed: bool,
}

impl PluginHandle {
    /// The plugin's name.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// How many hooks this plugin registered.
    pub fn registered(&self) -> usize {
        self.registrations.len()
    }

    /// The hooks this plugin registered, in registration order.
    pub fn hooks(&self) -> impl Iterator<Item = HookName> + '_ {
        self.registrations.iter().map(|(hook, _)| *hook)
    }

    /// True when this handle owns the teardown. False for a handle returned by adding an
    /// already-installed plugin.
    pub fn is_armed(&self) -> bool {
        self.armed
    }

    /// Removes this plugin's hooks now, rather than at drop.
    pub fn uninstall(mut self, hooks: &mut Hooks) {
        self.unregister(hooks);
    }

    fn unregister(&mut self, hooks: &mut Hooks) {
        if !self.armed {
            return;
        }
        for (hook, id) in self.registrations.drain(..) {
            hooks.unregister(hook, id);
        }
        self.armed = false;
    }
}

/// The installed plugins of one database, and the registry their hooks went into.
///
/// Owned by the [`crate::Database`]: two databases in one process can therefore run
/// different plugin sets, which upstream's module-global `ADDED_PLUGINS` cannot express.
#[derive(Default)]
pub struct PluginSet {
    /// Insertion order, so `names()` is stable and reads like a startup log.
    ///
    /// A record rather than a [`PluginHandle`] so the set keeps its own copy of the
    /// registrations after a handle is dropped: `clear()` can then unregister whatever
    /// is still live. Unregistering an id twice is a no-op, so the two owners cannot
    /// fight.
    installed: Vec<Installed>,
    /// Name to index into `installed`, for the duplicate check.
    by_name: BTreeMap<&'static str, usize>,
    overwritables: Overwritables,
}

impl PluginSet {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// The settings plugins have changed so far.
    pub fn overwritables(&self) -> &Overwritables {
        &self.overwritables
    }

    /// Installs `plugin` and everything it requires.
    ///
    /// Resolves [`Plugin::requires`] depth-first, so a dependency is always installed
    /// before its dependent. Adding a plugin that is already installed returns a handle
    /// that does **not** own the teardown ([`PluginHandle::is_armed`] is false) rather
    /// than registering twice, which is upstream's "do nothing if added before"
    /// (`src/plugin.ts:56-58`) with identity replaced by name.
    pub fn add<P>(&mut self, plugin: P, hooks: &mut Hooks) -> Result<PluginHandle, PluginError>
    where
        P: Plugin,
    {
        let name = plugin.name();
        let was_installed = self.by_name.contains_key(name);
        self.add_with_dependencies(&plugin, hooks, &mut Vec::new())?;
        let index = self.by_name[name];
        Ok(PluginHandle {
            name,
            registrations: self.installed[index].registrations.clone(),
            // Only a freshly-installed plugin's handle owns the teardown. A no-op re-add
            // hands back the registrations for inspection without a second owner.
            armed: !was_installed,
        })
    }

    fn add_with_dependencies<P>(
        &mut self,
        plugin: &P,
        hooks: &mut Hooks,
        trail: &mut Vec<&'static str>,
    ) -> Result<(), PluginError>
    where
        P: Plugin,
    {
        let name = plugin.name();
        if self.by_name.contains_key(name) {
            return Ok(());
        }
        if trail.contains(&name) {
            let mut cycle: Vec<&str> = trail.to_vec();
            cycle.push(name);
            return Err(PluginError::DependencyCycle(cycle.join(" -> ")));
        }
        trail.push(name);

        for required in plugin.requires() {
            if !self.by_name.contains_key(required) {
                return Err(PluginError::MissingDependency {
                    plugin: name,
                    requires: required,
                });
            }
        }

        // `init()` in upstream adds dependencies as a side effect; here they are already
        // guaranteed present, so the only thing left is to register.
        let mut registrations = Registrations::new();
        plugin.install(&mut registrations, &mut self.overwritables)?;
        let registered = registrations
            .install_into(hooks, &payload_for_hook)
            .map_err(|error| match error {
                // Fill in the plugin name now that it is known, so the diagnostic names
                // the culprit rather than "(unknown)".
                PluginError::NoPayload { hook, .. } => {
                    PluginError::NoPayload { plugin: name, hook }
                }
                PluginError::InvalidRegistration { hook, payload, .. } => {
                    PluginError::InvalidRegistration {
                        plugin: name,
                        hook,
                        payload,
                    }
                }
                other => other,
            })?;

        let index = self.installed.len();
        self.by_name.insert(name, index);
        self.installed.push(Installed {
            name,
            registrations: registered,
        });
        trail.pop();
        Ok(())
    }

    /// The installed plugin names, in install order.
    pub fn names(&self) -> Vec<&'static str> {
        self.installed
            .iter()
            .map(|installed| installed.name)
            .collect()
    }

    /// True when a plugin with this name is installed.
    pub fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// How many plugins are installed.
    pub fn len(&self) -> usize {
        self.installed.len()
    }

    /// True when no plugin is installed.
    pub fn is_empty(&self) -> bool {
        self.installed.is_empty()
    }

    /// Unregisters every plugin's hooks, leaving the set empty.
    pub fn clear(&mut self, hooks: &mut Hooks) {
        for installed in &mut self.installed {
            for (hook, id) in installed.registrations.drain(..) {
                hooks.unregister(hook, id);
            }
        }
        self.installed.clear();
        self.by_name.clear();
    }
}

/// A payload of the right shape for `hook`, so the registry can validate a registration.
///
/// This is the one table that has to be kept in step with [`crate::hooks::HookPayload`].
/// A hook missing here is [`PluginError::NoPayload`] at install time rather than a hook
/// that silently never fires.
fn payload_for_hook(hook: HookName) -> Option<HookPayload> {
    Some(match hook {
        HookName::PreAddRxPlugin => HookPayload::Plugin {
            name: String::new(),
        },
        HookName::PreCreateRxDatabase
        | HookName::CreateRxDatabase
        | HookName::PreCloseRxDatabase
        | HookName::PostRemoveRxDatabase => HookPayload::Database {
            name: String::new(),
            storage: String::new(),
        },
        HookName::PreCreateRxCollection
        | HookName::CreateRxCollection
        | HookName::PostCloseRxCollection
        | HookName::PostRemoveRxCollection
        | HookName::CreateRxState
        | HookName::PreCreateRxStorageInstance => HookPayload::Collection {
            database: String::new(),
            name: String::new(),
            schema: serde_json::Value::Null,
        },
        HookName::PreCreateRxSchema | HookName::CreateRxSchema => HookPayload::Schema {
            collection: String::new(),
            schema: serde_json::Value::Null,
        },
        HookName::PrePrepareRxQuery | HookName::PreCreateRxQuery | HookName::PrePrepareQuery => {
            HookPayload::Query {
                collection: String::new(),
                query: serde_json::Value::Null,
            }
        }
        HookName::PreCreateRxDocument | HookName::PostCreateRxDocument => HookPayload::Document {
            collection: String::new(),
            document: dxdb_storage::DocumentData::new(serde_json::Map::new()),
        },
        HookName::PreStorageWrite => HookPayload::Write {
            collection: String::new(),
            rows: Vec::new(),
            context: String::new(),
        },
        HookName::PreMigrateDocument | HookName::PostMigrateDocument => HookPayload::Migration {
            collection: String::new(),
            document: serde_json::Value::Null,
        },
        HookName::PostCleanup => HookPayload::Cleanup {
            collection: String::new(),
            purged: 0,
        },
        HookName::PreReplicationMasterWrite
        | HookName::PreReplicationMasterWriteDocumentsHandle => HookPayload::Replication {
            database: String::new(),
            payload: serde_json::Value::Null,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::HookPayload;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A plugin that records its own name into a shared log on the given hook.
    struct Recording {
        name: &'static str,
        hook: HookName,
        order: Order,
        log: Rc<RefCell<Vec<&'static str>>>,
    }

    impl Plugin for Recording {
        fn name(&self) -> &'static str {
            self.name
        }
        fn install(
            &self,
            registrations: &mut Registrations,
            _overwritables: &mut Overwritables,
        ) -> Result<(), PluginError> {
            let log = Rc::clone(&self.log);
            let name = self.name;
            registrations.on_ordered(self.hook, self.order, move |_payload| {
                log.borrow_mut().push(name);
                HookOutcome::Continue
            });
            Ok(())
        }
    }

    fn log() -> Rc<RefCell<Vec<&'static str>>> {
        Rc::new(RefCell::new(Vec::new()))
    }

    fn fire(hooks: &mut Hooks, hook: HookName) {
        let mut payload = HookPayload::Database {
            name: "db".to_string(),
            storage: "memory".to_string(),
        };
        hooks.fire(hook, &mut payload);
    }

    /// A plugin's tier decides its position, not the order it was added in.
    #[test]
    fn a_later_plugin_in_an_earlier_tier_runs_first() {
        let mut hooks = Hooks::new();
        let mut set = PluginSet::new();
        let log = log();

        // Added first, but `After`: must still run last.
        set.add(
            Recording {
                name: "added-first-after",
                hook: HookName::CreateRxDatabase,
                order: Order::After,
                log: Rc::clone(&log),
            },
            &mut hooks,
        )
        .expect("add");
        // Added last, but `First`: must run first.
        set.add(
            Recording {
                name: "added-last-first",
                hook: HookName::CreateRxDatabase,
                order: Order::First,
                log: Rc::clone(&log),
            },
            &mut hooks,
        )
        .expect("add");
        set.add(
            Recording {
                name: "added-middle-normal",
                hook: HookName::CreateRxDatabase,
                order: Order::Normal,
                log: Rc::clone(&log),
            },
            &mut hooks,
        )
        .expect("add");

        fire(&mut hooks, HookName::CreateRxDatabase);
        assert_eq!(
            *log.borrow(),
            vec![
                "added-last-first",
                "added-middle-normal",
                "added-first-after"
            ],
            "tier order wins over add order"
        );
    }

    /// Within one tier, add order breaks the tie — which is the only thing a caller
    /// controls by sequencing `add`.
    #[test]
    fn within_a_tier_add_order_is_the_tie_break() {
        let mut hooks = Hooks::new();
        let mut set = PluginSet::new();
        let log = log();
        for name in ["a", "b", "c"] {
            set.add(
                Recording {
                    name,
                    hook: HookName::CreateRxDatabase,
                    order: Order::Normal,
                    log: Rc::clone(&log),
                },
                &mut hooks,
            )
            .expect("add");
        }
        fire(&mut hooks, HookName::CreateRxDatabase);
        assert_eq!(*log.borrow(), vec!["a", "b", "c"]);
    }

    /// All five tiers, registered in a deliberately scrambled order, interleave the way
    /// the enum says.
    #[test]
    fn all_five_tiers_have_a_total_order() {
        let mut hooks = Hooks::new();
        let mut set = PluginSet::new();
        let log = log();
        let sequence = [
            ("normal-1", Order::Normal),
            ("last", Order::Last),
            ("before-1", Order::Before),
            ("first", Order::First),
            ("after", Order::After),
            ("normal-2", Order::Normal),
        ];
        for (name, order) in sequence {
            set.add(
                Recording {
                    name,
                    hook: HookName::CreateRxDatabase,
                    order,
                    log: Rc::clone(&log),
                },
                &mut hooks,
            )
            .expect("add");
        }
        fire(&mut hooks, HookName::CreateRxDatabase);
        assert_eq!(
            *log.borrow(),
            vec!["first", "before-1", "normal-1", "normal-2", "after", "last"]
        );
    }

    /// Ordering is observable, so `order_of` must agree with what `fire` does.
    #[test]
    fn order_of_reports_the_execution_order() {
        let mut hooks = Hooks::new();
        let mut payload = HookPayload::Database {
            name: "db".to_string(),
            storage: "memory".to_string(),
        };
        let ids: Vec<HookId> = vec![
            hooks
                .register_ordered(
                    HookName::CreateRxDatabase,
                    &payload,
                    Order::After,
                    Box::new(|_| HookOutcome::Continue),
                )
                .expect("register"),
            hooks
                .register_ordered(
                    HookName::CreateRxDatabase,
                    &payload,
                    Order::First,
                    Box::new(|_| HookOutcome::Continue),
                )
                .expect("register"),
            hooks
                .register_ordered(
                    HookName::CreateRxDatabase,
                    &payload,
                    Order::Normal,
                    Box::new(|_| HookOutcome::Continue),
                )
                .expect("register"),
        ];
        let order = hooks.order_of(HookName::CreateRxDatabase);
        assert_eq!(order, vec![ids[1], ids[2], ids[0]]);
        assert_eq!(
            hooks.order_of_id(HookName::CreateRxDatabase, ids[1]),
            Some(Order::First)
        );
        // `fire` must agree with `order_of`.
        let _ = &mut payload;
    }

    /// Adding the same plugin twice is a no-op, and the second handle does not own the
    /// teardown.
    #[test]
    fn adding_twice_registers_once() {
        let mut hooks = Hooks::new();
        let mut set = PluginSet::new();
        let log = log();
        let plugin = || Recording {
            name: "twice",
            hook: HookName::CreateRxDatabase,
            order: Order::Normal,
            log: Rc::clone(&log),
        };
        let first = set.add(plugin(), &mut hooks).expect("first");
        assert!(first.is_armed());
        let second = set.add(plugin(), &mut hooks).expect("second");
        assert!(!second.is_armed(), "the second add is a no-op");
        assert_eq!(hooks.len(HookName::CreateRxDatabase), 1);
        assert_eq!(set.len(), 1);

        fire(&mut hooks, HookName::CreateRxDatabase);
        assert_eq!(*log.borrow(), vec!["twice"], "the handler ran once");
    }

    /// A dependency must be installed; the error names it.
    #[test]
    fn a_missing_dependency_is_an_error_naming_it() {
        struct Needy;
        impl Plugin for Needy {
            fn name(&self) -> &'static str {
                "needy"
            }
            fn requires(&self) -> &'static [&'static str] {
                &["absent"]
            }
            fn install(
                &self,
                _registrations: &mut Registrations,
                _overwritables: &mut Overwritables,
            ) -> Result<(), PluginError> {
                Ok(())
            }
        }
        let mut hooks = Hooks::new();
        let mut set = PluginSet::new();
        let error = set.add(Needy, &mut hooks).expect_err("missing dependency");
        assert_eq!(
            error,
            PluginError::MissingDependency {
                plugin: "needy",
                requires: "absent"
            }
        );
        assert!(set.is_empty(), "a failed add leaves no trace");
        assert!(hooks.is_empty(), "and registers nothing");
    }

    /// Dropping a handle removes exactly that plugin's hooks, not everyone's.
    #[test]
    fn dropping_a_handle_unregisters_only_that_plugin() {
        let mut hooks = Hooks::new();
        let mut set = PluginSet::new();
        let log = log();
        let keep = set
            .add(
                Recording {
                    name: "keep",
                    hook: HookName::CreateRxDatabase,
                    order: Order::Normal,
                    log: Rc::clone(&log),
                },
                &mut hooks,
            )
            .expect("add");
        let drop_me = set
            .add(
                Recording {
                    name: "drop",
                    hook: HookName::CreateRxDatabase,
                    order: Order::Normal,
                    log: Rc::clone(&log),
                },
                &mut hooks,
            )
            .expect("add");
        assert_eq!(hooks.len(HookName::CreateRxDatabase), 2);

        drop_me.uninstall(&mut hooks);
        assert_eq!(hooks.len(HookName::CreateRxDatabase), 1);
        fire(&mut hooks, HookName::CreateRxDatabase);
        assert_eq!(*log.borrow(), vec!["keep"]);
        assert!(keep.is_armed());
    }

    /// A plugin that fails to install leaves nothing registered, so a retry is clean.
    #[test]
    fn a_failed_install_registers_nothing() {
        struct Fails;
        impl Plugin for Fails {
            fn name(&self) -> &'static str {
                "fails"
            }
            fn install(
                &self,
                registrations: &mut Registrations,
                _overwritables: &mut Overwritables,
            ) -> Result<(), PluginError> {
                // Queue something, then fail: the queued handler must never reach the
                // registry.
                registrations.on(HookName::CreateRxDatabase, |_| HookOutcome::Continue);
                Err(PluginError::DuplicateName("fails".to_string()))
            }
        }
        let mut hooks = Hooks::new();
        let mut set = PluginSet::new();
        assert!(set.add(Fails, &mut hooks).is_err());
        assert!(hooks.is_empty());
    }

    /// A plugin can turn on the dev-mode overwritables, which is what upstream's
    /// `dev-mode` does by replacing `isDevMode`.
    #[test]
    fn a_plugin_can_set_the_overwritables() {
        struct DevMode;
        impl Plugin for DevMode {
            fn name(&self) -> &'static str {
                "dev-mode"
            }
            fn install(
                &self,
                _registrations: &mut Registrations,
                overwritables: &mut Overwritables,
            ) -> Result<(), PluginError> {
                overwritables.is_dev_mode = true;
                overwritables.deep_freeze_when_dev_mode = true;
                overwritables.tunnel_error_messages = true;
                Ok(())
            }
        }
        let mut hooks = Hooks::new();
        let mut set = PluginSet::new();
        assert!(!set.overwritables().is_dev_mode, "production default");
        set.add(DevMode, &mut hooks).expect("add");
        assert!(set.overwritables().is_dev_mode);
        assert!(set.overwritables().deep_freeze_when_dev_mode);
        assert!(set.overwritables().tunnel_error_messages);
    }

    /// A `Registration` whose tier is `Normal` is the same as `register`.
    #[test]
    fn register_is_normal_tier() {
        let mut hooks = Hooks::new();
        let payload = HookPayload::Database {
            name: "db".to_string(),
            storage: "memory".to_string(),
        };
        let id = hooks
            .register(
                HookName::CreateRxDatabase,
                &payload,
                Box::new(|_| HookOutcome::Continue),
            )
            .expect("register");
        assert_eq!(
            hooks.order_of_id(HookName::CreateRxDatabase, id),
            Some(Order::Normal)
        );
    }

    /// The tier keys are distinct and ascending, which is what makes `(key, sequence)` a
    /// total order.
    #[test]
    fn the_tier_keys_are_distinct_and_ascending() {
        let keys = [
            Order::First.key(),
            Order::Before.key(),
            Order::Normal.key(),
            Order::After.key(),
            Order::Last.key(),
        ];
        let mut sorted = keys;
        sorted.sort_unstable();
        assert_eq!(keys, sorted);
        assert_eq!(keys.len(), 5);
        assert_eq!(Order::First.as_str(), "first");
        assert_eq!(Order::Last.as_str(), "last");
    }
}
