//! Databases: a name, a storage, and a set of collections.
//!
//! Ported from `src/rx-database.ts` (1,030 lines). The parts that are behaviour rather
//! than plumbing:
//!
//! * **Name validation.** Upstream rejects a name that a CouchDB/PouchDB endpoint could
//!   not carry (`DB1`), because a database name is part of every replication URL and a
//!   slash in it silently changes the path. The rule is the same character class.
//! * **Collection name uniqueness** (`DB2` for a duplicate), and that `addCollections`
//!   is atomic per batch in the sense that a bad schema fails before anything is created.
//! * **`databaseInstanceToken`.** A random token per database object, used as the second
//!   half of every revision this database writes. Two databases writing the same document
//!   produce different but height-comparable revisions, which is what makes conflict
//!   detection work across replicas.
//! * **Storage instantiations go through `RxStorage`**, never a concrete storage crate.
//!   That is PLAN.md 4.1's rule and the reason `dxdb-core` has no non-dev storage
//!   dependency.
//!
//! # What is not here
//!
//! `localDocuments`, the internal `RxDatabase` store, migration state, and the
//! `collections$` observable are M6/other-lane work. Multi-instance change propagation is
//! the storage's job (`change_stream`), not this layer's.

use std::cell::RefCell;
use std::collections::BTreeMap;

use dxdb_storage::{DynRxStorageInstance, InstanceCreationParams, RxStorage, StorageSchema};
use serde_json::Value;

use crate::collection::Collection;
use crate::error::{CoreError, CoreResult};
use crate::hooks::{HookName, HookOutcome, HookPayload, Hooks};

/// Characters a database name may contain.
///
/// Upstream's `DB1` rule: lower-case alphanumerics plus `-`, `_`, `$`, `(`, `)`, `+` and
/// `{`, because the name reaches a URL and a directory name. Upper case is rejected
/// rather than folded, since two names differing only in case would collide on a
/// case-insensitive filesystem.
fn is_valid_database_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_' | b'$' | b'(' | b')' | b'+' | b'{' | b'}')
        })
}

/// A random-ish token, used as the second half of every revision.
///
/// The requirement is uniqueness within a process and across restarts, not
/// unpredictability, so a counter mixed with the clock and the address of a heap
/// allocation is enough. `dxdb-storage`'s `now()` gives the monotonic part; the counter
/// guarantees two databases created in the same tick differ.
fn database_instance_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let tick = dxdb_storage::now();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    // Hex so it cannot contain `-`, which is the revision separator.
    format!(
        "{}{:x}",
        dxdb_jsnum::to_js_string(tick).replace('.', ""),
        counter
    )
}

/// How a collection is declared to `add_collections`.
#[derive(Clone, Debug)]
pub struct CollectionDefinition {
    /// The collection name.
    pub name: String,
    /// The schema, before defaulting.
    pub schema: Value,
}

impl CollectionDefinition {
    /// Convenience constructor.
    pub fn new(name: impl Into<String>, schema: Value) -> Self {
        CollectionDefinition {
            name: name.into(),
            schema,
        }
    }
}

/// One database.
pub struct Database {
    name: String,
    instance_token: String,
    storage: Box<dxdb_storage::DynRxStorage<'static>>,
    collections: RefCell<BTreeMap<String, std::rc::Rc<Collection>>>,
    hooks: RefCell<Hooks>,
    closed: std::cell::Cell<bool>,
}

impl Database {
    /// `createRxDatabase` (`src/rx-database.ts`).
    ///
    /// Validates the name, then fires the creation hooks. No storage instance is opened
    /// until a collection is added, which matches upstream: `createRxDatabase` itself
    /// touches storage only for the internal store.
    pub fn create(
        name: &str,
        storage: Box<dxdb_storage::DynRxStorage<'static>>,
    ) -> CoreResult<Self> {
        if !is_valid_database_name(name) {
            return Err(CoreError::Schema(format!(
                "invalid database name '{name}': use lower-case letters, digits and -_$(){{}}+ "
            )));
        }
        let database = Database {
            name: name.to_string(),
            instance_token: database_instance_token(),
            storage,
            collections: RefCell::new(BTreeMap::new()),
            hooks: RefCell::new(Hooks::new()),
            closed: std::cell::Cell::new(false),
        };

        let mut payload = HookPayload::Database {
            name: name.to_string(),
            storage: database.storage.name().to_string(),
        };
        if database
            .hooks
            .borrow_mut()
            .fire(HookName::PreCreateRxDatabase, &mut payload)
            == HookOutcome::Stop
        {
            return Err(CoreError::ShouldNeverHappen(
                "a preCreateRxDatabase hook vetoed the create".to_string(),
            ));
        }
        database
            .hooks
            .borrow_mut()
            .fire(HookName::CreateRxDatabase, &mut payload);
        Ok(database)
    }

    /// The database name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The token every revision this database writes is suffixed with.
    pub fn instance_token(&self) -> &str {
        &self.instance_token
    }

    /// The storage engine this database writes through.
    pub fn storage_name(&self) -> &str {
        self.storage.name()
    }

    /// True after [`Database::close`].
    pub fn is_closed(&self) -> bool {
        self.closed.get()
    }

    /// The database-level hook registry.
    pub fn hooks(&self) -> &RefCell<Hooks> {
        &self.hooks
    }

    /// `addCollections` (`src/rx-database.ts`): opens each collection's storage instance.
    ///
    /// The whole batch is validated before any instance is opened, so a bad schema in the
    /// third definition does not leave two collections created and the third missing.
    pub async fn add_collections(
        &self,
        definitions: Vec<CollectionDefinition>,
    ) -> CoreResult<Vec<String>> {
        if self.closed.get() {
            return Err(CoreError::ShouldNeverHappen(
                "cannot add collections to a closed database".to_string(),
            ));
        }

        // Validate first: names unique within the batch and against what exists, and every
        // schema buildable.
        let mut schemas = Vec::with_capacity(definitions.len());
        for definition in &definitions {
            if self.collections.borrow().contains_key(&definition.name) {
                return Err(CoreError::DuplicateCollection {
                    collection: definition.name.clone(),
                });
            }
            if schemas
                .iter()
                .any(|(name, _): &(String, dxdb_schema::Schema)| name == &definition.name)
            {
                return Err(CoreError::DuplicateCollection {
                    collection: definition.name.clone(),
                });
            }
            let schema = dxdb_schema::create_rx_schema(&definition.schema)
                .map_err(|error| CoreError::Schema(error.to_string()))?;
            schemas.push((definition.name.clone(), schema));
        }

        let mut created = Vec::with_capacity(schemas.len());
        for (name, schema) in schemas {
            let mut payload = HookPayload::Schema {
                collection: name.clone(),
                schema: schema.json_schema().clone(),
            };
            if self
                .hooks
                .borrow_mut()
                .fire(HookName::PreCreateRxSchema, &mut payload)
                == HookOutcome::Stop
            {
                return Err(CoreError::ShouldNeverHappen(format!(
                    "a hook vetoed creating collection '{name}'"
                )));
            }
            self.hooks
                .borrow_mut()
                .fire(HookName::CreateRxSchema, &mut payload);

            let mut collection_payload = HookPayload::Collection {
                database: self.name.clone(),
                name: name.clone(),
                schema: schema.json_schema().clone(),
            };
            if self
                .hooks
                .borrow_mut()
                .fire(HookName::PreCreateRxCollection, &mut collection_payload)
                == HookOutcome::Stop
            {
                return Err(CoreError::ShouldNeverHappen(format!(
                    "a hook vetoed creating collection '{name}'"
                )));
            }

            let storage_schema = StorageSchema::from_value(schema.json_schema())
                .map_err(|error| CoreError::Schema(error.to_string()))?;
            let params = InstanceCreationParams {
                database_instance_token: self.instance_token.clone(),
                database_name: self.name.clone(),
                collection_name: name.clone(),
                schema: storage_schema,
                options: serde_json::json!({}),
                multi_instance: true,
                dev_mode: true,
            };

            let mut instance_payload = HookPayload::Collection {
                database: self.name.clone(),
                name: name.clone(),
                schema: schema.json_schema().clone(),
            };
            self.hooks
                .borrow_mut()
                .fire(HookName::PreCreateRxStorageInstance, &mut instance_payload);

            let instance: Box<DynRxStorageInstance<'static>> = self
                .storage
                .create_storage_instance(params)
                .await
                .map_err(|error| CoreError::Storage(error.to_string()))?;

            let collection = std::rc::Rc::new(Collection::new(
                name.clone(),
                self.name.clone(),
                schema,
                instance,
                self.instance_token.clone(),
            ));
            self.collections
                .borrow_mut()
                .insert(name.clone(), std::rc::Rc::clone(&collection));

            self.hooks
                .borrow_mut()
                .fire(HookName::CreateRxCollection, &mut collection_payload);
            created.push(name);
        }
        Ok(created)
    }

    /// One collection by name.
    pub fn collection(&self, name: &str) -> CoreResult<std::rc::Rc<Collection>> {
        self.collections
            .borrow()
            .get(name)
            .cloned()
            .ok_or_else(|| CoreError::UnknownCollection {
                collection: name.to_string(),
            })
    }

    /// Every collection name, sorted, so iteration order is deterministic.
    pub fn collection_names(&self) -> Vec<String> {
        self.collections.borrow().keys().cloned().collect()
    }

    /// `close()` (`src/rx-database.ts:632`): closes every collection.
    pub async fn close(&self) -> CoreResult<()> {
        if self.closed.replace(true) {
            return Ok(());
        }
        let mut payload = HookPayload::Database {
            name: self.name.clone(),
            storage: self.storage.name().to_string(),
        };
        self.hooks
            .borrow_mut()
            .fire(HookName::PreCloseRxDatabase, &mut payload);

        let collections: Vec<std::rc::Rc<Collection>> =
            self.collections.borrow().values().cloned().collect();
        for collection in collections {
            collection.close().await?;
        }
        self.collections.borrow_mut().clear();
        Ok(())
    }

    /// `remove()` (`src/rx-database.ts:727`): closes, then deletes every collection's data.
    pub async fn remove(&self) -> CoreResult<()> {
        let collections: Vec<std::rc::Rc<Collection>> =
            self.collections.borrow().values().cloned().collect();
        for collection in collections {
            dxdb_storage::RxStorageInstance::remove(collection.storage())
                .await
                .map_err(|error| CoreError::Storage(error.to_string()))?;
        }
        self.close().await?;
        let mut payload = HookPayload::Database {
            name: self.name.clone(),
            storage: self.storage.name().to_string(),
        };
        self.hooks
            .borrow_mut()
            .fire(HookName::PostRemoveRxDatabase, &mut payload);
        Ok(())
    }
}
