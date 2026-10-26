//! Incremental per-document folder backup.
//!
//! Ported from `src/plugins/backup/` at upstream `3b763ee17` (413 lines): `index.ts` (281)
//! is the runner, `file-util.ts` (132) is the on-disk layout.
//!
//! # The layout, which is the contract
//!
//! Unlike `json-dump`, which produces one JSON blob, a backup is a **folder tree** that is
//! rewritten incrementally:
//!
//! ```text
//! <directory>/
//!   backup_meta.json          # { instanceToken, deleted, checkpoint }
//!   <docId>/document.json     # the document, without _rev
//!   <docId>/attachments/<id>  # raw attachment bytes
//! ```
//!
//! The meta file is at `path.join(directory, 'backup_meta.json')`
//! (`file-util.ts:95-100`); each document gets a folder named by its primary key
//! (`:124-131`); attachments are raw bytes at `<docFolder>/attachments/<attachmentId>`
//! (`index.ts:60-70`).
//!
//! # Why the meta file makes it incremental
//!
//! The backup records a checkpoint and a set of tombstones in `backup_meta.json`. On the
//! next run it reads only documents changed since that checkpoint
//! (`getChangedDocumentsSince`, `index.ts:96`) and:
//!
//! * **a deleted document's folder is removed** and its id noted, so a later restore does
//!   not resurrect it;
//! * **a changed document's folder is cleared and rewritten** — `clearFolder(docFolder)`
//!   (`index.ts:50`) is what prevents a stale attachment surviving an update that dropped
//!   it.
//!
//! That second point is the subtle one: rewriting `document.json` alone would leave an
//! attachment file from the previous revision on disk, and a restore would then attach
//! bytes the document no longer references. Clearing first is the fix, and it is why
//! [`FileSystem::clear_document`] exists as its own operation rather than being implied by
//! a write.
//!
//! # Raw bytes, not base64
//!
//! `json-dump` inlines attachments as base64 because a dump must be one JSON document. A
//! backup writes them as files (`index.ts:64-70`), which is why the same attachment is
//! handled two different ways by two plugins — worth stating because it looks like an
//! inconsistency until you see the constraint each is under.
//!
//! # Not ported
//!
//! The filesystem itself, the change stream that triggers a run, and the RxDB
//! replication-based restore path. [`FileSystem`] is the seam, so the incremental logic —
//! which is where the data-loss risk lives — is testable without one. `backupSingleDocument`
//! (`:39-…)` is ported as [`backup_document`].

#![forbid(unsafe_code)]

use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

/// The meta file name (`file-util.ts:97`).
pub const META_FILE: &str = "backup_meta.json";

/// The per-document file name (`index.ts:53`).
pub const DOCUMENT_FILE: &str = "document.json";

/// The per-document attachments folder (`index.ts:60`).
pub const ATTACHMENTS_FOLDER: &str = "attachments";

/// Errors.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum Error {
    /// The meta file could not be parsed.
    #[error("malformed backup meta: {0}")]
    MalformedMeta(String),
    /// A document has no primary field, so its folder cannot be named.
    #[error("document has no primary field `{0}`, so it cannot be backed up")]
    MissingPrimary(String),
    /// The filesystem failed.
    #[error("backup filesystem error: {0}")]
    FileSystem(String),
}

/// Result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// The meta file's content (`BackupMetaFileContent`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BackupMeta {
    /// The instance token of the database the backup was taken from. A restore into a
    /// different instance is legal; this is recorded so it can be reported.
    pub instance_token: String,
    /// The primary keys whose folders were removed because the document was deleted.
    ///
    /// Kept because removing a folder is not reversible: without this list a restore would
    /// have no way to know a document was ever there.
    pub deleted: BTreeSet<String>,
    /// How far the backup has read, so the next run is incremental
    /// (`index.ts:96`).
    pub checkpoint: Value,
}

impl BackupMeta {
    /// The wire shape.
    pub fn to_json(&self) -> Value {
        json!({
            "instanceToken": self.instance_token,
            "deleted": self.deleted.iter().cloned().collect::<Vec<_>>(),
            "checkpoint": self.checkpoint,
        })
    }

    /// Reads the wire shape (`getMeta`, `file-util.ts:102-115`).
    pub fn from_json(value: &Value) -> Result<Self> {
        let map = value
            .as_object()
            .ok_or_else(|| Error::MalformedMeta(value.to_string()))?;
        Ok(BackupMeta {
            instance_token: map
                .get("instanceToken")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            deleted: map
                .get("deleted")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            checkpoint: map.get("checkpoint").cloned().unwrap_or(Value::Null),
        })
    }
}

/// The filesystem, as far as a backup needs it.
///
/// Six operations, each mapping to one `file-util.ts` function, so the incremental logic is
/// testable without touching a disk.
pub trait FileSystem {
    /// Creates a folder if it does not exist (`ensureFolderExists`, `:13-21`).
    fn ensure_folder(&mut self, path: &str) -> Result<()>;

    /// Empties a folder but keeps it (`clearFolder`, `:22-26`).
    ///
    /// The reason this is separate from a recursive write: an update that drops an
    /// attachment must not leave the old file behind.
    fn clear_folder(&mut self, path: &str) -> Result<()>;

    /// Removes a folder entirely (`deleteFolder`, `:27-33`).
    fn delete_folder(&mut self, path: &str) -> Result<()>;

    /// Writes bytes to a file (`writeToFile`, `:62-84`).
    fn write_file(&mut self, path: &str, bytes: &[u8]) -> Result<()>;

    /// Writes JSON to a file (`writeJsonToFile`, `:85-94`).
    fn write_json(&mut self, path: &str, value: &Value) -> Result<()>;

    /// Reads JSON from a file, or `None` when it does not exist (`getMeta`, `:102-115`).
    fn read_json(&mut self, path: &str) -> Result<Option<Value>>;

    /// Whether a folder exists, so a restore can tell "absent" from "empty".
    fn folder_exists(&mut self, path: &str) -> Result<bool>;
}

/// The on-disk path layout (`file-util.ts:95-131`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    /// The backup root.
    pub directory: String,
}

impl Layout {
    /// A layout rooted at `directory`.
    pub fn new(directory: impl Into<String>) -> Self {
        Layout {
            directory: directory.into(),
        }
    }

    /// `path.join(directory, 'backup_meta.json')` (`:95-100`).
    pub fn meta_file(&self) -> String {
        join(&self.directory, META_FILE)
    }

    /// `path.join(directory, docId)` (`:124-131`).
    pub fn document_folder(&self, document_id: &str) -> String {
        join(&self.directory, document_id)
    }

    /// `path.join(docFolder, 'document.json')` (`index.ts:50-54`).
    pub fn document_file(&self, document_id: &str) -> String {
        join(&self.document_folder(document_id), DOCUMENT_FILE)
    }

    /// `path.join(docFolder, 'attachments')` (`index.ts:60-63`).
    pub fn attachments_folder(&self, document_id: &str) -> String {
        join(&self.document_folder(document_id), ATTACHMENTS_FOLDER)
    }

    /// `path.join(attachmentsFolder, attachmentId)` (`index.ts:66-69`).
    pub fn attachment_file(&self, document_id: &str, attachment_id: &str) -> String {
        join(&self.attachments_folder(document_id), attachment_id)
    }
}

/// `path.join` for the two-segment cases above.
fn join(left: &str, right: &str) -> String {
    if left.ends_with('/') {
        format!("{left}{right}")
    } else {
        format!("{left}/{right}")
    }
}

/// One document to back up.
#[derive(Clone, Debug, PartialEq)]
pub struct BackupDocument {
    /// The document's primary value.
    pub primary: String,
    /// The document data, `_rev` included — it is dropped on write (`index.ts:46`).
    pub data: Value,
    /// Attachment bytes by attachment id (`index.ts:64-70`).
    pub attachments: BTreeMap<String, Vec<u8>>,
}

/// `backupSingleDocument` (`index.ts:39-…`).
///
/// Clears the document's folder first, so an attachment removed since the last backup does
/// not survive as a stale file. That ordering is the whole reason this is not just two
/// writes.
pub fn backup_document<F: FileSystem>(
    filesystem: &mut F,
    layout: &Layout,
    document: &BackupDocument,
) -> Result<Vec<String>> {
    let document_folder = layout.document_folder(&document.primary);
    filesystem.ensure_folder(&document_folder)?;
    // `await clearFolder(docFolder)` (`:50`).
    filesystem.clear_folder(&document_folder)?;

    // `const data = rxDocument.toJSON(true)` with `_rev` dropped (`:46`, `:48-52`).
    let mut data = document.data.clone();
    if let Value::Object(map) = &mut data {
        map.remove("_rev");
    }
    let document_file = layout.document_file(&document.primary);
    filesystem.write_json(&document_file, &data)?;
    let mut written = vec![document_file];

    if !document.attachments.is_empty() {
        let attachments_folder = layout.attachments_folder(&document.primary);
        filesystem.ensure_folder(&attachments_folder)?;
        for (attachment_id, bytes) in &document.attachments {
            let path = layout.attachment_file(&document.primary, attachment_id);
            filesystem.write_file(&path, bytes)?;
            written.push(path);
        }
    }
    Ok(written)
}

/// Removes a deleted document's folder and records the id (`index.ts:96-…`).
pub fn backup_deletion<F: FileSystem>(
    filesystem: &mut F,
    layout: &Layout,
    meta: &mut BackupMeta,
    primary: &str,
) -> Result<()> {
    let folder = layout.document_folder(primary);
    if filesystem.folder_exists(&folder)? {
        filesystem.delete_folder(&folder)?;
    }
    meta.deleted.insert(primary.to_string());
    Ok(())
}

/// Reads the meta file, or a default when there is none (`getMeta`, `file-util.ts:102-115`).
pub fn read_meta<F: FileSystem>(filesystem: &mut F, layout: &Layout) -> Result<BackupMeta> {
    match filesystem.read_json(&layout.meta_file())? {
        None => Ok(BackupMeta::default()),
        Some(value) => BackupMeta::from_json(&value),
    }
}

/// Writes the meta file (`setMeta`, `file-util.ts:116-123`).
pub fn write_meta<F: FileSystem>(
    filesystem: &mut F,
    layout: &Layout,
    meta: &BackupMeta,
) -> Result<()> {
    filesystem.write_json(&layout.meta_file(), &meta.to_json())
}

/// Applies a batch of changes to the backup (`index.ts:90-…`).
///
/// Documents and deletions in one call, so a caller driving from
/// `getChangedDocumentsSince` does not have to interleave two loops and can write the meta
/// once per batch.
pub fn apply_batch<F: FileSystem>(
    filesystem: &mut F,
    layout: &Layout,
    meta: &mut BackupMeta,
    documents: &[BackupDocument],
    deletions: &[String],
    checkpoint: Value,
) -> Result<Vec<String>> {
    filesystem.ensure_folder(&layout.directory)?;
    let mut written = Vec::new();
    for document in documents {
        written.extend(backup_document(filesystem, layout, document)?);
    }
    for primary in deletions {
        backup_deletion(filesystem, layout, meta, primary)?;
    }
    meta.checkpoint = checkpoint;
    write_meta(filesystem, layout, meta)?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_layout_matches_upstreams_join_calls() {
        // `file-util.ts:95-131` and `index.ts:50-66`.
        let layout = Layout::new("/backup");
        assert_eq!(layout.meta_file(), "/backup/backup_meta.json");
        assert_eq!(layout.document_folder("doc1"), "/backup/doc1");
        assert_eq!(layout.document_file("doc1"), "/backup/doc1/document.json");
        assert_eq!(
            layout.attachments_folder("doc1"),
            "/backup/doc1/attachments"
        );
        assert_eq!(
            layout.attachment_file("doc1", "a.txt"),
            "/backup/doc1/attachments/a.txt"
        );
    }

    #[test]
    fn a_trailing_slash_does_not_double_up() {
        let layout = Layout::new("/backup/");
        assert_eq!(layout.meta_file(), "/backup/backup_meta.json");
    }

    #[test]
    fn the_meta_round_trips() {
        let meta = BackupMeta {
            instance_token: "tok".to_string(),
            deleted: BTreeSet::from(["a".to_string(), "b".to_string()]),
            checkpoint: json!({ "lwt": 5 }),
        };
        let parsed = BackupMeta::from_json(&meta.to_json()).expect("parse");
        assert_eq!(parsed, meta);
    }
}
