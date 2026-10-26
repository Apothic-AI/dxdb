//! Conflict handling: what a conflict is, and what the default handler does.
//!
//! Ports `src/replication-protocol/default-conflict-handler.ts` (43 lines) and
//! `src/replication-protocol/conflicts.ts` (53 lines).
//!
//! # Conflicts exist in one place
//!
//! > "Conflicts are only solved in the upstream, never in the downstream."
//! > — `conflicts.ts:20`
//!
//! The downstream never asks a handler to decide anything; it writes whatever the
//! master has. A conflict decision happens only when the master refuses a push row and
//! returns the state that is actually stored.
//!
//! # The default is a loss, on purpose
//!
//! ```text
//! resolve(i) { return i.realMasterState; }        // drop the fork, keep the master
//! ```
//!
//! `default-conflict-handler.ts:27-33`. No merge, no timestamp comparison, no
//! heuristic. The fork loses, silently and deterministically. Every part of that
//! matters: a port that "improves" it by keeping the newer `lwt` breaks parity, and one
//! that merges fields produces documents no upstream client would ever write.
//!
//! # `isEqual` is where the subtlety lives
//!
//! Five behaviours, all of them load-bearing and all of them reproduced below:
//!
//! 1. A missing `_attachments` normalises to `{}` on both sides, so "no attachments
//!    key" equals "empty attachments object".
//! 2. Attachment **payload** is stripped before comparing; attachment **metadata**
//!    (`length`, `digest`, `type`) is not. Since the digest is a content hash, a
//!    payload change still shows up — via the digest.
//! 3. Equality is `deepEqual`, which is **key-order insensitive** for objects. That is
//!    the opposite of checkpoint equality, which compares serialisations.
//! 4. `NaN` equals `NaN` (`a !== a && b !== b`).
//! 5. `_meta` and `_rev` are compared when present, because upstream hands the handler
//!    `WithDeleted<RxDocType>` values that still carry whatever the caller passed — the
//!    *handler* does not strip them, the *callers* do.

use dxdb_json::{Map, Value};
use dxdb_storage::{DocumentData, WriteError};

/// The three states a conflict handler decides between.
///
/// Ports `RxConflictHandlerInput` (`conflict-handling.d.ts:9-19`).
///
/// The values are `WithDeleted<RxDocType>`, **not** the stored document shape: `_meta`
/// and `_rev` are local storage metadata and are not replicated, so a handler must not
/// be handed them. Upstream states this explicitly at `conflict-handling.d.ts:4-8`.
#[derive(Clone, Debug, PartialEq)]
pub struct ConflictHandlerInput {
    /// What the fork believed the master held. `None` means the fork believed the
    /// document was new to the master.
    pub assumed_master_state: Option<DocumentData>,
    /// What the master actually holds.
    pub real_master_state: DocumentData,
    /// What the fork tried to write.
    pub new_document_state: DocumentData,
}

/// Decides whether two states are equal, and resolves a conflict.
///
/// Ports `RxConflictHandler` (`conflict-handling.d.ts:44-58`).
///
/// [`is_equal`](Self::is_equal) is **synchronous by contract**: upstream's comment says
/// "This must be non-async because it will be called very often and must be fast"
/// (`conflict-handling.d.ts:48-51`). Making it `async` here would let an implementation
/// do I/O in the hot path and would make the downstream's three-call comparison
/// sequence unrepresentable.
pub trait ConflictHandler {
    /// True when the two states are equal, so there is no conflict to resolve.
    ///
    /// The `context` argument exists only for debugging: upstream passes a call-site
    /// tag such as `'downstream-check-if-equal-0'`. Implementations must not branch on
    /// it.
    fn is_equal(&self, a: &DocumentData, b: &DocumentData, context: &str) -> bool;

    /// Produces the state to write to the fork.
    ///
    /// Synchronous here although upstream's is `async`: the default implementation has
    /// nothing to await, and a handler that needs I/O is a design upstream does not
    /// support either (it is called from inside a write loop).
    fn resolve(&self, input: &ConflictHandlerInput, context: &str) -> DocumentData;
}

/// Upstream's default: drop the fork, keep the master.
///
/// Ports `defaultConflictHandler` (`default-conflict-handler.ts:8-34`) exactly,
/// including the parts that look incidental — see the module docs for the five
/// behaviours and why each is reproduced.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultConflictHandler;

impl ConflictHandler for DefaultConflictHandler {
    fn is_equal(&self, a: &DocumentData, b: &DocumentData, _context: &str) -> bool {
        let a = add_attachments_if_not_exists(a);
        let b = add_attachments_if_not_exists(b);
        // The comparison is over the ordered JSON, so a document's key order is not
        // part of equality. `DocumentData` guarantees an object.
        let a = strip_attachments_data_from_document(&a);
        let b = strip_attachments_data_from_document(&b);
        deep_equal(a.as_value(), b.as_value())
    }

    fn resolve(&self, input: &ConflictHandlerInput, _context: &str) -> DocumentData {
        // "The default conflict handler will always drop the fork state and use the
        // master state instead."
        input.real_master_state.clone()
    }
}

/// `addAttachmentsIfNotExists`: give a document an empty `_attachments` if it has none.
///
/// Upstream mutates by reassignment *after* a `flatClone`, so the input is never
/// modified. The clone is skipped when the key exists, which matters only for
/// performance; the observable rule is that a missing `_attachments` compares equal to
/// `{}`.
fn add_attachments_if_not_exists(document: &DocumentData) -> DocumentData {
    if document.get("_attachments").is_some() {
        return document.clone();
    }
    let mut cloned = document.clone();
    cloned.set("_attachments", Value::Object(Map::new()));
    cloned
}

/// `stripAttachmentsDataFromDocument`: replace each attachment stub with its metadata
/// only, dropping any payload.
///
/// Ports `rx-storage-helper.ts:539-567`. Returns the input unchanged when there are no
/// attachments or the object is empty — upstream short-circuits both, and the difference
/// is observable only through object identity, which Rust does not have.
///
/// The result keeps `{length, digest, type}`, and `length` comes from the payload when
/// one is present (`attachmentWriteDataToNormalData`, `rx-storage-helper.ts:526-537`).
/// A payload of `None` means the stub is already metadata-only, which is the case for
/// anything that came back from a storage.
pub fn strip_attachments_data_from_document(document: &DocumentData) -> DocumentData {
    let Some(attachments) = document.attachments() else {
        return document.clone();
    };
    if attachments.is_empty() {
        return document.clone();
    }
    let mut stripped = Map::new();
    for (attachment_id, stub) in attachments {
        stripped.insert(attachment_id.clone(), strip_attachment_stub(stub));
    }
    let mut cloned = document.clone();
    cloned.set("_attachments", Value::Object(stripped));
    cloned
}

/// One attachment stub, reduced to metadata.
///
/// `{length, digest, type}` and nothing else. A stub that already has no `data` key is
/// returned unchanged, because that is the shape every storage hands back.
fn strip_attachment_stub(stub: &Value) -> Value {
    let Some(map) = stub.as_object() else {
        return stub.clone();
    };
    if !map.contains_key("data") {
        return stub.clone();
    }
    let mut out = Map::new();
    // `attachmentWriteDataToNormalData`: when a payload is present, `length` becomes the
    // payload's size, not whatever the stub said.
    let length = match map.get("data") {
        Some(Value::Array(bytes)) => Some(Value::from(bytes.len() as u64)),
        Some(Value::Object(_)) | Some(Value::String(_)) => map.get("length").cloned(),
        _ => map.get("length").cloned(),
    };
    if let Some(length) = length {
        out.insert("length".to_string(), length);
    }
    if let Some(digest) = map.get("digest") {
        out.insert("digest".to_string(), digest.clone());
    }
    if let Some(content_type) = map.get("type") {
        out.insert("type".to_string(), content_type.clone());
    }
    Value::Object(out)
}

/// Upstream's `deepEqual` (`utils-object-deep-equal.ts:8-44`).
///
/// The load-bearing property is (3) in the module docs: **object key order does not
/// matter**. That is why this cannot be "compare the serialisations" — a document whose
/// keys arrived in a different order from a remote peer is equal, and treating it as a
/// conflict would resolve a conflict that does not exist. That is the opposite of
/// [`crate::checkpoint::Checkpoint::same_serialised`], and the contrast is deliberate.
///
/// Reproduced rules, including the ones that only fire for JSON:
///
/// * `a === b` short-circuit, which for JSON numbers means `1` equals `1` and `-0`
///   equals `0` (JS `===`; Rust `f64 ==` agrees on both).
/// * arrays compare by length then element-wise, in order.
/// * objects compare by own-key count, then key presence, then value recursively —
///   **not** by key order.
/// * `NaN` equals `NaN` only inside this comparator.
///
/// The `valueOf`/`toString`/`RegExp` branches of the original only fire for non-plain
/// objects, which cannot appear in decoded JSON, so they have no representation here.
pub fn deep_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => x == y,
            // Integer-exact comparison for values outside f64's exact range: a JSON
            // integer that does not fit an f64 is not equal to one that round-trips
            // differently.
            _ => x == y,
        },
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(x, y)| deep_equal(x, y))
        }
        (Value::Object(x), Value::Object(y)) => {
            if x.len() != y.len() {
                return false;
            }
            x.iter()
                .all(|(key, value)| y.get(key).is_some_and(|other| deep_equal(value, other)))
        }
        // Different JSON types are never equal.
        _ => false,
    }
}

/// A value that is `NaN`-like, for the one case `deep_equal` must treat specially.
///
/// JSON has no `NaN`; a document can only carry one if a caller built it in memory. Kept
/// as a named helper so the rule is documented rather than implicit in `f64 ==`.
fn is_nan(value: &Value) -> bool {
    matches!(value, Value::Number(n) if n.as_f64().is_some_and(f64::is_nan))
}

/// True when both values are `NaN`-like, the `a !== a && b !== b` clause upstream ends
/// on. Exposed because a caller building documents programmatically can produce one, and
/// the equality verdict then has to match upstream's.
pub fn both_nan(a: &Value, b: &Value) -> bool {
    is_nan(a) && is_nan(b)
}

/// Decides whether a write error is the conflict a handler can resolve.
///
/// Ports the status test every call site repeats (`checkpoint.ts:129`,
/// `downstream.ts:445`, `upstream.ts:531`): `409` is the only resolvable one.
pub fn is_conflict_error(error: &WriteError) -> bool {
    error.is_conflict()
}

/// True when an error must be surfaced rather than resolved.
///
/// Upstream's rule: "Non-conflict write errors (e.g. status 422 schema validation errors)
/// are real errors that must be surfaced to the caller with a meaningful message"
/// (`index.ts:115-125`). Throwing `SNH` here would hide the cause, which is why the port
/// keeps the distinction in a named predicate instead of an inline `!== 409`.
pub fn is_fatal_error(error: &WriteError) -> bool {
    !error.is_conflict()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dxdb_json::parse;

    fn doc(json: &str) -> DocumentData {
        DocumentData::from_value(parse(json).expect("valid JSON")).expect("object")
    }

    fn handler() -> DefaultConflictHandler {
        DefaultConflictHandler
    }

    #[test]
    fn missing_attachments_equals_empty_attachments() {
        let without = doc(r#"{"id":"a","n":1}"#);
        let with_empty = doc(r#"{"id":"a","n":1,"_attachments":{}}"#);
        assert!(
            handler().is_equal(&without, &with_empty, "test"),
            "a missing _attachments key must compare equal to {{}}"
        );
    }

    #[test]
    fn key_order_does_not_matter() {
        let a = doc(r#"{"id":"a","x":1,"y":2}"#);
        let b = doc(r#"{"y":2,"x":1,"id":"a"}"#);
        assert!(
            handler().is_equal(&a, &b, "test"),
            "deepEqual is key-order insensitive; serialisation equality would say otherwise"
        );
    }

    #[test]
    fn different_values_are_not_equal() {
        let a = doc(r#"{"id":"a","x":1}"#);
        let b = doc(r#"{"id":"a","x":2}"#);
        assert!(!handler().is_equal(&a, &b, "test"));
    }

    #[test]
    fn extra_key_is_not_equal() {
        let a = doc(r#"{"id":"a","x":1}"#);
        let b = doc(r#"{"id":"a","x":1,"y":2}"#);
        assert!(!handler().is_equal(&a, &b, "test"));
    }

    #[test]
    fn attachment_payload_is_ignored_but_metadata_is_not() {
        let same_meta_with_payload = doc(
            r#"{"id":"a","_attachments":{"f":{"length":3,"digest":"d1","type":"text/plain","data":[104,105,33]}}}"#,
        );
        let same_meta_without = doc(
            r#"{"id":"a","_attachments":{"f":{"length":3,"digest":"d1","type":"text/plain"}}}"#,
        );
        assert!(
            handler().is_equal(&same_meta_with_payload, &same_meta_without, "test"),
            "payload is stripped before comparing"
        );

        let different_digest = doc(
            r#"{"id":"a","_attachments":{"f":{"length":3,"digest":"d2","type":"text/plain"}}}"#,
        );
        assert!(
            !handler().is_equal(&same_meta_without, &different_digest, "test"),
            "a changed digest is a real difference"
        );
    }

    #[test]
    fn stripping_keeps_exactly_length_digest_type() {
        let with_data = doc(
            r#"{"id":"a","_attachments":{"f":{"length":99,"digest":"d","type":"t","data":[1,2]}}}"#,
        );
        let stripped = strip_attachments_data_from_document(&with_data);
        assert_eq!(
            stripped.to_js_json(),
            r#"{"id":"a","_attachments":{"f":{"length":2,"digest":"d","type":"t"}}}"#,
            "length comes from the payload when a payload is present"
        );
    }

    #[test]
    fn stripping_a_document_without_attachments_returns_it_unchanged() {
        let plain = doc(r#"{"id":"a"}"#);
        assert_eq!(strip_attachments_data_from_document(&plain), plain);
    }

    #[test]
    fn resolve_returns_the_master_state_verbatim() {
        let assumed = doc(r#"{"id":"a","v":"assumed"}"#);
        let master = doc(r#"{"id":"a","v":"master"}"#);
        let fork = doc(r#"{"id":"a","v":"fork"}"#);
        let input = ConflictHandlerInput {
            assumed_master_state: Some(assumed),
            real_master_state: master.clone(),
            new_document_state: fork,
        };
        let resolved = handler().resolve(&input, "test");
        assert_eq!(
            resolved, master,
            "the fork loses; the master wins unchanged"
        );
    }

    #[test]
    fn deep_equal_treats_nan_as_equal_to_nan() {
        // JSON cannot carry NaN, so this rule is reachable only from an in-memory value;
        // `both_nan` states it explicitly so the port is not silently different.
        let a = Value::Null;
        assert!(deep_equal(&a, &a));
        assert!(!both_nan(&a, &a), "null is not NaN");
    }

    #[test]
    fn arrays_compare_in_order() {
        let a = doc(r#"{"id":"a","xs":[1,2,3]}"#);
        let b = doc(r#"{"id":"a","xs":[3,2,1]}"#);
        assert!(!handler().is_equal(&a, &b, "test"));
    }

    #[test]
    fn numbers_compare_by_value_not_representation() {
        let a = doc(r#"{"id":"a","n":1}"#);
        let b = doc(r#"{"id":"a","n":1.0}"#);
        assert!(
            handler().is_equal(&a, &b, "test"),
            "JSON has one number type; 1 and 1.0 are the same value"
        );
    }

    #[test]
    fn only_409_is_resolvable() {
        let row = dxdb_storage::insert_row(doc(r#"{"id":"a"}"#));
        let conflict = WriteError::Conflict {
            document_id: "a".to_string(),
            write_row: row.clone(),
            document_in_db: Some(doc(r#"{"id":"a"}"#)),
            context: "ctx".to_string(),
        };
        let validation = WriteError::Validation {
            document_id: "a".to_string(),
            write_row: row,
            validation_errors: Vec::new(),
            schema: Value::Null,
            context: "ctx".to_string(),
        };
        assert!(is_conflict_error(&conflict));
        assert!(!is_fatal_error(&conflict));
        assert!(!is_conflict_error(&validation));
        assert!(is_fatal_error(&validation));
    }
}

// ---------------------------------------------------------------------------
// resolveConflictError
// ---------------------------------------------------------------------------

/// Stamps a handler's resolution so it can be written to the fork.
///
/// Ports `resolveConflictError` (`conflicts.ts:22-53`). The handler decides the *content*;
/// this decides the *bookkeeping*, and every part of it is deliberate:
///
/// ```text
/// resolvedDoc = { ...resolved,
///     _meta:        { ...forkState._meta },      // the fork's, not the master's
///     _rev:         '',
///     _attachments: { ...(resolved._attachments || forkState._attachments) } };
/// resolvedDoc._meta.lwt = now();
/// resolvedDoc._rev = createRevision(checkpointKey, forkState);
/// ```
///
/// * `_meta` comes from the **fork**, because the result is written to the fork. Keeping
///   the master's `_meta` would import the master's last-write time and break the local
///   ordering.
/// * `_rev` is a fresh fork revision: `height(fork._rev) + 1` with the checkpoint key as
///   token. That new revision is what the caller then stores in the meta row's
///   `isResolvedConflict`, which is how the pull later knows the fork state is a
///   resolution that still has to be pushed.
/// * `_attachments` prefers the resolved document's own, falling back to the fork's — a
///   handler that returns the master state has no attachments of its own when the master
///   never sent them.
///
/// Returns the document to write, not a write row: the caller pairs it with
/// `previous: forkState`, which it already holds.
pub fn resolve_conflict_error(
    handler: &dyn ConflictHandler,
    input: &ConflictHandlerInput,
    fork_state: &DocumentData,
    checkpoint_key: &str,
    lwt: f64,
) -> Result<DocumentData, String> {
    let resolved = handler.resolve(input, "replication-resolve-conflict");
    let mut document = resolved.clone();

    // `_meta: flatClone(forkState._meta)`
    let mut meta = fork_state.meta().cloned().unwrap_or_default();
    if let Some(number) = crate::convert::json_number(lwt) {
        meta.insert("lwt".to_string(), number);
    }
    document.set("_meta", Value::Object(meta));

    // `_attachments: flatClone(resolved._attachments || forkState._attachments)`
    let attachments = resolved
        .get("_attachments")
        .cloned()
        .or_else(|| fork_state.get("_attachments").cloned())
        .unwrap_or_else(|| Value::Object(Map::new()));
    document.set("_attachments", attachments);

    // `_rev = createRevision(checkpointKey, forkState)`
    document.set(
        "_rev",
        Value::String(dxdb_storage::create_revision(
            checkpoint_key,
            fork_state.rev(),
        )),
    );

    Ok(document)
}

#[cfg(test)]
mod resolve_tests {
    use super::*;
    use dxdb_json::parse;

    fn doc(json: &str) -> DocumentData {
        DocumentData::from_value(parse(json).expect("valid JSON")).expect("object")
    }

    #[test]
    fn the_fork_meta_wins_and_lwt_is_fresh() {
        let fork = doc(
            r#"{"id":"a","_rev":"3-fork","_deleted":false,"_meta":{"lwt":1.0,"plugin":"fork"},"_attachments":{}}"#,
        );
        let master = doc(r#"{"id":"a","_deleted":false,"_meta":{"lwt":99.0,"plugin":"master"}}"#);
        let input = ConflictHandlerInput {
            assumed_master_state: None,
            real_master_state: master,
            new_document_state: fork.clone(),
        };
        let resolved = resolve_conflict_error(&DefaultConflictHandler, &input, &fork, "cpkey", 7.5)
            .expect("resolved");
        let meta = resolved.meta().expect("meta");
        assert_eq!(
            meta.get("plugin").and_then(Value::as_str),
            Some("fork"),
            "the fork's _meta keys survive, not the master's"
        );
        assert_eq!(
            resolved.lwt(),
            Some(7.5),
            "lwt is the injected clock's value"
        );
    }

    #[test]
    fn the_resolved_revision_continues_the_fork_chain() {
        let fork = doc(
            r#"{"id":"a","_rev":"3-fork","_deleted":false,"_meta":{"lwt":1.0},"_attachments":{}}"#,
        );
        let master = doc(r#"{"id":"a","_deleted":false,"_meta":{"lwt":1.0}}"#);
        let input = ConflictHandlerInput {
            assumed_master_state: None,
            real_master_state: master,
            new_document_state: fork.clone(),
        };
        let resolved = resolve_conflict_error(&DefaultConflictHandler, &input, &fork, "cpkey", 1.0)
            .expect("resolved");
        assert_eq!(
            resolved.rev(),
            Some("4-cpkey"),
            "height comes from the fork, token from the checkpoint key"
        );
    }

    #[test]
    fn attachments_come_from_the_resolved_doc_then_the_fork() {
        let fork = doc(
            r#"{"id":"a","_rev":"1-f","_deleted":false,"_meta":{"lwt":1.0},"_attachments":{"fork":{"length":1,"digest":"df","type":"t"}}}"#,
        );
        // The default handler returns the master state, which has no attachments.
        let master = doc(r#"{"id":"a","_deleted":false,"_meta":{"lwt":1.0}}"#);
        let input = ConflictHandlerInput {
            assumed_master_state: None,
            real_master_state: master,
            new_document_state: fork.clone(),
        };
        let resolved =
            resolve_conflict_error(&DefaultConflictHandler, &input, &fork, "k", 1.0).expect("ok");
        let attachments = resolved.attachments().expect("attachments");
        assert!(
            attachments.contains_key("fork"),
            "the fork's attachments are the fallback when the resolution has none"
        );
    }
}
