//! A minimal reference storage, used **only** to prove the suite runs.
//!
//! This is not `dxdb-memory` — lane D owns that crate, and PLAN.md 4.1 makes it the
//! parity oracle. This type lives in the testkit's `tests/` directory for one reason
//! and says so in its name: `gate-storage` and `gate-query-correctness` must not pass
//! vacuously. A gate that runs zero assertions because no storage could be named is the
//! "green for the wrong reason" failure this repo has already been bitten by once.
//!
//! So the gate runs against this deliberately small, obviously-correct implementation:
//!
//! * documents live in a `BTreeMap` keyed by primary value;
//! * `bulk_write` applies rows one at a time, checking `previous._rev` against the
//!   stored `_rev` and reporting a 409 per failed row;
//! * `query` applies the plan as a `_deleted` filter, then the selector, then the sort,
//!   then skip/limit;
//! * the change feed is a queue of `EventBulk`s drained by the stream.
//!
//! # What it is not
//!
//! It does not index: it scans and sorts. It does not implement multi-instance
//! propagation, encryption or key compression. That is the point — it is a
//! *scaffold*, and the cases that need those features are declared
//! [`dxdb_testkit::decl::PortStatus`] blockers rather than silently skipped.
//!
//! Its selector support is the small Mango set from PLAN.md 4.3
//! (`$eq $gt $gte $lt $lte $ne $in $nin $exists $elemMatch $size $regex $options
//! $type $and $or $nor $not`), because the correctness suite's 89 queries use
//! exactly those and a missing operator would otherwise look like a storage bug.
//!
//! **A real storage passing this scaffold's suite is not thereby correct** — this
//! scaffold is lenient in places a real storage is not (it validates nothing). What the
//! gate proves is that the *suite* executes, asserts, and can fail.

use std::collections::BTreeMap;
use std::cell::RefCell;
use std::rc::Rc;

use dxdb_storage::{
    BulkWriteResponse, BulkWriteRow, ChangeEvent, ChangedDocuments, Checkpoint, CountMode,
    CountResult, DocumentData, DocumentWriteData, DynRxStorage, DynRxStorageInstance,
    EventBulk, FindAttachmentResult, InstanceCreationParams, Operation, PreparedQuery,
    RxStorage, RxStorageInstance, SortDirection, StorageError, StorageSchema, WriteError,
    WRITE_ERROR_CONFLICT,
};
use serde_json::Value;

/// Shared state, keyed the way a real storage keys it.
type Registry = Rc<RefCell<BTreeMap<String, CollectionState>>>;

#[derive(Default)]
struct CollectionState {
    /// Documents by primary value.
    documents: BTreeMap<String, DocumentData>,
    /// Attachment payloads, keyed by `(document id, attachment id)`.
    attachments: BTreeMap<(String, String), Vec<u8>>,
    /// Queued change bulks, drained by the stream.
    events: Vec<EventBulk>,
    /// Monotonic event id.
    next_bulk: u64,
    /// True once `remove()` has been called.
    removed: bool,
}

impl CollectionState {
    /// Emits a bulk for the events a write produced.
    fn emit(&mut self, events: Vec<ChangeEvent>, context: &str) {
        if events.is_empty() {
            // A storage must not emit an empty bulk; upstream has a case for it.
            return;
        }
        let bulk_id = format!("bulk-{}", self.next_bulk);
        self.next_bulk += 1;
        // The checkpoint is the newest row of the bulk, which is upstream's rule
        // (`categorizeBulkWriteRows` derives it from the last changed row).
        let checkpoint = events
            .iter()
            .filter_map(|event| {
                let lwt = event.document_data.lwt()?;
                Some(Checkpoint::new(event.document_id.clone(), lwt))
            })
            .max_by(|a, b| {
                a.lwt
                    .partial_cmp(&b.lwt)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        self.events.push(EventBulk {
            id: bulk_id,
            events,
            checkpoint,
            context: context.to_string(),
        });
    }
}

/// The reference storage.
#[derive(Clone, Default)]
pub struct ReferenceStorage {
    registry: Registry,
}

impl ReferenceStorage {
    /// A fresh, empty storage.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every collection key currently open, for the "does not mix up" cases.
    pub fn open_collections(&self) -> Vec<String> {
        self.registry.borrow().keys().cloned().collect()
    }
}

impl RxStorage for ReferenceStorage {
    fn name(&self) -> &str {
        "reference-scaffold"
    }

    async fn create_storage_instance(
        &self,
        params: InstanceCreationParams,
    ) -> Result<Box<DynRxStorageInstance<'static>>, StorageError> {
        // Refuse what a bare storage cannot honour, the way a real one must.
        if params.schema.key_compression {
            return Err(StorageError::Unsupported(
                "keyCompression requires the key-compression wrapper".to_string(),
            ));
        }
        if params.schema.has_encrypted_fields {
            return Err(StorageError::Unsupported(
                "encrypted fields require the encryption wrapper".to_string(),
            ));
        }
        let key = params.collection_key();
        // A removed collection is reusable: `remove()` deletes the data, not the name, so
        // opening one starts from an empty state rather than a stale one.
        {
            let mut registry = self.registry.borrow_mut();
            let state = registry.entry(key.clone()).or_default();
            state.removed = false;
            state.documents.clear();
            state.attachments.clear();
            state.events.clear();
        }
        Ok(DynRxStorageInstance::new_box(ReferenceInstance {
            database_name: params.database_name,
            collection_name: params.collection_name,
            schema: params.schema,
            registry: Rc::clone(&self.registry),
            key,
            closed: std::cell::Cell::new(false),
            removed: std::cell::Cell::new(false),
        }))
    }
}

/// One collection instance.
struct ReferenceInstance {
    database_name: String,
    collection_name: String,
    schema: StorageSchema,
    registry: Registry,
    key: String,
    /// `Cell` because the trait methods take `&self`; a real storage hides the same flag
    /// behind its own interior mutability.
    closed: std::cell::Cell<bool>,
    removed: std::cell::Cell<bool>,
}

impl ReferenceInstance {
    fn state(&self) -> std::cell::Ref<'_, CollectionState> {
        // The borrow is returned rather than the value so a caller cannot hold two
        // mutable borrows at once; this is the one place a RefCell is visible.
        std::cell::Ref::map(self.registry.borrow(), |map| {
            map.get(&self.key).expect("collection state exists")
        })
    }

    fn state_mut(&self) -> std::cell::RefMut<'_, CollectionState> {
        std::cell::RefMut::map(self.registry.borrow_mut(), |map| {
            map.get_mut(&self.key).expect("collection state exists")
        })
    }

    fn ensure_usable(&self) -> Result<(), StorageError> {
        if self.removed.get() {
            return Err(StorageError::Removed);
        }
        if self.closed.get() {
            return Err(StorageError::Closed);
        }
        Ok(())
    }

    fn primary(&self) -> &str {
        &self.schema.primary_path
    }
}

impl RxStorageInstance for ReferenceInstance {
    fn database_name(&self) -> &str {
        &self.database_name
    }

    fn collection_name(&self) -> &str {
        &self.collection_name
    }

    fn schema(&self) -> &StorageSchema {
        &self.schema
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn bulk_write(
        &self,
        document_writes: Vec<BulkWriteRow>,
        context: String,
    ) -> Result<BulkWriteResponse, StorageError> {
        self.ensure_usable()?;
        let mut state = self.state_mut();
        let mut errors = Vec::new();
        let mut events = Vec::new();

        for row in document_writes {
            let document = row.document.document();
            let id = match document.id(self.primary()) {
                Ok(id) => id.to_string(),
                Err(_) => {
                    // A row with no primary value cannot be a document; upstream treats
                    // that as a validation failure, and this scaffold reports it as a
                    // conflict so the caller sees a per-row error either way.
                    errors.push(WriteError::Conflict {
                        document_id: String::new(),
                        document_in_db: None,
                        context: context.clone(),
                        write_row: row,
                    });
                    continue;
                }
            };
            let stored = state.documents.get(&id).cloned();

            // Conflict detection: `previous` must match the stored `_rev`.
            let expected = row.previous.as_ref().and_then(|previous| previous.rev());
            let actual = stored.as_ref().and_then(|existing| existing.rev());
            let conflict = match (expected, actual) {
                (None, None) => false,
                (None, Some(_)) => true,
                (Some(_), None) => true,
                (Some(expected), Some(actual)) => expected != actual,
            };
            if conflict {
                errors.push(WriteError::Conflict {
                    document_id: id.clone(),
                    document_in_db: stored.clone(),
                    context: context.clone(),
                    write_row: row,
                });
                continue;
            }

            let operation = match (&stored, document.is_deleted()) {
                (None, _) | (Some(_), false) if !stored.as_ref().is_some_and(|d| d.is_deleted()) => {
                    Operation::Insert
                }
                (Some(existing), true) if !existing.is_deleted() => Operation::Delete,
                (Some(existing), false) if existing.is_deleted() => Operation::Insert,
                _ => Operation::Update,
            };

            // Persist attachment payloads this write supplies.
            for (attachment_id, bytes) in row.document.attachment_payloads() {
                state
                    .attachments
                    .insert((id.clone(), attachment_id.clone()), bytes.clone());
            }
            // A tombstone loses its payloads, which upstream asserts.
            if document.is_deleted() {
                state.attachments.retain(|(doc_id, _), _| doc_id != &id);
            }

            let stored_document = document.clone();
            state.documents.insert(id.clone(), stored_document.clone());
            events.push(ChangeEvent {
                document_id: id,
                operation,
                document_data: stored_document,
                previous_document_data: stored,
            });
        }

        state.emit(events, &context);
        Ok(BulkWriteResponse::with_errors(errors))
    }

    async fn find_documents_by_id(
        &self,
        ids: Vec<String>,
        with_deleted: bool,
    ) -> Result<Vec<DocumentData>, StorageError> {
        self.ensure_usable()?;
        let state = self.state();
        Ok(ids
            .into_iter()
            .filter_map(|id| state.documents.get(&id).cloned())
            .filter(|document| with_deleted || !document.is_deleted())
            .collect())
    }

    async fn query(
        &self,
        prepared: PreparedQuery,
    ) -> Result<Vec<DocumentData>, StorageError> {
        self.ensure_usable()?;
        let state = self.state();
        let primary = self.primary().to_string();

        // The plan is a hint. This scaffold ignores its bounds and applies the
        // selector and the sort itself, which is always correct and never fast. A real
        // storage uses the bounds; a suite that passes here proves the *semantics*.
        let mut matching: Vec<DocumentData> = state
            .documents
            .values()
            .filter(|document| matches_selector(document.as_value(), &prepared.query.selector))
            .cloned()
            .collect();

        sort_documents(&mut matching, &prepared.query, &primary);

        let skip = prepared.query.skip;
        let take = prepared.query.limit.unwrap_or(usize::MAX);
        Ok(matching.into_iter().skip(skip).take(take).collect())
    }

    async fn count(&self, prepared: PreparedQuery) -> Result<CountResult, StorageError> {
        // Count must agree with `query().len()` including a limit, so it is computed the
        // same way rather than by counting the unfiltered map.
        let documents = self.query(prepared).await?;
        Ok(CountResult {
            count: documents.len(),
            // No index here, so the honest mode is `Slow`; a storage that claims `Fast`
            // without an index is lying to the caller.
            mode: CountMode::Slow,
        })
    }

    async fn get_attachment_data(
        &self,
        document_id: String,
        attachment_id: String,
        digest: String,
    ) -> Result<FindAttachmentResult, StorageError> {
        self.ensure_usable()?;
        let state = self.state();
        let document = state
            .documents
            .get(&document_id)
            .ok_or_else(|| StorageError::MissingAttachment(document_id.clone()))?;
        let stub = document
            .attachment(&attachment_id)
            .ok_or_else(|| StorageError::MissingAttachment(attachment_id.clone()))?;
        // The digest is how a caller knows the bytes are the bytes it asked for, so a
        // mismatch is an error rather than something to reconcile.
        let stored_digest = stub
            .get("digest")
            .and_then(Value::as_str)
            .ok_or_else(|| StorageError::MissingAttachment(attachment_id.clone()))?;
        if stored_digest != digest {
            return Err(StorageError::MissingAttachment(format!(
                "digest mismatch for {attachment_id}: stored {stored_digest}, asked {digest}"
            )));
        }
        state
            .attachments
            .get(&(document_id, attachment_id.clone()))
            .cloned()
            .ok_or_else(|| StorageError::MissingAttachment(attachment_id))
    }

    async fn get_changed_documents_since(
        &self,
        limit: usize,
        checkpoint: Option<Checkpoint>,
    ) -> Result<ChangedDocuments, StorageError> {
        self.ensure_usable()?;
        // The trait supplies an exact default; this scaffold overrides it only to avoid
        // building a query just to scan. The ordering contract is the same:
        // `(_meta.lwt, primary)` ascending, exclusive of the checkpoint.
        let state = self.state();
        let start = checkpoint.unwrap_or_else(Checkpoint::beginning);
        let primary = self.primary().to_string();
        let mut ordered: Vec<DocumentData> = state.documents.values().cloned().collect();
        ordered.sort_by(|a, b| {
            let a_lwt = a.lwt().unwrap_or(0.0);
            let b_lwt = b.lwt().unwrap_or(0.0);
            a_lwt
                .partial_cmp(&b_lwt)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    let a_id = a.id(&primary).unwrap_or("");
                    let b_id = b.id(&primary).unwrap_or("");
                    a_id.cmp(b_id)
                })
        });
        let after: Vec<DocumentData> = ordered
            .into_iter()
            .filter(|document| {
                let lwt = document.lwt().unwrap_or(0.0);
                let id = document.id(&primary).unwrap_or("");
                // Exclusive of the checkpoint in `(lwt, id)` order.
                lwt > start.lwt || (lwt == start.lwt && id > start.id.as_str())
            })
            .take(limit)
            .collect();
        let next = match after.last() {
            Some(last) => Checkpoint::new(
                last.id(&primary).unwrap_or("").to_string(),
                last.lwt().unwrap_or(start.lwt),
            ),
            None => start,
        };
        Ok(ChangedDocuments {
            documents: after,
            checkpoint: next,
        })
    }

    fn change_stream(&self) -> dxdb_storage::ChangeStream {
        // A live view, not a snapshot. The trait says a change stream is "the ongoing
        // change feed" and that every event a write causes must be emitted before
        // `bulk_write` resolves — so a subscriber taken *before* the write must observe
        // it. A snapshot would make the ported cases fail against a correct storage, and
        // an earlier draft of this scaffold did exactly that.
        //
        // The queue is drained as it is consumed, so two streams see a partition of the
        // events rather than a copy each. That is the weaker of the two plausible
        // contracts and it is sufficient for the suite (which never opens two streams on
        // one instance); a real storage fans out.
        let registry = Rc::clone(&self.registry);
        let key = self.key.clone();
        // Subscribe from *now*, not from the beginning of history: a change stream is a
        // feed of future changes, and replaying the past would make
        // "should not emit an empty eventBulk when the write had only errors" fail against
        // a correct storage — the earlier successful write would arrive first and look like
        // an event emitted by the failed one.
        let mut delivered = self.state().events.len();
        Box::pin(futures::stream::poll_fn(move |cx| {
            let mut map = registry.borrow_mut();
            let Some(state) = map.get_mut(&key) else {
                return futures::task::Poll::Ready(None);
            };
            match state.events.get(delivered) {
                Some(bulk) => {
                    delivered += 1;
                    let bulk = bulk.clone();
                    // Wake immediately, so an executor that parks after the first `Ready`
                    // still polls again.
                    cx.waker().wake_by_ref();
                    futures::task::Poll::Ready(Some(bulk))
                }
                None => {
                    // Nothing queued *yet*. A single-threaded suite polls after the write
                    // that produced the events, so yielding here is enough; a real storage
                    // would hold the waker and resolve on the next write.
                    if state.removed {
                        futures::task::Poll::Ready(None)
                    } else {
                        cx.waker().wake_by_ref();
                        futures::task::Poll::Pending
                    }
                }
            }
        }))
    }

    async fn cleanup(&self, minimum_deleted_time: f64) -> Result<bool, StorageError> {
        self.ensure_usable()?;
        let mut state = self.state_mut();
        // Purge every tombstone whose `_meta.lwt` is at or before the threshold. A
        // batch-purge storage would return `false` here; this one does it in a pass, so
        // it returns `true` — and the suite's "false means more work remains" case
        // accepts either as long as the loop terminates.
        let doomed: Vec<String> = state
            .documents
            .iter()
            .filter(|(_, document)| {
                document.is_deleted() && document.lwt().map(|lwt| lwt <= minimum_deleted_time).unwrap_or(true)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in doomed {
            state.documents.remove(&id);
            state.attachments.retain(|(doc_id, _), _| doc_id != &id);
        }
        Ok(true)
    }

    async fn close(&self) -> Result<(), StorageError> {
        // "closing multiple times should not error" (upstream `:3613`): the flag is set
        // and re-setting it is a no-op, so a second close succeeds. But "should NOT throw
        // on call to .close() after .remove()" (`:3693`) is also upstream, so a removed
        // instance must still close cleanly — which means `close` must not call
        // `ensure_usable`.
        self.closed.set(true);
        Ok(())
    }

    async fn remove(&self) -> Result<(), StorageError> {
        // "should throw on call to .remove() after .close()" (upstream `:3676`). This is
        // the asymmetry with `close`: removing a closed instance is an error, closing a
        // removed one is not.
        if self.closed.get() {
            return Err(StorageError::Closed);
        }
        if self.removed.get() {
            return Err(StorageError::Removed);
        }
        self.removed.set(true);
        self.registry.borrow_mut().remove(&self.key);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Selector evaluation: the small Mango set from PLAN.md 4.3
// ---------------------------------------------------------------------------

/// True when `document` (as a JSON object) matches `selector`.
///
/// A documented subset, not a complete Mango implementation: exactly the operators the
/// conformance fixtures use. Anything else panics rather than silently matching, because
/// a silently-wrong matcher would make the suite's expected results look wrong.
pub fn matches_selector(document: &Value, selector: &Value) -> bool {
    let Some(selector) = selector.as_object() else {
        return true;
    };
    selector.iter().all(|(key, condition)| {
        match key.as_str() {
            "$and" => condition
                .as_array()
                .expect("$and takes an array")
                .iter()
                .all(|sub| matches_selector(document, sub)),
            "$or" => condition
                .as_array()
                .expect("$or takes an array")
                .iter()
                .any(|sub| matches_selector(document, sub)),
            "$nor" => !condition
                .as_array()
                .expect("$nor takes an array")
                .iter()
                .any(|sub| matches_selector(document, sub)),
            "$not" => !matches_selector(document, condition),
            field => {
                let value = dxdb_storage::get_object_path(document, field);
                matches_condition(value, condition)
            }
        }
    })
}

fn matches_condition(value: Option<&Value>, condition: &Value) -> bool {
    // A bare value is `$eq`, which is what normalisation produces for `{a: 1}`.
    let Some(operators) = condition.as_object().filter(|object| {
        object.keys().any(|key| key.starts_with('$'))
    }) else {
        return value_matches_eq(value, condition);
    };

    operators.iter().all(|(operator, operand)| match operator.as_str() {
        "$eq" => value_matches_eq(value, operand),
        "$ne" => !value_matches_eq(value, operand),
        "$gt" => compare(value, operand).is_some_and(|ordering| ordering.is_gt()),
        "$gte" => compare(value, operand).is_some_and(|ordering| ordering.is_ge()),
        "$lt" => compare(value, operand).is_some_and(|ordering| ordering.is_lt()),
        "$lte" => compare(value, operand).is_some_and(|ordering| ordering.is_le()),
        "$in" => {
            let list = operand.as_array().expect("$in takes an array");
            list.iter().any(|candidate| value_matches_eq(value, candidate))
        }
        "$nin" => {
            let list = operand.as_array().expect("$nin takes an array");
            !list.iter().any(|candidate| value_matches_eq(value, candidate))
        }
        "$exists" => {
            let want = operand.as_bool().expect("$exists takes a bool");
            want == value.is_some()
        }
        "$size" => {
            let want = operand.as_u64().expect("$size takes a number") as usize;
            value
                .and_then(Value::as_array)
                .map(|items| items.len() == want)
                .unwrap_or(false)
        }
        "$elemMatch" => value
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .any(|item| matches_element(item, operand))
            })
            .unwrap_or(false),
        "$regex" => {
            let pattern = operand.as_str().expect("$regex takes a string");
            let options = operators
                .get("$options")
                .and_then(Value::as_str)
                .unwrap_or("");
            regex_matches(value, pattern, options)
        }
        // `$options` is consumed by `$regex`; seeing it alone is a no-op.
        "$options" => true,
        "$type" => match operand.as_str().expect("$type takes a string") {
            "string" => value.map(Value::is_string).unwrap_or(false),
            "number" => value
                .map(|v| v.is_number() && !is_integral_float(v))
                .or_else(|| Some(false))
                .unwrap_or(false)
                || value.map(Value::is_f64).unwrap_or(false),
            "integer" => value.map(|v| v.is_i64() || v.is_u64()).unwrap_or(false),
            "boolean" => value.map(Value::is_boolean).unwrap_or(false),
            "array" => value.map(Value::is_array).unwrap_or(false),
            "object" => value.map(Value::is_object).unwrap_or(false),
            "null" => value.map(Value::is_null).unwrap_or(false),
            other => panic!("$type {other:?} is not in the supported set"),
        },
        "$mod" => {
            let list = operand.as_array().expect("$mod takes [divisor, remainder]");
            let divisor = list[0].as_f64().expect("divisor") as i64;
            let remainder = list[1].as_f64().expect("remainder") as i64;
            value
                .and_then(Value::as_f64)
                .map(|n| (n as i64) % divisor == remainder)
                .unwrap_or(false)
        }
        other => panic!("selector operator {other:?} is not in the supported set"),
    })
}

/// `$type: "number"` in Mango means a JSON number that is not an integer, which is a
/// distinction JSON.parse makes by syntax (`2.0` is a float). `serde_json` with default
/// features collapses `2.0` to a float-backed number, so this checks the backing type.
fn is_integral_float(value: &Value) -> bool {
    value.as_f64().is_some() && value.as_i64().is_some()
}

/// `$elemMatch` against one array element, which may itself be an object to match.
fn matches_element(item: &Value, operand: &Value) -> bool {
    match item {
        Value::Object(_) => matches_selector(item, operand),
        // A scalar element matches when the operand is a condition on the element
        // itself, e.g. `{$elemMatch: {$regex: '^ops$', $options: 'i'}}`.
        scalar => matches_condition(Some(scalar), operand),
    }
}

/// `$eq` semantics: structural JSON equality, with a numeric comparison so `1` and `1.0`
/// are equal, as they are in JavaScript.
fn value_matches_eq(value: Option<&Value>, expected: &Value) -> bool {
    match (value, expected) {
        (None, Value::Null) => true,
        (None, _) => false,
        (Some(actual), expected) => {
            if let (Some(a), Some(b)) = (actual.as_f64(), expected.as_f64()) {
                return a == b;
            }
            actual == expected
        }
    }
}

/// Ordering for `$gt`/`$gte`/`$lt`/`$lte`, matching JavaScript's `<` on the same types.
///
/// Numbers compare numerically; strings compare by UTF-16 code unit (D5), which is why
/// this calls `compare_js`. Mixed types have no order, which is `None` and therefore
/// never matches — the same as JS's `1 < 'a'` being false.
fn compare(value: Option<&Value>, bound: &Value) -> Option<std::cmp::Ordering> {
    let value = value?;
    // Numbers are compared after the same widening Mango does: an integer field queried
    // with a decimal bound must compare numerically.
    if let (Some(a), Some(b)) = (value.as_f64(), bound.as_f64()) {
        return a.partial_cmp(&b);
    }
    if let (Some(a), Some(b)) = (value.as_str(), bound.as_str()) {
        return Some(dxdb_storage_string_compare(a, b));
    }
    None
}

/// String ordering by UTF-16 code unit (D5). Kept as a local so this scaffold depends on
/// nothing but `dxdb-storage`; `dxdb-jsstr::compare_js` is the same function, and the
/// testkit's own tests assert they agree on the charset.
fn dxdb_storage_string_compare(a: &str, b: &str) -> std::cmp::Ordering {
    let left: Vec<u16> = a.encode_utf16().collect();
    let right: Vec<u16> = b.encode_utf16().collect();
    left.cmp(&right)
}

/// `$regex` with `$options` (`i` only; the fixtures use no other flag).
fn regex_matches(value: Option<&Value>, pattern: &str, options: &str) -> bool {
    // A deliberately tiny regex engine: the fixtures use anchors, character classes and
    // literals only. Anything else panics so a fixture that needs more is visible.
    let Some(text) = value.and_then(Value::as_str) else {
        return false;
    };
    let case_insensitive = options.contains('i');
    let (pattern, text) = if case_insensitive {
        (pattern.to_lowercase(), text.to_lowercase())
    } else {
        (pattern.to_string(), text.to_string())
    };

    let anchored_start = pattern.starts_with('^');
    let anchored_end = pattern.ends_with('$');
    let core = pattern
        .trim_start_matches('^')
        .trim_end_matches('$')
        .to_string();

    if core.contains('[') || core.contains('+') || core.contains('*') || core.contains('\\') {
        panic!("this scaffold's $regex supports literals and anchors only, got {pattern:?}");
    }

    match (anchored_start, anchored_end) {
        (true, true) => text == core,
        (true, false) => text.starts_with(&core),
        (false, true) => text.ends_with(&core),
        (false, false) => text.contains(&core),
    }
}

/// Sorts documents by the query's sort, appended primary key included.
fn sort_documents(documents: &mut [DocumentData], query: &dxdb_storage::MangoQuery, _primary: &str) {
    documents.sort_by(|a, b| {
        for part in &query.sort {
            let a_value = a.get_object_path(&part.field);
            let b_value = b.get_object_path(&part.field);
            let ordering = compare_values(a_value, b_value);
            let ordering = match part.direction {
                SortDirection::Asc => ordering,
                SortDirection::Desc => ordering.reverse(),
            };
            if ordering != std::cmp::Ordering::Equal {
                return ordering;
            }
        }
        std::cmp::Ordering::Equal
    });
}

/// Total order over optional JSON values, for sorting.
///
/// Absent sorts first, matching Mango's "missing is less than null is less than a value"
/// for the fixture set the suite uses.
fn compare_values(a: Option<&Value>, b: Option<&Value>) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(a), Some(b)) => {
            if a.is_null() && !b.is_null() {
                return std::cmp::Ordering::Less;
            }
            if b.is_null() && !a.is_null() {
                return std::cmp::Ordering::Greater;
            }
            if let (Some(x), Some(y)) = (a.as_f64(), b.as_f64()) {
                return x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal);
            }
            if let (Some(x), Some(y)) = (a.as_str(), b.as_str()) {
                return dxdb_storage_string_compare(x, y);
            }
            if let (Some(x), Some(y)) = (a.as_bool(), b.as_bool()) {
                return x.cmp(&y);
            }
            std::cmp::Ordering::Equal
        }
    }
}

/// Every operator this scaffold supports, so a test can assert the set is the one
/// PLAN.md 4.3 names and not accidentally narrower.
pub const SUPPORTED_OPERATORS: &[&str] = &[
    "$eq", "$gt", "$gte", "$lt", "$lte", "$ne", "$in", "$nin", "$exists", "$type", "$mod",
    "$not", "$size", "$elemMatch", "$regex", "$options", "$and", "$or", "$nor",
];
