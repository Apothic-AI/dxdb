//! Documents: the handle a caller actually reads.
//!
//! Upstream's `RxDocument` (`src/rx-document.ts`, 627 lines) is a proxy-wrapped object
//! with a prototype merge (`rx-document-prototype-merge.ts`) that lets plugins add ORM
//! methods to it. Rust has neither proxies nor prototype mutation, so PLAN.md 4.3's
//! re-expression applies: the document is a value that holds the state plus a handle back
//! to its collection, and plugin methods become extension traits over it
//! (`OrmMethods` in TypeScript maps to a trait bound).
//!
//! # What is deliberately *not* here
//!
//! * **Proxies.** `RxDocument` is a `Proxy` so `doc.firstName` works and so a write
//!   through a stale handle can be detected. Here [`Document::get`] is explicit. A
//!   `Deref`-based illusion would hide the staleness problem rather than solve it.
//! * **Prototype methods.** `.update()`, `.modify()`, `.patch()`, `.remove()`,
//!   `.incrementalModify()` are collection operations that take a document, because they
//!   need the write queue, the hooks and the cache — all of which the collection owns.
//!   A document that owned them would have to own a reference to its collection anyway.
//! * **`toMutableJSON` vs `toJSON` deep-clone semantics** are preserved: `to_json` returns
//!   the stored state (which callers must not mutate), `to_mutable_json` returns a fresh
//!   copy.

use serde_json::Value;

use crate::error::{CoreError, CoreResult};

/// A document state plus the identity of the collection it belongs to.
///
/// Cloning a `Document` clones the state, which is what `flatCloneDocWithMeta` does
/// upstream when a write needs a new revision.
#[derive(Clone, Debug, PartialEq)]
pub struct Document {
    collection: String,
    primary_path: String,
    state: dxdb_storage::DocumentData,
}

impl Document {
    /// Builds a handle over a stored state.
    pub fn new(
        collection: impl Into<String>,
        primary_path: impl Into<String>,
        state: dxdb_storage::DocumentData,
    ) -> Self {
        Document {
            collection: collection.into(),
            primary_path: primary_path.into(),
            state,
        }
    }

    /// The collection name.
    pub fn collection(&self) -> &str {
        &self.collection
    }

    /// The primary value (`doc.primary` upstream).
    pub fn primary(&self) -> &str {
        // The state was validated when it entered the cache or came out of a storage, so
        // a missing primary key here is an internal invariant break rather than a user
        // error.
        self.state.id(&self.primary_path).unwrap_or_default()
    }

    /// `_rev`.
    pub fn revision(&self) -> &str {
        self.state.rev().unwrap_or_default()
    }

    /// The revision height, without allocating.
    pub fn revision_height(&self) -> u32 {
        dxdb_storage::get_height_of_revision(self.revision())
    }

    /// `_deleted`. A live handle is never a tombstone, so this is `false` for every
    /// document a caller holds; it exists because `to_json` includes the flag.
    pub fn is_deleted(&self) -> bool {
        self.state.is_deleted()
    }

    /// `_meta.lwt`.
    pub fn lwt(&self) -> Option<f64> {
        self.state.lwt()
    }

    /// A field by dot path (`doc.get('nested.field')`).
    pub fn get(&self, path: &str) -> Option<&Value> {
        self.state.get_object_path(path)
    }

    /// A field by dot path, as a string when it is one.
    pub fn get_str(&self, path: &str) -> Option<&str> {
        self.get(path).and_then(Value::as_str)
    }

    /// The stored state. Borrowed: callers must not need to mutate a stored document,
    /// and a write path goes through the collection.
    pub fn state(&self) -> &dxdb_storage::DocumentData {
        &self.state
    }

    /// `toJSON()`: the full stored state, `_rev`/`_meta`/`_deleted`/`_attachments`
    /// included.
    ///
    /// Returns a clone rather than a reference because the caller is asking for a value
    /// it owns; upstream returns a new object for the same reason.
    pub fn to_json(&self) -> Value {
        self.state.as_value().clone()
    }

    /// `toMutableJSON()`: a deep copy safe to mutate.
    ///
    /// Identical to [`Document::to_json`] because both already copy; the distinction is
    /// kept because upstream's `toJSON` deliberately returns the *internal* object in
    /// some paths and ported code may rely on the naming.
    pub fn to_mutable_json(&self) -> Value {
        self.state.as_value().clone()
    }

    /// Serialised with JavaScript number formatting, which is the form that reaches a
    /// hash or the wire (PLAN.md 3.1).
    pub fn to_js_json(&self) -> String {
        self.state.to_js_json()
    }

    /// Replaces the state, used by the collection after a write.
    pub fn with_state(mut self, state: dxdb_storage::DocumentData) -> Self {
        self.state = state;
        self
    }

    /// Fails when this handle's revision is not the stored one.
    ///
    /// Upstream detects this through the proxy: mutating a stale `RxDocument` throws
    /// "object has been changed". The equivalent here is an explicit check a write path
    /// performs, which is what makes the error message possible at all.
    pub fn ensure_revision(&self, stored: &dxdb_storage::DocumentData) -> CoreResult<()> {
        if self.revision() == stored.rev().unwrap_or_default() {
            return Ok(());
        }
        Err(CoreError::Conflict {
            id: self.primary().to_string(),
        })
    }
}

/// A patch, as accepted by `modify`/`patch` (`RxPatch` upstream).
///
/// Upstream applies patches with a deep merge where `undefined` deletes a key; in Rust a
/// JSON object cannot hold `undefined`, so a delete is expressed explicitly.
#[derive(Clone, Debug, PartialEq)]
pub enum PatchOperation {
    /// Deep-merge this object into the document.
    Merge(Value),
    /// Set one dot path to a value.
    Set {
        /// Dot path.
        path: String,
        /// New value.
        value: Value,
    },
    /// Remove one dot path.
    Remove {
        /// Dot path.
        path: String,
    },
}

impl PatchOperation {
    /// Builds a deep-merge patch.
    pub fn merge(value: Value) -> Self {
        PatchOperation::Merge(value)
    }

    /// Applies this operation to a document state, in place.
    pub fn apply(&self, state: &mut dxdb_storage::DocumentData) {
        match self {
            PatchOperation::Merge(patch) => deep_merge(state.as_object_mut(), patch),
            PatchOperation::Set { path, value } => {
                set_object_path(state.as_object_mut(), path, value.clone());
            }
            PatchOperation::Remove { path } => {
                remove_object_path(state.as_object_mut(), path);
            }
        }
    }
}

/// Deep-merges `patch` into `target` the way upstream's `modify` does.
///
/// Objects merge recursively; **arrays are replaced, not concatenated**, which is
/// upstream's behaviour and the one a caller notices.
fn deep_merge(target: &mut serde_json::Map<String, Value>, patch: &Value) {
    let Some(patch_map) = patch.as_object() else {
        return;
    };
    for (key, value) in patch_map {
        match (target.get_mut(key), value) {
            (Some(Value::Object(existing)), Value::Object(_)) => {
                deep_merge(existing, value);
            }
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

fn set_object_path(target: &mut serde_json::Map<String, Value>, path: &str, value: Value) {
    let segments: Vec<&str> = path.split('.').collect();
    let Some((last, parents)) = segments.split_last() else {
        return;
    };
    let mut current = target;
    for segment in parents {
        let entry = current
            .entry((*segment).to_string())
            .or_insert_with(|| Value::Object(Default::default()));
        if !entry.is_object() {
            *entry = Value::Object(Default::default());
        }
        current = entry.as_object_mut().expect("just made an object");
    }
    current.insert((*last).to_string(), value);
}

fn remove_object_path(target: &mut serde_json::Map<String, Value>, path: &str) {
    let segments: Vec<&str> = path.split('.').collect();
    let Some((last, parents)) = segments.split_last() else {
        return;
    };
    let mut current = target;
    for segment in parents {
        let Some(next) = current.get_mut(*segment).and_then(Value::as_object_mut) else {
            return;
        };
        current = next;
    }
    current.remove(*last);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document() -> Document {
        Document::new(
            "humans",
            "id",
            dxdb_storage::DocumentData::from_value(json!({
                "id": "a",
                "_rev": "3-token",
                "_deleted": false,
                "_meta": { "lwt": 1000.01 },
                "_attachments": {},
                "firstName": "Alice",
                "nested": { "level": 5, "tags": ["one"] }
            }))
            .expect("valid"),
        )
    }

    #[test]
    fn reads_fields_by_dot_path() {
        let doc = document();
        assert_eq!(doc.primary(), "a");
        assert_eq!(doc.revision(), "3-token");
        assert_eq!(doc.revision_height(), 3);
        assert_eq!(doc.get_str("firstName"), Some("Alice"));
        assert_eq!(
            doc.get_str("nested.level"),
            None,
            "a number is not a string"
        );
        assert_eq!(doc.get("nested.level"), Some(&json!(5)));
        assert_eq!(doc.get("nope"), None);
    }

    #[test]
    fn to_json_includes_the_meta_fields() {
        let json = document().to_json();
        assert_eq!(json.get("_rev"), Some(&json!("3-token")));
        assert_eq!(json.get("_deleted"), Some(&json!(false)));
        assert!(json.get("_meta").is_some());
    }

    /// A stale handle must be detectable rather than silently overwriting.
    #[test]
    fn a_stale_handle_reports_a_conflict() {
        let doc = document();
        let newer = dxdb_storage::DocumentData::from_value(json!({
            "id": "a", "_rev": "4-token", "_deleted": false,
            "_meta": { "lwt": 2.0 }, "_attachments": {}, "firstName": "Bob"
        }))
        .expect("valid");
        let error = doc.ensure_revision(&newer).expect_err("must conflict");
        assert!(error.is_conflict());
        // And a matching revision is fine.
        doc.ensure_revision(doc.state()).expect("same revision");
    }

    #[test]
    fn a_merge_patch_recurses_into_objects_and_replaces_arrays() {
        let mut state = document().state().clone();
        PatchOperation::merge(json!({
            "nested": { "level": 9, "tags": ["two", "three"] },
            "newField": true
        }))
        .apply(&mut state);
        assert_eq!(state.get_object_path("nested.level"), Some(&json!(9)));
        // Arrays are replaced, not concatenated: upstream's rule.
        assert_eq!(
            state.get_object_path("nested.tags"),
            Some(&json!(["two", "three"]))
        );
        assert_eq!(state.get("newField"), Some(&json!(true)));
        // Untouched fields survive.
        assert_eq!(state.get("firstName"), Some(&json!("Alice")));
    }

    #[test]
    fn set_and_remove_target_a_dot_path() {
        let mut state = document().state().clone();
        PatchOperation::Set {
            path: "nested.level".to_string(),
            value: json!(11),
        }
        .apply(&mut state);
        assert_eq!(state.get_object_path("nested.level"), Some(&json!(11)));

        PatchOperation::Remove {
            path: "nested.level".to_string(),
        }
        .apply(&mut state);
        assert_eq!(state.get_object_path("nested.level"), None);
        // The parent object survives with its other key.
        assert!(state.get_object_path("nested.tags").is_some());
    }

    #[test]
    fn setting_a_path_through_a_scalar_replaces_it_with_an_object() {
        let mut state = document().state().clone();
        PatchOperation::Set {
            path: "firstName.length".to_string(),
            value: json!(5),
        }
        .apply(&mut state);
        assert_eq!(
            state.get_object_path("firstName.length"),
            Some(&json!(5)),
            "a scalar in the path is replaced by an object rather than failing"
        );
    }
}
