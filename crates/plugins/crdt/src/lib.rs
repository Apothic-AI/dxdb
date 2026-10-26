//! Conflict-free replicated documents via an operation log.
//!
//! Ported from `src/plugins/crdt/index.ts` (577 lines) at upstream `3b763ee17`.
//!
//! # The model
//!
//! A CRDT collection marks one field as the CRDT field (`schema.crdt.field`), and every
//! write to such a document becomes an **operation** appended to it:
//!
//! ```text
//! crdts: {
//!   operations: [ [op], [op], [op] ],   // one inner array per write
//!   hash: "…"                            // hash of the creators
//! }
//! ```
//!
//! An operation is `{body: [entry…], creator, time}` (`index.ts:56-58`). Each `entry` is an
//! `{selector?, ifMatch?, ifNotMatch?}` triple: if the document matches `selector`
//! (or there is no selector), apply `ifMatch` as a mingo update; if it does not, apply
//! `ifNotMatch` (`:141-168`).
//!
//! The document's *visible* fields are then a **fold of the log** — and this is the point:
//! [`rebuild_from_crdt`] replays every operation over a base `{_deleted: false}` and
//! produces the document. Two replicas that have seen the same set of operations therefore
//! produce the same document, regardless of the order they arrived in, because merging
//! sorts by creator (`:132-134`, `:262-266`).
//!
//! # Why the merge is order-insensitive, and where that breaks down
//!
//! [`merge_crdt_fields`] (`:238-277`) walks both sides index by index, unions the operations
//! in each inner array, deduplicates **by `creator`** (`:257-263`), and sorts by creator.
//! That is a deterministic function of the *set* of operations, so merging is commutative —
//! which is what makes the conflict handler work (`:296-323`).
//!
//! It is only commutative for operations whose effects do **not** depend on order. Two
//! `$set`s to the same field resolve by creator order, deterministically but arbitrarily;
//! two `$inc`s both apply (which is the point — see the upstream test at `:617`,
//! "should merge the +1 increments"). A port that reordered operations differently would
//! break that, so the sort comparator is pinned by a test.
//!
//! # The write path is closed
//!
//! Upstream replaces eight document methods: `remove`, `incrementalRemove`, `patch`,
//! `incrementalPatch` and `update` are **redirected** through `updateCRDT` (`:339-437`),
//! while `modify`/`incrementalModify` **throw** `CRDT4`/`CRDT2` unless the caller is the
//! plugin itself (context `rx-crdt`, `:352-370`, `:437-450`). The reason is that a CRDT
//! document's visible fields must only ever change through the log: a plain `modify` would
//! write a field without recording an operation, and the next merge would silently undo it.
//!
//! That rule is modelled as [`CrdtError::DirectModify`], and the redirect is
//! [`redirect_patch`] / [`redirect_update`] rather than a patch of eight methods.
//!
//! # Not ported
//!
//! The document-method interception itself (an extension trait surfaces the equivalent),
//! `incrementalModify`'s retry loop, and the async `hashFunction` (this crate hashes
//! synchronously via `dxdb_json::canonical_bytes`, which is the same SHA-256 input
//! canonicalisation the schema hash uses). The CRDT field shape is
//! [`CrdtDocumentField`], and the schema fragment is [`crdt_schema_part`] so a caller can
//! compose it into their schema exactly as `getCRDTSchemaPart` is used.

#![forbid(unsafe_code)]

use serde_json::{json, Map, Value};
use thiserror::Error;

/// The context string the plugin uses to let its own write through
/// (`index.ts:325`).
pub const CRDT_CONTEXT: &str = "rx-crdt";

/// Errors, carrying upstream's codes.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum Error {
    /// `CRDT1`: the schema has no `crdt` block, so it is not a CRDT collection
    /// (`index.ts:39-44`, `:91-96`).
    #[error("CRDT1: the schema has no crdt block, so it cannot be written through updateCRDT")]
    NotACrdtSchema,
    /// `CRDT2`: `incrementalModify` was called from outside the plugin (`index.ts:445-449`).
    #[error("CRDT2: incrementalModify is not allowed on a CRDT document (context was {0})")]
    DirectIncrementalModify(String),
    /// `CRDT4`: `modify` was called on a CRDT document (`index.ts:365-369`).
    #[error("CRDT4: modify is not allowed on a CRDT document")]
    DirectModify,
    /// The CRDT field is not where the schema says it is.
    #[error("the CRDT field '{0}' is not an object in the document")]
    MissingCrdtField(String),
    /// An operation is not shaped like an operation.
    #[error("malformed CRDT operation: {0}")]
    Malformed(String),
    /// An `ifMatch`/`ifNotMatch` update was rejected by the modifier engine.
    #[error("CRDT update rejected: {0}")]
    Update(String),
}

/// Result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// One `{selector?, ifMatch?, ifNotMatch?}` entry (`index.ts:141-168`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CrdtEntry {
    /// When present, an `ifMatch` applies only if the document matches this selector.
    pub selector: Option<Value>,
    /// The update applied when the selector matches (or there is none).
    pub if_match: Option<Value>,
    /// The update applied when the selector does **not** match.
    pub if_not_match: Option<Value>,
}

impl CrdtEntry {
    /// An unconditional `$set`-style entry, which is what `patch`/`update` redirect to
    /// (`index.ts:381-437`).
    pub fn if_match(update: Value) -> Self {
        CrdtEntry {
            selector: None,
            if_match: Some(update),
            if_not_match: None,
        }
    }

    /// An entry with a selector and both branches (`ifMatch`/`ifNotMatch`).
    ///
    /// Upstream's `updateCRDT` reads `entry.selector` / `entry.ifMatch` / `entry.ifNotMatch`
    /// from the *entry itself*, not from the operation, so a selector travels with the entry
    /// (`:141-168`).
    pub fn conditional(
        selector: Value,
        if_match: Option<Value>,
        if_not_match: Option<Value>,
    ) -> Self {
        CrdtEntry {
            selector: Some(selector),
            if_match,
            if_not_match,
        }
    }

    /// The wire shape.
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        if let Some(selector) = &self.selector {
            map.insert("selector".to_string(), selector.clone());
        }
        if let Some(update) = &self.if_match {
            map.insert("ifMatch".to_string(), update.clone());
        }
        if let Some(update) = &self.if_not_match {
            map.insert("ifNotMatch".to_string(), update.clone());
        }
        Value::Object(map)
    }

    /// Reads the wire shape.
    pub fn from_json(value: &Value) -> Result<Self> {
        let map = value
            .as_object()
            .ok_or_else(|| Error::Malformed(format!("an entry must be an object, got {value}")))?;
        Ok(CrdtEntry {
            selector: map.get("selector").cloned(),
            if_match: map.get("ifMatch").cloned(),
            if_not_match: map.get("ifNotMatch").cloned(),
        })
    }
}

/// One operation (`index.ts:55-59`).
#[derive(Clone, Debug, PartialEq)]
pub struct CrdtOperation {
    /// The entries.
    pub body: Vec<CrdtEntry>,
    /// The writing instance's storage token. Also the merge tie-break.
    pub creator: String,
    /// When it was written.
    pub time: f64,
}

impl CrdtOperation {
    /// An operation.
    pub fn new(body: Vec<CrdtEntry>, creator: impl Into<String>, time: f64) -> Self {
        CrdtOperation {
            body,
            creator: creator.into(),
            time,
        }
    }

    /// The wire shape.
    pub fn to_json(&self) -> Value {
        json!({
            "body": self.body.iter().map(CrdtEntry::to_json).collect::<Vec<_>>(),
            "creator": self.creator,
            "time": self.time,
        })
    }

    /// Reads the wire shape.
    pub fn from_json(value: &Value) -> Result<Self> {
        let map = value
            .as_object()
            .ok_or_else(|| Error::Malformed(value.to_string()))?;
        let body = map
            .get("body")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Malformed("an operation needs a body array".to_string()))?
            .iter()
            .map(CrdtEntry::from_json)
            .collect::<Result<Vec<_>>>()?;
        if body.is_empty() {
            return Err(Error::Malformed(
                "an operation body needs at least one entry (schema: minItems 1)".to_string(),
            ));
        }
        Ok(CrdtOperation {
            body,
            creator: map
                .get("creator")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Malformed("an operation needs a creator".to_string()))?
                .to_string(),
            time: map.get("time").and_then(Value::as_f64).unwrap_or(0.0),
        })
    }
}

/// The CRDT field's value (`CRDTDocumentField`, `index.ts:60`).
///
/// `operations` is an array of arrays: one inner array per write. That nesting is not
/// decorative — [`merge_crdt_fields`] pairs writes by index, so a flat list would lose the
/// grouping that makes concurrent writes distinguishable.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CrdtDocumentField {
    /// One inner array per write.
    pub operations: Vec<Vec<CrdtOperation>>,
    /// A hash of the operations' creators (`hashCRDTOperations`, `:170-179`).
    ///
    /// Upstream hashes `operations.map(ops => ops.map(op => op.creator))` — the *creators
    /// only*, not the bodies. That is enough to answer "are these two CRDT states the same"
    /// for the conflict handler's `isEqual` (`:303-305`), because two replicas that have
    /// seen the same writes have the same creators per slot.
    pub hash: String,
}

impl CrdtDocumentField {
    /// An empty field.
    pub fn new() -> Self {
        Self::default()
    }

    /// The wire shape.
    pub fn to_json(&self) -> Value {
        json!({
            "operations": self
                .operations
                .iter()
                .map(|row| row.iter().map(CrdtOperation::to_json).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            "hash": self.hash,
        })
    }

    /// Reads the wire shape.
    pub fn from_json(value: &Value) -> Result<Self> {
        let map = value
            .as_object()
            .ok_or_else(|| Error::Malformed(value.to_string()))?;
        let operations = map
            .get("operations")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Malformed("crdts needs operations".to_string()))?
            .iter()
            .map(|row| {
                row.as_array()
                    .ok_or_else(|| {
                        Error::Malformed("each operations row must be an array".to_string())
                    })?
                    .iter()
                    .map(CrdtOperation::from_json)
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(CrdtDocumentField {
            operations,
            hash: map
                .get("hash")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    }

    /// `hashCRDTOperations` (`:170-179`): SHA-256 of the creators, in the same
    /// canonicalisation `dxdb-schema` uses.
    pub fn recompute_hash(&mut self) {
        let creators: Vec<Vec<&str>> = self
            .operations
            .iter()
            .map(|row| row.iter().map(|op| op.creator.as_str()).collect())
            .collect();
        let bytes = dxdb_json::canonical_bytes(&json!(creators));
        self.hash = dxdb_schema::sha256_hex(&bytes);
    }
}

/// The CRDT schema fragment (`getCRDTSchemaPart`, `:182-235`).
///
/// Returned as a `Value` so a caller composes it the way upstream does — assigning it to
/// `schema.properties[field]`.
pub fn crdt_schema_part() -> Value {
    json!({
        "type": "object",
        "properties": {
            "operations": {
                "type": "array",
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "body": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "selector": { "type": "object" },
                                        "ifMatch": { "type": "object" },
                                        "ifNotMatch": { "type": "object" }
                                    },
                                    "additionalProperties": false
                                },
                                "minItems": 1
                            },
                            "creator": { "type": "string" },
                            "time": {
                                "type": "number",
                                "minimum": 1,
                                "maximum": 1_000_000_000_000_000.0,
                                "multipleOf": 0.01
                            }
                        },
                        "additionalProperties": false,
                        "required": ["body", "creator", "time"]
                    }
                }
            },
            "hash": {
                "type": "string",
                // "set a minLength to not accidentally store an empty string" (`:224`)
                "minLength": 2
            }
        },
        "additionalProperties": false,
        "required": ["operations", "hash"]
    })
}

/// `sortOperationComparator` (`:132-134`): sort by creator, ascending.
///
/// Upstream returns `a.creator > b.creator ? 1 : -1`, i.e. it never returns 0. That matters
/// for equal creators, where it is an unstable direction, but a creator is a storage token
/// and two operations from one creator in one slot would be a bug rather than a tie. Kept
/// as `<`/`>` with `Equal` returned honestly so the sort is total, and descending-`>` for
/// `>` is upstream's.
pub fn compare_operations(left: &CrdtOperation, right: &CrdtOperation) -> std::cmp::Ordering {
    dxdb_jsstr::compare_js(&left.creator, &right.creator)
}

/// `runOperationOnDocument` (`:136-168`).
///
/// For each entry: if it has a selector and the document matches, apply `ifMatch`; if it has
/// a selector and does not match, apply `ifNotMatch`; if it has no selector, apply `ifMatch`.
///
/// `matches` is the caller's selector evaluator, because matching is `dxdb-query`'s job. It
/// is a parameter rather than an import so this crate does not depend on the query engine for
/// a fold — and so a test can drive both branches without a schema.
pub fn run_operation_on_document<F>(
    document: &Value,
    operation: &CrdtOperation,
    matches: F,
) -> Result<Value>
where
    F: Fn(&Value, &Value) -> bool,
{
    let mut current = document.clone();
    for entry in &operation.body {
        // `if (entryPart.selector) { ... } else { isMatching = true; }`
        let is_matching = match &entry.selector {
            Some(selector) => matches(&current, selector),
            None => true,
        };
        let update = if is_matching {
            entry.if_match.as_ref()
        } else {
            entry.if_not_match.as_ref()
        };
        if let Some(update) = update {
            current = dxdb_plugin_update::apply_update(&current, update)
                .map_err(|error| Error::Update(error.to_string()))?;
        }
    }
    Ok(current)
}

/// `rebuildFromCRDT` (`:279-295`).
///
/// Starts from `{_deleted: false}`, sets the CRDT field, then replays every operation in
/// order. The base is deliberately minimal: the visible fields are *derived*, so seeding them
/// from the stored document would let a stale field survive a rebuild.
pub fn rebuild_from_crdt<F>(
    document: &Value,
    field: &str,
    crdts: &CrdtDocumentField,
    matches: F,
) -> Result<Value>
where
    F: Fn(&Value, &Value) -> bool,
{
    let mut base = json!({ "_deleted": false });
    if let Value::Object(map) = &mut base {
        map.insert(field.to_string(), crdts.to_json());
    }
    for row in &crdts.operations {
        for operation in row {
            base = run_operation_on_document(&base, operation, &matches)?;
        }
    }
    let _ = document;
    Ok(base)
}

/// `mergeCRDTFields` (`:238-277`).
///
/// Swaps so the side with more rows is `A` ("to ensure we not miss out rows when iterating
/// over both fields", `:243-245`), then per index unions the operations, deduplicating by
/// creator, and sorts by creator. The result is a deterministic function of the union of
/// both sides' operations, which is why merging is commutative.
pub fn merge_crdt_fields(left: &CrdtDocumentField, right: &CrdtDocumentField) -> CrdtDocumentField {
    let (longer, shorter) = if left.operations.len() < right.operations.len() {
        (right, left)
    } else {
        (left, right)
    };

    let mut merged = CrdtDocumentField::new();
    for (index, row) in longer.operations.iter().enumerate() {
        let mut ops: Vec<CrdtOperation> = Vec::new();
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for operation in row {
            seen.insert(operation.creator.clone());
            ops.push(operation.clone());
        }
        if let Some(other_row) = shorter.operations.get(index) {
            for operation in other_row {
                if !seen.contains(&operation.creator) {
                    seen.insert(operation.creator.clone());
                    ops.push(operation.clone());
                }
            }
        }
        ops.sort_by(compare_operations);
        merged.operations.push(ops);
    }
    merged.recompute_hash();
    merged
}

/// The conflict handler's `isEqual` (`:303-305`): two CRDT states are equal when their
/// hashes are, because the hash covers the creators.
pub fn crdt_states_equal(left: &CrdtDocumentField, right: &CrdtDocumentField) -> bool {
    left.hash == right.hash
}

/// The conflict handler's `resolve` (`:306-321`): merge, then rebuild.
pub fn resolve_crdt_conflict<F>(
    field: &str,
    new_document: &Value,
    master_document: &Value,
    matches: F,
) -> Result<Value>
where
    F: Fn(&Value, &Value) -> bool + Copy,
{
    let new_crdt = read_crdt_field(new_document, field)?;
    let master_crdt = read_crdt_field(master_document, field)?;
    let merged = merge_crdt_fields(&new_crdt, &master_crdt);
    rebuild_from_crdt(new_document, field, &merged, matches)
}

/// Reads the CRDT field out of a document (`:60`, `:301-302`).
pub fn read_crdt_field(document: &Value, field: &str) -> Result<CrdtDocumentField> {
    let value = document
        .get(field)
        .ok_or_else(|| Error::MissingCrdtField(field.to_string()))?;
    CrdtDocumentField::from_json(value)
}

/// `updateCRDT`'s transformation, split from the write (`:26-84`).
///
/// Appends `entries` as a new inner array, recomputes the hash, applies the operation to the
/// document's visible fields, and writes the CRDT field back. `creator` is the storage token
/// and `time` the write time; both are parameters so the transformation is deterministic.
pub fn append_operation<F>(
    document: &Value,
    field: &str,
    entries: Vec<CrdtEntry>,
    creator: &str,
    time: f64,
    matches: F,
) -> Result<Value>
where
    F: Fn(&Value, &Value) -> bool + Copy,
{
    if entries.is_empty() {
        return Err(Error::Malformed(
            "updateCRDT needs at least one entry".to_string(),
        ));
    }
    let mut crdts = read_crdt_field(document, field)?;
    let operation = CrdtOperation::new(entries, creator, time);
    // "A new write will ALWAYS be an operation in the last array which was non existing
    // before" (`:66-69`) — a new inner array, always.
    crdts.operations.push(vec![operation.clone()]);
    crdts.recompute_hash();

    let with_visible = run_operation_on_document(document, &operation, matches)?;
    let mut output = with_visible;
    if let Value::Object(map) = &mut output {
        map.insert(field.to_string(), crdts.to_json());
    }
    Ok(output)
}

/// `insertCRDT`'s transformation, split from the insert (`:88-130`).
///
/// Builds a document from nothing: `{_deleted: false}`, the operation applied, then the CRDT
/// field. On an insert conflict upstream updates instead (`:116-127`); that is the caller's
/// branch because it needs a collection.
pub fn seeded_document<F>(
    field: &str,
    entries: Vec<CrdtEntry>,
    creator: &str,
    time: f64,
    matches: F,
) -> Result<Value>
where
    F: Fn(&Value, &Value) -> bool + Copy,
{
    if entries.is_empty() {
        return Err(Error::Malformed(
            "insertCRDT needs at least one entry".to_string(),
        ));
    }
    let operation = CrdtOperation::new(entries, creator, time);
    let mut document =
        run_operation_on_document(&json!({ "_deleted": false }), &operation, matches)?;
    let mut crdts = CrdtDocumentField::new();
    crdts.operations.push(vec![operation]);
    crdts.recompute_hash();
    if let Value::Object(map) = &mut document {
        map.insert(field.to_string(), crdts.to_json());
    }
    Ok(document)
}

/// The redirects that keep a CRDT document's visible fields derivable (`:339-437`).
///
/// `patch`, `incrementalPatch`, `update`, `remove` and `incrementalRemove` all become
/// `updateCRDT({ifMatch: ...})`; `modify`/`incrementalModify` are refused.
pub fn redirect_patch(patch: &Value) -> CrdtEntry {
    // `this.updateCRDT({ ifMatch: { $set: patch } })` (`:383-387`).
    CrdtEntry::if_match(json!({ "$set": patch }))
}

/// `remove`/`incrementalRemove` become `updateCRDT({ifMatch: {$set: {_deleted: true}}})`
/// (`:341-360`).
pub fn redirect_remove() -> CrdtEntry {
    CrdtEntry::if_match(json!({ "$set": { "_deleted": true } }))
}

/// `update` becomes `updateCRDT({ifMatch: updateObj})` (`:436-441`).
pub fn redirect_update(update: &Value) -> CrdtEntry {
    CrdtEntry::if_match(update.clone())
}

/// The refusal `incrementalModify` raises unless the caller is the plugin (`:437-450`).
pub fn check_incremental_modify(context: &str) -> Result<()> {
    if context == CRDT_CONTEXT {
        Ok(())
    } else {
        Err(Error::DirectIncrementalModify(context.to_string()))
    }
}

/// The refusal `modify` always raises on a CRDT document (`:362-370`).
pub fn check_modify() -> Result<()> {
    Err(Error::DirectModify)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn always_matches(_document: &Value, _selector: &Value) -> bool {
        true
    }

    fn never_matches(_document: &Value, _selector: &Value) -> bool {
        false
    }

    #[test]
    fn the_schema_part_has_the_two_required_fields() {
        let part = crdt_schema_part();
        let required: Vec<&str> = part["required"]
            .as_array()
            .expect("required")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(required, vec!["operations", "hash"]);
        assert_eq!(part["properties"]["hash"]["minLength"], json!(2));
        // The inner body needs at least one entry.
        assert_eq!(
            part["properties"]["operations"]["items"]["items"]["properties"]["body"]["minItems"],
            json!(1)
        );
    }

    #[test]
    fn a_crdt_field_round_trips_through_json() {
        let mut field = CrdtDocumentField::new();
        field.operations.push(vec![CrdtOperation::new(
            vec![CrdtEntry::if_match(json!({ "$set": { "a": 1 } }))],
            "creator-a",
            5.0,
        )]);
        field.recompute_hash();
        let parsed = CrdtDocumentField::from_json(&field.to_json()).expect("parse");
        assert_eq!(parsed, field);
        assert!(!field.hash.is_empty());
    }

    #[test]
    fn the_hash_covers_creators_only() {
        // `:170-179`. Two operations with the same creator and different bodies hash the
        // same, which is what `isEqual` relies on.
        let mut one = CrdtDocumentField::new();
        one.operations.push(vec![CrdtOperation::new(
            vec![CrdtEntry::if_match(json!({ "$set": { "a": 1 } }))],
            "creator-a",
            1.0,
        )]);
        one.recompute_hash();
        let mut two = CrdtDocumentField::new();
        two.operations.push(vec![CrdtOperation::new(
            vec![CrdtEntry::if_match(json!({ "$set": { "a": 999 } }))],
            "creator-a",
            2.0,
        )]);
        two.recompute_hash();
        assert_eq!(one.hash, two.hash, "the body is not hashed");
        assert!(crdt_states_equal(&one, &two));
    }

    #[test]
    fn a_different_creator_hashes_differently() {
        let mut one = CrdtDocumentField::new();
        one.operations.push(vec![CrdtOperation::new(
            vec![CrdtEntry::if_match(json!({ "$set": { "a": 1 } }))],
            "creator-a",
            1.0,
        )]);
        one.recompute_hash();
        let mut two = CrdtDocumentField::new();
        two.operations.push(vec![CrdtOperation::new(
            vec![CrdtEntry::if_match(json!({ "$set": { "a": 1 } }))],
            "creator-b",
            1.0,
        )]);
        two.recompute_hash();
        assert_ne!(one.hash, two.hash);
    }

    #[test]
    fn operations_sort_by_creator() {
        let a = CrdtOperation::new(
            vec![CrdtEntry::if_match(json!({ "$set": { "a": 1 } }))],
            "aaa",
            1.0,
        );
        let b = CrdtOperation::new(
            vec![CrdtEntry::if_match(json!({ "$set": { "a": 2 } }))],
            "bbb",
            2.0,
        );
        assert_eq!(compare_operations(&a, &b), std::cmp::Ordering::Less);
        assert_eq!(compare_operations(&b, &a), std::cmp::Ordering::Greater);
        assert_eq!(compare_operations(&a, &a), std::cmp::Ordering::Equal);
    }

    #[test]
    fn a_selector_that_does_not_match_takes_the_other_branch() {
        let operation = CrdtOperation::new(
            vec![CrdtEntry::conditional(
                json!({ "age": { "$gt": 5 } }),
                Some(json!({ "$set": { "branch": "matched" } })),
                Some(json!({ "$set": { "branch": "unmatched" } })),
            )],
            "creator-a",
            1.0,
        );
        let matched =
            run_operation_on_document(&json!({}), &operation, always_matches).expect("run");
        assert_eq!(matched["branch"], json!("matched"));
        let unmatched =
            run_operation_on_document(&json!({}), &operation, never_matches).expect("run");
        assert_eq!(unmatched["branch"], json!("unmatched"));
    }

    #[test]
    fn a_refused_incremental_modify_names_the_context() {
        assert!(check_incremental_modify(CRDT_CONTEXT).is_ok());
        assert_eq!(
            check_incremental_modify("user-write").expect_err("refused"),
            Error::DirectIncrementalModify("user-write".to_string())
        );
    }

    #[test]
    fn modify_is_always_refused() {
        assert_eq!(check_modify().expect_err("refused"), Error::DirectModify);
    }
}
