//! Doc-state conversion: what crosses the wire, and what stays local.
//!
//! Ports `src/replication-protocol/helper.ts` (178 lines) minus the remote-JSON
//! attachment shims, which are a transport concern (port plan §11.7) and live with the
//! transport that needs them.
//!
//! # Two shapes, and the difference is the whole point
//!
//! * **doc state** — `WithDeletedAndAttachments<RxDocType>`: the replicated document
//!   plus `_deleted` and possibly `_attachments`, and *never* `_meta` or `_rev` unless
//!   `keep_meta`. This is what a transport sends and receives.
//! * **write doc** — `RxDocumentData<RxDocType>`: the stored shape, which adds `_meta`
//!   and `_rev` and always has an `_attachments` object.
//!
//! Three functions and three directions:
//!
//! | function | direction | adds/removes |
//! | --- | --- | --- |
//! | [`doc_state_to_write_doc`] | wire to storage | adds `_meta`, `_rev`, `_attachments` |
//! | [`write_doc_to_doc_state`] | storage to wire | removes `_meta`, `_rev`, maybe `_attachments` |
//! | [`strip_attachments_data_from_meta_write_rows`] | meta write | payload to stub |
//!
//! # `_rev` generation is not a detail
//!
//! [`doc_state_to_write_doc`] calls `create_revision(token, previous)` when the incoming
//! state has no `_rev`, and `get_default_revision()` — the empty string — when
//! `keep_meta` is false, so the *storage* must replace it. That empty revision is
//! deliberate: upstream's comment says "Use a non-valid revision format, to ensure that
//! the RxStorage will throw when the revision is not replaced downstream"
//! (`utils-document.ts:34-41`). A port that generated a plausible revision here would
//! hide a missing assignment instead of failing.

use dxdb_json::{Map, Value};
use dxdb_storage::{DocumentData, DocumentWriteData};

use crate::conflict::strip_attachments_data_from_document;

/// Upstream's `getDefaultRevision()`: the empty string.
///
/// Not a valid `{height}-{token}`, so a storage that forgets to replace it fails loudly.
pub fn get_default_revision() -> String {
    String::new()
}

/// Builds the stored shape from a wire state.
///
/// Ports `docStateToWriteDoc` (`helper.ts:21-51`), argument for argument:
///
/// * `database_instance_token` — the revision token.
/// * `has_attachments` — when false, `_attachments` is forced to `{}` even if the wire
///   state carried stubs. That is how a collection without attachments refuses to store
///   them.
/// * `keep_meta` — when true, the incoming `_meta` and `_rev` are preserved; when false,
///   `_meta` becomes `{...previous._meta, lwt: now()}` and `_rev` becomes `""`.
/// * `previous` — the stored state being replaced, if any.
///
/// Note the order of operations: `_rev` is filled from `create_revision` only when it is
/// still falsy after the assignment, so a `keep_meta` write keeps the master's revision
/// and a normal write gets a fresh local one.
pub fn doc_state_to_write_doc(
    database_instance_token: &str,
    has_attachments: bool,
    keep_meta: bool,
    doc_state: &DocumentData,
    previous: Option<&DocumentData>,
    lwt: f64,
) -> DocumentWriteData {
    let mut write = doc_state.clone();

    // `_attachments: hasAttachments && docState._attachments ? docState._attachments : {}`
    let attachments = if has_attachments {
        doc_state
            .get("_attachments")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()))
    } else {
        Value::Object(Map::new())
    };
    write.set("_attachments", attachments);

    // `_meta: keepMeta ? docState._meta : {...previous._meta, lwt: now()}`
    let meta = if keep_meta {
        doc_state.get("_meta").cloned()
    } else {
        let mut meta = previous
            .and_then(DocumentData::meta)
            .cloned()
            .unwrap_or_default();
        if let Some(lwt) = json_number(lwt) {
            meta.insert("lwt".to_string(), lwt);
        }
        Some(Value::Object(meta))
    };
    match meta {
        Some(meta) => {
            write.set("_meta", meta);
        }
        None => {
            write.as_object_mut().remove("_meta");
        }
    }

    // `_rev: keepMeta ? docState._rev : getDefaultRevision()`
    let rev = if keep_meta {
        doc_state.get("_rev").cloned()
    } else {
        Some(Value::String(get_default_revision()))
    };
    match rev {
        Some(rev) => {
            write.set("_rev", rev);
        }
        None => {
            write.as_object_mut().remove("_rev");
        }
    }

    // `if (!docData._rev) docData._rev = createRevision(token, previous)`
    let needs_revision = match write.get("_rev") {
        Some(Value::String(rev)) => rev.is_empty(),
        Some(Value::Null) | None => true,
        // A non-string revision is malformed; replacing it is closer to upstream's
        // falsy check than preserving it, and the storage would reject it anyway.
        Some(_) => true,
    };
    if needs_revision {
        let revision = dxdb_storage::create_revision(
            database_instance_token,
            previous.and_then(DocumentData::rev),
        );
        write.set("_rev", Value::String(revision));
    }

    DocumentWriteData::new(write)
}

/// Builds the wire shape from a stored document.
///
/// Ports `writeDocToDocState` (`helper.ts:53-68`): a flat clone, then delete
/// `_attachments` unless `keep_attachments`, and `_meta`/`_rev` unless `keep_meta`.
///
/// The deletes are `delete`, not "set to null" — a `null` `_meta` would fail schema
/// validation on the receiving side, and a `null` `_attachments` would read as "this
/// document has no attachment information" instead of "no attachments".
pub fn write_doc_to_doc_state(
    write_doc: &DocumentData,
    keep_attachments: bool,
    keep_meta: bool,
) -> DocumentData {
    let mut out = write_doc.clone();
    if !keep_attachments {
        out.as_object_mut().remove("_attachments");
    }
    if !keep_meta {
        out.as_object_mut().remove("_meta");
        out.as_object_mut().remove("_rev");
    }
    out
}

/// Replaces attachment payloads with stubs on rows bound for the meta instance.
///
/// Ports `stripAttachmentsDataFromMetaWriteRows` (`helper.ts:71-86`).
///
/// When the collection has no attachments this is the identity, because there cannot be
/// a payload to strip. When it does, `docData` is stripped *inside the meta row's
/// document*, so the meta instance never stores a payload — a meta row is bookkeeping,
/// and duplicating attachment bytes into it would double the storage cost of every
/// replicated attachment.
pub fn strip_attachments_data_from_meta_write_rows(
    has_attachments: bool,
    rows: Vec<dxdb_storage::BulkWriteRow>,
) -> Vec<dxdb_storage::BulkWriteRow> {
    if !has_attachments {
        return rows;
    }
    rows.into_iter()
        .map(|row| {
            let mut document = row.document.document().clone();
            if let Some(doc_data) = document.get("docData").cloned() {
                if let Ok(inner) = DocumentData::from_value(doc_data) {
                    let stripped = strip_attachments_data_from_document(&inner);
                    document.set("docData", stripped.into_value());
                }
            }
            dxdb_storage::BulkWriteRow {
                previous: row.previous,
                document: DocumentWriteData::new(document),
            }
        })
        .collect()
}

/// `_meta.o` is the replication's write tag, and this is the shape it takes.
///
/// Ports the object built in `downstream.ts:392-397`:
/// `{_rev: <next height>, hash: <identifier hash>}`. It records "this revision was
/// written by the pull of replication X, as its revision number N", which lets the code
/// recognise a replicated write later without re-reading the meta instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteTag {
    /// The revision height the pull assigned.
    pub revision_height: u32,
    /// The replication's identifier hash.
    pub hash: String,
}

impl WriteTag {
    /// Builds the tag.
    pub fn new(revision_height: u32, hash: impl Into<String>) -> Self {
        WriteTag {
            revision_height,
            hash: hash.into(),
        }
    }

    /// The JSON shape, key order included: `{"_rev": n, "hash": "..."}`.
    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("_rev".to_string(), Value::from(self.revision_height));
        map.insert("hash".to_string(), Value::String(self.hash.clone()));
        Value::Object(map)
    }

    /// Parses the tag, returning `None` when it is absent or malformed.
    ///
    /// Malformed is not an error: the field is a hint, and a document written before the
    /// replication existed simply has none.
    pub fn from_meta(meta: Option<&Map<String, Value>>) -> Option<Self> {
        let tag = meta?.get("o")?.as_object()?;
        let height = tag.get("_rev")?.as_u64()?;
        let hash = tag.get("hash")?.as_str()?.to_string();
        Some(WriteTag {
            revision_height: u32::try_from(height).ok()?,
            hash,
        })
    }
}

/// True when the stored document was written by *this* replication at the expected
/// revision height.
///
/// Ports the guard in `downstream.ts:355-365`:
/// `_meta.o && _meta.o.hash === identifierHash && _meta.o._rev === getHeightOfRevision(_rev)`.
///
/// It means "the pull wrote this revision, and nothing has overwritten it since", which is
/// how a pull distinguishes its own write from a local edit that still has to go upstream.
pub fn was_written_by_replication(
    document: &DocumentData,
    identifier_hash: &str,
    revision_height: u32,
) -> bool {
    match WriteTag::from_meta(document.meta()) {
        Some(tag) => tag.hash == identifier_hash && tag.revision_height == revision_height,
        None => false,
    }
}

/// A JSON number for a finite `f64`.
///
/// The crate's single JSON-number constructor, so no call site has to reach through
/// `dxdb-json` to build one (a document's number type is `dxdb_json::Value`, and only its
/// re-exports are a consumer's business). `None` for a non-finite value, because JSON has
/// no way to spell `NaN` or infinity and silently substituting `null` would corrupt a
/// stored `lwt`.
pub fn json_number(value: f64) -> Option<Value> {
    // `Value::from(f64)` yields `Null` for a non-finite input, which is right for a
    // caller that just wants a value but wrong for `_meta.lwt`: a null timestamp would
    // sort before everything and break the `(_meta.lwt, primary)` order the change feed
    // depends on. So the caller gets `None` and decides.
    if value.is_finite() {
        Some(Value::from(value))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dxdb_json::parse;

    fn doc(json: &str) -> DocumentData {
        DocumentData::from_value(parse(json).expect("valid JSON")).expect("object")
    }

    #[test]
    fn a_fresh_doc_state_gets_meta_and_a_generated_revision() {
        let state = doc(r#"{"id":"a","v":1,"_deleted":false}"#);
        let write = doc_state_to_write_doc("tok", false, false, &state, None, 1.5);
        let stored = write.document();
        assert_eq!(
            stored.rev(),
            Some("1-tok"),
            "no previous state means height 1"
        );
        assert_eq!(stored.lwt(), Some(1.5));
        assert_eq!(
            stored.get("_attachments"),
            Some(&Value::Object(Map::new())),
            "the stored shape always carries _attachments"
        );
    }

    #[test]
    fn an_update_increments_the_revision_height() {
        let previous = doc(
            r#"{"id":"a","v":1,"_rev":"7-tok","_deleted":false,"_attachments":{},"_meta":{"lwt":1.0}}"#,
        );
        let state = doc(r#"{"id":"a","v":2,"_deleted":false}"#);
        let write = doc_state_to_write_doc("tok", false, false, &state, Some(&previous), 2.0);
        assert_eq!(write.document().rev(), Some("8-tok"));
    }

    #[test]
    fn previous_meta_keys_are_kept_but_lwt_is_fresh() {
        let previous = doc(
            r#"{"id":"a","_rev":"1-t","_deleted":false,"_attachments":{},"_meta":{"lwt":1.0,"plugin":"keepme"}}"#,
        );
        let state = doc(r#"{"id":"a","_deleted":false}"#);
        let write = doc_state_to_write_doc("tok", false, false, &state, Some(&previous), 9.5);
        let meta = write.document().meta().expect("meta present");
        assert_eq!(
            meta.get("plugin").and_then(Value::as_str),
            Some("keepme"),
            "other _meta keys survive a write"
        );
        assert_eq!(write.document().lwt(), Some(9.5));
    }

    #[test]
    fn keep_meta_preserves_the_masters_revision_and_meta() {
        let state = doc(
            r#"{"id":"a","_rev":"5-masterhash","_deleted":false,"_meta":{"lwt":42.0,"o":{"_rev":1,"hash":"h"}}}"#,
        );
        let write = doc_state_to_write_doc("tok", false, true, &state, None, 99.0);
        assert_eq!(
            write.document().rev(),
            Some("5-masterhash"),
            "keep_meta keeps the master's revision"
        );
        assert_eq!(write.document().lwt(), Some(42.0));
    }

    #[test]
    fn without_attachments_support_the_attachments_object_is_emptied() {
        let state = doc(
            r#"{"id":"a","_deleted":false,"_attachments":{"f":{"length":1,"digest":"d","type":"t"}}}"#,
        );
        let write = doc_state_to_write_doc("tok", false, false, &state, None, 1.0);
        assert_eq!(
            write.document().get("_attachments"),
            Some(&Value::Object(Map::new())),
            "a collection without attachments must not store stubs"
        );
    }

    #[test]
    fn with_attachments_support_the_stubs_survive() {
        let state = doc(
            r#"{"id":"a","_deleted":false,"_attachments":{"f":{"length":1,"digest":"d","type":"t"}}}"#,
        );
        let write = doc_state_to_write_doc("tok", true, false, &state, None, 1.0);
        let attachments = write.document().attachments().expect("attachments");
        assert!(attachments.contains_key("f"));
    }

    #[test]
    fn write_doc_to_doc_state_removes_local_fields() {
        let stored = doc(
            r#"{"id":"a","v":1,"_rev":"1-t","_meta":{"lwt":1.0},"_deleted":false,"_attachments":{"f":{}}}"#,
        );
        let state = write_doc_to_doc_state(&stored, false, false);
        assert_eq!(state.to_js_json(), r#"{"id":"a","v":1,"_deleted":false}"#);
    }

    #[test]
    fn keep_attachments_and_keep_meta_are_independent() {
        let stored = doc(
            r#"{"id":"a","_rev":"1-t","_meta":{"lwt":1.0},"_deleted":false,"_attachments":{"f":{}}}"#,
        );
        let attachments_only = write_doc_to_doc_state(&stored, true, false);
        assert!(attachments_only.get("_attachments").is_some());
        assert!(attachments_only.get("_meta").is_none());
        assert!(attachments_only.get("_rev").is_none());

        let meta_only = write_doc_to_doc_state(&stored, false, true);
        assert!(meta_only.get("_attachments").is_none());
        assert!(meta_only.get("_meta").is_some());
        assert!(meta_only.get("_rev").is_some());
    }

    #[test]
    fn the_write_tag_round_trips() {
        let tag = WriteTag::new(7, "abc");
        let value = tag.to_value();
        assert_eq!(value.get("_rev"), Some(&Value::from(7)));
        assert_eq!(value.get("hash").and_then(Value::as_str), Some("abc"));
        let mut meta = Map::new();
        meta.insert("o".to_string(), value);
        assert_eq!(WriteTag::from_meta(Some(&meta)), Some(tag));
    }

    #[test]
    fn a_document_without_a_tag_was_not_written_by_the_replication() {
        let document = doc(r#"{"id":"a","_rev":"1-t","_deleted":false,"_meta":{"lwt":1.0}}"#);
        assert!(!was_written_by_replication(&document, "h", 1));
    }

    #[test]
    fn the_tag_matches_only_at_the_same_hash_and_height() {
        let document = doc(
            r#"{"id":"a","_rev":"3-t","_deleted":false,"_meta":{"lwt":1.0,"o":{"_rev":3,"hash":"h"}}}"#,
        );
        assert!(was_written_by_replication(&document, "h", 3));
        assert!(
            !was_written_by_replication(&document, "other", 3),
            "a different replication's tag must not count"
        );
        assert!(
            !was_written_by_replication(&document, "h", 4),
            "a taller document was overwritten after the pull wrote it"
        );
    }

    #[test]
    fn meta_rows_without_attachments_are_untouched() {
        let row = dxdb_storage::insert_row(doc(r#"{"id":"a"}"#));
        let out = strip_attachments_data_from_meta_write_rows(false, vec![row.clone()]);
        assert_eq!(out, vec![row]);
    }
}
