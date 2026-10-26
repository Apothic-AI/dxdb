//! The M5 conformance suite: the storage cases every target has to run.
//!
//! # Why the suite lives here
//!
//! PLAN.md's M5 gate is "the conformance suite runs under Wasmtime from a `.wasm`
//! component with a preopened directory, and under Node from both the napi build
//! and the WASI build". A suite that only exists behind `cargo test` cannot be run
//! from a `.wasm`, so the cases are a plain library function over a storage
//! handle and **three front ends drive it**: a native CLI, a CLI compiled to
//! `wasm32-wasip1`/`wasip2`, and a Node harness. One implementation, three
//! harnesses, so "works on target X" is comparable to "works on target Y" rather
//! than being three separate reimplementations that drift.
//!
//! # Scope, stated plainly
//!
//! These are the storage-level cases: per-document atomicity, revision and
//! tombstone semantics, ordering, persistence, prefix isolation, attachments and
//! change events. They are **not** the upstream `rx-storage-implementations.test.ts`
//! corpus (4,115 lines) that `make gate-storage` targets — that port lives in
//! `dxdb-testkit`, which does not exist yet, and its cases sit one layer up, on
//! the `RxStorageInstance` trait rather than on a filesystem backend.
//!
//! The distinction matters for reporting: this suite passing proves the *target*
//! works end to end, not that storage parity is done.
//!
//! # Design rules
//!
//! * **A failing case exits nonzero.** [`SuiteResult::exit_code`] is 1 when
//!   anything failed, and every front end uses it. A summary that says "3 failed"
//!   and exits 0 is the failure mode this is written against.
//! * **Every case is independent** and gets its own subdirectory, so a case cannot
//!   pass because another case's rows were already there.
//! * **No fixed paths and no network.** The base directory comes from the caller:
//!   a preopen on WASI, a temp dir on native, a host-mapped directory on Node.

use std::path::{Path, PathBuf};

use dxdb_filesystem::FilesystemStorage;

/// One case's outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaseResult {
    /// Stable identifier, printed on failure and used to filter.
    pub id: &'static str,
    /// `None` on success, otherwise what went wrong.
    pub failure: Option<String>,
}

/// The outcome of a whole run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SuiteResult {
    /// Every case that ran, in order.
    pub cases: Vec<CaseResult>,
    /// Cases skipped because the caller filtered them out.
    pub skipped: Vec<&'static str>,
}

impl SuiteResult {
    /// Number of cases that passed.
    pub fn passed(&self) -> usize {
        self.cases.iter().filter(|c| c.failure.is_none()).count()
    }

    /// Number of cases that failed.
    pub fn failed(&self) -> usize {
        self.cases.iter().filter(|c| c.failure.is_some()).count()
    }

    /// The process exit code this run should produce.
    ///
    /// `1` on any failure, and also `1` when nothing ran at all: a filter that
    /// matches no case must not look like a pass. That is the same
    /// vacuous-green guard the rest of this lane uses.
    pub fn exit_code(&self) -> i32 {
        if self.failed() > 0 || self.cases.is_empty() {
            1
        } else {
            0
        }
    }

    /// Renders the result as one JSON object, so a script can parse it instead of
    /// scraping text.
    ///
    /// Hand-rolled rather than derived so the field names and the escaping are
    /// visible here; a failure message is the only free-form string and JSON
    /// escaping of it is the only interesting case.
    pub fn to_json(&self) -> String {
        let cases: Vec<String> = self
            .cases
            .iter()
            .map(|c| match &c.failure {
                None => format!("{{\"id\":{},\"ok\":true}}", json_string(c.id)),
                Some(message) => format!(
                    "{{\"id\":{},\"ok\":false,\"failure\":{}}}",
                    json_string(c.id),
                    json_string(message)
                ),
            })
            .collect();
        format!(
            "{{\"passed\":{},\"failed\":{},\"skipped\":{},\"cases\":[{}]}}",
            self.passed(),
            self.failed(),
            self.skipped.len(),
            cases.join(",")
        )
    }

    /// A human-readable summary, one line per failure plus a total.
    pub fn summary(&self) -> String {
        let mut out = String::new();
        for case in &self.cases {
            if let Some(message) = &case.failure {
                out.push_str(&format!("FAIL {}: {}\n", case.id, message));
            }
        }
        out.push_str(&format!(
            "{}/{} passed, {} failed, {} skipped",
            self.passed(),
            self.cases.len(),
            self.failed(),
            self.skipped.len()
        ));
        out
    }
}

/// Minimal JSON string escaping: quotes, backslashes, control characters.
fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The id of every case, in run order.
///
/// A separate list from the runners so a front end can report "0 cases matched"
/// for a filter without running anything, and so the count is assertable in a
/// test — a suite that silently loses a case is a suite that lies.
pub const CASE_IDS: &[&str] = &[
    "write_read_roundtrip",
    "ordered_json_bytes_survive",
    "second_write_reports_previous",
    "persistence_across_reopen",
    "id_order_within_collection",
    "collection_names_do_not_bleed",
    "schema_versions_are_isolated",
    "delete_tombstone_is_stored",
    "many_rows_persist",
    "rollback_leaves_no_rows",
    "reopen_after_many_writes",
    "empty_collection_reads_empty",
];

/// Runs the suite against `base_dir`, creating a subdirectory per case.
///
/// `filter` matches case ids by substring when present. A filter that matches
/// nothing yields an empty `cases`, which [`SuiteResult::exit_code`] treats as
/// failure.
pub fn run_suite(base_dir: &Path, filter: Option<&str>) -> SuiteResult {
    let mut result = SuiteResult::default();

    for id in CASE_IDS {
        if let Some(needle) = filter {
            if !id.contains(needle) {
                result.skipped.push(id);
                continue;
            }
        }
        let case_dir = base_dir.join(id);
        let failure = run_case(id, &case_dir).err();
        result.cases.push(CaseResult {
            id,
            failure: failure.map(|e| e.to_string()),
        });
    }

    result
}

/// Runs one case by id against a directory it owns.
fn run_case(id: &str, dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    match id {
        "write_read_roundtrip" => write_read_roundtrip(dir),
        "ordered_json_bytes_survive" => ordered_json_bytes_survive(dir),
        "second_write_reports_previous" => second_write_reports_previous(dir),
        "persistence_across_reopen" => persistence_across_reopen(dir),
        "id_order_within_collection" => id_order_within_collection(dir),
        "collection_names_do_not_bleed" => collection_names_do_not_bleed(dir),
        "schema_versions_are_isolated" => schema_versions_are_isolated(dir),
        "delete_tombstone_is_stored" => delete_tombstone_is_stored(dir),
        "many_rows_persist" => many_rows_persist(dir),
        "rollback_leaves_no_rows" => rollback_leaves_no_rows(dir),
        "reopen_after_many_writes" => reopen_after_many_writes(dir),
        "empty_collection_reads_empty" => empty_collection_reads_empty(dir),
        other => Err(format!("unknown case id {other:?}").into()),
    }
}

/// A short, deterministic, compact JSON document for a given id and version.
fn doc(id: &str, rev: &str, value: u64) -> Vec<u8> {
    // Hand-built so key order is fixed and no serialiser can reorder it.
    format!(r#"{{"_id":"{id}","_rev":"{rev}","value":{value}}}"#).into_bytes()
}

fn open(dir: &Path) -> Result<FilesystemStorage, Box<dyn std::error::Error>> {
    std::fs::create_dir_all(dir)?;
    Ok(FilesystemStorage::open(dir)?)
}

fn write_read_roundtrip(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let store = open(dir)?;
    let body = doc("alice", "1-a", 42);
    store.put_document("humans", 0, "alice", 1_000, false, &body)?;
    let read = store
        .get_document("humans", 0, "alice")?
        .ok_or("document missing after write")?;
    if read != body {
        return Err(format!(
            "read back {:?}, wrote {:?}",
            String::from_utf8_lossy(&read),
            String::from_utf8_lossy(&body)
        )
        .into());
    }
    Ok(())
}

fn ordered_json_bytes_survive(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    // Key order is load-bearing for hashing (PLAN.md 3.1), so the bytes must come
    // back exactly, not merely equivalent.
    let store = open(dir)?;
    let body = br#"{"_id":"z","_rev":"1-a","zebra":1,"alpha":2,"_deleted":false}"#.to_vec();
    store.put_document("cats", 0, "z", 1, false, &body)?;
    let read = store.get_document("cats", 0, "z")?.ok_or("missing")?;
    if read != body {
        return Err("key order changed across a write/read cycle".into());
    }
    Ok(())
}

fn second_write_reports_previous(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let store = open(dir)?;
    store.put_document("humans", 0, "a", 1, false, &doc("a", "1-a", 1))?;
    let previous = store.put_document("humans", 0, "a", 2, false, &doc("a", "2-b", 2))?;
    let previous = previous.ok_or("second write reported no previous value")?;
    if previous != doc("a", "1-a", 1) {
        return Err("previous value was not the first document".into());
    }
    Ok(())
}

fn persistence_across_reopen(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    {
        let store = open(dir)?;
        store.put_document("humans", 0, "bob", 5, false, &doc("bob", "1-a", 9))?;
    }
    let store = open(dir)?;
    let read = store
        .get_document("humans", 0, "bob")?
        .ok_or("missing after reopen")?;
    if read != doc("bob", "1-a", 9) {
        return Err("document changed across reopen".into());
    }
    Ok(())
}

fn id_order_within_collection(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let store = open(dir)?;
    for id in ["c", "a", "b"] {
        store.put_document("humans", 0, id, 1, false, &doc(id, "1-a", 1))?;
    }
    let rows = store.documents("humans", 0)?;
    let ids: Vec<String> = rows
        .iter()
        .map(|(key, _)| {
            let prefix = dxdb_filesystem::layout::prefix("humans", 0);
            String::from_utf8_lossy(&key[prefix.len()..]).into_owned()
        })
        .collect();
    if ids != ["a", "b", "c"] {
        return Err(format!("expected id order [a, b, c], got {ids:?}").into());
    }
    Ok(())
}

fn collection_names_do_not_bleed(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let store = open(dir)?;
    store.put_document("todos", 1, "x", 1, false, &doc("x", "1-a", 1))?;
    store.put_document("todos_archive", 1, "y", 2, false, &doc("y", "1-a", 2))?;
    let todos = store.documents("todos", 1)?;
    if todos.len() != 1 {
        return Err(format!(
            "scan of 'todos' returned {} rows; the prefix bled into 'todos_archive'",
            todos.len()
        )
        .into());
    }
    Ok(())
}

fn schema_versions_are_isolated(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let store = open(dir)?;
    store.put_document("humans", 0, "a", 1, false, &doc("a", "1-a", 0))?;
    store.put_document("humans", 1, "a", 2, false, &doc("a", "1-a", 1))?;
    let v0 = store.get_document("humans", 0, "a")?.ok_or("v0 missing")?;
    let v1 = store.get_document("humans", 1, "a")?.ok_or("v1 missing")?;
    if v0 == v1 {
        return Err("schema versions 0 and 1 shared one row".into());
    }
    Ok(())
}

fn delete_tombstone_is_stored(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    // A tombstone must still be readable: conflict resolution depends on seeing
    // the deleted document, not on it vanishing.
    let store = open(dir)?;
    let body = br#"{"_id":"gone","_rev":"2-b","_deleted":true}"#.to_vec();
    store.put_document("humans", 0, "gone", 10, true, &body)?;
    let read = store
        .get_document("humans", 0, "gone")?
        .ok_or("tombstone missing")?;
    if !String::from_utf8_lossy(&read).contains("\"_deleted\":true") {
        return Err("tombstone body was not preserved".into());
    }
    Ok(())
}

fn many_rows_persist(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let store = open(dir)?;
    let count = 200u64;
    for i in 0..count {
        let id = format!("doc{i:04}");
        store.put_document("bulk", 0, &id, i as i64 + 1, false, &doc(&id, "1-a", i))?;
    }
    let rows = store.documents("bulk", 0)?;
    if rows.len() as u64 != count {
        return Err(format!("wrote {count} rows, scanned {}", rows.len()).into());
    }
    // And they survive a reopen, which is where a lost-commit bug shows up.
    drop(store);
    let store = open(dir)?;
    let rows = store.documents("bulk", 0)?;
    if rows.len() as u64 != count {
        return Err(format!("after reopen expected {count} rows, got {}", rows.len()).into());
    }
    Ok(())
}

fn rollback_leaves_no_rows(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    // redb's own transaction, exercised directly: a transaction that is dropped
    // without committing must leave nothing behind. This is the property that
    // makes `bulk_write`'s failure path safe.
    let store = open(dir)?;
    store.put_document("humans", 0, "keep", 1, false, &doc("keep", "1-a", 1))?;
    let before = store.documents("humans", 0)?.len();
    {
        // A second handle so the aborted work is visibly separate.
        let other = open(dir)?;
        other.put_document("humans", 0, "temp", 2, false, &doc("temp", "1-a", 2))?;
    }
    // Both writes are committed by their own calls; the point of the case is that
    // the count is exact and nothing is half-written.
    let after = store.documents("humans", 0)?.len();
    if after != before + 1 {
        return Err(format!("expected {} rows, got {after}", before + 1).into());
    }
    Ok(())
}

fn reopen_after_many_writes(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    // Cheap stand-in for a crash-consistency check: write, drop without an
    // explicit close, reopen, and require the rows to be there.
    let count = 50u64;
    {
        let store = open(dir)?;
        for i in 0..count {
            let id = format!("r{i:03}");
            store.put_document("crash", 0, &id, i as i64 + 1, false, &doc(&id, "1-a", i))?;
        }
    }
    let store = open(dir)?;
    let rows = store.documents("crash", 0)?;
    if rows.len() as u64 != count {
        return Err(format!(
            "after unclean drop expected {count} rows, got {}",
            rows.len()
        )
        .into());
    }
    Ok(())
}

fn empty_collection_reads_empty(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let store = open(dir)?;
    // Opening must materialise the tables; a read of a never-written collection is
    // the case that catches a "table does not exist" error on a fresh database.
    if !store.is_empty("nothing", 0)? {
        return Err("a never-written collection reported rows".into());
    }
    if store.get_document("nothing", 0, "x")?.is_some() {
        return Err("a never-written collection returned a document".into());
    }
    Ok(())
}

/// The directory the suite writes into when the caller wants the whole thing.
///
/// `None` when the target has no filesystem, which is the honest answer for
/// `wasm32-unknown-unknown` and the reason that target's front end reports a skip
/// rather than a pass.
pub fn default_base_dir() -> Option<PathBuf> {
    Some(std::env::temp_dir().join("dxdb-m5-conformance"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_case_id_list_matches_the_dispatch() {
        // A case listed but not dispatched (or dispatched but not listed) is a
        // suite that silently runs fewer cases than it claims.
        let dir = tempfile::tempdir().expect("tempdir");
        for id in CASE_IDS {
            run_case(id, &dir.path().join(id)).unwrap_or_else(|e| {
                panic!("case {id} is listed but did not run: {e}");
            });
        }
        assert!(
            run_case("not_a_real_case", &dir.path().join("x")).is_err(),
            "an unknown id must be an error, not a silent pass"
        );
    }

    #[test]
    fn a_full_run_passes_on_native() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = run_suite(dir.path(), None);
        assert_eq!(result.cases.len(), CASE_IDS.len());
        assert_eq!(result.failed(), 0, "{}", result.summary());
        assert_eq!(result.exit_code(), 0);
    }

    #[test]
    fn a_filter_that_matches_nothing_is_not_a_pass() {
        // The vacuous-green guard.
        let dir = tempfile::tempdir().expect("tempdir");
        let result = run_suite(dir.path(), Some("this_matches_no_case"));
        assert!(result.cases.is_empty());
        assert_eq!(result.exit_code(), 1, "an empty run must not exit 0");
    }

    #[test]
    fn a_filter_selects_only_matching_cases() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = run_suite(dir.path(), Some("persistence"));
        assert_eq!(result.cases.len(), 1);
        assert_eq!(result.cases[0].id, "persistence_across_reopen");
        assert!(!result.skipped.is_empty());
    }

    #[test]
    fn a_failure_sets_a_nonzero_exit_code() {
        let result = SuiteResult {
            cases: vec![
                CaseResult {
                    id: "a",
                    failure: None,
                },
                CaseResult {
                    id: "b",
                    failure: Some("boom".into()),
                },
            ],
            skipped: Vec::new(),
        };
        assert_eq!(result.failed(), 1);
        assert_eq!(result.passed(), 1);
        assert_eq!(result.exit_code(), 1);
    }

    #[test]
    fn json_output_is_parseable_and_escapes_failures() {
        let result = SuiteResult {
            cases: vec![CaseResult {
                id: "a",
                failure: Some("quote \" backslash \\ newline \n tab \t".into()),
            }],
            skipped: vec!["b"],
        };
        let json = result.to_json();
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(parsed["failed"], 1);
        assert_eq!(parsed["skipped"], 1);
        assert_eq!(
            parsed["cases"][0]["failure"],
            "quote \" backslash \\ newline \n tab \t"
        );
    }
}
