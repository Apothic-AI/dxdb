//! A metadata ledger backed by `MemoryKv`, shared by both test binaries.
//!
//! Deliberately faithful rather than a mock: it stores metadata documents by key,
//! serves them in a stable order, and keeps the generation document. A double that
//! ignored the generation would make `INV-6` pass vacuously.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use dxdb_localstorage::{Kv, MemoryKv};
use dxdb_meta_cache::GENERATION_KEY;
use dxdb_meta_cache::{GenerationDoc, MetaCacheError, MetadataLedger};
use serde_json::{json, Value};

/// A ledger over a `MemoryKv`.
#[derive(Clone)]
pub struct KvLedger {
    kv: Rc<MemoryKv>,
    /// Counts document reads, so a test can prove a hit does not read them.
    reads: Rc<RefCell<usize>>,
    /// Counts generation writes.
    generation_writes: Rc<RefCell<usize>>,
    /// How many times this ledger's identity has been regenerated.
    ///
    /// Injected for tests; the real implementation draws 16 random bytes per
    /// incarnation, exactly as `dxdb-encryption` draws its salt.
    epoch: Rc<Cell<usize>>,
}

impl KvLedger {
    pub fn new() -> Self {
        Self {
            kv: Rc::new(MemoryKv::new()),
            reads: Rc::new(RefCell::new(0)),
            generation_writes: Rc::new(RefCell::new(0)),
            epoch: Rc::new(Cell::new(0)),
        }
    }

    pub fn kv(&self) -> Rc<MemoryKv> {
        Rc::clone(&self.kv)
    }

    pub fn document_reads(&self) -> usize {
        *self.reads.borrow()
    }

    /// Seeds `count` collection metadata documents.
    pub fn seed(&self, count: usize) -> Vec<Value> {
        let documents: Vec<Value> = (0..count)
            .map(|index| {
                json!({
                    "key": format!("collection-{index}"),
                    "data": {
                        "name": format!("collection-{index}"),
                        "schemaHash": format!("hash{index}"),
                        "version": 0
                    }
                })
            })
            .collect();
        self.replace_documents(documents.clone()).expect("seed");
        documents
    }

    /// Writes a generation document directly, bypassing the cache.
    ///
    /// Used to simulate another writer mutating metadata behind this cache's back,
    /// which is the situation `INV-1` exists for.
    pub fn bump_generation(&self) -> GenerationDoc {
        // After a `recreate` there is no document yet; a writer that mutates a
        // freshly-created database establishes generation 1 rather than panicking.
        let current = self
            .generation()
            .expect("read")
            .unwrap_or_else(|| GenerationDoc::first(self.fresh_instance_id()));
        let next = current.next().expect("room");
        self.set_generation(&next).expect("write");
        next
    }

    /// Removes the generation document, simulating a pre-cache database.
    pub fn clear_generation(&self) {
        self.kv.remove_item(GENERATION_KEY).expect("remove");
    }

    /// Simulates the database being dropped and recreated under the same name: the
    /// generation restarts, and the incarnation identity changes.
    pub fn recreate(&self) {
        self.clear_generation();
        self.epoch.set(self.epoch.get() + 1);
    }

    pub fn set_raw_generation(&self, text: &str) {
        self.kv.set_item(GENERATION_KEY, text).expect("write");
    }
}

impl Default for KvLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl MetadataLedger for KvLedger {
    fn documents(&self) -> Result<Vec<Value>, MetaCacheError> {
        *self.reads.borrow_mut() += 1;
        let mut documents = Vec::new();
        let mut keys = self
            .kv
            .keys()
            .map_err(|e| MetaCacheError::Store(e.to_string()))?;
        // Stable order, so a test failure prints a stable diff and so a snapshot's
        // document order does not depend on hash iteration.
        keys.sort();
        for key in keys {
            if key == GENERATION_KEY {
                continue;
            }
            let Some(raw) = self
                .kv
                .get_item(&key)
                .map_err(|e| MetaCacheError::Store(e.to_string()))?
            else {
                continue;
            };
            if let Ok(value) = serde_json::from_str::<Value>(&raw) {
                documents.push(value);
            }
        }
        Ok(documents)
    }

    fn replace_documents(&self, documents: Vec<Value>) -> Result<(), MetaCacheError> {
        // Remove the old ones first, so a replace that shrinks the set does not leave
        // orphans.
        let existing = self
            .kv
            .keys()
            .map_err(|e| MetaCacheError::Store(e.to_string()))?;
        for key in existing {
            if key != GENERATION_KEY {
                self.kv
                    .remove_item(&key)
                    .map_err(|e| MetaCacheError::Store(e.to_string()))?;
            }
        }
        for document in documents {
            let key = document
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or("unnamed")
                .to_string();
            self.kv
                .set_item(&key, &document.to_string())
                .map_err(|e| MetaCacheError::Store(e.to_string()))?;
        }
        Ok(())
    }

    fn generation(&self) -> Result<Option<GenerationDoc>, MetaCacheError> {
        let Some(raw) = self
            .kv
            .get_item(GENERATION_KEY)
            .map_err(|e| MetaCacheError::Store(e.to_string()))?
        else {
            return Ok(None);
        };
        // A corrupt document reads as missing rather than as a permanent veto.
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            return Ok(None);
        };
        Ok(GenerationDoc::from_value(&value))
    }

    fn set_generation(&self, document: &GenerationDoc) -> Result<(), MetaCacheError> {
        *self.generation_writes.borrow_mut() += 1;
        self.kv
            .set_item(GENERATION_KEY, &document.to_value().to_string())
            .map_err(|e| MetaCacheError::Store(e.to_string()))
    }

    fn fresh_instance_id(&self) -> String {
        // Derived from the number of `Recreate` steps taken, not from `kv.len()`: the key
        // count changes as documents come and go, so keying on it would hand out a new
        // identity on an ordinary open — which breaks `INV-3`'s premise that a mutation
        // is what invalidates. A recreation increments the counter, so a recreated
        // database gets a different identity without needing entropy in a test.
        let epoch = self.epoch.get();
        format!("instance-{epoch}")
    }
}
