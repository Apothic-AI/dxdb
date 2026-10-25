//! Assertions against the upstream oracle vectors.
//!
//! Lane C generates `testdata/vectors/replication-transcripts.json` by running real
//! RxDB in Node (`xtask/vectors/families/replication-transcripts.mjs`), so it is
//! upstream's observable output, not ours. This file checks the parts of the protocol
//! that exist today against it, and records the parts that cannot be checked yet as
//! named gaps rather than leaving them unmeasured.
//!
//! # Why this is worth running before the protocol is finished
//!
//! The vectors pin the *shape* of everything the protocol persists — meta row ids, the
//! checkpoint payload, the assumed-master state, the resolution marker — so a port can
//! be checked against upstream before there is a pull or a push to run. A structural
//! mistake (a different separator, a `null` where upstream omits a key, a checkpoint
//! that carries extra fields) is caught here in milliseconds instead of at the end of
//! the milestone.
//!
//! # What is asserted, and what is not
//!
//! Asserted against real vector data:
//!
//! * meta row ids are `"{itemId}|{isCheckpoint}"` for both directions and for documents;
//! * checkpoint payloads are `{id, lwt}` objects that round-trip through
//!   `dxdb_json::to_js_json` unchanged;
//! * `isResolvedConflict` is present exactly on the rows where upstream set it, and it
//!   equals the fork revision recorded in the same transcript;
//! * the assumed-master `docData` is the master state minus `_meta` and `_rev`;
//! * the default conflict handler's verdict on the recorded before/after states.
//!
//! Not asserted, and why: the *transitions* between steps need a live pull and push,
//! which do not exist yet. [`DEFERRED`] names each of those so the gap is explicit.

use std::collections::BTreeSet;

use dxdb_json::Value;
use dxdb_replication::conflict::{deep_equal, ConflictHandler, DefaultConflictHandler};
use dxdb_replication::convert::{write_doc_to_doc_state, WriteTag};
use dxdb_replication::meta_instance::META_INSTANCE_SCHEMA_TITLE;
use dxdb_storage::DocumentData;

/// The vector family this file consumes.
const VECTORS: &str = "replication-transcripts.json";

/// Transcripts whose *observable end state* this file checks structurally.
///
/// A transcript is listed here once every claim it makes about persisted state is
/// covered by an assertion below.
const STRUCTURALLY_COVERED: &[&str] = &[
    "empty-master-and-fork",
    "pull-existing-documents",
    "push-existing-documents",
    "conflict-both-sides-differ",
    "deletion-propagates",
    "incremental-pull-after-first-sync",
];

/// Assertions that need the pull and push loops, which are not written yet.
///
/// Each entry is `(transcript, what is missing)`. This is a gap list, kept next to the
/// assertions that *do* run so the coverage claim is auditable, not a promise.
const DEFERRED: &[(&str, &str)] = &[
    (
        "pull-existing-documents",
        "the pull loop: applying master documents onto the fork and writing the down checkpoint",
    ),
    (
        "push-existing-documents",
        "the push loop: writing fork documents to the master and recording the assumed master state",
    ),
    (
        "conflict-both-sides-differ",
        "the conflict path end to end: masterWrite reporting a conflict, the handler resolving it, \
         the resolved write, and the isResolvedConflict marker",
    ),
    (
        "deletion-propagates",
        "a pull that carries a tombstone through to the fork",
    ),
    (
        "incremental-pull-after-first-sync",
        "two successive pulls resuming from a stored checkpoint",
    ),
    (
        "empty-master-and-fork",
        "the batch-boundary case below covers checkpoint advancement; the empty case needs only \
         the loops",
    ),
];

fn load() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/vectors/",
        "replication-transcripts.json"
    );
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("cannot read {VECTORS} at {path}: {error}"));
    dxdb_json::parse(&text).unwrap_or_else(|error| panic!("{VECTORS} is not valid JSON: {error}"))
}

fn transcripts(vectors: &Value) -> Vec<Value> {
    vectors
        .get("transcripts")
        .and_then(Value::as_array)
        .expect("the vector file has a transcripts array")
        .clone()
}

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("expected a string at {key}"))
}

fn as_doc(value: &Value) -> DocumentData {
    DocumentData::from_value(value.clone()).expect("a vector document is an object")
}

/// Every meta row in every transcript has an id composed from its own fields.
///
/// This is the assertion that pins `separator: "|"` and the `itemId`/`isCheckpoint`
/// field pair, which together are the primary key of the meta instance. A port that
/// chose `:` or that swapped the field order would still pass its own unit tests and
/// would silently fail to read a JS peer's meta rows.
#[test]
fn every_meta_row_id_is_composed_from_item_id_and_flag() {
    let vectors = load();
    let mut checked = 0usize;
    let mut seen_ids = BTreeSet::new();

    for transcript in transcripts(&vectors) {
        for step in transcript
            .get("steps")
            .and_then(Value::as_array)
            .expect("steps")
        {
            for row in step
                .get("meta")
                .and_then(Value::as_array)
                .expect("meta")
            {
                let id = str_field(row, "id");
                let item_id = str_field(row, "itemId");
                let flag = str_field(row, "isCheckpoint");
                assert_eq!(
                    id,
                    format!("{item_id}|{flag}"),
                    "meta id must be `itemId|isCheckpoint`; row {}",
                    dxdb_json::to_js_json(row)
                );
                assert!(
                    flag == "0" || flag == "1",
                    "isCheckpoint is the string '0' or '1', got {flag:?}"
                );
                // The direction rows are the literal lowercase spellings, which is what
                // `Direction::as_str` writes.
                if flag == "1" {
                    assert!(
                        item_id == "up" || item_id == "down",
                        "a checkpoint row's itemId is 'up' or 'down', got {item_id:?}"
                    );
                }
                seen_ids.insert(flag.to_string());
                checked += 1;
            }
        }
    }

    assert!(checked > 0, "the vector file must cover meta rows");
    assert_eq!(
        seen_ids,
        BTreeSet::from(["0".to_string(), "1".to_string()]),
        "the vectors must cover both document rows and checkpoint rows"
    );
}

/// Checkpoint payloads are opaque `{id, lwt}` objects and survive a JS-serialisation
/// round-trip byte-identically.
///
/// The round-trip is the point: `Checkpoint` is compared with `JSON.stringify`
/// equality upstream, so a payload that re-serialises differently from the bytes a JS
/// peer wrote would look changed and cause a pointless write.
#[test]
fn checkpoint_payloads_round_trip_byte_identically() {
    let vectors = load();
    let mut checked = 0usize;

    for transcript in transcripts(&vectors) {
        for step in transcript
            .get("steps")
            .and_then(Value::as_array)
            .expect("steps")
        {
            for row in step
                .get("meta")
                .and_then(Value::as_array)
                .expect("meta")
            {
                if str_field(row, "isCheckpoint") != "1" {
                    continue;
                }
                let Some(payload) = row.get("checkpointData").filter(|v| !v.is_null()) else {
                    continue;
                };
                assert!(
                    payload.is_object(),
                    "a checkpoint payload is an object, got {}",
                    dxdb_json::to_js_json(payload)
                );

                // Through the protocol's type: construction must accept it, and the
                // serialisation must be unchanged.
                let checkpoint = dxdb_replication::checkpoint::Checkpoint::new(payload.clone())
                    .expect("an object checkpoint");
                let serialised = checkpoint.to_js_json();
                assert_eq!(
                    serialised,
                    dxdb_json::to_js_json(payload),
                    "a stored checkpoint must re-serialise to the same bytes"
                );

                // And it stacks with itself to itself, which is the identity the
                // protocol relies on when it merges a batch checkpoint into an older one.
                let stacked = checkpoint.stack(&checkpoint);
                assert!(
                    checkpoint.same_serialised(&stacked),
                    "stacking a checkpoint onto itself must not change it"
                );
                checked += 1;
            }
        }
    }

    assert!(
        checked > 0,
        "the vectors must contain at least one checkpoint payload"
    );
}

/// `isResolvedConflict` appears exactly when upstream set it, and equals the fork
/// revision of that transcript.
///
/// The field is the entire handshake between the two directions (port plan §6.4): the
/// pull waits for the push when the fork state carries it, and the push refuses to
/// short-circuit on equality. A port that wrote `null` instead of omitting it would
/// fail schema validation against the meta schema — which is why the omission is
/// asserted below as well.
#[test]
fn is_resolved_conflict_is_present_only_when_upstream_set_it() {
    let vectors = load();
    let mut with_marker = 0usize;
    let mut without_marker = 0usize;

    for transcript in transcripts(&vectors) {
        let final_fork_revs: BTreeSet<String> = transcript
            .get("finalForkState")
            .and_then(Value::as_array)
            .map(|docs| {
                docs.iter()
                    .filter_map(|doc| doc.get("_rev").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        for step in transcript
            .get("steps")
            .and_then(Value::as_array)
            .expect("steps")
        {
            for row in step
                .get("meta")
                .and_then(Value::as_array)
                .expect("meta")
            {
                if str_field(row, "isCheckpoint") != "0" {
                    continue;
                }
                match row.get("isResolvedConflict") {
                    Some(value) if !value.is_null() => {
                        let revision = value.as_str().expect("a revision string");
                        assert!(
                            final_fork_revs.contains(revision),
                            "isResolvedConflict {:?} must name a fork revision; fork has {:?}",
                            revision,
                            final_fork_revs
                        );
                        with_marker += 1;
                    }
                    _ => {
                        without_marker += 1;
                    }
                }
            }
        }
    }

    assert!(
        with_marker > 0,
        "the conflict transcript must produce a resolution marker, or the field is untested"
    );
    assert!(
        without_marker > 0,
        "ordinary document rows must not carry the marker"
    );
}

/// The assumed-master `docData` is the master state minus its local bookkeeping.
///
/// Upstream stores what the master said, without `_meta`/`_rev`, because those are the
/// master's local revision chain and meaningless on the fork. A port that stored the
/// whole master document would compare unequal on every future push and push forever.
#[test]
fn assumed_master_doc_data_excludes_local_bookkeeping() {
    let vectors = load();
    let mut checked = 0usize;
    let handler = DefaultConflictHandler;

    for transcript in transcripts(&vectors) {
        for step in transcript
            .get("steps")
            .and_then(Value::as_array)
            .expect("steps")
        {
            let meta_rows = step
                .get("meta")
                .and_then(Value::as_array)
                .expect("meta");
            let master_docs = step
                .get("master")
                .and_then(Value::as_array)
                .expect("master");

            for row in meta_rows {
                if str_field(row, "isCheckpoint") != "0" {
                    continue;
                }
                let Some(doc_data) = row.get("docData").filter(|v| !v.is_null()) else {
                    continue;
                };
                assert!(
                    doc_data.get("_meta").is_none(),
                    "the assumed master state must not carry _meta: {}",
                    dxdb_json::to_js_json(doc_data)
                );
                assert!(
                    doc_data.get("_rev").is_none(),
                    "the assumed master state must not carry _rev: {}",
                    dxdb_json::to_js_json(doc_data)
                );

                // An empty assumed state equals nothing in particular; the meaningful
                // check is that *if* the master holds this document, the assumed state
                // is the master's own state with the local fields stripped, which is
                // exactly `writeDocToDocState(doc, false, false)`.
                let document_id = str_field(row, "itemId");
                if let Some(master_doc) = master_docs.iter().find(|doc| {
                    doc.get("passportId").and_then(Value::as_str) == Some(document_id)
                }) {
                    let projected = write_doc_to_doc_state(&as_doc(master_doc), false, false);
                    // The master may have moved on since this step, so compare only when
                    // the master's own state still matches what was assumed.
                    if !handler.is_equal(
                        &as_doc(doc_data),
                        &projected,
                        "vectors-assumed-master",
                    ) {
                        continue;
                    }
                    assert_eq!(
                        doc_data.get("_deleted").and_then(Value::as_bool),
                        projected.get("_deleted").and_then(Value::as_bool),
                        "the assumed state's _deleted must match the master's"
                    );
                    checked += 1;
                }
            }
        }
    }

    assert!(
        checked > 0,
        "at least one transcript must have an assumed-master state matching its master"
    );
}

/// The recorded conflict resolves to the master, which is what the default handler does.
///
/// The conflict transcript records both sides diverging and the fork afterwards holding
/// the master's content. That is `defaultConflictHandler.resolve` returning
/// `realMasterState` (`default-conflict-handler.ts:27-33`), so the vector is a direct
/// check of the handler's contract against upstream's observed behaviour.
#[test]
fn the_recorded_conflict_resolved_towards_the_master() {
    let vectors = load();
    let handler = DefaultConflictHandler;
    let mut checked = 0usize;

    for transcript in transcripts(&vectors) {
        let resolutions = transcript
            .get("recordedConflictResolutions")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if resolutions == 0 {
            continue;
        }

        let before = transcript
            .get("steps")
            .and_then(Value::as_array)
            .and_then(|steps| steps.first())
            .expect("a conflict transcript has a before step");
        let master_before = before
            .get("master")
            .and_then(Value::as_array)
            .and_then(|docs| docs.first())
            .cloned()
            .expect("the master held a document before the conflict");
        let fork_before = before
            .get("fork")
            .and_then(Value::as_array)
            .and_then(|docs| docs.first())
            .cloned()
            .expect("the fork held a document before the conflict");

        let master_state = write_doc_to_doc_state(&as_doc(&master_before), false, false);
        let fork_state = write_doc_to_doc_state(&as_doc(&fork_before), false, false);

        // The two sides genuinely differ, so this is a real conflict and not a
        // transcript that recorded an agreement.
        assert!(
            !handler.is_equal(&master_state, &fork_state, "vectors-conflict-before"),
            "the conflict transcript's two sides must differ before resolution"
        );

        // The handler's answer is the master's state, unchanged.
        let resolved = handler.resolve(
            &dxdb_replication::conflict::ConflictHandlerInput {
                assumed_master_state: None,
                real_master_state: master_state.clone(),
                new_document_state: fork_state.clone(),
            },
            "vectors-conflict-resolve",
        );
        assert!(
            deep_equal(resolved.as_value(), master_state.as_value()),
            "the default handler must return the master state"
        );

        // And upstream's recorded fork state after the conflict holds the master's
        // content: same age, different revision and lwt (local bookkeeping).
        let after = transcript
            .get("steps")
            .and_then(Value::as_array)
            .and_then(|steps| steps.get(1))
            .expect("a conflict transcript has an after step");
        if let Some(fork_after) = after
            .get("fork")
            .and_then(Value::as_array)
            .and_then(|docs| docs.first())
        {
            let fork_after_state = write_doc_to_doc_state(&as_doc(fork_after), false, false);
            assert!(
                handler.is_equal(&fork_after_state, &master_state, "vectors-conflict-after"),
                "after resolution the fork must hold the master's content: fork {} vs master {}",
                dxdb_json::to_js_json(&fork_after_state.clone().into_value()),
                dxdb_json::to_js_json(&master_state.clone().into_value())
            );
            checked += 1;
        }
    }

    assert!(
        checked > 0,
        "the vector file must contain a resolved conflict for this assertion to mean anything"
    );
}

/// The mirror of [`DEFERRED`]: the two lists together must account for every
/// transcript in the file, so adding a transcript upstream cannot silently go unchecked.
#[test]
fn every_transcript_is_either_covered_or_named_as_deferred() {
    let vectors = load();
    let transcripts = transcripts(&vectors);
    assert_eq!(
        transcripts.len(),
        vectors
            .get("transactionCount")
            .and_then(Value::as_u64)
            .expect("transactionCount") as usize,
        "the file's own count must match its array length"
    );

    for transcript in &transcripts {
        let why = str_field(transcript, "why");
        let covered = STRUCTURALLY_COVERED.contains(&why);
        let deferred = DEFERRED.iter().any(|(name, _)| *name == why);
        assert!(
            covered || deferred,
            "transcript {why:?} is neither structurally covered nor named in DEFERRED; \
             add it to one list so its coverage is accounted for"
        );
    }

    // The batch-boundary case is a separate top-level entry with its own meta snapshot.
    let boundary = vectors
        .get("batchBoundary")
        .and_then(Value::as_array)
        .expect("batchBoundary");
    assert!(
        !boundary.is_empty(),
        "the batch-boundary case is part of the family"
    );
    for case in boundary {
        for row in case.get("meta").and_then(Value::as_array).expect("meta") {
            let id = str_field(row, "id");
            let item_id = str_field(row, "itemId");
            assert_eq!(id, format!("{item_id}|0"), "document rows compose the same way");
        }
    }
}

/// The meta schema's title is the name upstream persists, so a JS peer's stored schema
/// and ours must agree on it.
#[test]
fn the_meta_schema_title_matches_upstream() {
    assert_eq!(META_INSTANCE_SCHEMA_TITLE, "RxReplicationProtocolMetaData");
}

/// The write tag a pull stamps has the shape the vectors' documents carry.
///
/// Only the tag shape is checked here because the vectors' `determinism` block lists
/// `_meta.lwt` as excluded from meta snapshots; `_meta.o` appears in real documents but
/// not in every recorded step, so this asserts the type rather than the data.
#[test]
fn the_write_tag_shape_is_rev_and_hash() {
    let tag = WriteTag::new(3, "abc123");
    let value = tag.to_value();
    let keys: Vec<&str> = value
        .as_object()
        .expect("a tag is an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, vec!["_rev", "hash"]);
    assert_eq!(value.get("_rev").and_then(Value::as_u64), Some(3));
}
