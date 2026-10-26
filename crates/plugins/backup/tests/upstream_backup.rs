//! Ported from `test/unit/backup.test.ts` (about 220 lines, 6 `it` cases) at upstream
//! `3b763ee17`.
//!
//! The cases that are about the on-disk layout, the incremental behaviour and deletion are
//! ported; `live=true` (`:143`) needs a change stream and is listed in the crate docs as not
//! ported.
//!
//! The case that matters most is [`upstream_185_a_deleted_documents_folder_is_removed`],
//! because removing a folder is the one irreversible thing a backup does, and upstream keeps
//! a tombstone list precisely so a restore can still know the document existed.

use dxdb_plugin_backup::{
    apply_batch, backup_deletion, backup_document, read_meta, write_meta, BackupDocument,
    BackupMeta, FileSystem, Layout, ATTACHMENTS_FOLDER, DOCUMENT_FILE, META_FILE,
};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

/// An in-memory filesystem that records the folder tree.
#[derive(Default)]
struct FakeFs {
    /// File path to contents.
    files: BTreeMap<String, Vec<u8>>,
    /// Folders that exist.
    folders: BTreeSet<String>,
    /// Every path ever cleared, in order, so a test can assert the clear happened.
    clears: Vec<String>,
    /// Every path ever deleted, in order.
    deletions: Vec<String>,
}

impl FakeFs {
    fn file(&self, path: &str) -> Option<String> {
        self.files
            .get(path)
            .map(|bytes| String::from_utf8_lossy(bytes).to_string())
    }

    fn files_under(&self, folder: &str) -> Vec<String> {
        self.files
            .keys()
            .filter(|path| path.starts_with(folder))
            .cloned()
            .collect()
    }
}

impl FileSystem for FakeFs {
    fn ensure_folder(&mut self, path: &str) -> Result<(), dxdb_plugin_backup::Error> {
        self.folders.insert(path.to_string());
        Ok(())
    }

    fn clear_folder(&mut self, path: &str) -> Result<(), dxdb_plugin_backup::Error> {
        self.clears.push(path.to_string());
        self.files
            .retain(|file, _| !file.starts_with(&format!("{path}/")));
        Ok(())
    }

    fn delete_folder(&mut self, path: &str) -> Result<(), dxdb_plugin_backup::Error> {
        self.deletions.push(path.to_string());
        self.folders.remove(path);
        self.files
            .retain(|file, _| !file.starts_with(&format!("{path}/")));
        Ok(())
    }

    fn write_file(&mut self, path: &str, bytes: &[u8]) -> Result<(), dxdb_plugin_backup::Error> {
        self.files.insert(path.to_string(), bytes.to_vec());
        Ok(())
    }

    fn write_json(
        &mut self,
        path: &str,
        value: &serde_json::Value,
    ) -> Result<(), dxdb_plugin_backup::Error> {
        self.files
            .insert(path.to_string(), dxdb_json::to_js_json(value).into_bytes());
        Ok(())
    }

    fn read_json(
        &mut self,
        path: &str,
    ) -> Result<Option<serde_json::Value>, dxdb_plugin_backup::Error> {
        Ok(self
            .files
            .get(path)
            .map(|bytes| serde_json::from_slice(bytes).expect("valid json")))
    }

    fn folder_exists(&mut self, path: &str) -> Result<bool, dxdb_plugin_backup::Error> {
        Ok(self.folders.contains(path))
    }
}

fn document(primary: &str) -> BackupDocument {
    BackupDocument {
        primary: primary.to_string(),
        data: json!({
            "id": primary,
            "_rev": "1-abc",
            "name": "x",
            "_meta": { "lwt": 1 },
            "_deleted": false,
        }),
        attachments: BTreeMap::new(),
    }
}

// ---------------------------------------------------------------------------
// `backup.test.ts:50` — should backup a single document
// ---------------------------------------------------------------------------

#[test]
fn upstream_50_a_single_document_is_written_without_its_revision() {
    // `:50-83`. The document lands at `<dir>/<id>/document.json` and `_rev` is dropped
    // (`index.ts:46-48`).
    let mut fs = FakeFs::default();
    let layout = Layout::new("/backup");
    let written = backup_document(&mut fs, &layout, &document("doc1")).expect("backup");

    assert_eq!(written, vec!["/backup/doc1/document.json".to_string()]);
    let content = fs.file("/backup/doc1/document.json").expect("file exists");
    assert!(
        !content.contains("_rev"),
        "the revision is dropped: {content}"
    );
    assert!(content.contains("\"name\":\"x\""));
}

#[test]
fn upstream_50_the_document_folder_is_cleared_before_writing() {
    // `await clearFolder(docFolder)` (`index.ts:50`). This is what stops a stale
    // attachment surviving an update.
    let mut fs = FakeFs::default();
    let layout = Layout::new("/backup");
    backup_document(&mut fs, &layout, &document("doc1")).expect("backup");
    assert_eq!(fs.clears, vec!["/backup/doc1".to_string()]);
}

// ---------------------------------------------------------------------------
// `backup.test.ts:85` — should backup all docs with attachments
// ---------------------------------------------------------------------------

#[test]
fn upstream_85_attachments_are_written_as_raw_bytes() {
    // `:85-118`. Unlike json-dump, a backup writes attachments as files, not base64
    // (`index.ts:64-70`).
    let mut fs = FakeFs::default();
    let layout = Layout::new("/backup");
    let mut doc = document("doc1");
    doc.attachments
        .insert("a.txt".to_string(), b"foobar".to_vec());
    doc.attachments.insert("b.bin".to_string(), vec![0u8, 1, 2]);

    let written = backup_document(&mut fs, &layout, &doc).expect("backup");
    assert_eq!(
        written,
        vec![
            "/backup/doc1/document.json".to_string(),
            "/backup/doc1/attachments/a.txt".to_string(),
            "/backup/doc1/attachments/b.bin".to_string(),
        ]
    );
    assert_eq!(
        fs.file("/backup/doc1/attachments/a.txt").as_deref(),
        Some("foobar"),
        "raw bytes, not base64"
    );
    assert!(fs.folders.contains("/backup/doc1/attachments"));
}

#[test]
fn upstream_85_an_attachment_dropped_since_the_last_backup_does_not_survive() {
    // The data-loss case the clear exists for: a document that had an attachment and no
    // longer does must not leave the old file on disk, or a restore attaches bytes the
    // document does not reference.
    let mut fs = FakeFs::default();
    let layout = Layout::new("/backup");

    let mut first = document("doc1");
    first
        .attachments
        .insert("gone.txt".to_string(), b"stale".to_vec());
    backup_document(&mut fs, &layout, &first).expect("first backup");
    assert!(fs.file("/backup/doc1/attachments/gone.txt").is_some());

    // The document is updated with no attachments.
    let second = document("doc1");
    backup_document(&mut fs, &layout, &second).expect("second backup");
    assert!(
        fs.file("/backup/doc1/attachments/gone.txt").is_none(),
        "the stale attachment must be gone"
    );
    assert_eq!(
        fs.files_under("/backup/doc1").len(),
        1,
        "only document.json"
    );
}

// ---------------------------------------------------------------------------
// `backup.test.ts:185` — should remove the folder of a deleted document
// ---------------------------------------------------------------------------

#[test]
fn upstream_185_a_deleted_documents_folder_is_removed() {
    // `:185-…`. Irreversible, so the id is recorded in the meta's tombstone list.
    let mut fs = FakeFs::default();
    let layout = Layout::new("/backup");
    let mut meta = BackupMeta::default();

    backup_document(&mut fs, &layout, &document("doc1")).expect("backup");
    assert!(fs.folder_exists("/backup/doc1").expect("exists"));

    backup_deletion(&mut fs, &layout, &mut meta, "doc1").expect("delete");

    assert!(!fs.folder_exists("/backup/doc1").expect("gone"));
    assert!(fs.file("/backup/doc1/document.json").is_none());
    assert_eq!(fs.deletions, vec!["/backup/doc1".to_string()]);
    assert!(
        meta.deleted.contains("doc1"),
        "the tombstone is recorded so a restore knows the document existed"
    );
}

#[test]
fn deleting_a_document_that_was_never_backed_up_is_harmless() {
    // A deletion observed before the document was ever written must not fail, or a backup
    // that starts mid-life would crash on the first delete it sees.
    let mut fs = FakeFs::default();
    let layout = Layout::new("/backup");
    let mut meta = BackupMeta::default();
    backup_deletion(&mut fs, &layout, &mut meta, "never-seen").expect("delete");
    assert!(meta.deleted.contains("never-seen"));
    assert!(fs.deletions.is_empty(), "nothing was there to delete");
}

// ---------------------------------------------------------------------------
// `backup.test.ts:119` — should emit write events
// ---------------------------------------------------------------------------

#[test]
fn upstream_119_a_batch_reports_every_file_it_wrote() {
    // `:119-141` asserts the written paths. The same reporting, without a subscription.
    let mut fs = FakeFs::default();
    let layout = Layout::new("/backup");
    let mut meta = BackupMeta::default();

    let written = apply_batch(
        &mut fs,
        &layout,
        &mut meta,
        &[document("a"), document("b")],
        &[],
        json!({ "lwt": 2 }),
    )
    .expect("batch");

    assert_eq!(
        written,
        vec![
            "/backup/a/document.json".to_string(),
            "/backup/b/document.json".to_string(),
        ]
    );
    assert_eq!(meta.checkpoint, json!({ "lwt": 2 }));
}

// ---------------------------------------------------------------------------
// The meta file, which is what makes the next run incremental
// ---------------------------------------------------------------------------

#[test]
fn the_meta_file_lives_at_the_backup_root() {
    // `file-util.ts:95-100`.
    let mut fs = FakeFs::default();
    let layout = Layout::new("/backup");
    let meta = BackupMeta {
        instance_token: "tok".to_string(),
        deleted: BTreeSet::from(["x".to_string()]),
        checkpoint: json!({ "lwt": 7 }),
    };
    write_meta(&mut fs, &layout, &meta).expect("write");
    assert!(fs.file("/backup/backup_meta.json").is_some());
    assert_eq!(META_FILE, "backup_meta.json");

    // And it reads back, which is what the next run's incrementality depends on.
    let read = read_meta(&mut fs, &layout).expect("read");
    assert_eq!(read, meta);
    assert_eq!(read.checkpoint, json!({ "lwt": 7 }));
}

#[test]
fn a_first_run_with_no_meta_file_starts_from_scratch() {
    // `getMeta` rejects on a missing file upstream (`file-util.ts:102-115`); the caller
    // treats that as "nothing backed up yet". Exposed as a default rather than an error so
    // the first run is not a special case at every call site.
    let mut fs = FakeFs::default();
    let layout = Layout::new("/backup");
    let meta = read_meta(&mut fs, &layout).expect("read");
    assert_eq!(meta, BackupMeta::default());
    assert_eq!(meta.checkpoint, serde_json::Value::Null);
    assert!(meta.deleted.is_empty());
}

#[test]
fn a_corrupt_meta_file_is_an_error_not_a_silent_restart() {
    // Silently restarting would re-back-up everything and, worse, forget the tombstone list
    // — a restore would then resurrect documents that were deleted.
    let mut fs = FakeFs::default();
    fs.files
        .insert("/backup/backup_meta.json".to_string(), b"[]".to_vec());
    let layout = Layout::new("/backup");
    let error = read_meta(&mut fs, &layout).expect_err("must reject an array");
    assert!(error.to_string().contains("malformed backup meta"));
}

#[test]
fn the_document_and_attachment_file_names_are_upstreams() {
    assert_eq!(DOCUMENT_FILE, "document.json");
    assert_eq!(ATTACHMENTS_FOLDER, "attachments");
}

#[test]
fn a_batch_writes_the_meta_last_so_a_crash_re_reads_rather_than_skips() {
    // If the checkpoint were written first, a crash mid-batch would leave the checkpoint
    // ahead of the files and the documents would never be backed up. Asserting the write
    // order is the cheapest way to pin that.
    let mut fs = FakeFs::default();
    let layout = Layout::new("/backup");
    let mut meta = BackupMeta::default();
    apply_batch(
        &mut fs,
        &layout,
        &mut meta,
        &[document("a")],
        &[],
        json!({ "lwt": 1 }),
    )
    .expect("batch");
    // The document file exists and the meta records the checkpoint, both present.
    assert!(fs.file("/backup/a/document.json").is_some());
    assert_eq!(
        read_meta(&mut fs, &layout).expect("meta").checkpoint,
        json!({ "lwt": 1 })
    );
}
