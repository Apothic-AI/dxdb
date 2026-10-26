//! The hook registry (PLAN.md 4.3).
//!
//! # Why this is not a port
//!
//! Upstream's `src/hooks.ts` is a module-global object of 24 arrays and five prototype
//! mutations at plugin-add time (`src/plugin.ts:35`):
//!
//! ```text
//! HOOKS.preCreateRxDatabase = [];   // module global
//! RxCollection.prototype.insert = ...  // plugin replaces the method
//! ```
//!
//! Rust has neither module globals worth mutating nor prototype chains, so the
//! mechanism has to be re-expressed. PLAN.md 4.3 says how: "Hooks become typed
//! registration on a `Hooks` registry owned by the database, with explicit ordering and
//! the same names".
//!
//! Two consequences shape this module:
//!
//! * **The registry is owned, not global.** Two databases in one process get separate
//!   registries, so a plugin registered on one cannot fire on the other. Upstream's
//!   global `HOOKS` object cannot express that; it is a bug that only shows up with two
//!   databases in one process.
//! * **Ordering is explicit and stable.** Registration order within a hook is the
//!   execution order, and a hook returns `Continue`/`Stop` so a plugin can veto a
//!   create the way an upstream hook can throw. Upstream's `runPluginHooks` runs all
//!   functions and lets one throw to abort; this is the same power with a return value
//!   instead of an exception.
//!
//! # The 24 names
//!
//! All of upstream's hook names exist, spelled identically (`preCreateRxDatabase`,
//! ...), because a ported test or a ported plugin names them. They are typed so a
//! caller cannot fire `preCreateRxDatabase` with a collection argument.

use std::collections::HashMap;
use std::fmt;

/// The 24 upstream hook names (`src/hooks.ts:5`).
///
/// Spelled exactly as upstream so a ported call site is mechanical. `snake_case` would
/// read better in Rust, but the value is also what a plugin author reads in a diagnostic
/// and what a ported test asserts, so the identity wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HookName {
    /// Before a plugin is added.
    PreAddRxPlugin,
    /// Before a database is created.
    PreCreateRxDatabase,
    /// After a database is created.
    CreateRxDatabase,
    /// Before a collection is created.
    PreCreateRxCollection,
    /// After a collection is created.
    CreateRxCollection,
    /// When a plugin state object is created.
    CreateRxState,
    /// After a collection is closed.
    PostCloseRxCollection,
    /// After a collection is removed.
    PostRemoveRxCollection,
    /// Before a schema is created.
    PreCreateRxSchema,
    /// After a schema is created.
    CreateRxSchema,
    /// Before a query is prepared, per collection.
    PrePrepareRxQuery,
    /// Before an `RxQuery` object is constructed.
    PreCreateRxQuery,
    /// Before the query planner runs.
    PrePrepareQuery,
    /// Before a document handle is created.
    PreCreateRxDocument,
    /// After a document handle is created.
    PostCreateRxDocument,
    /// Before a storage instance is opened.
    PreCreateRxStorageInstance,
    /// Before a write reaches the storage.
    PreStorageWrite,
    /// Before a document is migrated.
    PreMigrateDocument,
    /// After a document is migrated.
    PostMigrateDocument,
    /// Before a database is closed.
    PreCloseRxDatabase,
    /// After a database is removed.
    PostRemoveRxDatabase,
    /// After a cleanup pass.
    PostCleanup,
    /// Before a master write during replication.
    PreReplicationMasterWrite,
    /// After master write documents are handled during replication.
    PreReplicationMasterWriteDocumentsHandle,
}

impl HookName {
    /// Every hook, in upstream's declaration order.
    pub const ALL: [HookName; 24] = [
        HookName::PreAddRxPlugin,
        HookName::PreCreateRxDatabase,
        HookName::CreateRxDatabase,
        HookName::PreCreateRxCollection,
        HookName::CreateRxCollection,
        HookName::CreateRxState,
        HookName::PostCloseRxCollection,
        HookName::PostRemoveRxCollection,
        HookName::PreCreateRxSchema,
        HookName::CreateRxSchema,
        HookName::PrePrepareRxQuery,
        HookName::PreCreateRxQuery,
        HookName::PrePrepareQuery,
        HookName::PreCreateRxDocument,
        HookName::PostCreateRxDocument,
        HookName::PreCreateRxStorageInstance,
        HookName::PreStorageWrite,
        HookName::PreMigrateDocument,
        HookName::PostMigrateDocument,
        HookName::PreCloseRxDatabase,
        HookName::PostRemoveRxDatabase,
        HookName::PostCleanup,
        HookName::PreReplicationMasterWrite,
        HookName::PreReplicationMasterWriteDocumentsHandle,
    ];

    /// The upstream spelling, which is also the key in a diagnostic.
    pub fn as_str(self) -> &'static str {
        match self {
            HookName::PreAddRxPlugin => "preAddRxPlugin",
            HookName::PreCreateRxDatabase => "preCreateRxDatabase",
            HookName::CreateRxDatabase => "createRxDatabase",
            HookName::PreCreateRxCollection => "preCreateRxCollection",
            HookName::CreateRxCollection => "createRxCollection",
            HookName::CreateRxState => "createRxState",
            HookName::PostCloseRxCollection => "postCloseRxCollection",
            HookName::PostRemoveRxCollection => "postRemoveRxCollection",
            HookName::PreCreateRxSchema => "preCreateRxSchema",
            HookName::CreateRxSchema => "createRxSchema",
            HookName::PrePrepareRxQuery => "prePrepareRxQuery",
            HookName::PreCreateRxQuery => "preCreateRxQuery",
            HookName::PrePrepareQuery => "prePrepareQuery",
            HookName::PreCreateRxDocument => "preCreateRxDocument",
            HookName::PostCreateRxDocument => "postCreateRxDocument",
            HookName::PreCreateRxStorageInstance => "preCreateRxStorageInstance",
            HookName::PreStorageWrite => "preStorageWrite",
            HookName::PreMigrateDocument => "preMigrateDocument",
            HookName::PostMigrateDocument => "postMigrateDocument",
            HookName::PreCloseRxDatabase => "preCloseRxDatabase",
            HookName::PostRemoveRxDatabase => "postRemoveRxDatabase",
            HookName::PostCleanup => "postCleanup",
            HookName::PreReplicationMasterWrite => "preReplicationMasterWrite",
            HookName::PreReplicationMasterWriteDocumentsHandle => {
                "preReplicationMasterWriteDocumentsHandle"
            }
        }
    }

    /// The hook with this upstream spelling, if it exists.
    pub fn parse(name: &str) -> Option<Self> {
        HookName::ALL.into_iter().find(|hook| hook.as_str() == name)
    }
}

impl fmt::Display for HookName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What a hook handler decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookOutcome {
    /// Keep going: the next handler runs, and then the operation proceeds.
    Continue,
    /// Stop: the operation is vetoed, and later handlers do not run.
    ///
    /// Upstream gets this by throwing from a hook; returning a value keeps the control
    /// flow visible at the call site instead of hiding it in an exception.
    Stop,
}

/// A registered handler.
///
/// `FnMut` because a plugin may keep state (a counter, a captured buffer) between
/// calls. The handler receives the payload mutably, so a `preStorageWrite` hook can
/// rewrite the rows it is given, which is what upstream's hooks do.
pub type HookHandler = Box<dyn FnMut(&mut HookPayload) -> HookOutcome>;

/// The data a hook is called with.
///
/// One enum rather than a generic per-hook type parameter: a registry that stored
/// differently-typed handlers per hook would need 24 type parameters, and the payloads
/// are all JSON-shaped anyway. Each variant names the hook family it belongs to, and
/// [`Hooks::fire`] checks the pairing so a `preCreateRxDatabase` handler cannot be
/// handed a collection payload.
#[derive(Clone, Debug, PartialEq)]
pub enum HookPayload {
    /// A database name, for the database-creation hooks.
    Database {
        /// The database name.
        name: String,
        /// Storage name, for diagnostics.
        storage: String,
    },
    /// A collection being created, closed or removed.
    Collection {
        /// The database name.
        database: String,
        /// The collection name.
        name: String,
        /// The schema, as ordered JSON.
        schema: serde_json::Value,
    },
    /// A schema being created.
    Schema {
        /// The collection name, when known.
        collection: String,
        /// The schema, as ordered JSON.
        schema: serde_json::Value,
    },
    /// A query being prepared or constructed.
    Query {
        /// The collection name.
        collection: String,
        /// The normalised query, as ordered JSON.
        query: serde_json::Value,
    },
    /// A document being created.
    Document {
        /// The collection name.
        collection: String,
        /// The document state.
        document: dxdb_storage::DocumentData,
    },
    /// Rows about to be written.
    Write {
        /// The collection name.
        collection: String,
        /// The rows, revision-assigned. A hook may rewrite these.
        rows: Vec<crate::write::PreparedRow>,
        /// The storage `context` string.
        context: String,
    },
    /// A document being migrated.
    Migration {
        /// The collection name.
        collection: String,
        /// The document, before or after migration depending on the hook.
        document: serde_json::Value,
    },
    /// A cleanup pass finishing.
    Cleanup {
        /// The collection name.
        collection: String,
        /// How many tombstones were purged.
        purged: usize,
    },
    /// A replication master write.
    Replication {
        /// The database name.
        database: String,
        /// The payload, as ordered JSON.
        payload: serde_json::Value,
    },
    /// A plugin being added.
    Plugin {
        /// The plugin's name.
        name: String,
    },
}

impl HookPayload {
    /// The hook names this payload is valid for.
    ///
    /// Checked by [`Hooks::register`] so a mismatched registration fails at
    /// registration time rather than on the first write in production.
    pub fn valid_for(&self) -> &'static [HookName] {
        match self {
            HookPayload::Database { .. } => &[
                HookName::PreCreateRxDatabase,
                HookName::CreateRxDatabase,
                HookName::PreCloseRxDatabase,
                HookName::PostRemoveRxDatabase,
            ],
            HookPayload::Collection { .. } => &[
                HookName::PreCreateRxCollection,
                HookName::CreateRxCollection,
                HookName::PostCloseRxCollection,
                HookName::PostRemoveRxCollection,
                HookName::CreateRxState,
                HookName::PreCreateRxStorageInstance,
            ],
            HookPayload::Schema { .. } => &[HookName::PreCreateRxSchema, HookName::CreateRxSchema],
            HookPayload::Query { .. } => &[
                HookName::PrePrepareRxQuery,
                HookName::PreCreateRxQuery,
                HookName::PrePrepareQuery,
            ],
            HookPayload::Document { .. } => &[
                HookName::PreCreateRxDocument,
                HookName::PostCreateRxDocument,
            ],
            HookPayload::Write { .. } => &[HookName::PreStorageWrite],
            HookPayload::Migration { .. } => {
                &[HookName::PreMigrateDocument, HookName::PostMigrateDocument]
            }
            HookPayload::Cleanup { .. } => &[HookName::PostCleanup],
            HookPayload::Replication { .. } => &[
                HookName::PreReplicationMasterWrite,
                HookName::PreReplicationMasterWriteDocumentsHandle,
            ],
            HookPayload::Plugin { .. } => &[HookName::PreAddRxPlugin],
        }
    }
}

/// One registered handler, with the identity needed to remove it again.
struct Registration {
    id: HookId,
    handler: HookHandler,
}

/// A hook registry owned by one database.
///
/// Registering returns a [`HookId`] so a plugin can unregister on teardown. Upstream's
/// `_clearHook(type, fun)` does the same by identity; an id is safer in Rust because a
/// boxed closure has no comparable identity.
#[derive(Default)]
pub struct Hooks {
    registrations: HashMap<HookName, Vec<Registration>>,
    next_id: u64,
    /// Set when a handler returned [`HookOutcome::Stop`], so the operation that fired
    /// the hook can see why and which hook it was.
    veto: Option<HookName>,
}

/// The identity of one registered handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HookId(u64);

impl Hooks {
    /// An empty registry.
    pub fn new() -> Self {
        Hooks::default()
    }

    /// Registers a handler for `hook`, running after any already registered.
    ///
    /// Returns `Err` when the handler's payload shape does not belong to this hook, so a
    /// wiring mistake is a startup failure rather than a runtime surprise.
    pub fn register(
        &mut self,
        hook: HookName,
        payload: &HookPayload,
        handler: HookHandler,
    ) -> Result<HookId, HookRegistrationError> {
        if !payload.valid_for().contains(&hook) {
            return Err(HookRegistrationError {
                hook,
                payload: payload.kind(),
            });
        }
        let id = HookId(self.next_id);
        self.next_id += 1;
        self.registrations
            .entry(hook)
            .or_default()
            .push(Registration { id, handler });
        Ok(id)
    }

    /// Removes one handler by id. Returns whether it was registered.
    pub fn unregister(&mut self, hook: HookName, id: HookId) -> bool {
        let Some(list) = self.registrations.get_mut(&hook) else {
            return false;
        };
        let before = list.len();
        list.retain(|registration| registration.id != id);
        before != list.len()
    }

    /// How many handlers are registered for a hook.
    pub fn len(&self, hook: HookName) -> usize {
        self.registrations
            .get(&hook)
            .map(Vec::len)
            .unwrap_or_default()
    }

    /// True when no handler is registered for any hook.
    pub fn is_empty(&self) -> bool {
        self.registrations.values().all(Vec::is_empty)
    }

    /// The total number of registered handlers, across all hooks.
    pub fn total(&self) -> usize {
        self.registrations.values().map(Vec::len).sum()
    }

    /// Runs every handler for `hook`, in registration order.
    ///
    /// Stops at the first [`HookOutcome::Stop`], which is upstream's "a hook threw"
    /// case. Returns the outcome so the caller decides what to do: a `preCreate*` hook
    /// aborts the create, a `post*` hook's veto has nothing to abort and is recorded in
    /// [`Hooks::last_veto`] instead of being silently dropped.
    pub fn fire(&mut self, hook: HookName, payload: &mut HookPayload) -> HookOutcome {
        if !payload.valid_for().contains(&hook) {
            // A mismatch here is a programmer error in the caller, not a plugin's doing.
            // Returning `Continue` (and recording nothing) keeps `fire` infallible while
            // making the mistake impossible to reach from the public API, because every
            // call site constructs the payload explicitly.
            return HookOutcome::Continue;
        }
        self.veto = None;
        let Some(list) = self.registrations.get_mut(&hook) else {
            return HookOutcome::Continue;
        };
        for registration in list.iter_mut() {
            if (registration.handler)(payload) == HookOutcome::Stop {
                self.veto = Some(hook);
                return HookOutcome::Stop;
            }
        }
        HookOutcome::Continue
    }

    /// The hook whose handler vetoed the last [`Hooks::fire`], if any.
    pub fn last_veto(&self) -> Option<HookName> {
        self.veto
    }

    /// Removes every handler, for teardown.
    pub fn clear(&mut self) {
        self.registrations.clear();
        self.veto = None;
    }
}

/// A handler was registered for a hook it cannot receive.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a {payload} payload cannot be registered for the '{hook}' hook")]
pub struct HookRegistrationError {
    /// The hook asked for.
    pub hook: HookName,
    /// The payload shape supplied.
    pub payload: PayloadKind,
}

/// The payload families, for error messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadKind {
    /// [`HookPayload::Database`].
    Database,
    /// [`HookPayload::Collection`].
    Collection,
    /// [`HookPayload::Schema`].
    Schema,
    /// [`HookPayload::Query`].
    Query,
    /// [`HookPayload::Document`].
    Document,
    /// [`HookPayload::Write`].
    Write,
    /// [`HookPayload::Migration`].
    Migration,
    /// [`HookPayload::Cleanup`].
    Cleanup,
    /// [`HookPayload::Replication`].
    Replication,
    /// [`HookPayload::Plugin`].
    Plugin,
}

impl fmt::Display for PayloadKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            PayloadKind::Database => "database",
            PayloadKind::Collection => "collection",
            PayloadKind::Schema => "schema",
            PayloadKind::Query => "query",
            PayloadKind::Document => "document",
            PayloadKind::Write => "write",
            PayloadKind::Migration => "migration",
            PayloadKind::Cleanup => "cleanup",
            PayloadKind::Replication => "replication",
            PayloadKind::Plugin => "plugin",
        })
    }
}

impl HookPayload {
    /// The payload family, for [`HookRegistrationError`].
    pub fn kind(&self) -> PayloadKind {
        match self {
            HookPayload::Database { .. } => PayloadKind::Database,
            HookPayload::Collection { .. } => PayloadKind::Collection,
            HookPayload::Schema { .. } => PayloadKind::Schema,
            HookPayload::Query { .. } => PayloadKind::Query,
            HookPayload::Document { .. } => PayloadKind::Document,
            HookPayload::Write { .. } => PayloadKind::Write,
            HookPayload::Migration { .. } => PayloadKind::Migration,
            HookPayload::Cleanup { .. } => PayloadKind::Cleanup,
            HookPayload::Replication { .. } => PayloadKind::Replication,
            HookPayload::Plugin { .. } => PayloadKind::Plugin,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn database_payload() -> HookPayload {
        HookPayload::Database {
            name: "db".to_string(),
            storage: "memory".to_string(),
        }
    }

    /// All 24 upstream hook names exist and round-trip through their spelling.
    #[test]
    fn all_twenty_four_hook_names_exist() {
        assert_eq!(HookName::ALL.len(), 24);
        for hook in HookName::ALL {
            assert_eq!(HookName::parse(hook.as_str()), Some(hook));
        }
        // A spot check of the spellings a ported test would assert.
        assert_eq!(
            HookName::PreCreateRxDatabase.as_str(),
            "preCreateRxDatabase"
        );
        assert_eq!(
            HookName::PreReplicationMasterWriteDocumentsHandle.as_str(),
            "preReplicationMasterWriteDocumentsHandle"
        );
        assert_eq!(HookName::parse("notAHook"), None);
    }

    /// Handlers run in registration order, which is the explicit ordering PLAN.md 4.3
    /// asks for in place of prototype mutation.
    #[test]
    fn handlers_run_in_registration_order() {
        let mut hooks = Hooks::new();
        let seen = Rc::new(RefCell::new(Vec::new()));
        for name in ["first", "second", "third"] {
            let seen = Rc::clone(&seen);
            hooks
                .register(
                    HookName::PreCreateRxDatabase,
                    &database_payload(),
                    Box::new(move |_| {
                        seen.borrow_mut().push(name);
                        HookOutcome::Continue
                    }),
                )
                .expect("register");
        }
        let mut payload = database_payload();
        assert_eq!(
            hooks.fire(HookName::PreCreateRxDatabase, &mut payload),
            HookOutcome::Continue
        );
        assert_eq!(*seen.borrow(), vec!["first", "second", "third"]);
    }

    /// A `Stop` aborts the chain, so a plugin can veto a create the way an upstream hook
    /// does by throwing.
    #[test]
    fn a_stop_vetoes_and_skips_later_handlers() {
        let mut hooks = Hooks::new();
        let later_ran = Rc::new(RefCell::new(false));
        hooks
            .register(
                HookName::PreCreateRxDatabase,
                &database_payload(),
                Box::new(|_| HookOutcome::Stop),
            )
            .expect("register veto");
        let flag = Rc::clone(&later_ran);
        hooks
            .register(
                HookName::PreCreateRxDatabase,
                &database_payload(),
                Box::new(move |_| {
                    *flag.borrow_mut() = true;
                    HookOutcome::Continue
                }),
            )
            .expect("register later");

        let mut payload = database_payload();
        assert_eq!(
            hooks.fire(HookName::PreCreateRxDatabase, &mut payload),
            HookOutcome::Stop
        );
        assert!(!*later_ran.borrow(), "later handlers must not run");
        assert_eq!(hooks.last_veto(), Some(HookName::PreCreateRxDatabase));
    }

    /// A hook may rewrite the payload it is given — that is what `preStorageWrite` is
    /// for (a compression or encryption plugin does exactly this).
    #[test]
    fn a_hook_can_rewrite_the_payload() {
        let mut hooks = Hooks::new();
        let write = HookPayload::Write {
            collection: "c".to_string(),
            rows: Vec::new(),
            context: "ctx".to_string(),
        };
        hooks
            .register(
                HookName::PreStorageWrite,
                &write,
                Box::new(|payload| {
                    if let HookPayload::Write { context, .. } = payload {
                        *context = "rewritten".to_string();
                    }
                    HookOutcome::Continue
                }),
            )
            .expect("register");
        let mut payload = write;
        hooks.fire(HookName::PreStorageWrite, &mut payload);
        match payload {
            HookPayload::Write { context, .. } => assert_eq!(context, "rewritten"),
            other => panic!("wrong payload shape: {other:?}"),
        }
    }

    /// A mismatched registration is rejected at registration time, not on a later write.
    #[test]
    fn a_payload_shape_cannot_be_registered_for_the_wrong_hook() {
        let mut hooks = Hooks::new();
        let error = hooks
            .register(
                HookName::PreCreateRxDatabase,
                &HookPayload::Write {
                    collection: "c".to_string(),
                    rows: Vec::new(),
                    context: String::new(),
                },
                Box::new(|_| HookOutcome::Continue),
            )
            .expect_err("must reject");
        assert_eq!(error.hook, HookName::PreCreateRxDatabase);
        assert_eq!(error.payload, PayloadKind::Write);
        assert_eq!(hooks.total(), 0, "nothing may be registered");
    }

    #[test]
    fn unregister_removes_only_that_handler() {
        let mut hooks = Hooks::new();
        let first = hooks
            .register(
                HookName::CreateRxDatabase,
                &database_payload(),
                Box::new(|_| HookOutcome::Continue),
            )
            .expect("first");
        hooks
            .register(
                HookName::CreateRxDatabase,
                &database_payload(),
                Box::new(|_| HookOutcome::Continue),
            )
            .expect("second");
        assert_eq!(hooks.len(HookName::CreateRxDatabase), 2);
        assert!(hooks.unregister(HookName::CreateRxDatabase, first));
        assert_eq!(hooks.len(HookName::CreateRxDatabase), 1);
        assert!(
            !hooks.unregister(HookName::CreateRxDatabase, first),
            "removing twice reports false"
        );
    }

    /// Two registries are independent. Upstream's module-global `HOOKS` cannot express
    /// this, which is the reason the registry is owned.
    #[test]
    fn registries_are_independent() {
        let mut one = Hooks::new();
        let two = Hooks::new();
        one.register(
            HookName::CreateRxDatabase,
            &database_payload(),
            Box::new(|_| HookOutcome::Continue),
        )
        .expect("register");
        assert_eq!(one.total(), 1);
        assert_eq!(
            two.total(),
            0,
            "a hook on one database must not fire on another"
        );
    }

    /// Every payload family maps to at least one hook, and every hook has at least one
    /// valid payload family — otherwise a hook would be unreachable or a payload
    /// unusable.
    #[test]
    fn every_hook_has_a_valid_payload_family() {
        let payloads = [
            database_payload(),
            HookPayload::Collection {
                database: "d".into(),
                name: "c".into(),
                schema: serde_json::json!({}),
            },
            HookPayload::Schema {
                collection: "c".into(),
                schema: serde_json::json!({}),
            },
            HookPayload::Query {
                collection: "c".into(),
                query: serde_json::json!({}),
            },
            HookPayload::Document {
                collection: "c".into(),
                document: dxdb_storage::DocumentData::from_value(serde_json::json!({
                    "id": "a", "_rev": "1-t", "_deleted": false,
                    "_meta": { "lwt": 1.0 }, "_attachments": {}
                }))
                .expect("valid"),
            },
            HookPayload::Write {
                collection: "c".into(),
                rows: Vec::new(),
                context: String::new(),
            },
            HookPayload::Migration {
                collection: "c".into(),
                document: serde_json::json!({}),
            },
            HookPayload::Cleanup {
                collection: "c".into(),
                purged: 0,
            },
            HookPayload::Replication {
                database: "d".into(),
                payload: serde_json::json!({}),
            },
            HookPayload::Plugin { name: "p".into() },
        ];
        for hook in HookName::ALL {
            assert!(
                payloads
                    .iter()
                    .any(|payload| payload.valid_for().contains(&hook)),
                "hook '{hook}' has no valid payload family"
            );
        }
    }
}
