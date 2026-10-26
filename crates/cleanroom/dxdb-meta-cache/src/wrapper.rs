//! The wrapper: validated metadata reads, and the single function every mutation goes
//! through.
//!
//! `specs/meta-cache.md` sections 3.2 (the check), 3.3 (the write-ordering rule) and
//! 3.4 (the invalidation path).
//!
//! # The shape
//!
//! [`MetadataLedger`] is the real metadata store — the thing that knows the database's
//! collection documents. [`MetaCache`] wraps it:
//!
//! * [`MetaCache::open`] reads the generation document (one read), validates the
//!   snapshot, and either serves the snapshot or pays a full read and writes a fresh one;
//! * [`MetaCache::write`] is **the only way to mutate metadata** and therefore the only
//!   place the generation is bumped. That single entry point is what `INV-6` means: it is
//!   not a callback list a caller must remember to register with.
//!
//! # Why a ledger trait rather than a storage instance
//!
//! `specs/meta-cache.md` assumed the port would have RxDB's internal store
//! (`_rxdb_internal`) with its collection documents, so the cache could sit in front of
//! it. `dxdb-core` does not have one yet — `Database::create` takes a storage and builds
//! collections directly, and `_rxdb_internal` exists nowhere in the workspace
//! (`docs/CLEANROOM-GAPS.md` G-054). Declaring the ledger here is the honest stand-in:
//! the contract the cache needs is "list the metadata documents" / "replace them", and
//! when the port grows an internal store it implements this trait. The cache's own
//! invariants are then already proven rather than having to be re-derived.

use std::cell::Cell;
use std::rc::Rc;

use serde_json::Value;

use crate::generation::GenerationDoc;
use crate::snapshot::{Snapshot, StaleReason};
use crate::{MetaCacheError, SnapshotStore};

/// The real metadata store.
///
/// Implementors are expected to be backed by whatever the port uses for collection
/// metadata. The trait is deliberately tiny: the cache needs to list the documents and
/// to replace them, and nothing else.
pub trait MetadataLedger {
    /// Every metadata document, in a stable order.
    ///
    /// "Metadata" means the internal store's documents — collection names, schemas,
    /// schema hashes, the storage token. Never user documents (`INV-10`).
    fn documents(&self) -> Result<Vec<Value>, MetaCacheError>;

    /// Replaces every metadata document with `documents`.
    ///
    /// This is the mutation. A caller that wants to add a collection, remove one, or
    /// rewrite a schema document goes through [`MetaCache::write`], which calls this.
    fn replace_documents(&self, documents: Vec<Value>) -> Result<(), MetaCacheError>;

    /// Reads the generation document, if the ledger has one.
    fn generation(&self) -> Result<Option<GenerationDoc>, MetaCacheError>;

    /// Writes the generation document.
    fn set_generation(&self, document: &GenerationDoc) -> Result<(), MetaCacheError>;

    /// A fresh random `instance_id` for a new incarnation.
    ///
    /// Injected rather than generated here because a browser build has no OS entropy
    /// source on `wasm32-unknown-unknown`; the platform supplies one, exactly as
    /// `dxdb-encryption` does for its salt.
    fn fresh_instance_id(&self) -> String;
}

/// One metadata mutation.
#[derive(Debug, Clone, PartialEq)]
pub enum MetadataWrite {
    /// Replace the whole document set.
    Replace(Vec<Value>),
    /// Add one document.
    Add(Value),
    /// Remove the document whose key field equals this value.
    Remove(String),
}

/// What [`MetaCache::open`] did, so a caller (and a test) can tell a hit from a miss.
#[derive(Debug, Clone, PartialEq)]
pub enum CacheOutcome {
    /// The snapshot validated; the documents came from it.
    Hit,
    /// No usable snapshot; the documents came from the store and a fresh snapshot was
    /// written.
    Miss {
        /// Why the previous snapshot was not usable. `Absent` on a first open.
        reason: StaleReason,
    },
}

/// Why a [`MetadataWrite`] was rejected before it reached the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    /// The cache is closed.
    Closed,
}

/// Settings. Every field is optional in spirit; the defaults are the documented
/// behaviour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaCacheSettings {
    /// Whether the cache is used at all.
    ///
    /// Added deliberately (`specs/meta-cache.md` section 2): a user needs a way to turn
    /// this off in the field without rebuilding. Not a documented upstream option, which
    /// is why it is recorded in the spec's API table.
    pub enabled: bool,
}

impl Default for MetaCacheSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// A metadata cache in front of a [`MetadataLedger`].
pub struct MetaCache {
    ledger: Rc<dyn MetadataLedger>,
    store: Rc<dyn SnapshotStore>,
    settings: MetaCacheSettings,
    /// The generation this cache currently believes in, once opened.
    current: Option<GenerationDoc>,
    /// The documents served at open, so a read does not touch the ledger.
    documents: Vec<Value>,
    /// The database name this cache was opened for, so `write` can drop the snapshot
    /// without the caller passing the name again.
    database_name: std::cell::RefCell<Option<String>>,
    closed: Cell<bool>,
    /// Reads the ledger has served, for `INV-9`'s cost assertion.
    ledger_reads: Cell<usize>,
    /// Snapshot writes attempted, for the same reason.
    snapshot_writes: Cell<usize>,
}

impl std::fmt::Debug for MetaCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetaCache")
            .field("settings", &self.settings)
            .field("generation", &self.current.as_ref().map(|g| g.generation))
            .field("documents", &self.documents.len())
            .finish_non_exhaustive()
    }
}

impl MetaCache {
    /// Builds a cache around `ledger` and `store`.
    pub fn new(
        ledger: Rc<dyn MetadataLedger>,
        store: Rc<dyn SnapshotStore>,
        settings: MetaCacheSettings,
    ) -> Self {
        Self {
            ledger,
            store,
            settings,
            current: None,
            documents: Vec::new(),
            database_name: std::cell::RefCell::new(None),
            closed: Cell::new(false),
            ledger_reads: Cell::new(0),
            snapshot_writes: Cell::new(0),
        }
    }

    /// Opens the cache for `database_name`, returning what happened.
    ///
    /// The check, in the order `specs/meta-cache.md` section 3.2 fixes:
    ///
    /// 1. read the generation document from the **real** store — the one read the whole
    ///    design spends;
    /// 2. read and validate the snapshot;
    /// 3. on a hit, serve the snapshot and do not touch the metadata documents;
    /// 4. on a miss, read them, rebuild, and write a fresh snapshot tagged with the
    ///    generation read **first** (section 3.3).
    pub fn open(&mut self, database_name: &str) -> Result<CacheOutcome, MetaCacheError> {
        if self.closed.get() {
            return Err(MetaCacheError::Closed);
        }
        *self.database_name.borrow_mut() = Some(database_name.to_string());
        if !self.settings.enabled {
            // A disabled cache is a pass-through: one full read, no snapshot traffic.
            self.documents = self.read_ledger_documents()?;
            // Creating the generation document is best-effort: a store that cannot be
            // written must not turn a readable database into an unopenable one.
            self.current = Some(self.ensure_generation_best_effort());
            return Ok(CacheOutcome::Miss {
                reason: StaleReason::Absent,
            });
        }

        // Step 1: the generation document. Read before the documents, and never
        // re-read before tagging — that ordering is `INV-1`'s mechanism.
        //
        // Best-effort creation, deliberately: if the store refuses the write, the
        // database must still open (`INV-7`). Serving the documents is the job; caching
        // them is an optimisation.
        let generation = self.ensure_generation_best_effort();
        self.current = Some(generation.clone());

        // Steps 2-3: validate, and serve on a hit.
        match Snapshot::read_valid(self.store.as_ref(), database_name, &generation) {
            Ok(snapshot) => {
                self.documents = snapshot.documents;
                Ok(CacheOutcome::Hit)
            }
            Err(reason) => {
                // Step 4: pay the read, then tag the fresh snapshot with the generation
                // read *first*. If a writer mutated the store between our generation read
                // and this read, the snapshot may hold slightly newer data under an older
                // tag, which the next reader rejects. Conservative, never stale.
                self.documents = self.read_ledger_documents()?;
                self.write_snapshot(database_name, &generation);
                Ok(CacheOutcome::Miss { reason })
            }
        }
    }

    /// The metadata documents.
    ///
    /// Served from whatever [`Self::open`] loaded, so a read never touches the ledger —
    /// that is the saving.
    pub fn documents(&self) -> Result<Vec<Value>, MetaCacheError> {
        if self.closed.get() {
            return Err(MetaCacheError::Closed);
        }
        if self.current.is_none() {
            return Err(MetaCacheError::Store(
                "open the cache before reading documents".to_string(),
            ));
        }
        Ok(self.documents.clone())
    }

    /// The generation this cache believes in.
    pub fn generation(&self) -> Option<u64> {
        self.current
            .as_ref()
            .map(|generation| generation.generation)
    }

    /// Applies a metadata mutation.
    ///
    /// **This is the only mutation path**, and therefore the only place the generation is
    /// bumped. That is `INV-6`: invalidation is not a callback a caller must remember to
    /// install, it is the entry point itself.
    pub fn write(&mut self, write: MetadataWrite) -> Result<(), MetaCacheError> {
        if self.closed.get() {
            return Err(MetaCacheError::WriteRejected(
                RejectReason::Closed.describe(),
            ));
        }
        // Apply the mutation to the ledger first. The snapshot is never authoritative
        // (`INV-2`), so a failure here leaves the store as it was and the cache
        // untouched.
        let mut documents = self.read_ledger_documents()?;
        match write {
            MetadataWrite::Replace(replacement) => documents = replacement,
            MetadataWrite::Add(document) => documents.push(document),
            MetadataWrite::Remove(key) => documents.retain(|document| {
                document.get("key").and_then(Value::as_str) != Some(key.as_str())
            }),
        }
        self.ledger.replace_documents(documents.clone())?;

        // Then bump the generation, in the same logical operation. A snapshot built
        // before this call carries the old generation and is rejected on the next open.
        let next = match self.current.as_ref() {
            Some(current) => current.next().ok_or_else(|| {
                MetaCacheError::WriteRejected("generation would overflow".to_string())
            })?,
            None => self.ensure_generation(false)?.next().ok_or_else(|| {
                MetaCacheError::WriteRejected("generation would overflow".to_string())
            })?,
        };
        self.ledger.set_generation(&next)?;
        self.current = Some(next.clone());
        self.documents = documents;
        // The bump above is the invalidation: any snapshot built before this call now
        // carries an older generation and is rejected on the next open (`INV-3`).
        // Deleting it is only an optimisation — a store that refuses the delete (or a
        // read-only context) must not resurrect anything, which is why the correctness of
        // `INV-3` does not depend on this call succeeding.
        if let Some(name) = self.last_database_name() {
            let _ = self.store.remove_item(&crate::snapshot_key(&name));
        }
        Ok(())
    }

    /// Closes the cache. A closed cache refuses reads and writes.
    pub fn close(&self) {
        self.closed.set(true);
    }

    /// How many times this cache has read the metadata documents from the ledger.
    ///
    /// Exposed because `INV-9` is a *cost* invariant: a hit must cost exactly one ledger
    /// read for the generation, not one per collection.
    pub fn ledger_document_reads(&self) -> usize {
        self.ledger_reads.get()
    }

    /// How many snapshot writes this cache has attempted.
    pub fn snapshot_writes(&self) -> usize {
        self.snapshot_writes.get()
    }

    fn read_ledger_documents(&self) -> Result<Vec<Value>, MetaCacheError> {
        self.ledger_reads.set(self.ledger_reads.get() + 1);
        self.ledger.documents()
    }

    /// The generation document, created if absent.
    ///
    /// A pre-cache database has no generation document; opening it must create one and
    /// otherwise leave the database untouched (acceptance 13). `create` is false when the
    /// caller wants to look without creating, which `write` uses before deciding.
    fn ensure_generation(&self, create: bool) -> Result<GenerationDoc, MetaCacheError> {
        if let Some(document) = self.ledger.generation()? {
            return Ok(document);
        }
        if !create {
            return Ok(GenerationDoc::first(self.ledger.fresh_instance_id()));
        }
        let document = GenerationDoc::first(self.ledger.fresh_instance_id());
        self.ledger.set_generation(&document)?;
        Ok(document)
    }

    /// The generation document, creating it if absent, **never failing**.
    ///
    /// A store that refuses writes (quota, private mode, a closed context) must not stop
    /// a database from opening: the cache's whole contract is that it can be ignored
    /// (`INV-7`). If the document cannot be written, we proceed with an in-memory
    /// identity, every snapshot fails to validate, and the next open is simply a slow
    /// one.
    fn ensure_generation_best_effort(&self) -> GenerationDoc {
        if let Ok(Some(document)) = self.ledger.generation() {
            return document;
        }
        let document = GenerationDoc::first(self.ledger.fresh_instance_id());
        let _ = self.ledger.set_generation(&document);
        document
    }

    fn write_snapshot(&self, database_name: &str, generation: &GenerationDoc) {
        self.snapshot_writes.set(self.snapshot_writes.get() + 1);
        Snapshot::new(generation, self.documents.clone()).write(self.store.as_ref(), database_name);
    }

    /// The database name last passed to [`Self::open`].
    ///
    /// Stored so [`Self::write`] can drop the snapshot without the caller having to pass
    /// the name again, which would be one more thing to get wrong at a call site.
    fn last_database_name(&self) -> Option<String> {
        self.database_name.borrow().clone()
    }
}

impl RejectReason {
    /// A human-readable message. Named `describe` rather than `to_string` so it does not
    /// silently shadow `ToString`, which would make a later `format!` do something
    /// different than a reader expects.
    fn describe(self) -> String {
        match self {
            RejectReason::Closed => "the cache is closed".to_string(),
        }
    }
}
